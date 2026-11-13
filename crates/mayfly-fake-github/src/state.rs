//! The fake's state and its in-process request handler.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, Semaphore, watch};

use crate::knobs::Knobs;
use crate::routes::{AuthPaths, Params, Route, route, split_query, url_path};
use crate::timefmt::rfc3339;
use crate::token::{admin_token, verify_app_jwt};
use crate::wire::{header, json, status};
use crate::world::World;
use crate::{control, rest, service};

pub use crate::token::JwtClaims;

/// Installation and registration tokens live one hour [docs].
const HOUR_SECS: u64 = 3600;

/// Where the App is installed and runners register: the path of the registration-token route
/// and the config URL the runner-registration exchange expects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// `/orgs/{org}/...`, config URL `https://github.com/{org}`.
    Organization(String),
    /// `/repos/{owner}/{repo}/...`, config URL `https://github.com/{owner}/{repo}`.
    Repository(String, String),
}

/// What the fake accepts and answers. Its `Debug` leaves the admin token key out.
#[derive(Clone, Serialize, Deserialize)]
pub struct FakeConfig {
    /// The App's client id, the issuer an App JWT must carry.
    pub app_client_id: String,
    /// The App's public key, PKCS#1 PEM ([`crate::AppKeyPair::public_pem`]).
    pub app_public_key_pem: String,
    /// The installation the access-token route serves.
    pub installation_id: u64,
    /// The scope runners register in.
    pub scope: Scope,
    /// The web base of the config URL, `https://github.com` unless GHES.
    pub web_base: String,
    /// The Actions service URL the exchange answers. Its path is the prefix of the service's
    /// routes; its host is not read (one listener serves every host).
    pub service_url: String,
    /// How long an admin token lives. [open: HW-02] (spec §12 item 5): short in tests so a client
    /// must refresh.
    pub admin_token_lifetime_secs: u64,
    /// The private key (PKCS#1 PEM, generated at run time) admin tokens are signed with, as RS256
    /// JWTs; no client verifies them (spec §1 step 4). A key that does not sign makes the
    /// exchange answer 500.
    pub admin_token_key_pem: String,
    /// The installation's granted permissions, as the access-token answer lists them; the REST
    /// routes of ADR 0090 refuse with 403 what they do not grant (ADR 0045).
    pub permissions: BTreeMap<String, String>,
}

impl fmt::Debug for FakeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeConfig")
            .field("app_client_id", &self.app_client_id)
            .field("installation_id", &self.installation_id)
            .field("scope", &self.scope)
            .field("web_base", &self.web_base)
            .field("service_url", &self.service_url)
            .field("admin_token_lifetime_secs", &self.admin_token_lifetime_secs)
            .field("admin_token_key_pem", &"<redacted>")
            .field("permissions", &self.permissions)
            .finish_non_exhaustive()
    }
}

/// A fault that replaces or bends one answer of a route, in the order queued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Fault {
    /// Answer this instead, without effect.
    Respond {
        /// The status.
        status: u16,
        /// Extra headers (`retry-after`, `x-ratelimit-remaining`).
        #[serde(default)]
        headers: Vec<(String, String)>,
        /// The body.
        #[serde(default)]
        body: String,
    },
    /// Never answer (a stalled service or a stalled poll); the client's deadline must end the
    /// wait. Ends with 503 only when the fake closes.
    Stall,
    /// Answer the runner-registration exchange with an admin token that has no `exp` claim.
    AdminTokenWithoutExp,
    /// Take the request's effect, then never answer: a response lost after its effect. Pinned by:
    /// `tests/wire/faults.rs::should_take_the_effect_and_never_answer_when_dropped_after_effect`.
    DropAfterEffect,
    /// Wait this long, then answer normally.
    Delay {
        /// Milliseconds.
        ms: u64,
    },
}

impl Fault {
    /// A bare status, with a message body.
    #[must_use]
    pub fn status(status: u16) -> Self {
        Self::Respond {
            status,
            headers: Vec::new(),
            body: serde_json::json!({"message": "fault injected"}).to_string(),
        }
    }

    /// An error of a class: `{"typeName", "message"}` (spec §2).
    #[must_use]
    pub fn typed(status: u16, type_name: &str, message: &str) -> Self {
        Self::Respond {
            status,
            headers: Vec::new(),
            body: serde_json::json!({"typeName": type_name, "message": message}).to_string(),
        }
    }

    /// A status with `Retry-After: <secs>` (spec §8).
    #[must_use]
    pub fn retry_after(status: u16, secs: u64) -> Self {
        Self::Respond {
            status,
            headers: vec![("retry-after".to_owned(), secs.to_string())],
            body: serde_json::json!({"message": "slow down"}).to_string(),
        }
    }

    /// A primary rate limit's 403: `X-RateLimit-Remaining: 0` and the reset instant (spec §8).
    #[must_use]
    pub fn rate_limited_403(reset_epoch_secs: u64) -> Self {
        Self::Respond {
            status: 403,
            headers: vec![
                ("x-ratelimit-remaining".to_owned(), "0".to_owned()),
                ("x-ratelimit-reset".to_owned(), reset_epoch_secs.to_string()),
            ],
            body: serde_json::json!({"message": "API rate limit exceeded"}).to_string(),
        }
    }

    /// JIT minting throttled: 429 `ConfigureTooManyRunnersException` (spec §8), with
    /// `Retry-After` when given (whether the service sends it is `[open: HW-02, item 6]`).
    #[must_use]
    pub fn too_many_runners(retry_after_secs: Option<u64>) -> Self {
        Self::Respond {
            status: 429,
            headers: retry_after_secs
                .map(|s| vec![("retry-after".to_owned(), s.to_string())])
                .unwrap_or_default(),
            body: serde_json::json!({
                "typeName": crate::wire::TOO_MANY_RUNNERS,
                "message": "Too many runners are being configured.",
            })
            .to_string(),
        }
    }

    /// A 200 with an empty body (server-observed on a poll, spec §4.1).
    #[must_use]
    pub fn empty_ok() -> Self {
        Self::Respond {
            status: 200,
            headers: Vec::new(),
            body: String::new(),
        }
    }
}

/// A request in the fake's own terms.
#[derive(Debug, Clone)]
pub struct FakeRequest {
    /// `GET`, `POST`, ...
    pub method: String,
    /// The path, with its query (`/x?a=1`) when it has one.
    pub path: String,
    /// Headers, names in any case.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

/// An answer in the fake's own terms.
#[derive(Clone, PartialEq, Eq)]
pub struct FakeResponse {
    /// The status.
    pub status: u16,
    /// Headers, lowercase names.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl fmt::Debug for FakeResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeResponse")
            .field("status", &self.status)
            .field("body_bytes", &self.body.len())
            .finish_non_exhaustive()
    }
}

impl FakeResponse {
    /// The body as JSON, `Null` when it is not.
    #[must_use]
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

/// How many requests each route answered successfully, and how many it saw.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    /// Installation tokens issued.
    pub installation_tokens: u64,
    /// Registration tokens issued.
    pub registration_tokens: u64,
    /// Admin tokens issued by the runner-registration exchange.
    pub admin_exchanges: u64,
    /// Requests seen on any route, faulted and refused ones included (control calls excluded).
    pub requests: u64,
    /// Sessions created.
    pub sessions_created: u64,
    /// Messages handed out by polls, redeliveries included.
    pub messages_delivered: u64,
    /// Messages acked.
    pub acks: u64,
    /// JIT configs generated.
    pub jit_configs: u64,
    /// Runners removed by a delete.
    pub runners_removed: u64,
    /// Workflow-run lookups answered 200.
    pub run_lookups: u64,
    /// Run cancels answered 202.
    pub run_cancels: u64,
    /// Listings of every runner answered (a listing by name is not counted).
    pub runner_listings: u64,
}

/// Holds a route's requests until released ([`FakeGithub::hold`]).
#[derive(Debug, Clone)]
pub struct Gate {
    inner: Arc<GateInner>,
}

#[derive(Debug)]
struct GateInner {
    arrived: Notify,
    open: Semaphore,
}

impl Gate {
    /// Waits until a request has reached the gate.
    pub async fn arrived(&self) {
        self.inner.arrived.notified().await;
    }

    /// Lets every held and later request through.
    pub fn release(&self) {
        self.inner.open.close();
    }
}

#[derive(Default)]
pub(crate) struct State {
    pub counts: Counts,
    faults: BTreeMap<Route, VecDeque<Fault>>,
    gates: BTreeMap<Route, Gate>,
    pub installation_tokens: BTreeMap<String, u64>,
    registration_tokens: BTreeMap<String, u64>,
    /// Admin tokens and their expiry (`None`: no `exp`).
    pub admin_tokens: BTreeMap<String, Option<u64>>,
    pub issued: Vec<String>,
    last_claims: Option<JwtClaims>,
    serial: u64,
    pub knobs: Knobs,
    pub world: World,
}

impl State {
    /// A fresh serial for a token or secret.
    pub fn serial(&mut self) -> u64 {
        self.serial += 1;
        self.serial
    }
}

/// What a route handler sees besides the state.
pub(crate) struct Ctx<'a> {
    pub config: &'a FakeConfig,
    pub now: u64,
    pub params: &'a Params,
    pub request: &'a FakeRequest,
}

impl Ctx<'_> {
    /// The service URL without a trailing slash.
    pub fn service_url(&self) -> &str {
        self.config.service_url.trim_end_matches('/')
    }
}

/// The fake GitHub. Clones share one state.
#[derive(Clone)]
pub struct FakeGithub {
    config: Arc<FakeConfig>,
    now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
    state: Arc<Mutex<State>>,
    changed: Arc<Notify>,
    closed: Arc<watch::Sender<bool>>,
}

impl fmt::Debug for FakeGithub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeGithub").finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct RegistrationBody {
    url: String,
    runner_event: String,
}

impl FakeGithub {
    /// A fake answering by `config` with the default [`Knobs`], reading the time (Unix-epoch
    /// milliseconds) from `now_ms`, so a test drives its clock and the client's together.
    #[must_use]
    pub fn new(config: FakeConfig, now_ms: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        let (closed, _) = watch::channel(false);
        Self {
            config: Arc::new(config),
            now_ms,
            state: Arc::new(Mutex::new(State::default())),
            changed: Arc::new(Notify::new()),
            closed: Arc::new(closed),
        }
    }

    /// As [`FakeGithub::new`], with `knobs`.
    #[must_use]
    pub fn with_knobs(
        config: FakeConfig,
        knobs: Knobs,
        now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        let fake = Self::new(config, now_ms);
        fake.state().knobs = knobs;
        fake
    }

    pub(crate) fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn now(&self) -> u64 {
        (self.now_ms)() / 1000
    }

    /// Wakes every held poll to look again.
    pub(crate) fn changed(&self) {
        self.changed.notify_waiters();
    }

    /// Ends every stalled and held request with 503, now and later; [`crate::FakeServer::stop`]
    /// calls it so a stalled request never holds the server open. Pinned by:
    /// `tests/wire/faults.rs::should_end_a_stalled_poll_with_503_when_the_fake_closes`.
    pub fn close(&self) {
        self.closed.send_replace(true);
    }

    pub(crate) async fn wait_closed(&self) {
        let mut closed = self.closed.subscribe();
        let _ = closed.wait_for(|c| *c).await;
    }

    /// What each route has answered so far.
    #[must_use]
    pub fn counts(&self) -> Counts {
        self.state().counts
    }

    /// Every token and secret the fake has issued (installation, registration, admin and queue
    /// tokens, JIT configs), for a test that scans output for a leak.
    #[must_use]
    pub fn issued_tokens(&self) -> Vec<String> {
        self.state().issued.clone()
    }

    /// The claims of the last App JWT accepted.
    #[must_use]
    pub fn last_app_jwt(&self) -> Option<JwtClaims> {
        self.state().last_claims
    }

    /// Queues `fault` as the next answer of `route`.
    pub fn fail_next(&self, route: Route, fault: Fault) {
        self.state()
            .faults
            .entry(route)
            .or_default()
            .push_back(fault);
    }

    /// Holds every request of `route` at a gate until the returned [`Gate`] is released.
    #[must_use]
    pub fn hold(&self, route: Route) -> Gate {
        let gate = Gate {
            inner: Arc::new(GateInner {
                arrived: Notify::new(),
                open: Semaphore::new(0),
            }),
        };
        self.state().gates.insert(route, gate.clone());
        gate
    }

    /// Answers `request`.
    pub async fn handle(&self, request: FakeRequest) -> FakeResponse {
        let (path, query) = split_query(&request.path);
        if let Some(rest) = path.strip_prefix("/_fake/") {
            return control::handle_http(self, &request.method, rest, &request.body);
        }
        let found = {
            let queue_base = self.state().knobs.queue_url_base.clone();
            let queue = url_path(queue_base.as_deref().unwrap_or(&self.config.service_url));
            route(
                &request.method,
                path,
                &url_path(&self.config.service_url),
                &queue,
                &self.auth_paths(),
            )
        };
        let Some((route, mut params)) = found else {
            self.state().counts.requests += 1;
            return status(404, "Not Found");
        };
        params.query.extend(query);
        let gate = self.state().gates.get(&route).cloned();
        if let Some(gate) = gate {
            gate.inner.arrived.notify_one();
            // A closed semaphore is an open gate: acquire fails at once.
            let _ = gate.inner.open.acquire().await;
        }
        let fault = {
            let mut state = self.state();
            state.counts.requests += 1;
            state.faults.get_mut(&route).and_then(VecDeque::pop_front)
        };
        let mut without_exp = false;
        let mut drop_answer = false;
        match fault {
            Some(Fault::Respond {
                status,
                headers,
                body,
            }) => {
                return FakeResponse {
                    status,
                    headers,
                    body: body.into_bytes(),
                };
            }
            Some(Fault::Stall) => {
                self.wait_closed().await;
                return status(503, "closed");
            }
            Some(Fault::Delay { ms }) => crate::wait::delay(Duration::from_millis(ms)).await,
            Some(Fault::AdminTokenWithoutExp) => without_exp = true,
            Some(Fault::DropAfterEffect) => drop_answer = true,
            None => {}
        }
        let answer = if route == Route::Poll {
            service::poll(self, &params, &request).await
        } else {
            self.answer(route, &params, &request, without_exp)
        };
        if drop_answer {
            self.wait_closed().await;
            return status(503, "closed");
        }
        answer
    }

    fn auth_paths(&self) -> AuthPaths {
        AuthPaths {
            access: format!(
                "/app/installations/{}/access_tokens",
                self.config.installation_id
            ),
            registration: match &self.config.scope {
                Scope::Organization(org) => {
                    format!("/orgs/{org}/actions/runners/registration-token")
                }
                Scope::Repository(owner, repo) => {
                    format!("/repos/{owner}/{repo}/actions/runners/registration-token")
                }
            },
        }
    }

    fn answer(
        &self,
        route: Route,
        params: &Params,
        request: &FakeRequest,
        without_exp: bool,
    ) -> FakeResponse {
        let now = self.now();
        let mut state = self.state();
        let authorization = header(request, "authorization").unwrap_or_default();
        let ctx = Ctx {
            config: &self.config,
            now,
            params,
            request,
        };
        let response = match route {
            Route::InstallationToken => self.installation_token(&mut state, &authorization, now),
            Route::RegistrationToken => self.registration_token(&mut state, &authorization, now),
            Route::RunnerRegistration => self.runner_registration(
                &mut state,
                &authorization,
                &request.body,
                now,
                without_exp,
            ),
            Route::WorkflowRun | Route::CancelRun | Route::ForkPrSetting | Route::RunnerGroup => {
                rest::answer(&mut state, route, &ctx)
            }
            Route::Poll => status(500, "the poll is answered elsewhere"),
            _ => service::answer(&mut state, route, &ctx),
        };
        let knobs = state.knobs.clone();
        state.world.assign(&knobs, now);
        drop(state);
        self.changed();
        response
    }

    fn installation_token(&self, state: &mut State, authorization: &str, now: u64) -> FakeResponse {
        let Some(jwt) = authorization.strip_prefix("Bearer ") else {
            return status(401, "A JSON web token could not be decoded");
        };
        let claims = match verify_app_jwt(
            jwt,
            &self.config.app_public_key_pem,
            &self.config.app_client_id,
            now,
        ) {
            Ok(claims) => claims,
            Err(_) => return status(401, "A JSON web token could not be decoded"),
        };
        state.last_claims = Some(claims);
        let serial = state.serial();
        let token = format!("ghs_fake{serial:08}");
        let expires_at = now + HOUR_SECS;
        state.installation_tokens.insert(token.clone(), expires_at);
        state.issued.push(token.clone());
        state.counts.installation_tokens += 1;
        json(
            201,
            &serde_json::json!({
                "token": token,
                "expires_at": rfc3339(expires_at),
                "permissions": self.config.permissions,
                "repository_selection": "selected",
            }),
        )
    }

    fn registration_token(&self, state: &mut State, authorization: &str, now: u64) -> FakeResponse {
        let live = authorization
            .strip_prefix("Bearer ")
            .and_then(|token| state.installation_tokens.get(token))
            .is_some_and(|expires_at| *expires_at > now);
        if !live {
            return status(401, "Bad credentials");
        }
        let serial = state.serial();
        let token = format!("FAKEREG{serial:08}");
        let expires_at = now + HOUR_SECS;
        state.registration_tokens.insert(token.clone(), expires_at);
        state.issued.push(token.clone());
        state.counts.registration_tokens += 1;
        json(
            201,
            &serde_json::json!({"token": token, "expires_at": rfc3339(expires_at)}),
        )
    }

    fn runner_registration(
        &self,
        state: &mut State,
        authorization: &str,
        body: &[u8],
        now: u64,
        without_exp: bool,
    ) -> FakeResponse {
        let live = authorization
            .strip_prefix("RemoteAuth ")
            .and_then(|token| state.registration_tokens.get(token))
            .is_some_and(|expires_at| *expires_at > now);
        if !live {
            return status(401, "Bad credentials");
        }
        let expected = match &self.config.scope {
            Scope::Organization(org) => format!("{}/{org}", self.config.web_base),
            Scope::Repository(owner, repo) => {
                format!("{}/{owner}/{repo}", self.config.web_base)
            }
        };
        let accepted = serde_json::from_slice::<RegistrationBody>(body)
            .is_ok_and(|b| b.url == expected && b.runner_event == "register");
        if !accepted {
            return status(400, "Invalid runner registration request");
        }
        let serial = state.serial();
        let expires_at = (!without_exp).then_some(now + self.config.admin_token_lifetime_secs);
        let Some(token) = admin_token(serial, expires_at, &self.config.admin_token_key_pem) else {
            return status(500, "The fake's admin token key does not sign.");
        };
        state.admin_tokens.insert(token.clone(), expires_at);
        state.issued.push(token.clone());
        state.counts.admin_exchanges += 1;
        json(
            200,
            &serde_json::json!({"url": self.config.service_url, "token": token}),
        )
    }

    /// The notification every change of the world sends; a waiter enables its `notified()`
    /// before it looks, so no change between the look and the wait is missed.
    pub(crate) fn changes(&self) -> Arc<Notify> {
        Arc::clone(&self.changed)
    }
}

/// Answers 401 unless `token` is a live admin token at `now`.
pub(crate) fn admin_ok(state: &State, request: &FakeRequest, now: u64) -> bool {
    crate::wire::bearer(request)
        .and_then(|token| state.admin_tokens.get(&token).copied())
        .is_some_and(|expires| expires.is_none_or(|e| e > now))
}

/// The answer for a request that does not carry what the route needs.
pub(crate) fn unauthorized() -> FakeResponse {
    status(401, "Unauthorized")
}

/// The answer for a route whose subject does not exist.
pub(crate) fn not_found() -> FakeResponse {
    status(404, "Not Found")
}
