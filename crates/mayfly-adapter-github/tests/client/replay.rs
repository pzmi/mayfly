//! The recorded-fixture replayer (spec §10, plan §7.4): the fixtures in
//! `tests/fixtures/github-scaleset/` are the contract's wire, one file per case of
//! `mayfly_testkit::contracts::scaleset`. The replayer answers the client's requests from a
//! case's exchanges, in order, and refuses any request the recording does not hold (method, URL,
//! query, credential kind, the headers and JSON body it names), so the client passes the suite
//! here only by sending exactly the recorded requests.
//!
//! The fixtures are **synthetic**: written from `doc/github-scaleset.md`, never captured from
//! GitHub and never copied from upstream (ADR 0048); each says so in its `synthetic` and `note`
//! fields, and GH-00's redacting capture (HW-02) replaces them. They hold no credential: every
//! token is minted here at run time and named in a fixture only by a placeholder
//! (`<token-N>`), so a fixture is safe to publish.
//!
//! The auth chain (spec §1) is not part of a recording: the replayer answers it itself.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use mayfly_adapter_github::{HttpRequest, HttpResponse, Transport, TransportError};
use mayfly_control::github::{RunnerId, WorkflowJobId};
use mayfly_domain::Millis;
use mayfly_testkit::clock::FakeClock;
use mayfly_testkit::contracts::scaleset::{
    CASES, ScaleSetCase, ScaleSetWorld, WorldFault, check_scaleset_contract,
};
use serde_json::{Value, json};

use crate::contract::{Client, client};
use crate::support::{INSTALLATION, ORG, START};

/// Where the recordings live.
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/github-scaleset")
}

/// The service URL the replayer's exchange answers (spec §1 step 4).
const SERVICE: &str = "https://pipelines.example/svc";

#[derive(Debug, Clone)]
struct Exchange {
    method: String,
    /// Scheme, host and path.
    url: String,
    query: BTreeMap<String, String>,
    /// `admin`, `token-N` (a queue token a session answered), or `none`.
    auth: String,
    headers: BTreeMap<String, String>,
    body: Option<Value>,
    response: Response,
}

#[derive(Debug, Clone)]
enum Response {
    Answer {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// The connection broke after the request took effect: a lost answer.
    Lost,
}

#[derive(Debug, Default)]
struct ReplayState {
    exchanges: VecDeque<Exchange>,
    admin_token: String,
    mismatches: Vec<String>,
}

/// A transport that answers from a recording.
#[derive(Debug, Clone)]
pub struct Replay {
    state: Arc<Mutex<ReplayState>>,
    tokens: Arc<BTreeMap<String, String>>,
}

/// The run-time secret a placeholder `<token-N>` stands for (a queue token, a JIT config): built
/// from its parts, so no token-shaped literal is in the repository.
fn run_time_token(n: &str) -> String {
    ["replay", n, "credential"].join("-")
}

impl Replay {
    /// A replayer of case file `name` (with the files it includes).
    pub fn load(name: &str) -> Self {
        let mut exchanges = VecDeque::new();
        let mut tokens = BTreeMap::new();
        load_into(name, &mut exchanges, &mut tokens);
        Self {
            state: Arc::new(Mutex::new(ReplayState {
                exchanges,
                admin_token: String::new(),
                mismatches: Vec::new(),
            })),
            tokens: Arc::new(tokens),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ReplayState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Requests that did not match, and exchanges never asked for.
    pub fn problems(&self) -> Vec<String> {
        let state = self.state();
        let mut problems = state.mismatches.clone();
        problems.extend(
            state
                .exchanges
                .iter()
                .map(|e| format!("not replayed: {} {}", e.method, e.url)),
        );
        problems
    }

    fn auth_chain(&self, request: &HttpRequest) -> Option<HttpResponse> {
        let path = request.url.path();
        let now_secs = START.get() / 1000;
        let installation = format!("/app/installations/{INSTALLATION}/access_tokens");
        let registration = format!("/orgs/{ORG}/actions/runners/registration-token");
        let body = if path == installation {
            json!({
                "token": (["replay", "installation", "credential"].join("-")),
                "expires_at": "2026-11-10T13:00:00Z",
                "permissions": {"organization_self_hosted_runners": "write"},
            })
        } else if path == registration {
            json!({
                "token": (["replay", "registration", "credential"].join("-")),
                "expires_at": "2026-11-10T13:00:00Z",
            })
        } else if path == "/actions/runner-registration" {
            let claims = URL_SAFE_NO_PAD.encode(json!({"exp": now_secs + 3600}).to_string());
            let header = URL_SAFE_NO_PAD.encode(json!({"alg": "none"}).to_string());
            let token = [header, claims, "replay".to_owned()].join(".");
            self.state().admin_token.clone_from(&token);
            json!({"url": SERVICE, "token": token})
        } else {
            return None;
        };
        Some(HttpResponse {
            status: 201,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: body.to_string().into_bytes(),
        })
    }

    fn expected_credential(&self, auth: &str, admin: &str) -> Option<String> {
        match auth {
            "admin" => Some(format!("Bearer {admin}")),
            "none" => None,
            queue => Some(format!(
                "Bearer {}",
                self.tokens.get(queue).cloned().unwrap_or_default()
            )),
        }
    }

    fn check(&self, expected: &Exchange, request: &HttpRequest, admin: &str) -> Vec<String> {
        let mut wrong = Vec::new();
        let at = format!("{} {}", expected.method, expected.url);
        if request.method.as_str() != expected.method {
            wrong.push(format!("{at}: method {}", request.method.as_str()));
        }
        let mut url = request.url.clone();
        url.set_query(None);
        if url.as_str() != expected.url {
            wrong.push(format!("{at}: url {url}"));
        }
        let query: BTreeMap<String, String> = request
            .url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        if query != expected.query {
            wrong.push(format!("{at}: query {query:?}"));
        }
        let credential = request.authorization.as_ref().map(|c| c.expose().clone());
        if credential != self.expected_credential(&expected.auth, admin) {
            wrong.push(format!("{at}: not the {} credential", expected.auth));
        }
        for (name, value) in &expected.headers {
            let sent = request
                .headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str());
            if sent != Some(value.as_str()) {
                wrong.push(format!("{at}: header {name} = {sent:?}"));
            }
        }
        if let Some(body) = &expected.body {
            let sent: Option<Value> = request
                .body
                .as_deref()
                .and_then(|b| serde_json::from_slice(b).ok());
            if sent.as_ref() != Some(body) {
                wrong.push(format!("{at}: body {sent:?}"));
            }
        }
        wrong
    }
}

impl Transport for Replay {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        if let Some(answer) = self.auth_chain(&request) {
            return Ok(answer);
        }
        let (next, admin) = {
            let mut state = self.state();
            (state.exchanges.pop_front(), state.admin_token.clone())
        };
        let Some(expected) = next else {
            self.state().mismatches.push(format!(
                "unrecorded request: {} {}",
                request.method.as_str(),
                request.url.path()
            ));
            return Ok(refusal());
        };
        let wrong = self.check(&expected, &request, &admin);
        if !wrong.is_empty() {
            self.state().mismatches.extend(wrong);
            return Ok(refusal());
        }
        match expected.response {
            Response::Answer {
                status,
                headers,
                body,
            } => Ok(HttpResponse {
                status,
                headers,
                body,
            }),
            Response::Lost => Err(TransportError::Io),
        }
    }
}

/// What the replayer answers a request the recording does not hold: a refusal no case expects.
fn refusal() -> HttpResponse {
    HttpResponse {
        status: 418,
        headers: Vec::new(),
        body: br#"{"message":"not in the recording"}"#.to_vec(),
    }
}

fn read(name: &str) -> Value {
    let path = fixtures().join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path).expect("a fixture");
    serde_json::from_str(&text).expect("a JSON fixture")
}

/// Replaces every string `<token-N>` in `value` with its run-time token.
fn substitute(value: &mut Value, tokens: &mut BTreeMap<String, String>) {
    match value {
        Value::String(s) => {
            if let Some(n) = s.strip_prefix("<token-").and_then(|r| r.strip_suffix('>')) {
                let token = run_time_token(n);
                tokens.insert(format!("token-{n}"), token.clone());
                *s = token;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| substitute(v, tokens)),
        Value::Object(fields) => fields.values_mut().for_each(|v| substitute(v, tokens)),
        _ => {}
    }
}

fn load_into(
    name: &str,
    exchanges: &mut VecDeque<Exchange>,
    tokens: &mut BTreeMap<String, String>,
) {
    let fixture = read(name);
    for include in fixture["include"].as_array().into_iter().flatten() {
        load_into(
            include.as_str().expect("an include name"),
            exchanges,
            tokens,
        );
    }
    for exchange in fixture["exchanges"].as_array().expect("exchanges") {
        let request = &exchange["request"];
        let response = &exchange["response"];
        let strings = |v: &Value| -> BTreeMap<String, String> {
            v.as_object()
                .into_iter()
                .flatten()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
                .collect()
        };
        let response = if response["lost"].as_bool() == Some(true) {
            Response::Lost
        } else {
            let body = match (&response["json"], &response["text"]) {
                (Value::Null, Value::String(text)) => text.clone().into_bytes(),
                (Value::Null, _) => Vec::new(),
                (json, _) => {
                    let mut json = json.clone();
                    substitute(&mut json, tokens);
                    json.to_string().into_bytes()
                }
            };
            Response::Answer {
                status: u16::try_from(response["status"].as_u64().expect("a status"))
                    .expect("a status"),
                headers: strings(&response["headers"]).into_iter().collect(),
                body,
            }
        };
        exchanges.push_back(Exchange {
            method: request["method"].as_str().expect("a method").to_owned(),
            url: request["url"].as_str().expect("a url").to_owned(),
            query: strings(&request["query"]),
            auth: request["auth"].as_str().unwrap_or("admin").to_owned(),
            headers: strings(&request["headers"]),
            body: match &request["json"] {
                Value::Null => None,
                json => Some(json.clone()),
            },
            response,
        });
    }
}

fn file_name(case: &str) -> String {
    case.to_ascii_lowercase()
}

struct ReplayWorld {
    replay: Replay,
    clock: FakeClock,
    client: Client<Replay>,
}

impl ReplayWorld {
    fn new(case: &str) -> Self {
        let clock = FakeClock::at(START);
        let replay = Replay::load(&file_name(case));
        let client = client(replay.clone(), &clock);
        Self {
            replay,
            clock,
            client,
        }
    }
}

impl ScaleSetWorld for ReplayWorld {
    type Api = Client<Replay>;
    type Registry = Client<Replay>;

    fn api(&self) -> &Client<Replay> {
        &self.client
    }

    fn registry(&self) -> &Client<Replay> {
        &self.client
    }

    // A recording already holds what GitHub's users did: the script is its exchanges.
    async fn queue_jobs(&self, _count: usize, _label: &str, _run_id: u64) {}

    async fn start_job(&self, _job: &WorkflowJobId, _runner: RunnerId) {}

    async fn inject(&self, _fault: WorldFault) {}

    fn slept(&self) -> Vec<Millis> {
        self.clock.sleeps()
    }
}

#[tokio::test(start_paused = true)]
async fn should_pass_the_ports_contract_when_the_recordings_are_replayed() {
    // given
    let problems = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&problems);
    let world = move |case: ScaleSetCase| {
        let seen = Arc::clone(&seen);
        async move {
            let world = ReplayWorld::new(case.id);
            seen.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((case.id, world.replay.clone()));
            world
        }
    };

    // when
    let report = check_scaleset_contract(world).await;

    // then
    assert!(report.failed.is_empty(), "{:#?}", report.failed);
    let unmatched: Vec<(&str, Vec<String>)> = problems
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .map(|(id, replay)| (*id, replay.problems()))
        .filter(|(_, p)| !p.is_empty())
        .collect();
    assert!(unmatched.is_empty(), "{unmatched:#?}");
}

#[test]
fn should_label_every_recording_synthetic_when_no_capture_has_replaced_it() {
    // given
    let mut names: Vec<String> = std::fs::read_dir(fixtures())
        .expect("the fixtures directory")
        .map(|e| {
            e.expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();

    // when
    let unlabelled: Vec<&String> = names
        .iter()
        .filter(|n| {
            let fixture = read(n.trim_end_matches(".json"));
            fixture["synthetic"] != Value::Bool(true)
                || !fixture["note"]
                    .as_str()
                    .is_some_and(|t| t.contains("Synthetic"))
        })
        .collect();

    // then: every case has its recording, and every recording is labelled
    assert!(unlabelled.is_empty(), "{unlabelled:?}");
    for case in CASES {
        assert!(
            names.contains(&format!("{}.json", file_name(case.id))),
            "no recording for {}",
            case.id
        );
    }
}
