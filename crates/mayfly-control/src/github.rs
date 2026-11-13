//! The GitHub ports (plan §4, §7.2, §7.3): [`ScaleSetApi`], the Runner Scale Set protocol as
//! the listener and the registration manager use it, and [`RunnerRegistry`], the runner
//! endpoints of `doc/github-scaleset.md` §2 and §7.
//!
//! The adapter (`mayfly-adapter-github`, GH-03) speaks the wire; the in-memory fake lives in
//! `mayfly-testkit::github`; both pass the contract suite `mayfly_testkit::contracts::scaleset`
//! (ver §5). What crosses these ports is already the domain's language: messages arrive as
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
    AssignedJob, EventName, JobKey, Offer, RepositoryName, Statistics, WorkflowJobId, WorkflowRunId,
};
pub use mayfly_control_domain::registration::{JobResult, RunnerId, ScaleSetId};
pub use mayfly_control_domain::scaleset::{JobFact, MessageId, MessageItem, SessionId};

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
