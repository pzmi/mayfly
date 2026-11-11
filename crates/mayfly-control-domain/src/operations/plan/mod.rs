//! The `RecoveryPlan` aggregate (`doc/domain-model.md` §6.9): one repair of a host in safe
//! mode, stream `recovery-<plan_id>`, `plan_id` derived from the plan's content.
//!
//! Plain data and two pure functions (ADR 0022). It protects alone:
//!
//! - **I31**: a plan is bound to the journal's heads, a complete inventory, GitHub's runner
//!   listing (or its unreachability) and the run epoch it was drawn from. Each action's intent
//!   is recorded only while every binding holds, and its outcome only after its intent; a
//!   binding that moved records `RecoveryPlanRefused{stale}` instead, so the action never runs
//!   and the plan is applied no further (F45)
//!   (`tests/recovery_plan/rules.rs::should_refuse_the_plan_when_it_is_applied_after_the_world_moved`,
//!   `tests/recovery_plan/rules.rs::should_refuse_an_outcome_when_its_intent_is_not_recorded`,
//!   `tests/recovery_plan/properties.rs::should_record_no_intent_when_a_binding_moved`,
//!   `tests/recovery_plan/properties.rs::should_record_each_outcome_after_its_intent_when_commands_are_random`).
//! - **No destructive or asserted action**: its actions are the recovery table's safe-mode
//!   proposals ([`ActionKind::from_proposal`]), so a plan holds no retire, retry, delete or
//!   `assert_outcome` (ADR 0025, OD10), and an adoption needs the VM's instance id
//!   (`tests/recovery_plan/rules.rs::should_hold_no_destructive_action_when_built_from_proposals`,
//!   `tests/recovery_plan/rules.rs::should_refuse_a_plan_when_an_adoption_has_no_evidence`).
//! - **Immutable once proposed, a single operator approval, resumable application**: actions
//!   apply in order, a failed one stops the plan where it can resume, and an approved plan
//!   that has not started expires.
//! - **I18**: an older run epoch is refused (`stale_run`); a newer one is recorded as
//!   `RecoveryPlanEpochAdvanced`.
//!
//! It applies through ordinary commands on the attempts, which the repair use case issues
//! between an action's intent and its outcome; it never writes another stream's events.
//! "At most one plan applying per host" is P20's rule. The model is `formal/recovery.qnt`
//! (`proposePlan`, `approvePlan`, `planIntentWith`, `planOutcomeWith`, `refusePlan`,
//! `completePlan`, invariant `i31PlanBound`); what this aggregate adds beyond it is listed in
//! `doc/formal-models.md` (recovery, "Not modelled").

mod cases;
mod decide;
mod events;
mod state;

pub use decide::{Decision, decide, evolve};
pub use events::{
    RecoveryActionApplied, RecoveryActionStarted, RecoveryPlanApproved, RecoveryPlanCompleted,
    RecoveryPlanEpochAdvanced, RecoveryPlanEvent, RecoveryPlanExpired, RecoveryPlanProposed,
    RecoveryPlanRefused,
};
pub use state::{
    ActionKind, ActionOutcome, Bindings, GithubBinding, PlanAction, PlanPhase, RecoveryPlanCommand,
    RecoveryPlanRejection, RecoveryPlanState, StaleBinding, stale,
};

use crate::aggregate::{Aggregate, DeterminismCase, DeterminismCases};
use crate::command::Command;

/// The `RecoveryPlan` aggregate: a marker grouping its data and functions (ADR 0022).
#[derive(Debug, Clone, Copy, Default)]
pub struct RecoveryPlan;

impl Aggregate for RecoveryPlan {
    const NAME: &'static str = "recovery";
    type State = RecoveryPlanState;
    type Body = RecoveryPlanCommand;
    type Event = RecoveryPlanEvent;
    type Rejection = RecoveryPlanRejection;

    fn initial() -> RecoveryPlanState {
        RecoveryPlanState::initial()
    }

    fn decide(state: &RecoveryPlanState, command: Command<RecoveryPlanCommand>) -> Decision {
        decide::decide(state, command)
    }

    fn evolve(state: RecoveryPlanState, event: &RecoveryPlanEvent) -> RecoveryPlanState {
        decide::evolve(state, event)
    }
}

impl DeterminismCases for RecoveryPlan {
    fn determinism_cases() -> Vec<DeterminismCase<Self>> {
        cases::cases()
    }
}
