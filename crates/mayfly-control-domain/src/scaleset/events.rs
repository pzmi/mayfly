//! The scale set's events (`doc/domain-model.md` §6.1), with golden samples under
//! `tests/fixtures/events/<type>/`. No event carries a token: the session id is journaled, its
//! queue token never is (ADR 0014).

use mayfly_domain::{Epoch, PoolName};
use serde::{Deserialize, Serialize};

use super::state::{
    CancelAnswer, DegradedCause, JobFact, LossCause, MessageId, MessageOrigin, SessionId,
};
use crate::attempt::RunnerRequestId;
use crate::demand::{Capacity, Declined, JobKey, RefusalReason, Statistics, TrustPolicy};
use crate::event::DomainEvent;
use crate::event_set;
use crate::pool::TrustTier;
use crate::registration::ScaleSetId;

/// The scale set accepted a command under a newer run epoch than any before (I18); recorded
/// ahead of that command's events so `evolve` knows the epoch to fence with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ScaleSetEpochAdvanced {
    /// The new highest run epoch.
    pub epoch: Epoch,
}

impl DomainEvent for ScaleSetEpochAdvanced {
    const EVENT_TYPE: &'static str = "scale_set_epoch_advanced";
    const SCHEMA_VERSION: u32 = 1;
}

/// The pool's scale set exists at GitHub (found or created; never deleted by mayfly, ADR 0040).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ScaleSetEnsured {
    /// The pool.
    pub pool: PoolName,
    /// GitHub's id of the scale set.
    pub github_id: ScaleSetId,
    /// The pool's tier.
    pub tier: TrustTier,
}

impl DomainEvent for ScaleSetEnsured {
    const EVENT_TYPE: &'static str = "scale_set_ensured";
    const SCHEMA_VERSION: u32 = 1;
}

/// A session create was decided and is about to be sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionOpenRequested {}

impl DomainEvent for SessionOpenRequested {
    const EVENT_TYPE: &'static str = "session_open_requested";
    const SCHEMA_VERSION: u32 = 1;
}

/// A session opened; its initial statistics are authoritative and desired is recomputed from
/// them, without any message (plan §7.2 step 5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionOpened {
    /// The session.
    pub session_id: SessionId,
    /// Its initial statistics.
    pub statistics: Statistics,
    /// `min(deliverable, totalAssignedJobs)` of them.
    pub desired: u32,
}

impl DomainEvent for SessionOpened {
    const EVENT_TYPE: &'static str = "session_opened";
    const SCHEMA_VERSION: u32 = 1;
}

/// The session, or its create, was lost; the scale set backs off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionLost {
    /// Why.
    pub cause: LossCause,
    /// The session to delete before the next create, if one was live.
    pub stale_session: Option<SessionId>,
}

impl DomainEvent for SessionLost {
    const EVENT_TYPE: &'static str = "session_lost";
    const SCHEMA_VERSION: u32 = 1;
}

/// Creates answered 409 [`super::SESSION_CONFLICT_ESCALATION`] times in a row since a session
/// last opened: a session mayfly has no record of blocks the pool. Loud for the operator; the
/// creates go on backing off and the session is never deleted (A3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionConflictEscalated {
    /// The 409s in a row so far.
    pub conflicts: u32,
}

impl DomainEvent for SessionConflictEscalated {
    const EVENT_TYPE: &'static str = "session_conflict_escalated";
    const SCHEMA_VERSION: u32 = 1;
}

/// A journaled stale session was deleted (204, or 404: it had ended).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionDeleted {
    /// The session.
    pub session_id: SessionId,
}

impl DomainEvent for SessionDeleted {
    const EVENT_TYPE: &'static str = "session_deleted";
    const SCHEMA_VERSION: u32 = 1;
}

/// One polled message, in one unit: the statistics (applied only when newer than every message
/// recorded, A1), the acquire plan (journaled before the call), the declined offers, the job
/// facts, the unknown types counted and the new desired count (plan §7.2 step 3). A message
/// that is not [`MessageOrigin::Fresh`] is recorded so it can be acked and applies nothing: its
/// lists are empty, its unknown types 0 and desired unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MessageRecorded {
    /// The session it was polled in.
    pub session_id: SessionId,
    /// Its id.
    pub message_id: MessageId,
    /// Where it stands against the session's floor (A1).
    pub origin: MessageOrigin,
    /// Its statistics, when the envelope carried them.
    pub statistics: Option<Statistics>,
    /// Whether they replaced the latest ones (false for an older message's).
    pub statistics_applied: bool,
    /// Reservations open before the message that its job facts released, in arrival order.
    pub released: Vec<RunnerRequestId>,
    /// The request ids planned and still pending at the end of the batch: the listener acquires
    /// these (`AcquireJobs`), and only these.
    pub acquire: Vec<RunnerRequestId>,
    /// The offers left, with why.
    pub declined: Vec<Declined>,
    /// The batch's job facts, in arrival order.
    pub job_facts: Vec<JobFact>,
    /// Unknown message types in the message (R7).
    pub unknown_types: u32,
    /// `min(deliverable, totalAssignedJobs)` of the latest statistics.
    pub desired: u32,
}

impl DomainEvent for MessageRecorded {
    const EVENT_TYPE: &'static str = "message_recorded";
    const SCHEMA_VERSION: u32 = 1;
}

/// `AcquireJobs` answered: the acquired ids are reserved until a runner takes them; the others
/// are released (A4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AcquireObserved {
    /// Acquired.
    pub acquired: Vec<RunnerRequestId>,
    /// Asked for and not acquired.
    pub released: Vec<RunnerRequestId>,
}

impl DomainEvent for AcquireObserved {
    const EVENT_TYPE: &'static str = "acquire_observed";
    const SCHEMA_VERSION: u32 = 1;
}

/// The capacity policy's input was applied: what the next poll sends, and desired recomputed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CapacityApplied {
    /// The input.
    pub capacity: Capacity,
    /// The `maxCapacity` the input sets (ADR 0029), a snapshot at the time it was applied: while a
    /// refusal sink is up the wire max also follows the latest statistics' assigned count, so a
    /// poller reads [`super::ScaleSetState::wire_max`], never this field (GH-04).
    pub wire_max: u32,
    /// `min(deliverable, totalAssignedJobs)` of the latest statistics.
    pub desired: u32,
}

impl DomainEvent for CapacityApplied {
    const EVENT_TYPE: &'static str = "capacity_applied";
    const SCHEMA_VERSION: u32 = 1;
}

/// The live session is degraded; it keeps recording, never enters safe mode (R7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionDegraded {
    /// Why.
    pub cause: DegradedCause,
}

impl DomainEvent for SessionDegraded {
    const EVENT_TYPE: &'static str = "session_degraded";
    const SCHEMA_VERSION: u32 = 1;
}

/// The operator cleared the degradation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionRecovered {}

impl DomainEvent for SessionRecovered {
    const EVENT_TYPE: &'static str = "session_recovered";
    const SCHEMA_VERSION: u32 = 1;
}

/// The live session is being deleted in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ClosingStarted {}

impl DomainEvent for ClosingStarted {
    const EVENT_TYPE: &'static str = "closing_started";
    const SCHEMA_VERSION: u32 = 1;
}

/// The closing session was deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionClosed {
    /// The session.
    pub session_id: SessionId,
}

impl DomainEvent for SessionClosed {
    const EVENT_TYPE: &'static str = "session_closed";
    const SCHEMA_VERSION: u32 = 1;
}

/// A session's first message came strictly below its floor: it cannot be a redelivery (those
/// come oldest first), so GitHub restarted its numbering (A1 refuted, `doc/github-scaleset.md`
/// §4.1). Recorded for the operator ahead of the message, which applies as new; the cursor and
/// the floor restart from it (`formal/scaleset.qnt`'s `a1RestartIsRecoveredTest`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MessageIdsRestarted {
    /// The message that starts the new numbering.
    pub message_id: MessageId,
    /// The session's floor it came below.
    pub floor: MessageId,
}

impl DomainEvent for MessageIdsRestarted {
    const EVENT_TYPE: &'static str = "message_ids_restarted";
    const SCHEMA_VERSION: u32 = 1;
}

/// The pool's trust policy (GH-08) now classifies every later lookup (ADR 0090).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TrustPolicyApplied {
    /// The policy.
    pub policy: TrustPolicy,
}

impl DomainEvent for TrustPolicyApplied {
    const EVENT_TYPE: &'static str = "trust_policy_applied";
    const SCHEMA_VERSION: u32 = 1;
}

/// A pending assignment's workflow run was looked up and the job classified trusted: a runner
/// may start for it (ADR 0090).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AssignedJobTrusted {
    /// The job.
    pub job: JobKey,
}

impl DomainEvent for AssignedJobTrusted {
    const EVENT_TYPE: &'static str = "assigned_job_trusted";
    const SCHEMA_VERSION: u32 = 1;
}

/// A pending assignment was refused: no runner starts for it, and it goes to a refusal sink or,
/// with `cancel_refused_runs`, its run is cancelled (ADR 0090). A recorded fact for the
/// operator, never only a log line; the use case that records it logs `job_refused` at WARN
/// (GH-08).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AssignedJobRefused {
    /// The job.
    pub job: JobKey,
    /// Why.
    pub reason: RefusalReason,
}

impl DomainEvent for AssignedJobRefused {
    const EVENT_TYPE: &'static str = "assigned_job_refused";
    const SCHEMA_VERSION: u32 = 1;
}

/// A refusal sink was decided for a refused job (IMG-03): the wire max is pinned at the assigned
/// count until it ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SinkStarted {
    /// The refused job it was started for (GitHub picks the job it gets).
    pub job: JobKey,
}

impl DomainEvent for SinkStarted {
    const EVENT_TYPE: &'static str = "sink_started";
    const SCHEMA_VERSION: u32 = 1;
}

/// The refusal sink's VM was deleted: the pin lifts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SinkEnded {}

impl DomainEvent for SinkEnded {
    const EVENT_TYPE: &'static str = "sink_ended";
    const SCHEMA_VERSION: u32 = 1;
}

/// GitHub answered the cancel of a refused job's run; it is never asked again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CancelAnswered {
    /// The refused job.
    pub job: JobKey,
    /// The answer.
    pub answer: CancelAnswer,
}

impl DomainEvent for CancelAnswered {
    const EVENT_TYPE: &'static str = "cancel_answered";
    const SCHEMA_VERSION: u32 = 1;
}

/// A create or read answered 404 for the journaled scale set: GitHub deleted it as dormant
/// (`doc/github-scaleset.md` §3.1). The stream is `Unconfigured` until it is re-created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ScaleSetVanished {
    /// The scale set that vanished.
    pub github_id: ScaleSetId,
}

impl DomainEvent for ScaleSetVanished {
    const EVENT_TYPE: &'static str = "scale_set_vanished";
    const SCHEMA_VERSION: u32 = 1;
}

/// The vanished scale set was created again: a new id, a new numbering (cursor and floor reset),
/// and the old set's reservations, classifications and refusals dropped, since its assigned jobs
/// went back to GitHub's queue (`formal/scaleset.qnt`'s `recreateScaleSet`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ScaleSetRecreated {
    /// GitHub's id of the new scale set.
    pub github_id: ScaleSetId,
}

impl DomainEvent for ScaleSetRecreated {
    const EVENT_TYPE: &'static str = "scale_set_recreated";
    const SCHEMA_VERSION: u32 = 1;
}

event_set! {
    /// Everything that happens to a pool's scale set.
    pub enum ScaleSetEvent {
        /// See [`ScaleSetEpochAdvanced`].
        ScaleSetEpochAdvanced(ScaleSetEpochAdvanced),
        /// See [`ScaleSetEnsured`].
        ScaleSetEnsured(ScaleSetEnsured),
        /// See [`SessionOpenRequested`].
        SessionOpenRequested(SessionOpenRequested),
        /// See [`SessionOpened`].
        SessionOpened(SessionOpened),
        /// See [`SessionLost`].
        SessionLost(SessionLost),
        /// See [`SessionConflictEscalated`].
        SessionConflictEscalated(SessionConflictEscalated),
        /// See [`SessionDeleted`].
        SessionDeleted(SessionDeleted),
        /// See [`MessageRecorded`].
        MessageRecorded(MessageRecorded),
        /// See [`AcquireObserved`].
        AcquireObserved(AcquireObserved),
        /// See [`CapacityApplied`].
        CapacityApplied(CapacityApplied),
        /// See [`SessionDegraded`].
        SessionDegraded(SessionDegraded),
        /// See [`SessionRecovered`].
        SessionRecovered(SessionRecovered),
        /// See [`ClosingStarted`].
        ClosingStarted(ClosingStarted),
        /// See [`SessionClosed`].
        SessionClosed(SessionClosed),
        /// See [`MessageIdsRestarted`].
        MessageIdsRestarted(MessageIdsRestarted),
        /// See [`TrustPolicyApplied`].
        TrustPolicyApplied(TrustPolicyApplied),
        /// See [`AssignedJobTrusted`].
        AssignedJobTrusted(AssignedJobTrusted),
        /// See [`AssignedJobRefused`].
        AssignedJobRefused(AssignedJobRefused),
        /// See [`SinkStarted`].
        SinkStarted(SinkStarted),
        /// See [`SinkEnded`].
        SinkEnded(SinkEnded),
        /// See [`CancelAnswered`].
        CancelAnswered(CancelAnswered),
        /// See [`ScaleSetVanished`].
        ScaleSetVanished(ScaleSetVanished),
        /// See [`ScaleSetRecreated`].
        ScaleSetRecreated(ScaleSetRecreated),
    }
}
