//! The scale set's folded state, the value types its commands and events carry, its commands and
//! its refusals.

use std::collections::{BTreeMap, BTreeSet};

use mayfly_domain::{Epoch, OwnedVmName, PoolName};
use serde::{Deserialize, Serialize};

use crate::aggregate::{CommandBody, Rejection};
use crate::attempt::RunnerRequestId;
use crate::demand::{
    AssignedJob, Capacity, JobKey, Offer, RunLookup, RunnerGate, Statistics, TrustPolicy, WireMode,
};
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
/// may change; HW-02's capture (`doc/github-scaleset.md` §12 item 9: how long an abandoned
/// session blocks a new one) informs it. Pinned by
/// `tests/scaleset/rules.rs::should_escalate_once_when_creates_keep_answering_conflict`.
pub const SESSION_CONFLICT_ESCALATION: u32 = 5;

/// A message's `messageId`. Assumed to increase per scale set, across sessions (A1,
/// client-assumed, `doc/github-scaleset.md` §4.1 and §12 item 4): the statistics cursor is kept
/// across sessions, so an older message's statistics never replace newer ones, and a session's
/// floor (the cursor at its open) tells a message recorded in an earlier session from a new one.
/// A session's first message strictly below the floor starts a new numbering
/// (`MessageIdsRestarted`, KER-06G).
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
    /// A message at or below the cursor that is neither the stream's last record redelivered as
    /// the session's first message nor a restarted numbering: an id reused within a session or
    /// delivered out of order (A1 refuted). Nothing it carried was applied; the operator decides
    /// (`MessageOrigin::IdRegressed`).
    MessageIdsRegressed,
}

/// Where a recorded message stands against the session's floor (A1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageOrigin {
    /// Above the cursor, or the first message of a restarted numbering (`MessageIdsRestarted`
    /// recorded before it): a message this stream has not recorded; it applies in full.
    Fresh,
    /// The stream's last record (its id is the floor), redelivered as the session's first
    /// message because its ack was lost: recorded so it can be acked, it applies nothing again
    /// (no statistics, no plan, no release, no drift).
    Redelivered,
    /// At or below the cursor otherwise: A1 does not explain it (a reused id, even one with a
    /// receipt, is never a duplicate), so it applies nothing and the session is degraded
    /// (`DegradedCause::MessageIdsRegressed`).
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
    /// `JobAssigned`: never counted for demand. On github.com (`runnerRequestId` 0) the job is
    /// classified from `job` before any runner starts (ADR 0090).
    Assigned {
        /// The job's `runnerRequestId` (0 on github.com: never a key there).
        request_id: RunnerRequestId,
        /// The job's key and classifying fields, when the message carried a `jobId` and a
        /// `workflowRunId` (KER-06G; absent in records written before it). Without them the
        /// assignment cannot be keyed: it is never classified trusted, so it backs no runner and
        /// holds nothing, but no `AssignedJobRefused` is recorded for it and no sink or cancel can
        /// target it (a known gap until HW-02 shows whether github.com ever omits them; the
        /// in-guest hook still fails it, ADR 0090).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job: Option<AssignedJob>,
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
        /// The job's key, when the message named one: releases its classification (KER-06G).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job: Option<JobKey>,
    },
    /// `JobCompleted` (a cancel included): the reservation, if any, is released.
    Completed {
        /// The job.
        request_id: RunnerRequestId,
        /// Its result.
        result: JobResult,
        /// The runner's id, when the message carried one.
        runner_id: Option<RunnerId>,
        /// The job's key, when the message named one: releases its classification (KER-06G).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job: Option<JobKey>,
    },
}

impl JobFact {
    /// The job the fact is about.
    #[must_use]
    pub const fn request_id(&self) -> RunnerRequestId {
        match self {
            Self::Assigned { request_id, .. }
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
    /// A `MessageIdsRestarted` was recorded (A1 refuted, recovered).
    pub renumbered: bool,
    /// The pool's trust policy (GH-08); none applied refuses every assignment.
    pub policy: Option<TrustPolicy>,
    /// Assigned directly, its workflow run not looked up yet: no runner starts for it (ADR 0090).
    pub pending: BTreeMap<JobKey, Assignment>,
    /// Assigned directly and classified trusted, no `JobStarted` or `JobCompleted` yet. Kept
    /// with its assignment so that a changed trust policy sends it back to `pending`.
    pub trusted: BTreeMap<JobKey, Assignment>,
    /// Assigned directly and refused (`AssignedJobRefused`), until a job fact, a new assignment
    /// of it or a re-created scale set shows it gone. Holds nothing.
    pub refused: BTreeSet<JobKey>,
    /// Refused jobs whose cancel was answered: never asked again.
    pub cancel_tried: BTreeSet<JobKey>,
    /// A refusal sink is up: the wire max is pinned at the assigned count.
    pub sink_live: bool,
    /// Refused jobs a sink was started for: one sink per refusal.
    pub sink_tried: BTreeSet<JobKey>,
}

/// A job assigned to the scale set and not acquired by mayfly, as the stream keeps it while it
/// is pending or trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    /// Its `runnerRequestId`: 0 on github.com; on GHES the id an acquire may still name, which
    /// then makes it an acquired job and drops this entry (the model's union
    /// `acquired ∪ trusted`, never counted twice).
    pub request_id: RunnerRequestId,
    /// The assignment's key and classifying fields.
    pub job: AssignedJob,
}

/// What to do about a refused job now (ADR 0090, amended 2026-11-12, option A).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusalAction {
    /// `StartSink`: a disposable Linux runner whose hook fails whatever job it gets.
    StartSink(JobKey),
    /// `CancelRun` (`cancel_refused_runs`): ask GitHub to cancel the job's run.
    CancelRun(JobKey),
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
            renumbered: false,
            policy: None,
            pending: BTreeMap::new(),
            trusted: BTreeMap::new(),
            refused: BTreeSet::new(),
            cancel_tried: BTreeSet::new(),
            sink_live: false,
            sink_tried: BTreeSet::new(),
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

    /// The `maxCapacity` to send on the next poll ([`crate::demand::wire_max`]): pinned at the
    /// latest statistics' assigned count while a refusal sink is up.
    #[must_use]
    pub const fn wire_max(&self) -> u32 {
        crate::demand::wire_max(&self.capacity, self.sink_pin())
    }

    /// The assigned count the wire max is pinned at while a sink is up.
    #[must_use]
    pub const fn sink_pin(&self) -> Option<u32> {
        if !self.sink_live {
            return None;
        }
        match self.statistics {
            Some(s) => Some(s.total_assigned_jobs),
            None => Some(0),
        }
    }

    /// Whether a message whose `(session, message)` receipt exists is a `DuplicateMessage`, acked
    /// without a write: only when its id is the cursor's, since within a session only the cursor
    /// can be redelivered (oldest first). A receipt for any other id is a reused id (a restarted
    /// numbering), recorded anew so that it degrades the session; the receipt check alone would
    /// drop it silently (`formal/scaleset.qnt`'s `record`, `restartWithinASessionDegradesTest`;
    /// the listener's check, GH-04). Pinned by
    /// `tests/scaleset/rules.rs::should_degrade_the_session_when_an_id_at_or_below_the_cursor_comes_later_in_a_session`.
    #[must_use]
    pub fn is_duplicate(&self, message_id: MessageId) -> bool {
        self.cursor == Some(message_id)
    }

    /// Whether the stream's scale set was found gone and is not re-created yet.
    #[must_use]
    pub const fn vanished(&self) -> bool {
        match self.phase {
            Phase::Unconfigured => self.github_id.is_some(),
            Phase::Ensured
            | Phase::Opening
            | Phase::Active
            | Phase::Backoff(_)
            | Phase::Degraded(_)
            | Phase::Closing
            | Phase::Closed => false,
        }
    }

    /// Whether the cancel option is on.
    #[must_use]
    pub fn cancels_refused_runs(&self) -> bool {
        self.policy.as_ref().is_some_and(|p| p.cancel_refused_runs)
    }

    /// Jobs mayfly classified trusted that wait for a runner: acquired offers and trusted
    /// assignments (the model's `vmWaiting`).
    #[must_use]
    pub fn waiting(&self) -> u32 {
        let assigned = self
            .trusted
            .values()
            .filter(|a| !self.acquired.contains(&a.request_id))
            .count();
        let waiting = self.acquired.len().saturating_add(assigned);
        u32::try_from(waiting).unwrap_or(u32::MAX)
    }

    /// [`crate::demand::runner_may_start`] for this scale set with `idle` runners started and
    /// waiting for a job. Pinned by
    /// `tests/scaleset/trust.rs::should_start_no_runner_when_an_assignment_is_pending_or_refused`.
    #[must_use]
    pub fn runner_may_start(&self, idle: u32) -> bool {
        crate::demand::runner_may_start(&RunnerGate {
            mode: self.capacity.mode,
            active: self.plans_offers(),
            waiting: self.waiting(),
            idle,
        })
    }

    /// Whether a sink may start now for some refused job: the pool open and the session active,
    /// no cancel option, no sink up, and no trusted or pending assignment GitHub could hand it
    /// instead (the model's `startSink` guard, `sinkOnlyForARefusedJob`).
    #[must_use]
    pub fn sink_may_start(&self) -> bool {
        let open = match self.capacity.mode {
            WireMode::Open => true,
            WireMode::Paused | WireMode::Draining | WireMode::SafeMode => false,
        };
        open && self.plans_offers()
            && !self.cancels_refused_runs()
            && !self.sink_live
            && self.pending.is_empty()
            && self.waiting() == 0
    }

    /// The next refusal action the control plane takes, if any: with `cancel_refused_runs`, a
    /// cancel of a refused run not answered yet; otherwise a sink for a refused job none was
    /// started for, when [`Self::sink_may_start`]. Pinned by
    /// `tests/scaleset/trust.rs::should_cancel_only_a_refused_run_when_the_option_is_on`.
    #[must_use]
    pub fn refusal_action(&self) -> Option<RefusalAction> {
        if self.cancels_refused_runs() {
            return self
                .refused
                .iter()
                .find(|job| !self.cancel_tried.contains(*job))
                .map(|job| RefusalAction::CancelRun(job.clone()));
        }
        if !self.sink_may_start() {
            return None;
        }
        self.refused
            .iter()
            .find(|job| !self.sink_tried.contains(*job))
            .map(|job| RefusalAction::StartSink(job.clone()))
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
    /// The pool's trust policy (GH-08), applied to every later lookup.
    ApplyTrustPolicy(TrustPolicy),
    /// A pending assignment's workflow run was looked up (GH-10).
    RecordRunLookup {
        /// The job.
        job: JobKey,
        /// What the lookup answered.
        lookup: RunLookup,
    },
    /// A refusal sink is about to be started for a refused job (IMG-03).
    StartSink {
        /// The refused job.
        job: JobKey,
    },
    /// The refusal sink's VM was deleted.
    RecordSinkEnded,
    /// A cancel of a refused job's run was answered (GH-10).
    RecordCancelAnswer {
        /// The refused job.
        job: JobKey,
        /// The answer.
        answer: CancelAnswer,
    },
    /// A create or read answered 404 for the journaled scale set: GitHub deleted it as dormant.
    RecordScaleSetVanished {
        /// The journaled scale set.
        github_id: ScaleSetId,
    },
    /// The vanished scale set was created again under a new id (ADR 0040: mayfly re-creates its
    /// own set, never deletes one).
    RecordScaleSetRecreated {
        /// GitHub's id of the new scale set.
        github_id: ScaleSetId,
    },
}

/// What GitHub answered to a cancel of a refused run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelAnswer {
    /// 202: GitHub accepted the cancel; a `JobCompleted` follows.
    Accepted,
    /// 409 or an error answer: nothing changed at GitHub; its release fails the job.
    Refused,
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
        "apply_trust_policy",
        "record_run_lookup",
        "start_sink",
        "record_sink_ended",
        "record_cancel_answer",
        "record_scale_set_vanished",
        "record_scale_set_recreated",
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
            Self::ApplyTrustPolicy(_) => "apply_trust_policy",
            Self::RecordRunLookup { .. } => "record_run_lookup",
            Self::StartSink { .. } => "start_sink",
            Self::RecordSinkEnded => "record_sink_ended",
            Self::RecordCancelAnswer { .. } => "record_cancel_answer",
            Self::RecordScaleSetVanished { .. } => "record_scale_set_vanished",
            Self::RecordScaleSetRecreated { .. } => "record_scale_set_recreated",
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
    /// The scale set vanished: it is re-created (`RecordScaleSetRecreated`), never ensured anew.
    ScaleSetVanished,
    /// The scale set did not vanish.
    NotVanished,
    /// The job is not a pending assignment.
    NotPending,
    /// The job is not refused.
    NotRefused,
    /// A sink may not start now: the pool or session is held, the cancel option is on, a sink is
    /// up or was started for the job, or a trusted or pending assignment is known.
    SinkNotDue,
    /// No sink is up.
    NoSink,
    /// No cancel is due for the job: the option is off, or its cancel was answered already.
    CancelNotDue,
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
        "scale_set_vanished",
        "not_vanished",
        "not_pending",
        "not_refused",
        "sink_not_due",
        "no_sink",
        "cancel_not_due",
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
            Self::ScaleSetVanished => "scale_set_vanished",
            Self::NotVanished => "not_vanished",
            Self::NotPending => "not_pending",
            Self::NotRefused => "not_refused",
            Self::SinkNotDue => "sink_not_due",
            Self::NoSink => "no_sink",
            Self::CancelNotDue => "cancel_not_due",
        }
    }
}
