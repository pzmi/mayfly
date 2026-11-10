//! The scale set's events (`doc/domain-model.md` §6.1), with golden samples under
//! `tests/fixtures/events/<type>/`. No event carries a token: the session id is journaled, its
//! queue token never is (ADR 0014).

use mayfly_domain::{Epoch, PoolName};
use serde::{Deserialize, Serialize};

use super::state::{DegradedCause, JobFact, LossCause, MessageId, SessionId};
use crate::attempt::RunnerRequestId;
use crate::demand::{Capacity, Declined, Statistics};
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

/// A session create is about to be sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionOpening {}

impl DomainEvent for SessionOpening {
    const EVENT_TYPE: &'static str = "session_opening";
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
/// facts, the unknown types counted and the new desired count (plan §7.2 step 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MessageRecorded {
    /// The session it was polled in.
    pub session_id: SessionId,
    /// Its id.
    pub message_id: MessageId,
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
    /// The `maxCapacity` every poll now sends (ADR 0029).
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

event_set! {
    /// Everything that happens to a pool's scale set.
    pub enum ScaleSetEvent {
        /// See [`ScaleSetEpochAdvanced`].
        ScaleSetEpochAdvanced(ScaleSetEpochAdvanced),
        /// See [`ScaleSetEnsured`].
        ScaleSetEnsured(ScaleSetEnsured),
        /// See [`SessionOpening`].
        SessionOpening(SessionOpening),
        /// See [`SessionOpened`].
        SessionOpened(SessionOpened),
        /// See [`SessionLost`].
        SessionLost(SessionLost),
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
    }
}
