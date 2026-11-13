//! [`ScaleSetClient`]: the `ScaleSetApi` and `RunnerRegistry` ports over the auth chain (spec
//! §2–§7), written from `doc/github-scaleset.md` (ADR 0048).
//!
//! - Every Actions-service call carries the admin token and `api-version=6.0-preview` in its
//!   query (spec §2); a 401 drops that admin token and the call is made once more with a fresh
//!   one (a token revoked before its `exp`).
//! - The queue calls (poll, ack) and acquire carry the session's queue token; a 401 refreshes the
//!   session once (`PATCH`) and retries once, and racing callers refresh once (spec §3.1, §3.3).
//!   A refresh answering `RunnerScaleSetSessionExpiredException`, or a second 401, is
//!   [`Failure::SessionExpired`]: the session is dropped, never refreshed again.
//! - Every URL the service hands back that a credential goes to (the queue URL, from a create or
//!   a refresh) passes [`credential_base_problem`] first, or the answer is protocol drift.
//! - Errors are classed by `typeName`, never by status (spec §2); a scale-set-scoped 404 without
//!   a class is [`Failure::ScaleSetGone`] (spec §3.1).
//! - Nothing here deletes a scale set (ADR 0040) or removes a runner other than by its id.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use mayfly_control::github::{
    Failure, GitHubCallError, JitConfig, MessageId, Offer, OpenedSession, PolledMessage,
    RemovalOutcome, RunnerId, RunnerInfo, RunnerRegistry, RunnerRequestId, ScaleSetApi, ScaleSetId,
    ScaleSetInfo, ScaleSetSpec, SessionId, Statistics,
};
use mayfly_control::secrets::SecretProvider;
use mayfly_control::store::{Clock, Rng};
use mayfly_domain::{Deadline, Millis, Secret};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use url::Url;

use crate::auth::AppAuth;
use crate::config::credential_base_problem;
use crate::error::{ErrorClass, ErrorKind, GitHubError};
use crate::http::{HttpRequest, HttpResponse, Method, Transport};
use crate::metrics::Operation;
use crate::retry::Phase;
use crate::wire::{
    decode_message, decode_offer, decode_runner, decode_scale_set, decode_statistics,
    session_id_from_wire, session_id_to_wire,
};

/// The Actions service's API version, in every service call's query (spec §2) and in the
/// poll's `Accept` (spec §4.1).
pub const API_VERSION: &str = "6.0-preview";

/// The longest one poll attempt may take: the hold is about 50 s and the reference bounds an
/// attempt at two minutes (spec §4.1).
pub const POLL_ATTEMPT_TIMEOUT: Millis = Millis::new(120_000);

/// A poll's whole budget, retries included: one held attempt and a retry after a short wait.
pub const POLL_DEADLINE: Millis = Millis::new(180_000);

/// The work folder a JIT runner is configured with when the caller names none.
pub const DEFAULT_WORK_FOLDER: &str = "_work";

/// The session the client holds for one scale set. Its `Debug` prints neither the token nor the
/// queue URL's path (it may name the tenant).
struct LiveSession {
    id: SessionId,
    queue_url: Url,
    token: Secret<String>,
}

/// The Scale Set protocol and the runner endpoints for one installation (ADR 0040: one
/// organisation, or one repository for tests).
pub struct ScaleSetClient<T, C, R, P> {
    auth: AppAuth<T, C, R, P>,
    sessions: Mutex<BTreeMap<u64, Arc<LiveSession>>>,
    /// One lock per scale set, serialising its session's refreshes so racing 401s refresh once;
    /// a refresh of one scale set never waits on another's.
    refreshes: Mutex<BTreeMap<u64, Arc<Mutex<()>>>>,
}

impl<T, C, R, P> fmt::Debug for ScaleSetClient<T, C, R, P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScaleSetClient")
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}

impl<T: Transport, C: Clock, R: Rng, P: SecretProvider> ScaleSetClient<T, C, R, P> {
    /// A client over `auth`, holding no session yet.
    pub fn new(auth: AppAuth<T, C, R, P>) -> Self {
        Self {
            auth,
            sessions: Mutex::new(BTreeMap::new()),
            refreshes: Mutex::new(BTreeMap::new()),
        }
    }

    /// The auth chain (its metrics, the permission probe).
    pub const fn auth(&self) -> &AppAuth<T, C, R, P> {
        &self.auth
    }

    /// Whether a session is held for `scale_set`.
    pub async fn holds_session(&self, scale_set: ScaleSetId) -> bool {
        self.sessions.lock().await.contains_key(&scale_set.0)
    }

    /// One Actions-service call with the admin token, retried once with a fresh token after a
    /// 401.
    async fn service(
        &self,
        op: Operation,
        method: Method,
        segments: &[&str],
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<HttpResponse, GitHubError> {
        let body = body.map(|b| b.to_string().into_bytes());
        let mut connection = self.auth.admin_connection().await?;
        let mut renewed = false;
        loop {
            let url = service_url(&connection.service_url, segments, query);
            let token = &connection.token;
            let result = self
                .auth
                .client()
                .call(op, Phase::Ordinary, || HttpRequest {
                    method,
                    url: url.clone(),
                    authorization: Some(Secret::new(format!("Bearer {}", token.expose()))),
                    headers: json_headers(body.is_some()),
                    body: body.clone(),
                })
                .await;
            match result {
                Err(error) if status_of(&error) == Some(401) && !renewed => {
                    self.auth.invalidate(&connection).await;
                    connection = self.auth.admin_connection().await?;
                    renewed = true;
                }
                other => return other,
            }
        }
    }

    async fn live(&self, scale_set: ScaleSetId) -> Result<Arc<LiveSession>, GitHubCallError> {
        self.sessions
            .lock()
            .await
            .get(&scale_set.0)
            .cloned()
            .ok_or_else(|| {
                GitHubCallError::new(Failure::NoSession, "no session is held for the scale set")
            })
    }

    async fn forget(&self, scale_set: ScaleSetId, session: SessionId) {
        let mut sessions = self.sessions.lock().await;
        if sessions.get(&scale_set.0).is_some_and(|s| s.id == session) {
            sessions.remove(&scale_set.0);
        }
    }

    /// A call on the session's queue token: a 401 refreshes the session once and retries once
    /// (spec §3.3); a second 401 loses it.
    async fn queue_call(
        &self,
        scale_set: ScaleSetId,
        op: Operation,
        attempt_timeout: Millis,
        budget: Millis,
        build: impl Fn(&LiveSession) -> HttpRequest,
    ) -> Result<HttpResponse, GitHubCallError> {
        let client = self.auth.client();
        let deadline = Deadline::after(client.clock().now(), budget);
        let mut session = self.live(scale_set).await?;
        let mut refreshed = false;
        loop {
            let current = Arc::clone(&session);
            let result = client
                .call_bounded(op, Phase::Ordinary, deadline, attempt_timeout, || {
                    build(&current)
                })
                .await;
            match result {
                Err(error) if status_of(&error) == Some(401) => {
                    if refreshed {
                        self.forget(scale_set, session.id).await;
                        return Err(GitHubCallError::new(
                            Failure::SessionExpired,
                            format!("{error}, again after a refresh"),
                        ));
                    }
                    session = self.refresh_session(scale_set, &session).await?;
                    refreshed = true;
                }
                Err(error) => return Err(call_error(&error)),
                Ok(answer) => return Ok(answer),
            }
        }
    }

    /// Refreshes `stale` unless another caller already replaced it (spec §3.1 "Refresh"). A
    /// refresh answering `RunnerScaleSetSessionExpiredException`, or 404, loses the session: it
    /// is never refreshed again. So does a refresh answering an unusable session (drift).
    ///
    /// Pinned by: `tests/client/scaleset.rs::should_refresh_once_when_queue_calls_race_on_an_expired_token`
    /// `::should_lose_the_session_when_its_refresh_answers_404` and
    /// `::should_refuse_a_refreshed_queue_url_when_a_token_could_leak_through_it`.
    async fn refresh_session(
        &self,
        scale_set: ScaleSetId,
        stale: &Arc<LiveSession>,
    ) -> Result<Arc<LiveSession>, GitHubCallError> {
        let lock = Arc::clone(self.refreshes.lock().await.entry(scale_set.0).or_default());
        let _serial = lock.lock().await;
        let current = self.live(scale_set).await?;
        if !Arc::ptr_eq(&current, stale) {
            return Ok(current);
        }
        let id = stale.id;
        let set = scale_set.0.to_string();
        let session = session_id_to_wire(id);
        let answer = self
            .service(
                Operation::SessionRefresh,
                Method::Patch,
                &[
                    "_apis",
                    "runtime",
                    "runnerscalesets",
                    &set,
                    "sessions",
                    &session,
                ],
                &[],
                None,
            )
            .await;
        let answer = match answer {
            Ok(answer) => answer,
            Err(error) => {
                let mut failed = call_error(&error);
                if failed.failure == (Failure::Refused { status: 404 }) {
                    failed.failure = Failure::SessionExpired;
                }
                if failed.failure == Failure::SessionExpired {
                    self.forget(scale_set, id).await;
                }
                return Err(failed);
            }
        };
        // A refresh answering an unusable session (a queue URL a token could leak through, no
        // token) loses it: the client keeps no session it cannot call safely.
        let (fresh, _statistics) = match decode_session(&answer, Some(id)) {
            Ok(decoded) => decoded,
            Err(failed) => {
                self.forget(scale_set, id).await;
                return Err(failed);
            }
        };
        let fresh = Arc::new(fresh);
        self.sessions
            .lock()
            .await
            .insert(scale_set.0, Arc::clone(&fresh));
        Ok(fresh)
    }

    /// A create answered 200 with an unusable session (no statistics, no token, a queue URL a
    /// token could leak through): the session exists at GitHub but mayfly cannot use it. When its
    /// id decodes it is deleted at once, best effort, so it does not block the next create with
    /// 409; A3 holds, since mayfly deletes only a session it has a record of, and this answer is
    /// that record. The drift is returned either way, saying whether the delete went through;
    /// the caller journals no session.
    ///
    /// Pinned by: `mayfly-testkit/src/contracts/scaleset.rs::unusable_session_case` (SS-13) and
    /// `tests/client/scaleset.rs::should_refuse_a_queue_url_when_a_token_could_leak_through_it`.
    async fn abandon(
        &self,
        scale_set: ScaleSetId,
        answer: &HttpResponse,
        failed: GitHubCallError,
    ) -> GitHubCallError {
        let id = body_json(answer).ok().and_then(|body| {
            body.get("sessionId")
                .and_then(Value::as_str)
                .and_then(session_id_from_wire)
        });
        let Some(id) = id else {
            return failed;
        };
        let what = match self.delete_session(scale_set, id).await {
            Ok(()) => "the unusable session was deleted",
            Err(_) => "the unusable session could not be deleted",
        };
        GitHubCallError::new(failed.failure, format!("{}; {what}", failed.detail))
    }

    /// The id of the runner group `name`: exactly one expected (spec §2).
    async fn runner_group(&self, name: &str) -> Result<u64, GitHubCallError> {
        let answer = self
            .service(
                Operation::RunnerGroup,
                Method::Get,
                &["_apis", "runtime", "runnergroups", ""],
                &[("groupName", name.to_owned())],
                None,
            )
            .await
            .map_err(|e| call_error(&e))?;
        let groups = list_values(&answer)?;
        match groups.as_slice() {
            [] => Err(GitHubCallError::new(
                Failure::Refused { status: 404 },
                "the runner group was not found",
            )),
            [group] => group
                .get("id")
                .and_then(Value::as_u64)
                .filter(|id| *id > 0)
                .ok_or_else(|| drift("the runner group has no id")),
            _ => Err(drift("more than one runner group has the name")),
        }
    }
}

impl<T: Transport, C: Clock, R: Rng, P: SecretProvider> ScaleSetApi for ScaleSetClient<T, C, R, P> {
    async fn find_scale_set(
        &self,
        spec: &ScaleSetSpec,
    ) -> Result<Option<ScaleSetInfo>, GitHubCallError> {
        let group = self.runner_group(&spec.runner_group).await?;
        let answer = self
            .service(
                Operation::ScaleSetRead,
                Method::Get,
                &["_apis", "runtime", "runnerscalesets"],
                &[
                    ("runnerGroupId", group.to_string()),
                    ("name", spec.name.clone()),
                ],
                None,
            )
            .await
            .map_err(|e| call_error(&e))?;
        let found = list_values(&answer)?;
        match found.as_slice() {
            [] => Ok(None),
            [set] => decode_scale_set(set)
                .map(Some)
                .ok_or_else(|| drift("the scale set lacks an id or a name")),
            _ => Err(drift("more than one scale set has the name in the group")),
        }
    }

    async fn get_scale_set(&self, id: ScaleSetId) -> Result<ScaleSetInfo, GitHubCallError> {
        let set = id.0.to_string();
        let answer = self
            .service(
                Operation::ScaleSetRead,
                Method::Get,
                &["_apis", "runtime", "runnerscalesets", &set],
                &[],
                None,
            )
            .await
            .map_err(|e| scoped_error(&e))?;
        decode_scale_set(&body_json(&answer)?)
            .ok_or_else(|| drift("the scale set lacks an id or a name"))
    }

    async fn create_scale_set(&self, spec: &ScaleSetSpec) -> Result<ScaleSetInfo, GitHubCallError> {
        let group = self.runner_group(&spec.runner_group).await?;
        let labels: Vec<Value> = spec
            .labels
            .iter()
            .map(|name| json!({"type": "System", "name": name}))
            .collect();
        let body = json!({
            "name": spec.name,
            "runnerGroupId": group,
            "labels": labels,
            // The image pins its runner (ADR 0087): the runner must not update itself.
            "RunnerSetting": {"disableUpdate": true},
        });
        let answer = self
            .service(
                Operation::ScaleSetCreate,
                Method::Post,
                &["_apis", "runtime", "runnerscalesets"],
                &[],
                Some(body),
            )
            .await
            .map_err(|e| call_error(&e))?;
        decode_scale_set(&body_json(&answer)?)
            .ok_or_else(|| drift("the created scale set lacks an id or a name"))
    }

    async fn create_session(
        &self,
        scale_set: ScaleSetId,
        owner: &str,
    ) -> Result<OpenedSession, GitHubCallError> {
        let set = scale_set.0.to_string();
        let answer = self
            .service(
                Operation::SessionCreate,
                Method::Post,
                &["_apis", "runtime", "runnerscalesets", &set, "sessions"],
                &[],
                Some(json!({"ownerName": owner})),
            )
            .await
            .map_err(|e| scoped_error(&e))?;
        let decoded = decode_session(&answer, None).and_then(|(session, statistics)| {
            let statistics = statistics.ok_or_else(|| drift("the session has no statistics"))?;
            Ok((session, statistics))
        });
        let (session, statistics) = match decoded {
            Ok(decoded) => decoded,
            Err(failed) => return Err(self.abandon(scale_set, &answer, failed).await),
        };
        let session_id = session.id;
        self.sessions
            .lock()
            .await
            .insert(scale_set.0, Arc::new(session));
        Ok(OpenedSession {
            session_id,
            statistics,
        })
    }

    /// A session already gone, `RunnerScaleSetSessionExpiredException` (server-observed) or a
    /// 404, is deleted. A 404 here never reads as the scale set gone: only a create or a read of
    /// the scale set says that (spec §3.1), so a stale session's delete after a restart never
    /// sends the stream to re-create a live set, and a delete on a scale set GitHub deleted is
    /// done.
    ///
    /// Pinned by: `mayfly-testkit/src/contracts/scaleset.rs::stale_delete_case` (SS-12) and
    /// `mayfly-testkit/src/contracts/scaleset.rs::gone_case` (SS-11).
    async fn delete_session(
        &self,
        scale_set: ScaleSetId,
        session: SessionId,
    ) -> Result<(), GitHubCallError> {
        let set = scale_set.0.to_string();
        let wire = session_id_to_wire(session);
        let answer = self
            .service(
                Operation::SessionDelete,
                Method::Delete,
                &[
                    "_apis",
                    "runtime",
                    "runnerscalesets",
                    &set,
                    "sessions",
                    &wire,
                ],
                &[],
                None,
            )
            .await;
        let outcome = match answer {
            Ok(_) => Ok(()),
            Err(error) => {
                let failed = call_error(&error);
                match failed.failure {
                    Failure::SessionExpired | Failure::Refused { status: 404 } => Ok(()),
                    _ => Err(failed),
                }
            }
        };
        if outcome.is_ok() {
            self.forget(scale_set, session).await;
        }
        outcome
    }

    async fn poll(
        &self,
        scale_set: ScaleSetId,
        last: Option<MessageId>,
        max_capacity: u32,
    ) -> Result<Option<PolledMessage>, GitHubCallError> {
        let answer = self
            .queue_call(
                scale_set,
                Operation::Poll,
                POLL_ATTEMPT_TIMEOUT,
                POLL_DEADLINE,
                |session| {
                    let mut url = session.queue_url.clone();
                    if let Some(MessageId(id)) = last.filter(|m| m.0 > 0) {
                        url.query_pairs_mut()
                            .append_pair("lastMessageId", &id.to_string());
                    }
                    HttpRequest {
                        method: Method::Get,
                        url,
                        authorization: Some(Secret::new(format!(
                            "Bearer {}",
                            session.token.expose()
                        ))),
                        headers: vec![
                            (
                                "accept",
                                format!("application/json; api-version={API_VERSION}"),
                            ),
                            ("x-scalesetmaxcapacity", max_capacity.to_string()),
                        ],
                        body: None,
                    }
                },
            )
            .await?;
        match answer.status {
            202 => Ok(None),
            200 => decode_message(&answer.body).map(Some).ok_or_else(|| {
                GitHubCallError::new(
                    Failure::Unavailable,
                    "the poll answered 200 with an empty or undecodable body",
                )
            }),
            status => Err(drift(format!("the poll answered {status}"))),
        }
    }

    async fn ack(&self, scale_set: ScaleSetId, message: MessageId) -> Result<(), GitHubCallError> {
        let id = message.0.to_string();
        let budget = self.auth.client().policy().deadline;
        let answer = self
            .queue_call(
                scale_set,
                Operation::Ack,
                self.auth.client().policy().attempt_timeout,
                budget,
                |session| {
                    let mut url = session.queue_url.clone();
                    if let Ok(mut path) = url.path_segments_mut() {
                        path.pop_if_empty().push(&id);
                    }
                    HttpRequest {
                        method: Method::Delete,
                        url,
                        authorization: Some(Secret::new(format!(
                            "Bearer {}",
                            session.token.expose()
                        ))),
                        headers: Vec::new(),
                        body: None,
                    }
                },
            )
            .await;
        match answer {
            Ok(_) => Ok(()),
            // Spec §4.4: whether a 404 means "already acked" is open; the inbox record exists,
            // so it is done.
            Err(e) if e.failure == (Failure::Refused { status: 404 }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn acquire_jobs(
        &self,
        scale_set: ScaleSetId,
        requests: &BTreeSet<RunnerRequestId>,
    ) -> Result<BTreeSet<RunnerRequestId>, GitHubCallError> {
        if requests.is_empty() {
            return Ok(BTreeSet::new());
        }
        let service = self
            .auth
            .admin_connection()
            .await
            .map_err(|e| call_error(&e))?
            .service_url
            .clone();
        let set = scale_set.0.to_string();
        let url = service_url(
            &service,
            &["_apis", "runtime", "runnerscalesets", &set, "acquirejobs"],
            &[],
        );
        let body = Value::from(requests.iter().map(|r| r.0).collect::<Vec<u64>>())
            .to_string()
            .into_bytes();
        let policy = self.auth.client().policy();
        let answer = self
            .queue_call(
                scale_set,
                Operation::AcquireJobs,
                policy.attempt_timeout,
                policy.deadline,
                |session| HttpRequest {
                    method: Method::Post,
                    url: url.clone(),
                    authorization: Some(Secret::new(format!("Bearer {}", session.token.expose()))),
                    headers: json_headers(true),
                    body: Some(body.clone()),
                },
            )
            .await?;
        let acquired: BTreeSet<RunnerRequestId> = list_values(&answer)?
            .iter()
            .filter_map(Value::as_u64)
            .map(RunnerRequestId)
            .filter(|id| requests.contains(id))
            .collect();
        Ok(acquired)
    }

    async fn acquirable_jobs(&self, scale_set: ScaleSetId) -> Result<Vec<Offer>, GitHubCallError> {
        let set = scale_set.0.to_string();
        let answer = self
            .service(
                Operation::AcquirableJobs,
                Method::Get,
                &[
                    "_apis",
                    "runtime",
                    "runnerscalesets",
                    &set,
                    "acquirablejobs",
                ],
                &[],
                None,
            )
            .await
            .map_err(|e| call_error(&e))?;
        if answer.status == 204 {
            return Ok(Vec::new());
        }
        Ok(list_values(&answer)?
            .iter()
            .filter_map(decode_offer)
            .collect())
    }

    async fn generate_jit_config(
        &self,
        scale_set: ScaleSetId,
        runner_name: &str,
        work_folder: &str,
    ) -> Result<JitConfig, GitHubCallError> {
        let set = scale_set.0.to_string();
        let work_folder = if work_folder.is_empty() {
            DEFAULT_WORK_FOLDER
        } else {
            work_folder
        };
        let answer = self
            .service(
                Operation::JitConfig,
                Method::Post,
                &[
                    "_apis",
                    "runtime",
                    "runnerscalesets",
                    &set,
                    "generatejitconfig",
                ],
                &[],
                Some(json!({"name": runner_name, "workFolder": work_folder})),
            )
            .await
            .map_err(|e| call_error(&e))?;
        let mut body = body_json(&answer)?;
        let runner = body
            .get("runner")
            .and_then(decode_runner)
            .ok_or_else(|| drift("the JIT answer has no runner"))?;
        let encoded = match body.get_mut("encodedJITConfig").map(Value::take) {
            Some(Value::String(config)) if !config.is_empty() => Secret::new(config),
            _ => return Err(drift("the JIT answer has no config")),
        };
        Ok(JitConfig { runner, encoded })
    }
}

impl<T: Transport, C: Clock, R: Rng, P: SecretProvider> RunnerRegistry
    for ScaleSetClient<T, C, R, P>
{
    async fn get_runner(&self, id: RunnerId) -> Result<Option<RunnerInfo>, GitHubCallError> {
        let runner = id.0.to_string();
        let answer = self
            .service(
                Operation::RunnerRead,
                Method::Get,
                &["_apis", "distributedtask", "pools", "0", "agents", &runner],
                &[],
                None,
            )
            .await;
        match answer {
            Ok(answer) => decode_runner(&body_json(&answer)?)
                .map(Some)
                .ok_or_else(|| drift("the runner lacks an id or a name")),
            Err(error) if class_of(&error) == Some(ErrorClass::AgentNotFound) => Ok(None),
            Err(error) => Err(call_error(&error)),
        }
    }

    async fn find_runners(&self, name: &str) -> Result<Vec<RunnerInfo>, GitHubCallError> {
        let answer = self
            .service(
                Operation::RunnerRead,
                Method::Get,
                &["_apis", "distributedtask", "pools", "0", "agents"],
                &[("agentName", name.to_owned())],
                None,
            )
            .await
            .map_err(|e| call_error(&e))?;
        Ok(decode_runners(&answer)?
            .into_iter()
            .filter(|r| r.name == name)
            .collect())
    }

    async fn list_runners(&self) -> Result<Vec<RunnerInfo>, GitHubCallError> {
        let answer = self
            .service(
                Operation::RunnerRead,
                Method::Get,
                &["_apis", "distributedtask", "pools", "0", "agents"],
                &[],
                None,
            )
            .await
            .map_err(|e| call_error(&e))?;
        decode_runners(&answer)
    }

    async fn remove_runner(&self, id: RunnerId) -> Result<RemovalOutcome, GitHubCallError> {
        let runner = id.0.to_string();
        let answer = self
            .service(
                Operation::RunnerRemove,
                Method::Delete,
                &["_apis", "distributedtask", "pools", "0", "agents", &runner],
                &[],
                None,
            )
            .await;
        match answer {
            Ok(_) => Ok(RemovalOutcome::Gone),
            Err(error) => match class_of(&error) {
                Some(ErrorClass::AgentNotFound) => Ok(RemovalOutcome::Gone),
                Some(ErrorClass::JobStillRunning) => Ok(RemovalOutcome::Busy),
                _ => Err(call_error(&error)),
            },
        }
    }
}

/// `base` with `segments` appended and `query` plus `api-version` as its query (spec §2).
fn service_url(base: &Url, segments: &[&str], query: &[(&str, String)]) -> Url {
    let mut url = base.clone();
    if let Ok(mut path) = url.path_segments_mut() {
        path.pop_if_empty().extend(segments);
    }
    {
        let mut pairs = url.query_pairs_mut();
        for (name, value) in query {
            pairs.append_pair(name, value);
        }
        pairs.append_pair("api-version", API_VERSION);
    }
    url
}

fn json_headers(with_body: bool) -> Vec<(&'static str, String)> {
    let mut headers = vec![("accept", "application/json".to_owned())];
    if with_body {
        headers.push(("content-type", "application/json".to_owned()));
    }
    headers
}

/// A create's or refresh's answer as the held session and its statistics. A refresh must
/// answer the same session (`expect`).
fn decode_session(
    answer: &HttpResponse,
    expect: Option<SessionId>,
) -> Result<(LiveSession, Option<Statistics>), GitHubCallError> {
    let mut body = body_json(answer)?;
    let id = body
        .get("sessionId")
        .and_then(Value::as_str)
        .and_then(session_id_from_wire)
        .ok_or_else(|| drift("the session has no sessionId"))?;
    if expect.is_some_and(|e| e != id) {
        return Err(drift("the refresh answered another session"));
    }
    let queue_url = body
        .get("messageQueueUrl")
        .and_then(Value::as_str)
        .and_then(|u| Url::parse(u).ok())
        .ok_or_else(|| drift("the session has no messageQueueUrl"))?;
    if credential_base_problem(&queue_url).is_some() {
        return Err(drift(
            "the messageQueueUrl is not https (or loopback http), or carries a query, fragment or user info",
        ));
    }
    let statistics = body.get("statistics").and_then(decode_statistics);
    let token = match body.get_mut("messageQueueAccessToken").map(Value::take) {
        Some(Value::String(token)) if !token.is_empty() => Secret::new(token),
        _ => return Err(drift("the session has no messageQueueAccessToken")),
    };
    Ok((
        LiveSession {
            id,
            queue_url,
            token,
        },
        statistics,
    ))
}

fn body_json(answer: &HttpResponse) -> Result<Value, GitHubCallError> {
    serde_json::from_slice(answer.body_without_bom())
        .map_err(|_| drift("a success answer is not JSON"))
}

/// Every runner of a list answer. One that does not decode is drift, never dropped: the reaper
/// concludes an absence from a listing by name (P10), so a runner skipped here could be recorded
/// absent while it lives. Pinned by
/// `tests/client/scaleset.rs::should_answer_drift_when_a_listed_runner_cannot_be_decoded`.
fn decode_runners(answer: &HttpResponse) -> Result<Vec<RunnerInfo>, GitHubCallError> {
    list_values(answer)?
        .iter()
        .map(|value| {
            decode_runner(value).ok_or_else(|| drift("a listed runner lacks an id or a name"))
        })
        .collect()
}

/// The `value` array of a `{count, value}` answer (spec §2).
fn list_values(answer: &HttpResponse) -> Result<Vec<Value>, GitHubCallError> {
    match body_json(answer)? {
        Value::Object(mut fields) => match fields.remove("value") {
            Some(Value::Array(values)) => Ok(values),
            _ => Err(drift("a list answer has no value array")),
        },
        _ => Err(drift("a list answer is not an object")),
    }
}

fn drift(detail: impl Into<String>) -> GitHubCallError {
    GitHubCallError::new(Failure::ProtocolDrift, detail)
}

const fn status_of(error: &GitHubError) -> Option<u16> {
    match &error.kind {
        ErrorKind::Status { status, .. } => Some(*status),
        _ => None,
    }
}

const fn class_of(error: &GitHubError) -> Option<ErrorClass> {
    match &error.kind {
        ErrorKind::Status { class, .. } => *class,
        _ => None,
    }
}

/// The port's class of a client error: by `typeName` first, never by status alone (spec §2).
/// Pinned by: `scaleset::tests::should_class_by_type_name_when_the_status_says_otherwise`.
fn call_error(error: &GitHubError) -> GitHubCallError {
    let auth_step = matches!(
        error.op,
        Operation::InstallationToken | Operation::RegistrationToken | Operation::AdminExchange
    );
    let failure = match &error.kind {
        ErrorKind::Status {
            status,
            rate_limit,
            class,
            ..
        } => match class {
            Some(ErrorClass::SessionConflict) => Failure::SessionConflict,
            Some(ErrorClass::SessionExpired) => Failure::SessionExpired,
            Some(ErrorClass::AgentExists) => Failure::NameTaken,
            Some(ErrorClass::TooManyRunners) => Failure::Throttled,
            _ if rate_limit.is_some() => Failure::Throttled,
            _ if *status >= 500 => Failure::Unavailable,
            _ if auth_step => Failure::Credentials,
            _ => Failure::Refused { status: *status },
        },
        ErrorKind::Transport { .. } | ErrorKind::DeadlineExceeded { .. } => Failure::Unavailable,
        ErrorKind::Decode { .. } => Failure::ProtocolDrift,
        ErrorKind::Config(_)
        | ErrorKind::Secret(_)
        | ErrorKind::Jwt(_)
        | ErrorKind::AdminTokenTooShortLived { .. } => Failure::Credentials,
    };
    GitHubCallError::new(failure, error.to_string())
}

/// As [`call_error`], for a session create or a scale-set read only: there a 404 of no class
/// is that scale set gone (spec §3.1, GH-00R; that it answers 404 is `[open: HW-02, item 9]`).
/// No other call reads a 404 as gone.
fn scoped_error(error: &GitHubError) -> GitHubCallError {
    let mut failed = call_error(error);
    if failed.failure == (Failure::Refused { status: 404 })
        && matches!(class_of(error), None | Some(ErrorClass::Other))
    {
        failed.failure = Failure::ScaleSetGone;
    }
    failed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(op: Operation, status: u16, class: Option<ErrorClass>) -> GitHubError {
        GitHubError {
            op,
            kind: ErrorKind::Status {
                endpoint: "https://pipelines.example/x".to_owned(),
                status,
                rate_limit: None,
                class,
                attempts: 1,
            },
        }
    }

    #[test]
    fn should_class_by_type_name_when_the_status_says_otherwise() {
        // given: the busy class and the conflict at statuses that would say otherwise
        let cases = [
            (
                status(
                    Operation::SessionCreate,
                    400,
                    Some(ErrorClass::SessionConflict),
                ),
                Failure::SessionConflict,
            ),
            (
                status(
                    Operation::SessionRefresh,
                    404,
                    Some(ErrorClass::SessionExpired),
                ),
                Failure::SessionExpired,
            ),
            (
                status(Operation::JitConfig, 400, Some(ErrorClass::AgentExists)),
                Failure::NameTaken,
            ),
            (
                status(Operation::JitConfig, 429, Some(ErrorClass::TooManyRunners)),
                Failure::Throttled,
            ),
            (
                status(Operation::SessionCreate, 409, None),
                Failure::Refused { status: 409 },
            ),
            (status(Operation::Poll, 503, None), Failure::Unavailable),
            (
                status(Operation::RegistrationToken, 403, None),
                Failure::Credentials,
            ),
        ];

        // when
        let got: Vec<Failure> = cases.iter().map(|(e, _)| call_error(e).failure).collect();

        // then
        let expected: Vec<Failure> = cases.iter().map(|(_, f)| *f).collect();
        assert_eq!(got, expected);
        assert_eq!(
            scoped_error(&status(Operation::SessionCreate, 404, None)).failure,
            Failure::ScaleSetGone
        );
        assert_eq!(
            scoped_error(&status(
                Operation::JitConfig,
                404,
                Some(ErrorClass::AgentNotFound)
            ))
            .failure,
            Failure::Refused { status: 404 }
        );
    }

    #[test]
    fn should_add_the_api_version_when_building_a_service_url() {
        // given
        let base = Url::parse("https://pipelines.example/tenant/").expect("url");

        // when
        let url = service_url(
            &base,
            &["_apis", "runtime", "runnergroups", ""],
            &[("groupName", "My Group".to_owned())],
        );

        // then
        assert_eq!(
            url.as_str(),
            "https://pipelines.example/tenant/_apis/runtime/runnergroups/?groupName=My+Group&api-version=6.0-preview"
        );
    }
}
