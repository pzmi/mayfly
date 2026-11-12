//! A fake on a paused clock and a small client of the tests' own.

use std::sync::{Arc, OnceLock};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rsa::PrivateKey;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::sha2::Sha256;
use serde_json::{Value, json};

use mayfly_fake_github::contract::{WireSetup, contract_fake};
use mayfly_fake_github::{
    AppKeyPair, FakeGithub, FakeRequest, FakeResponse, Flow, JobSpec, Knobs, generate_app_key,
};

/// 2026-11-10T12:00:00Z.
pub const START_MS: u64 = 1_794_312_000_000;

/// One synthetic key pair per test binary: generating one takes about a second.
pub fn keys() -> &'static AppKeyPair {
    static KEYS: OnceLock<AppKeyPair> = OnceLock::new();
    KEYS.get_or_init(generate_app_key)
}

/// Wall-clock milliseconds that advance with Tokio's (paused) clock. Call inside the runtime.
pub fn clock() -> Arc<dyn Fn() -> u64 + Send + Sync> {
    let base = tokio::time::Instant::now();
    Arc::new(move || START_MS + u64::try_from(base.elapsed().as_millis()).unwrap_or(u64::MAX))
}

/// The contract fake (organisation `acme`, every permission) with `knobs`.
pub fn fake_with(knobs: Knobs) -> (FakeGithub, WireSetup) {
    let keys = keys();
    let (fake, setup) = contract_fake(&keys.public_pem, &keys.private_pem, knobs.flow, clock());
    let queue_url_base = fake.knobs().queue_url_base;
    fake.update_knobs(|k| {
        *k = Knobs {
            queue_url_base,
            ..knobs
        }
    });
    (fake, setup)
}

/// The contract fake with the default knobs.
pub fn fake() -> (FakeGithub, WireSetup) {
    fake_with(Knobs::default())
}

/// A job asking for the label `mf`, of run `run_id`.
pub fn job(run_id: u64) -> JobSpec {
    JobSpec {
        run_id,
        labels: vec!["mf".to_owned()],
        ..JobSpec::default()
    }
}

fn path(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.find('/')
        .map_or("/".to_owned(), |at| rest[at..].to_owned())
}

/// A request to the fake; `url` may be absolute.
pub fn req(method: &str, url: &str, auth: Option<&str>, body: Option<Value>) -> FakeRequest {
    let mut headers = Vec::new();
    if let Some(auth) = auth {
        headers.push(("authorization".to_owned(), auth.to_owned()));
    }
    FakeRequest {
        method: method.to_owned(),
        path: path(url),
        headers,
        body: body.map(|b| b.to_string().into_bytes()).unwrap_or_default(),
    }
}

/// A session as the tests hold it.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub queue: String,
    pub token: String,
}

/// A connected client: the installation token, the service URL and an admin token.
pub struct Api {
    pub fake: FakeGithub,
    pub installation: String,
    pub service: String,
    pub admin: String,
}

impl Api {
    /// Runs the auth chain.
    pub async fn connect(fake: &FakeGithub, setup: &WireSetup) -> Self {
        let now = (setup.now_secs)();
        let key = PrivateKey::from_pkcs1_pem(&setup.app_private_pem).expect("key");
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = URL_SAFE_NO_PAD.encode(
            json!({"iat": now - 60, "exp": now + 540, "iss": setup.app_client_id}).to_string(),
        );
        let signed = format!("{header}.{claims}");
        let signature = key
            .pkcs1v15_sign::<Sha256>(signed.as_bytes())
            .expect("sign");
        let jwt = format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature));
        let access = format!("/app/installations/{}/access_tokens", setup.installation_id);
        let installation = token(
            &fake
                .handle(req("POST", &access, Some(&format!("Bearer {jwt}")), None))
                .await,
        );
        let registration = token(
            &fake
                .handle(req(
                    "POST",
                    &setup.registration_path,
                    Some(&format!("Bearer {installation}")),
                    None,
                ))
                .await,
        );
        let exchange = fake
            .handle(req(
                "POST",
                "/actions/runner-registration",
                Some(&format!("RemoteAuth {registration}")),
                Some(json!({"url": setup.config_url, "runner_event": "register"})),
            ))
            .await
            .json();
        Self {
            fake: fake.clone(),
            installation,
            service: exchange["url"].as_str().expect("url").to_owned(),
            admin: exchange["token"].as_str().expect("admin").to_owned(),
        }
    }

    /// A service call with the admin token and `api-version`.
    pub async fn svc(&self, method: &str, path: &str, body: Option<Value>) -> FakeResponse {
        let separator = if path.contains('?') { '&' } else { '?' };
        let url = format!(
            "{}/_apis/{path}{separator}api-version=6.0-preview",
            self.service
        );
        self.fake
            .handle(req(
                method,
                &url,
                Some(&format!("Bearer {}", self.admin)),
                body,
            ))
            .await
    }

    /// A REST call with the installation token.
    pub async fn rest(&self, method: &str, path: &str) -> FakeResponse {
        self.fake
            .handle(req(
                method,
                path,
                Some(&format!("Bearer {}", self.installation)),
                None,
            ))
            .await
    }

    /// Creates the scale set `mf` (label `mf`) in the default group.
    pub async fn scale_set(&self) -> u64 {
        let created = self
            .svc(
                "POST",
                "runtime/runnerscalesets",
                Some(json!({"name": "mf", "runnerGroupId": 1})),
            )
            .await;
        assert_eq!(created.status, 200, "create scale set");
        created.json()["id"].as_u64().expect("id")
    }

    /// Opens a session on `scale_set`.
    pub async fn open(&self, scale_set: u64) -> Session {
        let answer = self
            .svc(
                "POST",
                &format!("runtime/runnerscalesets/{scale_set}/sessions"),
                Some(json!({"ownerName": "host-a/pool"})),
            )
            .await;
        assert_eq!(answer.status, 200, "create session");
        session(&answer.json())
    }

    /// Polls with `capacity`, after `last` when given.
    pub async fn poll(&self, s: &Session, capacity: u64, last: Option<u64>) -> FakeResponse {
        let url = match last {
            Some(last) => format!("{}?lastMessageId={last}", s.queue),
            None => s.queue.clone(),
        };
        let mut request = req("GET", &url, Some(&format!("Bearer {}", s.token)), None);
        request
            .headers
            .push(("X-ScaleSetMaxCapacity".to_owned(), capacity.to_string()));
        self.fake.handle(request).await
    }

    /// Acks `message`.
    pub async fn ack(&self, s: &Session, message: u64) -> FakeResponse {
        self.fake
            .handle(req(
                "DELETE",
                &format!("{}/{message}", s.queue),
                Some(&format!("Bearer {}", s.token)),
                None,
            ))
            .await
    }

    /// Mints a JIT config for `name`; returns the runner id.
    pub async fn jit(&self, scale_set: u64, name: &str) -> FakeResponse {
        self.svc(
            "POST",
            &format!("runtime/runnerscalesets/{scale_set}/generatejitconfig"),
            Some(json!({"name": name, "workFolder": "_work"})),
        )
        .await
    }
}

/// A session from a create or refresh answer.
pub fn session(body: &Value) -> Session {
    Session {
        id: body["sessionId"].as_str().expect("sessionId").to_owned(),
        queue: body["messageQueueUrl"].as_str().expect("queue").to_owned(),
        token: body["messageQueueAccessToken"]
            .as_str()
            .expect("token")
            .to_owned(),
    }
}

fn token(answer: &FakeResponse) -> String {
    assert_eq!(answer.status, 201, "token");
    answer.json()["token"].as_str().expect("token").to_owned()
}

/// The id and job messages of a 200 poll.
pub fn message(answer: &FakeResponse) -> (u64, Vec<Value>) {
    assert_eq!(answer.status, 200, "poll answered {}", answer.status);
    let body = answer.json();
    let jobs = serde_json::from_str(body["body"].as_str().expect("body")).expect("array");
    (body["messageId"].as_u64().expect("id"), jobs)
}

/// The default knobs with `flow`.
pub fn knobs(flow: Flow) -> Knobs {
    Knobs {
        flow,
        ..Knobs::default()
    }
}
