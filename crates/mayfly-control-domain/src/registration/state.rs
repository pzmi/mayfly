//! The registration's folded state, the value types its commands and events carry, its commands
//! and its refusals.

use std::collections::BTreeSet;

use mayfly_domain::{AttemptId, Epoch, EventId, OwnedVmName};
use serde::{Deserialize, Serialize};

use crate::aggregate::{CommandBody, Rejection};
use crate::attempt::RunnerRequestId;

/// GitHub's id of a runner, answered with its JIT config (`doc/github-scaleset.md` §6): the only
/// handle mayfly deregisters by (I19).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunnerId(pub u64);

/// GitHub's id of a runner scale set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScaleSetId(pub u64);

/// The scale set's session as the policy read it when it asked for a registration: minting is
/// frozen while it is degraded (P21).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionHealth {
    /// Minting allowed.
    Healthy,
    /// GitHub's auth or session is failing: no new JIT config.
    Degraded,
}

/// Where the registration's runner is, as mayfly has observed it (`doc/domain-model.md` §6.5).
/// `PresumedLeaked` and `DeregistrationRequested` of the domain model are the
/// [`RegistrationState::leaked`] and [`RegistrationState::deregistration`] overlays: a leaked
/// runner still moves through these phases (it may still connect and take a job), and a
/// deregistration may be pending in any phase that holds a runner id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// No `RegistrationRequested` yet.
    Unrequested,
    /// The JIT config is being minted (P9).
    Requested,
    /// The mint's outcome is unknown: GitHub may hold a runner of this name. Left only on the
    /// reaper's observation (P10), never by a new mint.
    IssueUnknown,
    /// Minted: the runner id is journaled; the JIT config is with the broker.
    Issued,
    /// The JIT config reached the guest's launcher (`LaunchRunner` observed done).
    Delivered,
    /// GitHub showed the issued runner id online (ADR 0038).
    Connected,
    /// The runner took its job (`JobStarted`).
    Consumed,
    /// Its job completed (`JobCompleted`).
    Completed,
    /// GitHub no longer lists the runner: removed (204) or already gone (404). Terminal.
    Deregistered,
    /// GitHub never held a runner for it: the mint was refused, or the reaper found no runner
    /// of its name. Terminal.
    Absent,
}

/// Why a registration is presumed leaked (P13): its JIT config may have reached something other
/// than this attempt's runner, so the runner is deregistered and the attempt never serves a job
/// (I22).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeakReason {
    /// The launch's outcome is unknown (`RunnerLaunchUnknown`).
    LaunchUnknown,
    /// A restart orphaned the JIT handle (`JitHandleOrphaned`).
    HandleOrphaned,
    /// The issued runner id was not seen online by the delivery deadline (ADR 0038).
    NotSeenOnline,
    /// The guest's identity was doubted after delivery.
    IdentityDoubt,
    /// A secret was found where it must never be.
    RedactionViolation,
    /// The mint's outcome was unknown and the reaper found the runner by name: mayfly never held
    /// its config, GitHub's reply may have gone anywhere (P10).
    IssueUnknown,
}

/// Why a deregistration was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeregistrationReason {
    /// An attempt's retirement (P11): deregister before stop. A busy answer cancels exactly this
    /// retirement.
    Retirement {
        /// The attempt's `RetirementRequested` event: P11's idempotency key.
        retirement: EventId,
    },
    /// The registration is presumed leaked (P13).
    PresumedLeaked,
    /// The reaper found the journaled runner not live (P14).
    NotLive,
}

impl DeregistrationReason {
    /// The retirement this request serves, if any.
    #[must_use]
    pub const fn retirement(self) -> Option<EventId> {
        match self {
            Self::Retirement { retirement } => Some(retirement),
            Self::PresumedLeaked | Self::NotLive => None,
        }
    }
}

/// GitHub's answer to the removal of a runner by its id (`doc/github-scaleset.md` §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeregistrationAnswer {
    /// 204: removed.
    Removed,
    /// 404: not found, already gone (the service removes an ephemeral runner after its job).
    NotFound,
    /// The runner is busy: 409 `JobStillRunningException` on the Actions-service path mayfly
    /// uses, 422 on the REST path. Named by its meaning, never by a status code.
    Busy,
}

/// How a deregistration ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Removal {
    /// 204.
    Removed,
    /// 404.
    NotFound,
}

/// A job's result as `JobCompleted` reported it. The full set of GitHub's values is unverified
/// until GH-00's capture (`doc/github-scaleset.md` §4.3); anything else is `other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobResult {
    /// `succeeded`.
    Succeeded,
    /// `failed`.
    Failed,
    /// `canceled`.
    Canceled,
    /// Any other value.
    Other,
}

/// The state of one registration stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationState {
    /// Where the runner is.
    pub phase: Phase,
    /// The highest run epoch accepted (I18).
    pub epoch: Epoch,
    /// The attempt it registers a runner for.
    pub attempt_id: Option<AttemptId>,
    /// The runner name: the attempt's VM name, minted at most once (I19).
    pub runner_name: Option<OwnedVmName>,
    /// The scale set the runner joins.
    pub scale_set: Option<ScaleSetId>,
    /// The journaled runner id, from `RegistrationIssued` or `RegistrationFoundByName`.
    pub runner_id: Option<RunnerId>,
    /// The job the runner took.
    pub request_id: Option<RunnerRequestId>,
    /// The job's result.
    pub result: Option<JobResult>,
    /// Presumed leaked, and why (P13).
    pub leaked: Option<LeakReason>,
    /// A deregistration waiting for GitHub's answer.
    pub deregistration: Option<DeregistrationReason>,
    /// The retirements GitHub answered busy for: final for each of them (I21).
    pub busy_refused: BTreeSet<EventId>,
}

impl RegistrationState {
    /// The state of a stream with no events.
    #[must_use]
    pub const fn initial() -> Self {
        Self {
            phase: Phase::Unrequested,
            epoch: Epoch::new(0),
            attempt_id: None,
            runner_name: None,
            scale_set: None,
            runner_id: None,
            request_id: None,
            result: None,
            leaked: None,
            deregistration: None,
            busy_refused: BTreeSet::new(),
        }
    }
}

/// `RequestRegistration`: the activation manager asks for one JIT runner (P9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RequestRegistration {
    /// The attempt (the stream is `registration-<attempt_id>`).
    pub attempt_id: AttemptId,
    /// The runner name: the attempt's VM name.
    pub runner_name: OwnedVmName,
    /// The scale set.
    pub scale_set: ScaleSetId,
    /// The scale set's session as the policy read it.
    pub session: SessionHealth,
}

/// The commands of `doc/domain-model.md` §6.5, plus the online confirmation of ADR 0038
/// (`RecordConnected`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum RegistrationCommand {
    /// Ask for the registration: once per stream, ever.
    RequestRegistration(RequestRegistration),
    /// The mint answered with the runner id (the JIT config went to the broker, not here).
    RecordIssued {
        /// GitHub's runner id.
        runner_id: RunnerId,
    },
    /// The mint's outcome is unknown (a lost reply, a crash before its record).
    RecordIssueUnknown,
    /// The JIT config reached the guest's launcher.
    RecordDelivered,
    /// GitHub shows a runner id online for this registration (ADR 0038).
    RecordConnected {
        /// The runner id seen online; it must be the issued one.
        runner_id: RunnerId,
    },
    /// `JobStarted` named this runner.
    RecordConsumed {
        /// The job.
        request_id: RunnerRequestId,
    },
    /// `JobCompleted` named this runner.
    RecordCompleted {
        /// The job's result.
        result: JobResult,
    },
    /// P13: presume the JIT config leaked.
    PresumeLeaked {
        /// Why.
        reason: LeakReason,
    },
    /// Remove the runner by its journaled id. `PresumedLeaked` and `NotLive` are accepted whether
    /// or not the stream records a leak: P13 may send them with the `PresumeLeaked` in flight, and
    /// P14's `NotLive` is a policy input (GitHub shows the runner not live), so the aggregate is
    /// deliberately looser here than the model's `requestDeregistration`, which takes the leak
    /// path only once `leaked` is set. Only a journaled runner id is ever required (I19).
    RequestDeregistration {
        /// Why.
        reason: DeregistrationReason,
    },
    /// GitHub answered the removal.
    RecordDeregistration {
        /// The answer.
        answer: DeregistrationAnswer,
    },
    /// The reaper listed runners by this registration's name and found one (P10).
    RecordFoundByName {
        /// Its id.
        runner_id: RunnerId,
    },
    /// GitHub holds no runner for this registration: the mint was refused, or the reaper found
    /// none of its name.
    RecordAbsent,
}

impl CommandBody for RegistrationCommand {
    const TYPES: &'static [&'static str] = &[
        "request_registration",
        "record_issued",
        "record_issue_unknown",
        "record_delivered",
        "record_connected",
        "record_consumed",
        "record_completed",
        "presume_leaked",
        "request_deregistration",
        "record_deregistration",
        "record_found_by_name",
        "record_absent",
    ];

    fn command_type(&self) -> &'static str {
        match self {
            Self::RequestRegistration(_) => "request_registration",
            Self::RecordIssued { .. } => "record_issued",
            Self::RecordIssueUnknown => "record_issue_unknown",
            Self::RecordDelivered => "record_delivered",
            Self::RecordConnected { .. } => "record_connected",
            Self::RecordConsumed { .. } => "record_consumed",
            Self::RecordCompleted { .. } => "record_completed",
            Self::PresumeLeaked { .. } => "presume_leaked",
            Self::RequestDeregistration { .. } => "request_deregistration",
            Self::RecordDeregistration { .. } => "record_deregistration",
            Self::RecordFoundByName { .. } => "record_found_by_name",
            Self::RecordAbsent => "record_absent",
        }
    }
}

/// A refusal of the `RunnerRegistration` aggregate. Its serde form is its code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationRejection {
    /// The command's run epoch is older than the registration's (I18).
    StaleRun,
    /// The scale set's session is degraded: no JIT config is minted.
    ScaleSetDegraded,
    /// The runner name is not the attempt's VM name.
    RunnerNameMismatch,
    /// The stream already asked for its registration: a name is issued at most once (I19).
    AlreadyRequested,
    /// The stream has no `RegistrationRequested`.
    NotRequested,
    /// Deregistered or absent: nothing more happens to it.
    Terminal,
    /// No mint is waiting for its outcome.
    NoIssuePending,
    /// No issue of unknown outcome to resolve.
    IssueNotUnknown,
    /// The JIT config was not issued.
    NotIssued,
    /// The JIT config was not delivered.
    NotDelivered,
    /// A runner id other than the journaled one.
    RunnerIdMismatch,
    /// No runner id is journaled: nothing can be deregistered or bound (I19).
    NoRunnerId,
    /// The runner already took another job.
    AlreadyConsumed,
    /// No job was recorded.
    NotConsumed,
    /// GitHub answered busy for this retirement: it is withdrawn, never retried (I21).
    BusyRefused,
    /// No deregistration is waiting for its answer.
    NoDeregistrationPending,
    /// The registration is presumed leaked: its config is never delivered (I22).
    PresumedLeaked,
}

impl Rejection for RegistrationRejection {
    const CODES: &'static [&'static str] = &[
        "stale_run",
        "scale_set_degraded",
        "runner_name_mismatch",
        "already_requested",
        "not_requested",
        "terminal",
        "no_issue_pending",
        "issue_not_unknown",
        "not_issued",
        "not_delivered",
        "runner_id_mismatch",
        "no_runner_id",
        "already_consumed",
        "not_consumed",
        "busy_refused",
        "no_deregistration_pending",
        "presumed_leaked",
    ];

    fn code(&self) -> &'static str {
        match self {
            Self::StaleRun => "stale_run",
            Self::ScaleSetDegraded => "scale_set_degraded",
            Self::RunnerNameMismatch => "runner_name_mismatch",
            Self::AlreadyRequested => "already_requested",
            Self::NotRequested => "not_requested",
            Self::Terminal => "terminal",
            Self::NoIssuePending => "no_issue_pending",
            Self::IssueNotUnknown => "issue_not_unknown",
            Self::NotIssued => "not_issued",
            Self::NotDelivered => "not_delivered",
            Self::RunnerIdMismatch => "runner_id_mismatch",
            Self::NoRunnerId => "no_runner_id",
            Self::AlreadyConsumed => "already_consumed",
            Self::NotConsumed => "not_consumed",
            Self::BusyRefused => "busy_refused",
            Self::NoDeregistrationPending => "no_deregistration_pending",
            Self::PresumedLeaked => "presumed_leaked",
        }
    }
}
