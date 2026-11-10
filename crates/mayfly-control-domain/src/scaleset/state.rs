//! The scale set's folded state, the value types its commands and events carry, its commands and
//! its refusals.

use std::collections::BTreeSet;

use mayfly_domain::{Epoch, OwnedVmName, PoolName};
use serde::{Deserialize, Serialize};

use crate::aggregate::{CommandBody, Rejection};
use crate::attempt::RunnerRequestId;
use crate::demand::{Capacity, Offer, Statistics};
use crate::pool::TrustTier;
use crate::registration::{JobResult, RunnerId, ScaleSetId};

/// Unknown message types a session tolerates before it is `Degraded{protocol_drift}` (plan §7.2
/// step 6, R7). Never safe mode. Pinned by
/// `tests/scaleset/rules.rs::should_degrade_the_session_when_unknown_types_reach_the_threshold`.
pub const PROTOCOL_DRIFT_THRESHOLD: u32 = 3;

/// Consecutive creates answered 409 (`LossCause::Conflicted`, A3) after which the scale set
/// escalates to the operator with `SessionConflictEscalated`: a session mayfly has no record of
/// is blocking the pool longer than an abandoned one should live. The creates go on backing off
/// (the session may still end on its own) and mayfly never deletes it. A tuning value the owner
/// may change; GH-00's capture (item 2: how long an abandoned session blocks a new one) informs
/// it. Pinned by
/// `tests/scaleset/rules.rs::should_escalate_once_when_creates_keep_answering_conflict`.
pub const SESSION_CONFLICT_ESCALATION: u32 = 5;

/// A message's `messageId`. Assumed to increase per scale set, across sessions (A1,
/// `doc/github-scaleset.md` §12 item 1): the statistics cursor is kept across sessions, so an
/// older message's statistics never replace newer ones, and a session's floor (the cursor at its
/// open) tells a message recorded in an earlier session from a new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageId(pub u64);

/// A session's `sessionId`, a UUID, as its two 64-bit halves. Journaled so that a restarted
/// daemon deletes it before it creates another (`doc/github-scaleset.md` §3.2); its token never
/// is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionId {
    /// The UUID's high 64 bits.
    pub high: u64,
    /// The UUID's low 64 bits.
    pub low: u64,
}

impl SessionId {
    /// From a UUID's 128 bits.
    #[must_use]
    pub const fn from_u128(value: u128) -> Self {
        Self {
            high: (value >> 64) as u64,
            low: value as u64,
        }
    }

    /// The UUID's 128 bits.
    #[must_use]
    pub const fn as_u128(self) -> u128 {
        ((self.high as u128) << 64) | self.low as u128
    }
}

/// Why a session was lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LossCause {
    /// A queue call answered 401 again after one refresh, or the session is gone (§3.3).
    Expired,
    /// A create answered 409: a session lives that mayfly holds no record of (A3, §3.1).
    Conflicted,
    /// A request ran out of retries (§8).
    Failed,
    /// The daemon restarted holding the session: it is deleted before a new one (§3.2).
    Restarted,
}

/// Why a session is degraded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DegradedCause {
    /// Unknown message types reached [`PROTOCOL_DRIFT_THRESHOLD`] (R7).
    ProtocolDrift,
    /// A message at or below the session's floor that A1 does not explain: GitHub's ids did not
    /// increase across sessions (A1 refuted, or not delivered oldest first). Nothing it carried
    /// was applied; the operator decides (`MessageOrigin::IdRegressed`).
    MessageIdsRegressed,
}

/// Where a recorded message stands against the session's floor (A1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageOrigin {
    /// Above the floor: a message this stream has not recorded; it applies in full.
    Fresh,
    /// The stream's last record (its id is the floor), redelivered as the session's first
    /// message because its ack was lost: recorded so it can be acked, it applies nothing again
    /// (no statistics, no plan, no release, no drift).
    Redelivered,
    /// At or below the floor otherwise: A1 does not explain it, so it applies nothing and the
    /// session is degraded (`DegradedCause::MessageIdsRegressed`).
    IdRegressed,
}

/// The session's phase (plan §7.2): `Unconfigured → Ensured → Opening → Active ⇄ Backoff |
/// Degraded → Closing → Closed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// No `ScaleSetEnsured` yet.
    Unconfigured,
    /// The scale set exists at GitHub; no session was opened.
    Ensured,
    /// A session create is in flight.
    Opening,
    /// A session is live: poll, record, acquire, ack.
    Active,
    /// No session: waiting to open one again.
    Backoff(LossCause),
    /// A session is live and recording, flagged for the operator (minting is frozen, P21).
    Degraded(DegradedCause),
    /// The live session is being deleted (orderly shutdown, drain completion).
    Closing,
    /// The session was deleted in order.
    Closed,
}

/// A job fact a message carried (`doc/github-scaleset.md` §4.3): input for policies, never demand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobFact {
    /// `JobAssigned`: never counted for demand.
    Assigned {
        /// The job.
        request_id: RunnerRequestId,
    },
    /// `JobStarted`: the job is bound to a runner; its reservation is released.
    Started {
        /// The job.
        request_id: RunnerRequestId,
        /// The runner's id, when the message carried one.
        runner_id: Option<RunnerId>,
        /// The runner's name when it is one of mayfly's VM names (the ACL parses it; any other
        /// name is `None`).
        runner_name: Option<OwnedVmName>,
    },
    /// `JobCompleted` (a cancel included): the reservation, if any, is released.
    Completed {
        /// The job.
        request_id: RunnerRequestId,
        /// Its result.
        result: JobResult,
        /// The runner's id, when the message carried one.
        runner_id: Option<RunnerId>,
    },
}

impl JobFact {
    /// The job the fact is about.
    #[must_use]
    pub const fn request_id(&self) -> RunnerRequestId {
        match self {
            Self::Assigned { request_id }
            | Self::Started { request_id, .. }
            | Self::Completed { request_id, .. } => *request_id,
        }
    }

    /// Whether the fact releases the job's reservation: `JobStarted` binds it, `JobCompleted`
    /// ends it; `JobAssigned` does not (A2).
    #[must_use]
    pub const fn releases(&self) -> bool {
        match self {
            Self::Assigned { .. } => false,
            Self::Started { .. } | Self::Completed { .. } => true,
        }
    }
}

/// One element of a message's batch, in arrival order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum MessageItem {
    /// `JobAvailable`.
    Offer(Offer),
    /// `JobAssigned`, `JobStarted` or `JobCompleted`.
    Fact(JobFact),
    /// A job message type, or an envelope type, this build does not know (R7).
    Unknown,
}

/// The state of one `scaleset-<pool>` stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaleSetState {
    /// The session's phase.
    pub phase: Phase,
    /// The highest run epoch accepted (I18).
    pub epoch: Epoch,
    /// The pool (one scale set per pool, ADR 0040).
    pub pool: Option<PoolName>,
    /// GitHub's id of the scale set.
    pub github_id: Option<ScaleSetId>,
    /// The pool's trust tier (I32).
    pub tier: Option<TrustTier>,
    /// The live session.
    pub session: Option<SessionId>,
    /// A journaled session to delete before the next create (§3.2).
    pub stale_session: Option<SessionId>,
    /// The highest message id recorded, in any session (A1).
    pub cursor: Option<MessageId>,
    /// The cursor when the live session opened: a message at or below it was recorded in an
    /// earlier session, or A1 does not hold ([`MessageOrigin`]).
    pub session_floor: Option<MessageId>,
    /// A message was recorded in the live session.
    pub session_recorded: bool,
    /// Creates answered 409 since a session last opened (A3,
    /// [`SESSION_CONFLICT_ESCALATION`]).
    pub open_conflicts: u32,
    /// A message's statistics replace the latest ones only when its id is above this one: the
    /// message they came from, or, after a session opened, the cursor at the opening (whose
    /// statistics are newer than every message recorded before it). A message emitted before
    /// the open but recorded after it is above it too: no id tells it apart, and its older
    /// statistics apply until the backlog is consumed (A6, `doc/github-scaleset.md` §12).
    pub statistics_after: Option<MessageId>,
    /// The latest statistics applied: a session's initial ones, or a newer message's.
    pub statistics: Option<Statistics>,
    /// The capacity policy's last input.
    pub capacity: Capacity,
    /// `min(deliverable, totalAssignedJobs)` of the latest statistics.
    pub desired: u32,
    /// Planned by a recorded message, acquire outcome not observed yet (A4).
    pub acquiring: BTreeSet<RunnerRequestId>,
    /// Acquired, with no `JobStarted` or `JobCompleted` yet.
    pub acquired: BTreeSet<RunnerRequestId>,
    /// Unknown message types since the session opened or recovered.
    pub drift: u32,
}

impl ScaleSetState {
    /// The state of a stream with no events.
    #[must_use]
    pub const fn initial() -> Self {
        Self {
            phase: Phase::Unconfigured,
            epoch: Epoch::new(0),
            pool: None,
            github_id: None,
            tier: None,
            session: None,
            stale_session: None,
            cursor: None,
            session_floor: None,
            session_recorded: false,
            open_conflicts: 0,
            statistics_after: None,
            statistics: None,
            capacity: Capacity::NONE,
            desired: 0,
            acquiring: BTreeSet::new(),
            acquired: BTreeSet::new(),
            drift: 0,
        }
    }

    /// Every open reservation: what the acquire budget counts on top of the statistics (A2).
    #[must_use]
    pub fn reserved(&self) -> BTreeSet<RunnerRequestId> {
        self.acquiring.union(&self.acquired).copied().collect()
    }

    /// Whether offers are planned: only in an active session. A degraded session freezes
    /// minting (P21) and a closing one ends; both decline offers as held. Pinned by
    /// `tests/scaleset/rules.rs::should_plan_no_offer_when_the_session_is_degraded_or_closing`.
    #[must_use]
    pub const fn plans_offers(&self) -> bool {
        match self.phase {
            Phase::Active => true,
            Phase::Unconfigured
            | Phase::Ensured
            | Phase::Opening
            | Phase::Backoff(_)
            | Phase::Degraded(_)
            | Phase::Closing
            | Phase::Closed => false,
        }
    }

    /// Whether repeated 409s on create were escalated to the operator.
    #[must_use]
    pub const fn conflict_escalated(&self) -> bool {
        self.open_conflicts >= SESSION_CONFLICT_ESCALATION
    }

    /// The `maxCapacity` to send on the next poll ([`crate::demand::wire_max`]).
    #[must_use]
    pub const fn wire_max(&self) -> u32 {
        crate::demand::wire_max(&self.capacity)
    }
}

/// `EnsureScaleSet`: the scale set was found or created at GitHub for the pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EnsureScaleSet {
    /// The pool (the stream is `scaleset-<pool>`).
    pub pool: PoolName,
    /// GitHub's id of the scale set.
    pub github_id: ScaleSetId,
    /// The pool's tier.
    pub tier: TrustTier,
}

/// `RecordMessage`: one polled message, recorded before it is acked (I12). The command id is
/// `(session, message)`, so a redelivery in the same session replays the receipt
/// (`DuplicateMessage`) and the listener acks it without a write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordMessage {
    /// The session the message was polled in.
    pub session_id: SessionId,
    /// Its id.
    pub message_id: MessageId,
    /// Its statistics, when the envelope carried them.
    pub statistics: Option<Statistics>,
    /// Its batch, in arrival order.
    pub items: Vec<MessageItem>,
}

/// The commands of `doc/domain-model.md` §6.1. Acquisition is planned inside `RecordMessage`
/// (journaled before the call, `doc/github-scaleset.md` §6) and its answer is the observation
/// `RecordAcquireResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ScaleSetCommand {
    /// The scale set exists for the pool.
    EnsureScaleSet(EnsureScaleSet),
    /// A session create is about to be sent.
    BeginOpen,
    /// The create answered.
    RecordSessionOpened {
        /// The new session.
        session_id: SessionId,
        /// Its initial statistics: authoritative, desired is recomputed from them.
        statistics: Statistics,
    },
    /// The session, or its create, is lost.
    RecordSessionLoss {
        /// Why.
        cause: LossCause,
    },
    /// A journaled session was deleted (204, or 404: already gone).
    RecordSessionDeleted {
        /// The session.
        session_id: SessionId,
    },
    /// A polled message.
    RecordMessage(RecordMessage),
    /// `AcquireJobs` answered.
    RecordAcquireResult {
        /// The ids the call asked for: pending ones.
        requested: BTreeSet<RunnerRequestId>,
        /// The ids GitHub answered acquired; the rest are released (A4).
        acquired: BTreeSet<RunnerRequestId>,
    },
    /// The capacity policy's input for this scale set.
    ApplyCapacity(Capacity),
    /// The operator cleared a degraded session.
    RecordSessionRecovered,
    /// Delete the live session in order.
    StartClosing,
}

impl CommandBody for ScaleSetCommand {
    const TYPES: &'static [&'static str] = &[
        "ensure_scale_set",
        "begin_open",
        "record_session_opened",
        "record_session_loss",
        "record_session_deleted",
        "record_message",
        "record_acquire_result",
        "apply_capacity",
        "record_session_recovered",
        "start_closing",
    ];

    fn command_type(&self) -> &'static str {
        match self {
            Self::EnsureScaleSet(_) => "ensure_scale_set",
            Self::BeginOpen => "begin_open",
            Self::RecordSessionOpened { .. } => "record_session_opened",
            Self::RecordSessionLoss { .. } => "record_session_loss",
            Self::RecordSessionDeleted { .. } => "record_session_deleted",
            Self::RecordMessage(_) => "record_message",
            Self::RecordAcquireResult { .. } => "record_acquire_result",
            Self::ApplyCapacity(_) => "apply_capacity",
            Self::RecordSessionRecovered => "record_session_recovered",
            Self::StartClosing => "start_closing",
        }
    }
}

/// A refusal of the `ScaleSet` aggregate. Its serde form is its code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScaleSetRejection {
    /// The command's run epoch is older than the scale set's (I18).
    StaleRun,
    /// No `ScaleSetEnsured` yet.
    NotEnsured,
    /// The scale set is ensured for another pool, GitHub id or tier.
    ScaleSetMismatch,
    /// A journaled session must be deleted before a new create (§3.2).
    StaleSessionPending,
    /// A session is live already.
    SessionLive,
    /// The session is being closed.
    Closing,
    /// No create is in flight.
    NotOpening,
    /// No session is live.
    NoSession,
    /// The message or the open names another session than the live one.
    SessionMismatch,
    /// The session is neither the stale nor the closing one: mayfly never deletes a session it
    /// has no record of (A3).
    UnknownSession,
    /// The answer names an id that is not pending.
    NotAcquiring,
    /// The answer acquired an id the call did not ask for.
    AcquiredUnrequested,
    /// The reported max exceeds the deliverable (I07, I24).
    ReportedOverDeliverable,
    /// An older capacity input than the applied one.
    StaleCapacity,
    /// The applied revision with other values.
    CapacityConflict,
    /// The session is not degraded.
    NotDegraded,
}

impl Rejection for ScaleSetRejection {
    const CODES: &'static [&'static str] = &[
        "stale_run",
        "not_ensured",
        "scale_set_mismatch",
        "stale_session_pending",
        "session_live",
        "closing",
        "not_opening",
        "no_session",
        "session_mismatch",
        "unknown_session",
        "not_acquiring",
        "acquired_unrequested",
        "reported_over_deliverable",
        "stale_capacity",
        "capacity_conflict",
        "not_degraded",
    ];

    fn code(&self) -> &'static str {
        match self {
            Self::StaleRun => "stale_run",
            Self::NotEnsured => "not_ensured",
            Self::ScaleSetMismatch => "scale_set_mismatch",
            Self::StaleSessionPending => "stale_session_pending",
            Self::SessionLive => "session_live",
            Self::Closing => "closing",
            Self::NotOpening => "not_opening",
            Self::NoSession => "no_session",
            Self::SessionMismatch => "session_mismatch",
            Self::UnknownSession => "unknown_session",
            Self::NotAcquiring => "not_acquiring",
            Self::AcquiredUnrequested => "acquired_unrequested",
            Self::ReportedOverDeliverable => "reported_over_deliverable",
            Self::StaleCapacity => "stale_capacity",
            Self::CapacityConflict => "capacity_conflict",
            Self::NotDegraded => "not_degraded",
        }
    }
}
