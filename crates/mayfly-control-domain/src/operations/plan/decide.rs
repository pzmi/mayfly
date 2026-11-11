//! `decide` and `evolve` for the recovery plan: every phase against every command, as explicit
//! match arms with no wildcard (`ARCH-exhaustive-transition`); the outcome of every pair is
//! pinned by `tests/recovery_plan/matrix.rs::should_define_every_pair_when_each_phase_meets_each_command`.

use std::collections::BTreeSet;

use mayfly_domain::{Epoch, Millis, StateDigest};

use super::events::{
    RecoveryActionApplied, RecoveryActionStarted, RecoveryPlanApproved, RecoveryPlanCompleted,
    RecoveryPlanEpochAdvanced, RecoveryPlanEvent, RecoveryPlanExpired, RecoveryPlanProposed,
    RecoveryPlanRefused,
};
use super::state::{
    ActionKind, ActionOutcome, Bindings, PlanAction, PlanPhase, RecoveryPlanCommand,
    RecoveryPlanRejection, RecoveryPlanState, stale,
};
use crate::command::{Actor, Command};

/// What `decide` returns.
pub type Decision = Result<Vec<RecoveryPlanEvent>, RecoveryPlanRejection>;

/// Decides `command` against `state`. An older run epoch is refused (I18); an accepted command
/// under a newer one records `RecoveryPlanEpochAdvanced` ahead of its events.
///
/// # Errors
/// The [`RecoveryPlanRejection`] that names why the command is not allowed.
pub fn decide(state: &RecoveryPlanState, command: Command<RecoveryPlanCommand>) -> Decision {
    let Command {
        issued_at,
        run_epoch,
        actor,
        body,
        ..
    } = command;
    if run_epoch < state.epoch {
        return Err(RecoveryPlanRejection::StaleRun);
    }
    let events = match body {
        RecoveryPlanCommand::ProposePlan {
            bindings,
            actions,
            expires_at,
        } => decide_propose(state, bindings, actions, expires_at)?,
        RecoveryPlanCommand::ApprovePlan => decide_approve(state, actor, issued_at)?,
        RecoveryPlanCommand::BeginAction { index, current } => {
            decide_begin(state, index, &current, issued_at)?
        }
        RecoveryPlanCommand::RecordOutcome {
            index,
            outcome,
            heads_after,
        } => decide_outcome(state, index, outcome, heads_after)?,
        RecoveryPlanCommand::CompletePlan { current } => decide_complete(state, &current)?,
        RecoveryPlanCommand::ExpirePlan => decide_expire(state, issued_at)?,
    };
    Ok(with_epoch(state.epoch, run_epoch, events))
}

fn with_epoch(
    stored: Epoch,
    run_epoch: Epoch,
    events: Vec<RecoveryPlanEvent>,
) -> Vec<RecoveryPlanEvent> {
    if events.is_empty() || run_epoch <= stored {
        return events;
    }
    let mut all = Vec::with_capacity(events.len() + 1);
    all.push(RecoveryPlanEvent::RecoveryPlanEpochAdvanced(
        RecoveryPlanEpochAdvanced { epoch: run_epoch },
    ));
    all.extend(events);
    all
}

/// A plan names each attempt once, and adopts only on the instance id that proves ownership
/// (ADR 0026). Pinned by `tests/recovery_plan/rules.rs::should_refuse_a_plan_when_an_adoption_has_no_evidence`.
fn decide_propose(
    state: &RecoveryPlanState,
    bindings: Bindings,
    actions: Vec<PlanAction>,
    expires_at: Millis,
) -> Decision {
    match state.phase {
        PlanPhase::Unproposed => {}
        PlanPhase::Proposed
        | PlanPhase::Approved
        | PlanPhase::Applying
        | PlanPhase::Failed
        | PlanPhase::Completed
        | PlanPhase::Refused(_)
        | PlanPhase::Expired => return Err(RecoveryPlanRejection::AlreadyProposed),
    }
    let mut targets = BTreeSet::new();
    for action in &actions {
        if !targets.insert(action.attempt_id) {
            return Err(RecoveryPlanRejection::DuplicateTarget);
        }
        let needs_evidence = match action.kind {
            ActionKind::Adopt => true,
            ActionKind::MarkLost
            | ActionKind::Quarantine(_)
            | ActionKind::Keep
            | ActionKind::Report
            | ActionKind::Ignore => false,
        };
        if needs_evidence && action.instance_id.is_none() {
            return Err(RecoveryPlanRejection::NoEvidence);
        }
    }
    Ok(vec![RecoveryPlanEvent::RecoveryPlanProposed(
        RecoveryPlanProposed {
            bindings,
            actions,
            expires_at,
        },
    )])
}

/// One approval, by an operator, before the expiry.
fn decide_approve(state: &RecoveryPlanState, actor: Actor, at: Millis) -> Decision {
    match state.phase {
        PlanPhase::Proposed => {}
        PlanPhase::Approved | PlanPhase::Applying | PlanPhase::Failed => {
            return Err(RecoveryPlanRejection::AlreadyApproved);
        }
        PlanPhase::Completed | PlanPhase::Refused(_) | PlanPhase::Expired => {
            return Err(RecoveryPlanRejection::Closed);
        }
        PlanPhase::Unproposed => return Err(RecoveryPlanRejection::NotProposed),
    }
    let operator_uid = match actor {
        Actor::Operator { uid } => uid,
        Actor::Automation { .. } | Actor::Startup => {
            return Err(RecoveryPlanRejection::OperatorOnly);
        }
    };
    if at >= state.expires_at {
        return Err(RecoveryPlanRejection::PlanExpired);
    }
    Ok(vec![RecoveryPlanEvent::RecoveryPlanApproved(
        RecoveryPlanApproved { operator_uid },
    )])
}

fn at_next(state: &RecoveryPlanState, index: u32) -> bool {
    usize::try_from(index).is_ok_and(|i| i == state.next && i < state.actions.len())
}

/// I31: an action's intent is recorded only while every binding holds; a binding that moved
/// refuses the plan instead, so its command never runs (F45). A repeated intent of the open
/// action resumes it. Pinned by
/// `tests/recovery_plan/rules.rs::should_refuse_the_plan_when_it_is_applied_after_the_world_moved`
/// and `tests/recovery_plan/properties.rs::should_record_no_intent_when_a_binding_moved`.
fn decide_begin(state: &RecoveryPlanState, index: u32, current: &Bindings, at: Millis) -> Decision {
    match state.phase {
        PlanPhase::Approved => {
            if at >= state.expires_at {
                return Err(RecoveryPlanRejection::PlanExpired);
            }
        }
        PlanPhase::Applying | PlanPhase::Failed => {}
        PlanPhase::Proposed => return Err(RecoveryPlanRejection::NotApproved),
        PlanPhase::Unproposed => return Err(RecoveryPlanRejection::NotProposed),
        PlanPhase::Completed | PlanPhase::Refused(_) | PlanPhase::Expired => {
            return Err(RecoveryPlanRejection::Closed);
        }
    }
    if !at_next(state, index) {
        return Err(RecoveryPlanRejection::OutOfOrder);
    }
    let Some(drawn) = state.bindings else {
        return Err(RecoveryPlanRejection::NotProposed);
    };
    if let Some(moved) = stale(&drawn, current) {
        return Ok(vec![RecoveryPlanEvent::RecoveryPlanRefused(
            RecoveryPlanRefused { stale: moved },
        )]);
    }
    if state.intent_open {
        return Ok(vec![]);
    }
    Ok(vec![RecoveryPlanEvent::RecoveryActionStarted(
        RecoveryActionStarted { index },
    )])
}

/// I31: an outcome only after its intent. Pinned by
/// `tests/recovery_plan/rules.rs::should_refuse_an_outcome_when_its_intent_is_not_recorded`.
fn decide_outcome(
    state: &RecoveryPlanState,
    index: u32,
    outcome: ActionOutcome,
    heads_after: StateDigest,
) -> Decision {
    match state.phase {
        PlanPhase::Applying => {}
        PlanPhase::Approved | PlanPhase::Failed => return Err(RecoveryPlanRejection::NoIntent),
        PlanPhase::Proposed => return Err(RecoveryPlanRejection::NotApproved),
        PlanPhase::Unproposed => return Err(RecoveryPlanRejection::NotProposed),
        PlanPhase::Completed | PlanPhase::Refused(_) | PlanPhase::Expired => {
            return Err(RecoveryPlanRejection::Closed);
        }
    }
    if !state.intent_open || !at_next(state, index) {
        return Err(RecoveryPlanRejection::NoIntent);
    }
    Ok(vec![RecoveryPlanEvent::RecoveryActionApplied(
        RecoveryActionApplied {
            index,
            outcome,
            heads_after,
        },
    )])
}

/// Completed only with every action applied and every binding still holding.
fn decide_complete(state: &RecoveryPlanState, current: &Bindings) -> Decision {
    match state.phase {
        PlanPhase::Approved | PlanPhase::Applying => {}
        PlanPhase::Failed => return Err(RecoveryPlanRejection::ActionsRemaining),
        PlanPhase::Proposed => return Err(RecoveryPlanRejection::NotApproved),
        PlanPhase::Unproposed => return Err(RecoveryPlanRejection::NotProposed),
        PlanPhase::Completed | PlanPhase::Refused(_) | PlanPhase::Expired => {
            return Err(RecoveryPlanRejection::Closed);
        }
    }
    if state.intent_open || state.next < state.actions.len() {
        return Err(RecoveryPlanRejection::ActionsRemaining);
    }
    let Some(drawn) = state.bindings else {
        return Err(RecoveryPlanRejection::NotProposed);
    };
    Ok(vec![match stale(&drawn, current) {
        Some(moved) => RecoveryPlanEvent::RecoveryPlanRefused(RecoveryPlanRefused { stale: moved }),
        None => RecoveryPlanEvent::RecoveryPlanCompleted(RecoveryPlanCompleted {}),
    }])
}

fn decide_expire(state: &RecoveryPlanState, at: Millis) -> Decision {
    match state.phase {
        PlanPhase::Proposed | PlanPhase::Approved => {}
        PlanPhase::Applying | PlanPhase::Failed => return Err(RecoveryPlanRejection::InProgress),
        PlanPhase::Unproposed => return Err(RecoveryPlanRejection::NotProposed),
        PlanPhase::Completed | PlanPhase::Refused(_) | PlanPhase::Expired => {
            return Err(RecoveryPlanRejection::Closed);
        }
    }
    if at < state.expires_at {
        return Err(RecoveryPlanRejection::NotExpired);
    }
    Ok(vec![RecoveryPlanEvent::RecoveryPlanExpired(
        RecoveryPlanExpired {},
    )])
}

/// Applies one recorded event.
#[must_use]
pub fn evolve(mut state: RecoveryPlanState, event: &RecoveryPlanEvent) -> RecoveryPlanState {
    match event {
        RecoveryPlanEvent::RecoveryPlanEpochAdvanced(e) => state.epoch = e.epoch,
        RecoveryPlanEvent::RecoveryPlanProposed(e) => {
            state.phase = PlanPhase::Proposed;
            state.bindings = Some(e.bindings);
            state.actions.clone_from(&e.actions);
            state.expires_at = e.expires_at;
        }
        RecoveryPlanEvent::RecoveryPlanApproved(e) => {
            state.phase = PlanPhase::Approved;
            state.approved_by = Some(e.operator_uid);
        }
        RecoveryPlanEvent::RecoveryActionStarted(_) => {
            state.phase = PlanPhase::Applying;
            state.intent_open = true;
        }
        RecoveryPlanEvent::RecoveryActionApplied(e) => {
            state.intent_open = false;
            if let Some(bindings) = state.bindings.as_mut() {
                bindings.heads = e.heads_after;
            }
            match e.outcome {
                ActionOutcome::Applied => {
                    state.phase = PlanPhase::Applying;
                    state.next = state.next.saturating_add(1);
                }
                ActionOutcome::Failed => state.phase = PlanPhase::Failed,
            }
        }
        RecoveryPlanEvent::RecoveryPlanCompleted(_) => state.phase = PlanPhase::Completed,
        RecoveryPlanEvent::RecoveryPlanRefused(e) => {
            state.phase = PlanPhase::Refused(e.stale);
            state.intent_open = false;
        }
        RecoveryPlanEvent::RecoveryPlanExpired(_) => state.phase = PlanPhase::Expired,
    }
    state
}
