//! The fake's state and its in-process request handler.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use serde::Deserialize;
use tokio::sync::{Notify, Semaphore};

use crate::timefmt::rfc3339;
use crate::token::{admin_token, verify_app_jwt};

pub use crate::token::JwtClaims;

/// Installation and registration tokens live one hour [docs].
const HOUR_SECS: u64 = 3600;

/// Where the App is installed and runners register: the path of the registration-token route
/// and the config URL the runner-registration exchange expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// `/orgs/{org}/...`, config URL `https://github.com/{org}`.
    Organization(String),
    /// `/repos/{owner}/{repo}/...`, config URL `https://github.com/{owner}/{repo}`.
    Repository(String, String),
}

/// What the fake accepts and answers.
#[derive(Debug, Clone)]
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
    /// The Actions service URL the exchange answers (opaque to the client).
    pub service_url: String,
    /// How long an admin token lives. Unverified (spec §12 item 4): short in tests so a client
    /// must refresh.
    pub admin_token_lifetime_secs: u64,
    /// The installation's granted permissions, as the access-token answer lists them.
    pub permissions: BTreeMap<String, String>,
}

/// A route of the auth chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Route {
    /// `POST /app/installations/{id}/access_tokens`.
    InstallationToken,
    /// `POST /orgs/{org}/actions/runners/registration-token` (or the repository's).
    RegistrationToken,
    /// `POST /actions/runner-registration`.
    RunnerRegistration,
}

/// A fault that replaces one answer of a route, in the order queued.
#[derive(Debug, Clone)]
pub enum Fault {
    /// Answer this instead, without effect.
    Respond {
        /// The status.
        status: u16,
        /// Extra headers (`retry-after`, `x-ratelimit-remaining`).
        headers: Vec<(String, String)>,
        /// The body.
        body: String,
    },
    /// Never answer (a stalled service); the client's deadline must end the wait.
    Stall,
    /// Answer the runner-registration exchange with an admin token that has no `exp` claim.
    AdminTokenWithoutExp,
}

/// A request in the fake's own terms.
#[derive(Debug, Clone)]
pub struct FakeRequest {
    /// `GET`, `POST`, ...
    pub method: String,
    /// The path, without the query.
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

/// How many requests each route answered successfully, and how many it saw.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Installation tokens issued.
    pub installation_tokens: u64,
    /// Registration tokens issued.
    pub registration_tokens: u64,
    /// Admin tokens issued by the runner-registration exchange.
    pub admin_exchanges: u64,
    /// Requests seen on any route, faulted and refused ones included.
    pub requests: u64,
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
struct State {
    counts: Counts,
    faults: BTreeMap<Route, VecDeque<Fault>>,
    gates: BTreeMap<Route, Gate>,
    installation_tokens: BTreeMap<String, u64>,
    registration_tokens: BTreeMap<String, u64>,
    issued: Vec<String>,
    last_claims: Option<JwtClaims>,
    serial: u64,
}

/// The fake GitHub. Clones share one state.
#[derive(Clone)]
pub struct FakeGithub {
    config: Arc<FakeConfig>,
    now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
    state: Arc<Mutex<State>>,
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

enum Prepared {
    Answer(FakeResponse),
    Stall,
}

impl FakeGithub {
    /// A fake answering by `config`, reading the time (Unix-epoch milliseconds) from `now_ms`,
    /// so a test drives its clock and the client's together.
    #[must_use]
    pub fn new(config: FakeConfig, now_ms: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        Self {
            config: Arc::new(config),
            now_ms,
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What each route has answered so far.
    #[must_use]
    pub fn counts(&self) -> Counts {
        self.state().counts
    }

    /// Every token the fake has issued, for a test that scans output for a leak.
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
        let Some(route) = self.route(&request) else {
            self.state().counts.requests += 1;
            return status(404, "Not Found");
        };
        let gate = self.state().gates.get(&route).cloned();
        if let Some(gate) = gate {
            gate.inner.arrived.notify_one();
            // A closed semaphore is an open gate: acquire fails at once.
            let _ = gate.inner.open.acquire().await;
        }
        let prepared = self.answer(route, &request);
        match prepared {
            Prepared::Answer(response) => response,
            Prepared::Stall => std::future::pending().await,
        }
    }

    fn route(&self, request: &FakeRequest) -> Option<Route> {
        if request.method != "POST" {
            return None;
        }
        let access = format!(
            "/app/installations/{}/access_tokens",
            self.config.installation_id
        );
        let registration = match &self.config.scope {
            Scope::Organization(org) => format!("/orgs/{org}/actions/runners/registration-token"),
            Scope::Repository(owner, repo) => {
                format!("/repos/{owner}/{repo}/actions/runners/registration-token")
            }
        };
        let path = request.path.as_str();
        if path == access {
            Some(Route::InstallationToken)
        } else if path == registration {
            Some(Route::RegistrationToken)
        } else if path == "/actions/runner-registration" {
            Some(Route::RunnerRegistration)
        } else {
            None
        }
    }

    fn answer(&self, route: Route, request: &FakeRequest) -> Prepared {
        let now = (self.now_ms)() / 1000;
        let mut state = self.state();
        state.counts.requests += 1;
        let fault = state.faults.get_mut(&route).and_then(VecDeque::pop_front);
        let without_exp = match fault {
            Some(Fault::Respond {
                status,
                headers,
                body,
            }) => {
                return Prepared::Answer(FakeResponse {
                    status,
                    headers,
                    body: body.into_bytes(),
                });
            }
            Some(Fault::Stall) => return Prepared::Stall,
            Some(Fault::AdminTokenWithoutExp) => true,
            None => false,
        };
        let authorization = header(request, "authorization").unwrap_or_default();
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
        };
        Prepared::Answer(response)
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
        state.serial += 1;
        let token = format!("ghs_fake{:08}", state.serial);
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
        state.serial += 1;
        let token = format!("FAKEREG{:08}", state.serial);
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
        state.serial += 1;
        let expires_at = (!without_exp).then_some(now + self.config.admin_token_lifetime_secs);
        let token = admin_token(state.serial, expires_at);
        state.issued.push(token.clone());
        state.counts.admin_exchanges += 1;
        json(
            200,
            &serde_json::json!({"url": self.config.service_url, "token": token}),
        )
    }
}

fn header(request: &FakeRequest, name: &str) -> Option<String> {
    request
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

fn json(status: u16, body: &serde_json::Value) -> FakeResponse {
    FakeResponse {
        status,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: body.to_string().into_bytes(),
    }
}

fn status(code: u16, message: &str) -> FakeResponse {
    json(code, &serde_json::json!({"message": message}))
}
