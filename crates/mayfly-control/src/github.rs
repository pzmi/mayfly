//! The GitHub ports (plan §4, §7.2, §7.3): [`ScaleSetApi`], the Runner Scale Set protocol as
//! the listener and the registration manager use it, [`RunnerRegistry`], the runner
//! endpoints of `doc/github-scaleset.md` §2 and §7, and [`WorkflowRuns`], the REST workflow-run
//! lookup and cancel of ADR 0090's per-job check (spec §6.2, GH-10).
//!
//! The adapter (`mayfly-adapter-github`, GH-03, GH-10) speaks the wire; the in-memory fake lives
//! in `mayfly-testkit::github`; both pass the contract suites
//! `mayfly_testkit::contracts::scaleset` and `mayfly_testkit::contracts::runs` (ver §5). What
//! crosses these ports is already the domain's language: messages arrive as
//! [`MessageItem`]s in arrival order, unknown types kept as [`MessageItem::Unknown`] (protocol
//! drift, plan §7.2 step 6), statistics as [`Statistics`], offers as [`Offer`]s. Strings from
//! GitHub are bounded by the adapter before they get here (dm §9.1).
//!
//! The control-domain types re-exported below are the ports' vocabulary: an adapter names them
//! through this module, so `mayfly-adapter-github` depends on `mayfly-control` alone (plan §4).
//! A message's `statistics` is `None` when the envelope lacked any of the seven counts (spec
//! §2: all seven are present on github.com); the listener reads that as drift (GH-04).
//!
//! **Credentials never cross.** The admin and queue tokens stay inside the adapter: a session is
//! named to the caller by its [`SessionId`] alone, which is journaled (spec §3.2); the queue
//! token, its URL and their refresh on 401 (spec §3.1, §3.3) are the adapter's. A JIT config is a
//! [`Secret`]. A [`GitHubCallError`]'s text is redacted: no token, query string or body.
//!
//! **Failures are classes, decided by `typeName`, never by status** (spec §2):
//! [`Failure`]. A failure that leaves the outcome unknown is [`Failure::Unavailable`]: the
//! caller never reads it as success or as a refusal (plan §6).
//!
//! The ports never delete a scale set (ADR 0040), and the registry removes a runner by its id
//! only, never by name (ADR 0029).

use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;

use mayfly_domain::Secret;

pub use mayfly_control_domain::attempt::RunnerRequestId;
pub use mayfly_control_domain::demand::{
    AssignedJob, EventName, JobKey, Offer, RepositoryName, RunFacts, RunLookup, Statistics,
    WorkflowJobId, WorkflowRunId,
};
pub use mayfly_control_domain::registration::{JobResult, RunnerId, ScaleSetId};
pub use mayfly_control_domain::scaleset::{
    CancelAnswer, JobFact, MessageId, MessageItem, ScaleSetCommand, SessionId,
};

/// What mayfly asks of the scale set it ensures for a pool (spec §3.1 "Ensure").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaleSetSpec {
    /// The scale set's name, unique within its runner group.
    pub name: String,
    /// The runner group's name (looked up by name: exactly one expected, spec §2).
    pub runner_group: String,
    /// Its labels; a job routed to it names them (`requestLabels`).
    pub labels: Vec<String>,
}

/// A scale set as GitHub answered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaleSetInfo {
    /// Its id, journaled by `EnsureScaleSet`.
    pub id: ScaleSetId,
    /// Its name.
    pub name: String,
    /// Its runner group's id.
    pub runner_group_id: u64,
    /// Its labels' names.
    pub labels: Vec<String>,
}

/// A session the adapter opened and holds (its queue URL and token stay inside it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenedSession {
    /// The session, journaled (spec §3.2).
    pub session_id: SessionId,
    /// The statistics the create answered: the session's initial demand (plan §7.2 step 5).
    pub statistics: Statistics,
}

/// One polled message (spec §4.2, §4.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolledMessage {
    /// Its `messageId`.
    pub message_id: MessageId,
    /// Its statistics, when the envelope carried them.
    pub statistics: Option<Statistics>,
    /// Its batch, in arrival order; an unknown envelope type is one [`MessageItem::Unknown`].
    pub items: Vec<MessageItem>,
}

/// A runner as the runner endpoints answer it (spec §2 "Runner reference").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerInfo {
    /// Its id.
    pub id: RunnerId,
    /// Its name (bounded; for mayfly's runners, the VM name).
    pub name: String,
    /// `runnerScaleSetId`, when the answer carried it.
    pub scale_set: Option<ScaleSetId>,
    /// `status == "online"`, when the answer carried a status (client-assumed shape).
    pub online: Option<bool>,
    /// `busy`, when the answer carried it (client-assumed shape).
    pub busy: Option<bool>,
}

/// A JIT configuration (spec §6.3): the runner it registered and the encoded config, a secret
/// that only the guest's launcher ever reads (ADR 0038).
#[derive(Debug)]
pub struct JitConfig {
    /// The registered runner; its id is journaled.
    pub runner: RunnerInfo,
    /// `encodedJITConfig`.
    pub encoded: Secret<String>,
}

/// What a removal by id answered (spec §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalOutcome {
    /// 204 or `AgentNotFoundException`: the runner is gone, which is not proof that mayfly
    /// removed it.
    Gone,
    /// `JobStillRunningException` (400 or 409): the runner is busy; nothing changed.
    Busy,
}

/// A workflow run as the lookup answered it (`GET /repos/{owner}/{repo}/actions/runs/{run_id}`,
/// spec §2, §6.2), every string bounded by the adapter. Its classifying part is
/// [`WorkflowRun::facts`]; the actor is kept for the operator's view of a call and never enters
/// the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRun {
    /// `id`: the run asked for (an answer for another run is protocol drift).
    pub id: WorkflowRunId,
    /// `repository.full_name`: the base repository.
    pub repository: RepositoryName,
    /// `head_repository.full_name`: a fork's when it is not the base.
    pub head_repository: RepositoryName,
    /// `event`; one the ACL cannot name is [`EventName::Unknown`] (unclassifiable, A5).
    pub event: EventName,
    /// `actor.login`, when the answer carried one.
    pub actor: Option<String>,
}

impl WorkflowRun {
    /// What the classification reads of it ([`mayfly_control_domain::demand::classify_assigned`]).
    #[must_use]
    pub fn facts(&self) -> RunFacts {
        RunFacts {
            repository: Some(self.repository.clone()),
            head_repository: Some(self.head_repository.clone()),
            event_name: self.event,
        }
    }
}

/// The observation a lookup's result makes: its facts, or [`RunLookup::Failed`] for every
/// failure (an error, a timeout, an absent run, a missing field, a throttle), so the domain
/// refuses the job `lookup_failed` (ADR 0090: a failed lookup refuses; fail closed). Pinned by
/// `github::tests::should_answer_failed_when_a_lookup_fails_in_any_way`.
#[must_use]
pub fn run_lookup(result: &Result<WorkflowRun, GitHubCallError>) -> RunLookup {
    match result {
        Ok(run) => RunLookup::Found(run.facts()),
        Err(_) => RunLookup::Failed,
    }
}

/// The observation a cancel's result makes, if any (ADR 0090, spec §6.2): an answer (202, 409 or
/// any other answered error) is `RecordCancelAnswer`, recorded once and never asked again; a lost
/// answer (no response, a deadline, no token to send) records nothing, so the cancel stays due
/// and is asked again (`ScaleSetState::refusal_action`). Pinned by
/// `github::tests::should_record_only_an_answer_when_a_cancel_is_answered_or_lost`.
#[must_use]
pub fn cancel_observation(
    job: &JobKey,
    result: &Result<CancelAnswer, GitHubCallError>,
) -> Option<ScaleSetCommand> {
    result
        .as_ref()
        .ok()
        .map(|answer| ScaleSetCommand::RecordCancelAnswer {
            job: job.clone(),
            answer: *answer,
        })
}

/// Asks GitHub to cancel a refused job's run and answers the observation to record, if any
/// (`RefusalAction::CancelRun`, GH-08 drives it). `repository` is the assignment's; a refused job
/// whose assignment named none can never be cancelled, so its cancel is recorded
/// [`CancelAnswer::Refused`] at once, with no request: a cancel that can never be sent never
/// stays due, so it can neither come back on every pass nor hold back the next refused job's
/// cancel. Otherwise as [`cancel_observation`]. Pinned by
/// `tests/store/listener.rs::should_record_refused_without_a_call_when_a_cancel_can_never_be_sent`.
pub async fn cancel_refused<W: WorkflowRuns>(
    runs: &W,
    job: &JobKey,
    repository: Option<&RepositoryName>,
) -> Option<ScaleSetCommand> {
    let answer = match repository {
        Some(repository) => runs.cancel_run(repository, job.run_id).await,
        None => Ok(CancelAnswer::Refused),
    };
    cancel_observation(job, &answer)
}

/// The class of a failed call (spec §2, §3, §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// A create answered `RunnerScaleSetSessionConflictException`: a session lives (A3).
    SessionConflict,
    /// The session is gone: a refresh answered `RunnerScaleSetSessionExpiredException`, or a
    /// queue call answered 401 again after one refresh (spec §3.1, §3.3). Open a new one.
    SessionExpired,
    /// The adapter holds no live session for the scale set (none opened, or it was lost).
    NoSession,
    /// The scale set the call named answered 404: GitHub deleted it (dormant, spec §3.1).
    ScaleSetGone,
    /// `AgentExistsException`: the JIT name is taken (spec §6.3). Never re-minted under a new
    /// name (I19).
    NameTaken,
    /// A rate limit, or `ConfigureTooManyRunnersException`, still standing after the retries.
    Throttled,
    /// A definite refusal of this request, with its status.
    Refused {
        /// The status.
        status: u16,
    },
    /// The App's credentials could not be used (the key, the JWT, the exchange).
    Credentials,
    /// No definite answer: a transport error, a deadline, a 5xx after the retries, a poll's 200
    /// with an empty or undecodable body. The outcome is unknown.
    Unavailable,
    /// The service answered what the protocol does not allow (a success without a required
    /// field, a queue URL a token could leak through, two scale sets of one name).
    ProtocolDrift,
}

impl Failure {
    /// A short, fixed name of the class.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionConflict => "session_conflict",
            Self::SessionExpired => "session_expired",
            Self::NoSession => "no_session",
            Self::ScaleSetGone => "scale_set_gone",
            Self::NameTaken => "name_taken",
            Self::Throttled => "throttled",
            Self::Refused { .. } => "refused",
            Self::Credentials => "credentials",
            Self::Unavailable => "unavailable",
            Self::ProtocolDrift => "protocol_drift",
        }
    }
}

/// A failed call: its class and redacted text (no token, query string or response body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubCallError {
    /// The class.
    pub failure: Failure,
    /// What happened, redacted.
    pub detail: String,
}

impl GitHubCallError {
    /// A failure of `failure` described by `detail`.
    #[must_use]
    pub fn new(failure: Failure, detail: impl Into<String>) -> Self {
        Self {
            failure,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for GitHubCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.failure.as_str(), self.detail)
    }
}

impl std::error::Error for GitHubCallError {}

/// The Runner Scale Set protocol (spec §2–§6), at organisation scope (repository scope for
/// tests, ADR 0040). Every call has a deadline; none deletes a scale set.
pub trait ScaleSetApi: Send + Sync {
    /// The scale set named `spec.name` in the runner group `spec.runner_group`, if any. More
    /// than one is [`Failure::ProtocolDrift`].
    ///
    /// # Errors
    /// [`GitHubCallError`].
    fn find_scale_set(
        &self,
        spec: &ScaleSetSpec,
    ) -> impl Future<Output = Result<Option<ScaleSetInfo>, GitHubCallError>> + Send;

    /// The scale set `id`; [`Failure::ScaleSetGone`] when GitHub deleted it.
    ///
    /// # Errors
    /// [`GitHubCallError`].
    fn get_scale_set(
        &self,
        id: ScaleSetId,
    ) -> impl Future<Output = Result<ScaleSetInfo, GitHubCallError>> + Send;

    /// Creates the scale set `spec` describes.
    ///
    /// # Errors
    /// [`GitHubCallError`].
    fn create_scale_set(
        &self,
        spec: &ScaleSetSpec,
    ) -> impl Future<Output = Result<ScaleSetInfo, GitHubCallError>> + Send;

    /// Opens a session on `scale_set` for `owner` (the host and pool, never a secret) and
    /// holds it. [`Failure::SessionConflict`] when one lives (spec §3.1).
    ///
    /// # Errors
    /// [`GitHubCallError`].
    fn create_session(
        &self,
        scale_set: ScaleSetId,
        owner: &str,
    ) -> impl Future<Output = Result<OpenedSession, GitHubCallError>> + Send;

    /// Deletes `session` (the live one, or one journaled before a restart). A session already
    /// gone (`RunnerScaleSetSessionExpiredException`) counts as deleted.
    ///
    /// # Errors
    /// [`GitHubCallError`].
    fn delete_session(
        &self,
        scale_set: ScaleSetId,
        session: SessionId,
    ) -> impl Future<Output = Result<(), GitHubCallError>> + Send;

    /// The long poll (spec §4.1) on the held session: `last` is sent as `lastMessageId` (only
    /// when given and above 0), `max_capacity` as `X-ScaleSetMaxCapacity`. `None` is the 202 of
    /// an expired hold. A 401 refreshes the session and retries once.
    ///
    /// # Errors
    /// [`GitHubCallError`]; a 200 whose body is empty or undecodable is
    /// [`Failure::Unavailable`] (nothing to record or ack).
    fn poll(
        &self,
        scale_set: ScaleSetId,
        last: Option<MessageId>,
        max_capacity: u32,
    ) -> impl Future<Output = Result<Option<PolledMessage>, GitHubCallError>> + Send;

    /// Acknowledges `message` (spec §4.4); a 404 counts as done (the inbox record exists).
    ///
    /// # Errors
    /// [`GitHubCallError`].
    fn ack(
        &self,
        scale_set: ScaleSetId,
        message: MessageId,
    ) -> impl Future<Output = Result<(), GitHubCallError>> + Send;

    /// Acquires `requests` (GHES, spec §6.1) and answers the ids GitHub listed acquired, within
    /// those asked for: an id not listed is not acquired (A4).
    ///
    /// # Errors
    /// [`GitHubCallError`].
    fn acquire_jobs(
        &self,
        scale_set: ScaleSetId,
        requests: &BTreeSet<RunnerRequestId>,
    ) -> impl Future<Output = Result<BTreeSet<RunnerRequestId>, GitHubCallError>> + Send;

    /// The offers waiting for the scale set (GHES, listed after a session (re)opens, spec §6.1).
    ///
    /// # Errors
    /// [`GitHubCallError`].
    fn acquirable_jobs(
        &self,
        scale_set: ScaleSetId,
    ) -> impl Future<Output = Result<Vec<Offer>, GitHubCallError>> + Send;

    /// Registers a JIT runner named `runner_name` (once per registration, I19) with
    /// `work_folder` (spec §6.3).
    ///
    /// # Errors
    /// [`GitHubCallError`]: [`Failure::NameTaken`] for a taken name, [`Failure::Throttled`]
    /// for a throttle the retries did not outlast.
    fn generate_jit_config(
        &self,
        scale_set: ScaleSetId,
        runner_name: &str,
        work_folder: &str,
    ) -> impl Future<Output = Result<JitConfig, GitHubCallError>> + Send;
}

/// The runner endpoints (spec §2, §7): get and remove by id, list by name or all. Nothing here
/// registers a runner with a registration token (ADR 0028).
pub trait RunnerRegistry: Send + Sync {
    /// The runner `id`, `None` when it does not exist.
    ///
    /// # Errors
    /// [`GitHubCallError`].
    fn get_runner(
        &self,
        id: RunnerId,
    ) -> impl Future<Output = Result<Option<RunnerInfo>, GitHubCallError>> + Send;

    /// The runners named exactly `name`.
    ///
    /// # Errors
    /// [`GitHubCallError`].
    fn find_runners(
        &self,
        name: &str,
    ) -> impl Future<Output = Result<Vec<RunnerInfo>, GitHubCallError>> + Send;

    /// Every runner the service lists (`agents` without `agentName`): the reaper's listing
    /// (policy P14, CP-05), which it reads at most once an hour.
    ///
    /// # Errors
    /// [`GitHubCallError`].
    fn list_runners(&self)
    -> impl Future<Output = Result<Vec<RunnerInfo>, GitHubCallError>> + Send;

    /// Removes the runner `id`, and only it.
    ///
    /// # Errors
    /// [`GitHubCallError`].
    fn remove_runner(
        &self,
        id: RunnerId,
    ) -> impl Future<Output = Result<RemovalOutcome, GitHubCallError>> + Send;
}

/// The REST workflow-run endpoints of ADR 0090's per-job check (spec §2, §6.2), on the
/// installation token: the lookup of an assigned job's run ("Actions: Read") and the cancel of a
/// refused run (`cancel_refused_runs` only, "Actions: Write", ADR 0045). Every call has a
/// deadline. `repository` is the assignment's `owner/name`; one that is not a plain
/// `owner/name` is never put in a URL ([`Failure::ProtocolDrift`]).
pub trait WorkflowRuns: Send + Sync {
    /// The run `run` of `repository`. Any failure is for the caller to read as
    /// [`RunLookup::Failed`] ([`run_lookup`]): an absent run, a timeout, a 200 without a field
    /// the classification needs, a `repository` that is not a plain `owner/name`.
    ///
    /// # Errors
    /// [`GitHubCallError`].
    fn workflow_run(
        &self,
        repository: &RepositoryName,
        run: WorkflowRunId,
    ) -> impl Future<Output = Result<WorkflowRun, GitHubCallError>> + Send;

    /// Asks GitHub to cancel the run `run` of `repository`, once: never retried past an
    /// answered attempt (a 401, which did nothing, is resent once with a fresh token). 202 is
    /// [`CancelAnswer::Accepted`]; 409 (the run is over) and any other answered error are
    /// [`CancelAnswer::Refused`], and so is a `repository` that is not a plain `owner/name`,
    /// which no request can ever name: nothing is sent, and it never stays due.
    ///
    /// # Errors
    /// [`GitHubCallError`] when nothing answered (a lost answer, a deadline) or nothing could be
    /// sent this time (the credentials): the cancel's outcome is not known and nothing is
    /// recorded ([`cancel_observation`]), so it is asked again.
    fn cancel_run(
        &self,
        repository: &RepositoryName,
        run: WorkflowRunId,
    ) -> impl Future<Output = Result<CancelAnswer, GitHubCallError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> WorkflowRun {
        WorkflowRun {
            id: WorkflowRunId(7),
            repository: RepositoryName("acme/app".to_owned()),
            head_repository: RepositoryName("contributor/app".to_owned()),
            event: EventName::PullRequest,
            actor: Some("contributor".to_owned()),
        }
    }

    fn job() -> JobKey {
        JobKey {
            job_id: WorkflowJobId("11".to_owned()),
            run_id: WorkflowRunId(7),
        }
    }

    #[test]
    fn should_answer_failed_when_a_lookup_fails_in_any_way() {
        // given
        let failures = [
            Failure::Refused { status: 404 },
            Failure::Unavailable,
            Failure::Throttled,
            Failure::ProtocolDrift,
            Failure::Credentials,
        ];

        // when
        let found = run_lookup(&Ok(run()));
        let failed: Vec<RunLookup> = failures
            .iter()
            .map(|f| run_lookup(&Err(GitHubCallError::new(*f, "synthetic"))))
            .collect();

        // then: the facts carry the fork's head; every failure refuses
        assert_eq!(
            found,
            RunLookup::Found(RunFacts {
                repository: Some(RepositoryName("acme/app".to_owned())),
                head_repository: Some(RepositoryName("contributor/app".to_owned())),
                event_name: EventName::PullRequest,
            })
        );
        assert!(failed.iter().all(|l| *l == RunLookup::Failed));
    }

    #[test]
    fn should_record_only_an_answer_when_a_cancel_is_answered_or_lost() {
        // given
        let answers = [
            Ok(CancelAnswer::Accepted),
            Ok(CancelAnswer::Refused),
            Err(GitHubCallError::new(Failure::Unavailable, "lost")),
        ];

        // when
        let observed: Vec<Option<ScaleSetCommand>> = answers
            .iter()
            .map(|a| cancel_observation(&job(), a))
            .collect();

        // then
        let record = |answer| Some(ScaleSetCommand::RecordCancelAnswer { job: job(), answer });
        assert_eq!(
            observed,
            [
                record(CancelAnswer::Accepted),
                record(CancelAnswer::Refused),
                None
            ]
        );
    }
}
