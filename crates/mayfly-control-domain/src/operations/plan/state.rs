//! The recovery plan's folded state, its commands and its refusals.

use std::collections::BTreeMap;

use mayfly_domain::{AttemptId, Epoch, Millis, ProviderInstanceId, StateDigest};
use serde::{Deserialize, Serialize};

use crate::aggregate::{CommandBody, Rejection};
use crate::attempt::QuarantineReason;
use crate::operations::recovery::Proposal;

/// What a plan knew of GitHub when it was drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GithubBinding {
    /// GitHub answered: the digest of its runner listing.
    Snapshot(StateDigest),
    /// GitHub could not answer; a later answer is a world that moved.
    Unreachable,
}

/// The bindings of a plan drawn by KER-10 (`RecoveryPlanProposed`): one digest of every head.
/// A digest cannot tell the plan's own writes from another writer's, so such a plan is never
/// applied: its first intent or completion records `RecoveryPlanRefused{journal_heads}` and a
/// new plan is drawn (`tests/recovery_plan/rules.rs::should_refuse_a_legacy_plan_when_it_is_applied`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyBindings {
    /// The digest of the journal's stream heads the plan read.
    pub heads: StateDigest,
    /// The digest of the complete inventory the plan read.
    pub inventory: StateDigest,
    /// GitHub's runner listing, or that GitHub was unreachable.
    pub github: GithubBinding,
    /// The run epoch the plan was drawn under.
    pub epoch: Epoch,
}

/// The journal stream of an attempt, as the store names it (`attempt-<id>`): the stream a plan
/// action's own write goes to.
#[must_use]
pub fn attempt_stream(attempt_id: AttemptId) -> String {
    format!("attempt-{attempt_id}")
}

/// What a plan is bound to (I31): it applies only while all four are as they were drawn, the
/// heads advanced by the plan's own writes and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bindings {
    /// The revision of every stream the plan read, by stream id: the attempts it acts on (each
    /// action's [`attempt_stream`] must be here) and whatever else its decision read. The use
    /// case binds the same heads as read conditions of each unit it appends for the plan
    /// (ADR 0041), so a stream that moves between its read and the append refuses the unit.
    pub heads: BTreeMap<String, u64>,
    /// The digest of the complete inventory the plan read (an incomplete one binds no plan:
    /// [`ActionKind::from_proposal`]).
    pub inventory: StateDigest,
    /// GitHub's runner listing, or that GitHub was unreachable.
    pub github: GithubBinding,
    /// The run epoch the plan was drawn under.
    pub epoch: Epoch,
}

/// The first binding of `drawn` that `current` no longer matches, in a fixed order. `current`
/// is what the use case read; the store's read conditions on the same heads keep it honest.
#[must_use]
pub fn stale(drawn: &Bindings, current: &Bindings) -> Option<StaleBinding> {
    if drawn.heads != current.heads {
        Some(StaleBinding::JournalHeads)
    } else if drawn.inventory != current.inventory {
        Some(StaleBinding::Inventory)
    } else if drawn.github != current.github {
        Some(StaleBinding::Github)
    } else if drawn.epoch != current.epoch {
        Some(StaleBinding::Epoch)
    } else {
        None
    }
}

/// Which binding moved (`RecoveryPlanRefused{stale}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleBinding {
    /// A stream the plan read was written since.
    JournalHeads,
    /// The inventory differs.
    Inventory,
    /// GitHub's runner listing differs, or GitHub answers now.
    Github,
    /// The daemon restarted: another run epoch.
    Epoch,
}

/// A plan action's kind: exactly the safe-mode proposals of the recovery table
/// ([`crate::operations::recovery::Proposal`]), so a plan can hold no retire, retry, delete or
/// asserted outcome (ADR 0025, OD10; `doc/domain-model.md` §6.9). Pinned by
/// `tests/recovery_plan/rules.rs::should_hold_no_destructive_action_when_built_from_proposals`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// Adopt the attempt under the run's epoch (`Attempt.Adopt`); needs the VM's instance id.
    Adopt,
    /// Record the complete listing's absence on the attempt, which records its VM vanished and
    /// closes it (DEC-07). Irreversible.
    MarkLost,
    /// Quarantine the attempt (`Attempt.Quarantine`).
    Quarantine(QuarantineReason),
    /// Already quarantined: nothing to do.
    Keep,
    /// Report the name for operator review.
    Report,
    /// Nothing to do.
    Ignore,
}

impl ActionKind {
    /// The action for a safe-mode proposal; `None` for
    /// [`Proposal::WaitForInventory`]: no plan binds an incomplete inventory (I25).
    #[must_use]
    pub const fn from_proposal(proposal: Proposal) -> Option<Self> {
        match proposal {
            Proposal::WaitForInventory => None,
            Proposal::Adopt => Some(Self::Adopt),
            Proposal::MarkLost => Some(Self::MarkLost),
            Proposal::Quarantine(reason) => Some(Self::Quarantine(reason)),
            Proposal::Keep => Some(Self::Keep),
            Proposal::Report => Some(Self::Report),
            Proposal::Ignore => Some(Self::Ignore),
        }
    }

    /// Whether the action cannot be undone (`RecoveryPlanView` flags it).
    #[must_use]
    pub const fn irreversible(self) -> bool {
        match self {
            Self::MarkLost => true,
            Self::Adopt | Self::Quarantine(_) | Self::Keep | Self::Report | Self::Ignore => false,
        }
    }
}

/// The plan's own write for one action: the action's attempt stream, its revision before the
/// attempt command's unit and after it, as that unit's receipt gives them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnWrite {
    /// The stream written.
    pub stream: String,
    /// Its revision before the write.
    pub before: u64,
    /// Its revision after the write.
    pub after: u64,
}

/// One action of a plan, on one attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanAction {
    /// The attempt.
    pub attempt_id: AttemptId,
    /// The instance id the inventory showed under its name: the ownership evidence an adoption
    /// needs (ADR 0026).
    pub instance_id: Option<ProviderInstanceId>,
    /// What to do.
    pub kind: ActionKind,
}

/// How one action's ordinary command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionOutcome {
    /// The command was accepted.
    Applied,
    /// The command was refused: the plan stops, resumable at this action.
    Failed,
}

/// The plan's phase (`doc/domain-model.md` §6.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanPhase {
    /// No `RecoveryPlanProposed` yet.
    Unproposed,
    /// Drawn, awaiting an operator's approval.
    Proposed,
    /// Approved; no action started.
    Approved,
    /// Applying, at [`RecoveryPlanState::next`].
    Applying,
    /// An action failed; resumable at [`RecoveryPlanState::next`].
    Failed,
    /// Every action applied, bound to the end.
    Completed,
    /// Refused because the world moved (I31, F45).
    Refused(StaleBinding),
    /// Not approved or started before its expiry.
    Expired,
}

/// The state of one `recovery-<plan_id>` stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryPlanState {
    /// The phase.
    pub phase: PlanPhase,
    /// The highest run epoch accepted (I18).
    pub epoch: Epoch,
    /// What the plan is bound to; a head advances only by the plan's own write.
    pub bindings: Option<Bindings>,
    /// Drawn by KER-10's `RecoveryPlanProposed`, bound by a digest: never applied.
    pub legacy: bool,
    /// The actions, immutable once proposed.
    pub actions: Vec<PlanAction>,
    /// After this instant an unstarted plan is not approved or started.
    pub expires_at: Millis,
    /// The approving operator's uid.
    pub approved_by: Option<u32>,
    /// The index of the next action to apply.
    pub next: usize,
    /// The intent of action [`Self::next`] is recorded and its outcome is not.
    pub intent_open: bool,
}

impl RecoveryPlanState {
    /// The state of a stream with no events.
    #[must_use]
    pub const fn initial() -> Self {
        Self {
            phase: PlanPhase::Unproposed,
            epoch: Epoch::new(0),
            bindings: None,
            legacy: false,
            actions: Vec::new(),
            expires_at: Millis::new(0),
            approved_by: None,
            next: 0,
            intent_open: false,
        }
    }
}

/// The commands of `doc/domain-model.md` §6.9. A plan applies through ordinary commands on the
/// attempts (`Attempt.Adopt`, `Attempt.Quarantine`, …), each between its intent
/// ([`Self::BeginAction`]) and its outcome ([`Self::RecordOutcome`]); it never writes another
/// stream's events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum RecoveryPlanCommand {
    /// Draw the plan (`mayfly recover --dry-run`, P20).
    ProposePlan {
        /// What it is bound to.
        bindings: Bindings,
        /// Its actions, from the recovery table's proposals.
        actions: Vec<PlanAction>,
        /// Its expiry.
        expires_at: Millis,
    },
    /// An operator approves it (`mayfly recover --apply <plan-id>`).
    ApprovePlan,
    /// The intent of action `index`, with the bindings as they stand now.
    BeginAction {
        /// The action.
        index: u32,
        /// The bindings now.
        current: Bindings,
    },
    /// The outcome of action `index`.
    RecordOutcome {
        /// The action.
        index: u32,
        /// How its command ended.
        outcome: ActionOutcome,
        /// The plan's own write, from the attempt command's receipt: required when the command
        /// was applied, absent when it was refused.
        write: Option<OwnWrite>,
    },
    /// Every action applied: complete, with the bindings as they stand now.
    CompletePlan {
        /// The bindings now.
        current: Bindings,
    },
    /// The plan's expiry passed (P20's timer).
    ExpirePlan,
}

impl CommandBody for RecoveryPlanCommand {
    const TYPES: &'static [&'static str] = &[
        "propose_plan",
        "approve_plan",
        "begin_action",
        "record_outcome",
        "complete_plan",
        "expire_plan",
    ];

    fn command_type(&self) -> &'static str {
        match self {
            Self::ProposePlan { .. } => "propose_plan",
            Self::ApprovePlan => "approve_plan",
            Self::BeginAction { .. } => "begin_action",
            Self::RecordOutcome { .. } => "record_outcome",
            Self::CompletePlan { .. } => "complete_plan",
            Self::ExpirePlan => "expire_plan",
        }
    }
}

/// A refusal of the `RecoveryPlan` aggregate. Its serde form is its code. A stale binding is
/// not a refusal but the recorded `RecoveryPlanRefused`, so the plan stays refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPlanRejection {
    /// The command's run epoch is older than the plan's (I18).
    StaleRun,
    /// A plan drawn under another run epoch than the command's.
    EpochMismatch,
    /// An action's attempt stream is not among the bound heads.
    UnboundTarget,
    /// An outcome whose write is not the action's own: missing for an applied command, present
    /// for a refused one, on another stream, or not advancing it.
    WrongWrite,
    /// No plan is proposed on the stream.
    NotProposed,
    /// The plan is proposed already; it is immutable.
    AlreadyProposed,
    /// Two actions name one attempt.
    DuplicateTarget,
    /// An adoption without the instance id that proves ownership (ADR 0026).
    NoEvidence,
    /// Only an operator approves a plan.
    OperatorOnly,
    /// The plan's expiry passed before it was approved or started.
    PlanExpired,
    /// The plan is approved already (single approval).
    AlreadyApproved,
    /// The plan is not approved.
    NotApproved,
    /// Not the next action.
    OutOfOrder,
    /// An outcome without its intent (I31).
    NoIntent,
    /// Actions remain to apply.
    ActionsRemaining,
    /// The expiry has not passed.
    NotExpired,
    /// Application started: the plan no longer expires; it completes, fails or is refused.
    InProgress,
    /// The plan is completed, refused or expired.
    Closed,
}

impl Rejection for RecoveryPlanRejection {
    const CODES: &'static [&'static str] = &[
        "stale_run",
        "epoch_mismatch",
        "unbound_target",
        "wrong_write",
        "not_proposed",
        "already_proposed",
        "duplicate_target",
        "no_evidence",
        "operator_only",
        "plan_expired",
        "already_approved",
        "not_approved",
        "out_of_order",
        "no_intent",
        "actions_remaining",
        "not_expired",
        "in_progress",
        "closed",
    ];

    fn code(&self) -> &'static str {
        match self {
            Self::StaleRun => "stale_run",
            Self::EpochMismatch => "epoch_mismatch",
            Self::UnboundTarget => "unbound_target",
            Self::WrongWrite => "wrong_write",
            Self::NotProposed => "not_proposed",
            Self::AlreadyProposed => "already_proposed",
            Self::DuplicateTarget => "duplicate_target",
            Self::NoEvidence => "no_evidence",
            Self::OperatorOnly => "operator_only",
            Self::PlanExpired => "plan_expired",
            Self::AlreadyApproved => "already_approved",
            Self::NotApproved => "not_approved",
            Self::OutOfOrder => "out_of_order",
            Self::NoIntent => "no_intent",
            Self::ActionsRemaining => "actions_remaining",
            Self::NotExpired => "not_expired",
            Self::InProgress => "in_progress",
            Self::Closed => "closed",
        }
    }
}
