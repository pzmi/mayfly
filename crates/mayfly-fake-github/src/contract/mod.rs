//! The wire contract of the Scale Set protocol (`ScaleSetApi`) and the runner endpoints
//! (`RunnerRegistry`), as `doc/github-scaleset.md` states it: what any server implementing the
//! spec answers, checked over HTTP exchanges alone (ver §5).
//!
//! The suite is a set of cases `WC-01`.. over a factory of [`WireWorld`]s, each case on a fresh
//! world: this fake in process ([`InProcess`]) or on a socket ([`Socket`]), and, owner-gated, a
//! live disposable organisation (HW-02), whose world brings its own [`WireTarget`] and no
//! [`JobScript`]. A case that needs a scripted job runs only where the world has a script and is
//! reported as not run otherwise, never as passed. [`check_wire_contract`] returns a [`Report`]
//! of failed cases; [`broken`] holds deliberately broken fakes, each of which fails named cases
//! (`tests/wire/contract.rs`).
//!
//! The suite sets no deadline of its own (ADR 0023: no timer the test-sleep rule cannot see):
//! against the fake a request is answered or the fake is closed; a live run (HW-02) is bounded
//! by its job's timeout.
//!
//! The suite asserts only what the spec settles (server-observed, doc- or maintainer-stated) or
//! the spec's own reading of an assumption where a fake knob can flip it; where an answer is
//! open it accepts each answer the spec allows (a gone runner: 204 or 404).

mod cases;

pub mod broken;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::knobs::{Flow, Knobs};
use crate::state::{FakeConfig, FakeGithub, FakeRequest, Scope};
use crate::world::JobSpec;

pub use cases::CASES;

/// A request of the suite: an absolute URL, since the service and queue URLs come from answers.
#[derive(Clone, PartialEq, Eq)]
pub struct WireRequest {
    /// `GET`, `POST`, ...
    pub method: String,
    /// The absolute URL, with its query.
    pub url: String,
    /// Headers.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl std::fmt::Debug for WireRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WireRequest")
            .field("method", &self.method)
            .field("path", &path_of(&self.url))
            .finish_non_exhaustive()
    }
}

/// An answer to the suite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireResponse {
    /// The status.
    pub status: u16,
    /// Headers, lowercase names.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl WireResponse {
    /// The body as JSON, `Null` when it is not.
    #[must_use]
    pub fn json(&self) -> serde_json::Value {
        let body = self
            .body
            .strip_prefix("\u{feff}".as_bytes())
            .unwrap_or(&self.body);
        serde_json::from_slice(body).unwrap_or(serde_json::Value::Null)
    }

    /// The `typeName` of an error body, empty when it has none.
    #[must_use]
    pub fn type_name(&self) -> String {
        self.json()["typeName"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }
}

/// Sends one request and returns the answer, or why there was none.
pub trait WireTarget: Send + Sync {
    /// Sends `request`.
    fn send(
        &self,
        request: WireRequest,
    ) -> impl Future<Output = Result<WireResponse, String>> + Send;
}

/// What GitHub's users and runners do, scripted: the fake's control API here; on a live
/// organisation, whatever HW-02 drives (a workflow dispatch, a runner).
pub trait JobScript: Send + Sync {
    /// Queues `count` jobs asking for `label`, of workflow run `run_id` triggered by `event`
    /// from `head_repository` (`owner/name`; empty: the base repository).
    fn queue_jobs(
        &self,
        count: usize,
        label: &str,
        run_id: u64,
        event: &str,
        head_repository: &str,
    ) -> impl Future<Output = Result<(), String>> + Send;
    /// The runner `runner_id` takes the assigned job `job_id` (the `jobId` of its message).
    fn start_job(
        &self,
        job_id: &str,
        runner_id: u64,
    ) -> impl Future<Output = Result<(), String>> + Send;
    /// The service expires the scale set's live session.
    fn expire_session(&self, scale_set: u64) -> impl Future<Output = Result<(), String>> + Send;
    /// The live session's queue token stops working.
    fn expire_queue_token(&self, scale_set: u64)
    -> impl Future<Output = Result<(), String>> + Send;
}

/// Where a world's App, installation and organisation are.
#[derive(Clone)]
pub struct WireSetup {
    /// The REST base (`https://api.github.com`, `https://<host>/api/v3`).
    pub api_base: String,
    /// The config URL of the runner-registration exchange (`https://github.com/<org>`).
    pub config_url: String,
    /// The registration-token path (`/orgs/<org>/actions/runners/registration-token`).
    pub registration_path: String,
    /// The installation.
    pub installation_id: u64,
    /// The App's client id.
    pub app_client_id: String,
    /// The App's private key, PKCS#1 PEM. Synthetic for the fake.
    pub app_private_pem: String,
    /// The runner group scale sets go in.
    pub runner_group: String,
    /// The organisation and repository runs belong to.
    pub owner: String,
    /// The repository runs belong to.
    pub repository: String,
    /// A name no scale set or runner of this world has used, to keep cases apart on a live
    /// organisation.
    pub unique: String,
    /// The time, Unix-epoch seconds.
    pub now_secs: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl std::fmt::Debug for WireSetup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WireSetup")
            .field("api_base", &self.api_base)
            .finish_non_exhaustive()
    }
}

/// A world the suite runs one case in.
pub trait WireWorld: WireTarget {
    /// Its script.
    type Script: JobScript;
    /// Where its App and organisation are.
    fn setup(&self) -> &WireSetup;
    /// Its script, if it has one.
    fn script(&self) -> Option<&Self::Script>;
}

/// The outcome of a run of the suite.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Cases that passed.
    pub passed: Vec<&'static str>,
    /// Cases that failed, with the first broken expectation.
    pub failed: Vec<(&'static str, String)>,
    /// Cases not run, with the reason (a world without a script or without the flow).
    pub not_run: Vec<(&'static str, &'static str)>,
}

impl Report {
    /// The ids of the failed cases.
    #[must_use]
    pub fn failed_ids(&self) -> Vec<&'static str> {
        self.failed.iter().map(|(id, _)| *id).collect()
    }
}

/// Runs every case of [`CASES`], each on a fresh world from `world`, which answers `None` for a
/// flow it cannot serve (a live github.com organisation has no GHES flow).
pub async fn check_wire_contract<W, F, Fut>(world: F) -> Report
where
    W: WireWorld,
    F: Fn(Flow) -> Fut,
    Fut: Future<Output = Option<W>>,
{
    let mut report = Report::default();
    for case in CASES {
        let Some(w) = world(case.flow).await else {
            report
                .not_run
                .push((case.id, "the world does not serve this flow"));
            continue;
        };
        if case.needs_script && w.script().is_none() {
            report
                .not_run
                .push((case.id, "the world has no job script"));
            continue;
        }
        match cases::run(case.id, &w).await {
            Ok(()) => report.passed.push(case.id),
            Err(why) => report.failed.push((case.id, why)),
        }
    }
    report
}

fn path_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.find('/').map_or("/", |at| &rest[at..])
}

/// The fake answered in process.
#[derive(Debug, Clone)]
pub struct InProcess(pub FakeGithub);

impl WireTarget for InProcess {
    async fn send(&self, request: WireRequest) -> Result<WireResponse, String> {
        let answer = self
            .0
            .handle(FakeRequest {
                method: request.method,
                path: path_of(&request.url).to_owned(),
                headers: request.headers,
                body: request.body,
            })
            .await;
        Ok(WireResponse {
            status: answer.status,
            headers: answer.headers,
            body: answer.body,
        })
    }
}

/// A server on a loopback socket, reached by a minimal HTTP/1.1 client of the suite's own
/// (one request per connection), whatever host the URL names.
#[derive(Debug, Clone, Copy)]
pub struct Socket(pub SocketAddr);

impl WireTarget for Socket {
    async fn send(&self, request: WireRequest) -> Result<WireResponse, String> {
        exchange(self.0, request).await
    }
}

async fn exchange(addr: SocketAddr, request: WireRequest) -> Result<WireResponse, String> {
    let mut stream = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
    let mut head = format!(
        "{} {} HTTP/1.1\r\nhost: {}\r\nconnection: close\r\ncontent-length: {}\r\n",
        request.method,
        path_of(&request.url),
        addr,
        request.body.len()
    );
    for (name, value) in &request.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stream
        .write_all(&request.body)
        .await
        .map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .await
        .map_err(|e| e.to_string())?;
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Result<WireResponse, String> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("no end of headers")?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|e| e.to_string())?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or("no status line")?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let mut body = raw[split + 4..].to_vec();
    if headers
        .iter()
        .any(|(n, v)| n == "transfer-encoding" && v.eq_ignore_ascii_case("chunked"))
    {
        body = dechunk(&body)?;
    }
    Ok(WireResponse {
        status,
        headers,
        body,
    })
}

fn dechunk(mut raw: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let line_end = raw
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("bad chunk")?;
        let size = std::str::from_utf8(&raw[..line_end])
            .ok()
            .and_then(|s| usize::from_str_radix(s.split(';').next()?.trim(), 16).ok())
            .ok_or("bad chunk size")?;
        raw = &raw[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        out.extend_from_slice(raw.get(..size).ok_or("short chunk")?);
        raw = raw.get(size + 2..).ok_or("short chunk")?;
    }
}

/// The fake's world: a target over it and its control API as the script.
#[derive(Debug, Clone)]
pub struct FakeWorld<T> {
    /// The fake, for its control API.
    pub fake: FakeGithub,
    /// How the suite reaches it.
    pub target: T,
    /// Its App and organisation.
    pub setup: WireSetup,
}

impl<T: WireTarget> WireTarget for FakeWorld<T> {
    fn send(
        &self,
        request: WireRequest,
    ) -> impl Future<Output = Result<WireResponse, String>> + Send {
        self.target.send(request)
    }
}

impl<T: WireTarget> WireWorld for FakeWorld<T> {
    type Script = FakeGithub;

    fn setup(&self) -> &WireSetup {
        &self.setup
    }

    fn script(&self) -> Option<&FakeGithub> {
        Some(&self.fake)
    }
}

impl JobScript for FakeGithub {
    async fn queue_jobs(
        &self,
        count: usize,
        label: &str,
        run_id: u64,
        event: &str,
        head_repository: &str,
    ) -> Result<(), String> {
        for _ in 0..count {
            self.queue_job(JobSpec {
                run_id,
                event: event.to_owned(),
                head_repository: head_repository.to_owned(),
                labels: vec![label.to_owned()],
                ..JobSpec::default()
            });
        }
        Ok(())
    }

    async fn start_job(&self, job_id: &str, runner_id: u64) -> Result<(), String> {
        let job = job_id.parse().map_err(|_| "not a fake job id".to_owned())?;
        FakeGithub::start_job(self, job, runner_id)
    }

    async fn expire_session(&self, scale_set: u64) -> Result<(), String> {
        FakeGithub::expire_session(self, scale_set)
    }

    async fn expire_queue_token(&self, scale_set: u64) -> Result<(), String> {
        FakeGithub::expire_queue_token(self, scale_set)
    }
}

/// The App client id of [`contract_fake`].
pub const CONTRACT_APP: &str = "Iv1.contract";
/// The organisation of [`contract_fake`]; [`JobSpec::default`] runs are its repository's.
pub const CONTRACT_ORG: &str = "acme";

/// A fake and the setup the suite reaches it with: organisation scope, every permission the
/// suite's routes need (spec §13), `flow`, and the App key `public_pem`/`private_pem`
/// (generate them at run time, [`crate::generate_app_key`]).
#[must_use]
pub fn contract_fake(
    public_pem: &str,
    private_pem: &str,
    flow: Flow,
    now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
) -> (FakeGithub, WireSetup) {
    let config = FakeConfig {
        app_client_id: CONTRACT_APP.to_owned(),
        app_public_key_pem: public_pem.to_owned(),
        installation_id: 11,
        scope: Scope::Organization(CONTRACT_ORG.to_owned()),
        web_base: "https://github.com".to_owned(),
        service_url: "http://fake.invalid/service".to_owned(),
        admin_token_lifetime_secs: 3600,
        admin_token_key_pem: private_pem.to_owned(),
        permissions: [
            ("organization_self_hosted_runners", "write"),
            ("actions", "write"),
            ("organization_administration", "read"),
            ("metadata", "read"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect(),
    };
    let knobs = Knobs {
        flow,
        queue_url_base: Some("http://broker.fake.invalid/broker".to_owned()),
        ..Knobs::default()
    };
    let reader = Arc::clone(&now_ms);
    let fake = FakeGithub::with_knobs(config, knobs, now_ms);
    let setup = WireSetup {
        api_base: "http://api.fake.invalid".to_owned(),
        config_url: format!("https://github.com/{CONTRACT_ORG}"),
        registration_path: format!("/orgs/{CONTRACT_ORG}/actions/runners/registration-token"),
        installation_id: 11,
        app_client_id: CONTRACT_APP.to_owned(),
        app_private_pem: private_pem.to_owned(),
        runner_group: "Default".to_owned(),
        owner: CONTRACT_ORG.to_owned(),
        repository: "app".to_owned(),
        unique: "contract".to_owned(),
        now_secs: Arc::new(move || reader() / 1000),
    };
    (fake, setup)
}
