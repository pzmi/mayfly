//! `decide` and `evolve` for the recovery plan: every phase against every command, as explicit
//! match arms with no wildcard (`ARCH-exhaustive-transition`); the outcome of every pair is
//! pinned by `tests/recovery_plan/matrix.rs::should_define_every_pair_when_each_phase_meets_each_command`.

use std::collections::BTreeSet;

use mayfly_domain::{Epoch, Millis};

use super::events::{
    RecoveryActionRecorded, RecoveryActionStarted, RecoveryPlanApproved, RecoveryPlanCompleted,
    RecoveryPlanDrawn, RecoveryPlanEpochAdvanced, RecoveryPlanEvent, RecoveryPlanExpired,
    RecoveryPlanRefused,
};
use super::state::{
    ActionKind, ActionOutcome, Bindings, Observed, OwnWrite, PlanAction, PlanPhase,
    RecoveryPlanCommand, RecoveryPlanRejection, RecoveryPlanState, StaleBinding, attempt_stream,
    stale,
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
        } => decide_propose(state, run_epoch, bindings, actions, expires_at)?,
        RecoveryPlanCommand::ApprovePlan => decide_approve(state, actor, run_epoch, issued_at)?,
        RecoveryPlanCommand::BeginAction { index, current } => {
            decide_begin(state, index, &current, run_epoch, issued_at)?
        }
        RecoveryPlanCommand::RecordOutcome {
            index,
            outcome,
            write,
        } => decide_outcome(state, index, outcome, write)?,
        RecoveryPlanCommand::CompletePlan { current } => {
            decide_complete(state, &current, run_epoch)?
        }
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

fn refused(moved: StaleBinding) -> RecoveryPlanEvent {
    RecoveryPlanEvent::RecoveryPlanRefused(RecoveryPlanRefused { stale: moved })
}

/// A plan is drawn under the run epoch that proposes it, names each attempt once with its
/// stream among the bound heads, and adopts only on the instance id that proves ownership
/// (ADR 0026). Pinned by
/// `tests/recovery_plan/rules.rs::should_refuse_a_plan_when_an_adoption_has_no_evidence` and
/// `should_refuse_a_plan_when_its_bindings_do_not_cover_its_run_and_targets`.
fn decide_propose(
    state: &RecoveryPlanState,
    run_epoch: Epoch,
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
    if bindings.epoch != run_epoch {
        return Err(RecoveryPlanRejection::EpochMismatch);
    }
    let mut targets = BTreeSet::new();
    for action in &actions {
        if !targets.insert(action.attempt_id) {
            return Err(RecoveryPlanRejection::DuplicateTarget);
        }
        if !bindings
            .heads
            .contains_key(&attempt_stream(action.attempt_id))
        {
            return Err(RecoveryPlanRejection::UnboundTarget);
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
    Ok(vec![RecoveryPlanEvent::RecoveryPlanDrawn(
        RecoveryPlanDrawn {
            bindings,
            actions,
            expires_at,
        },
    )])
}

/// One approval, by an operator, before the expiry and in the run the plan was drawn in: a plan
/// of an earlier run is stale whatever an operator says (`epoch_mismatch`). An outcome is not so
/// checked: once an action's command ran, its outcome must be recorded whatever run records it
/// (intent then outcome, resumable across a restart).
fn decide_approve(
    state: &RecoveryPlanState,
    actor: Actor,
    run_epoch: Epoch,
    at: Millis,
) -> Decision {
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
    if state
        .bindings
        .as_ref()
        .is_some_and(|drawn| drawn.epoch != run_epoch)
    {
        return Err(RecoveryPlanRejection::EpochMismatch);
    }
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

/// Whether the plan still holds, against the command's own run epoch (fenced, I18) and the
/// heads the use case read: the first binding that moved, if any. A plan drawn by KER-10 is
/// bound by a digest that cannot be checked per stream, so it never holds.
fn moved(state: &RecoveryPlanState, current: &Observed, run_epoch: Epoch) -> Option<StaleBinding> {
    if state.legacy {
        return Some(StaleBinding::JournalHeads);
    }
    let Some(drawn) = state.bindings.as_ref() else {
        return Some(StaleBinding::JournalHeads);
    };
    if run_epoch != drawn.epoch {
        return Some(StaleBinding::Epoch);
    }
    stale(drawn, current)
}

/// I31: an action's intent is recorded only while every binding holds, the run epoch the
/// command is issued under included; a binding that moved refuses the plan instead, so its
/// command never runs (F45). A repeated intent of the open action resumes it. Pinned by
/// `tests/recovery_plan/rules.rs::should_refuse_the_plan_when_it_is_applied_after_the_world_moved`,
/// `should_refuse_the_plan_when_the_command_runs_under_another_epoch` and
/// `tests/recovery_plan/properties.rs::should_record_no_intent_when_a_binding_moved`.
fn decide_begin(
    state: &RecoveryPlanState,
    index: u32,
    current: &Observed,
    run_epoch: Epoch,
    at: Millis,
) -> Decision {
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
    if let Some(moved) = moved(state, current, run_epoch) {
        return Ok(vec![refused(moved)]);
    }
    if state.intent_open {
        return Ok(vec![]);
    }
    Ok(vec![RecoveryPlanEvent::RecoveryActionStarted(
        RecoveryActionStarted { index },
    )])
}

/// I31: an outcome only after its intent, with the plan's own write: an applied command wrote
/// the action's attempt stream once, a refused one wrote nothing. The binding advances that one
/// head, and only from the revision the plan bound: a write that started elsewhere means another
/// writer moved the stream, and the plan is refused with the outcome (KER-10F). Pinned by
/// `tests/recovery_plan/rules.rs::should_refuse_an_outcome_when_its_intent_is_not_recorded`,
/// `should_refuse_the_plan_when_its_own_stream_moved_before_its_write` and
/// `should_refuse_the_plan_when_another_stream_moved_before_the_next_action`.
fn decide_outcome(
    state: &RecoveryPlanState,
    index: u32,
    outcome: ActionOutcome,
    write: Option<OwnWrite>,
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
    let target = state
        .actions
        .get(state.next)
        .map(|a| attempt_stream(a.attempt_id));
    let foreign = match (outcome, &write) {
        (ActionOutcome::Applied, Some(w)) => {
            if target.as_ref() != Some(&w.stream) || w.after <= w.before {
                return Err(RecoveryPlanRejection::WrongWrite);
            }
            let bound = state
                .bindings
                .as_ref()
                .and_then(|b| b.heads.get(&w.stream).copied());
            bound != Some(w.before)
        }
        (ActionOutcome::Failed, None) => false,
        (ActionOutcome::Applied, None) | (ActionOutcome::Failed, Some(_)) => {
            return Err(RecoveryPlanRejection::WrongWrite);
        }
    };
    let mut events = vec![RecoveryPlanEvent::RecoveryActionRecorded(
        RecoveryActionRecorded {
            index,
            outcome,
            write,
        },
    )];
    if foreign || state.legacy {
        events.push(refused(StaleBinding::JournalHeads));
    }
    Ok(events)
}

/// Completed only with every action applied and every binding still holding, under the run
/// epoch the plan was drawn in.
fn decide_complete(state: &RecoveryPlanState, current: &Observed, run_epoch: Epoch) -> Decision {
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
    Ok(vec![match moved(state, current, run_epoch) {
        Some(moved) => refused(moved),
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

fn evolve_outcome(state: &mut RecoveryPlanState, outcome: ActionOutcome) {
    state.intent_open = false;
    match outcome {
        ActionOutcome::Applied => {
            state.phase = PlanPhase::Applying;
            state.next = state.next.saturating_add(1);
        }
        ActionOutcome::Failed => state.phase = PlanPhase::Failed,
    }
}

/// Applies one recorded event.
#[must_use]
pub fn evolve(mut state: RecoveryPlanState, event: &RecoveryPlanEvent) -> RecoveryPlanState {
    match event {
        RecoveryPlanEvent::RecoveryPlanEpochAdvanced(e) => state.epoch = e.epoch,
        RecoveryPlanEvent::RecoveryPlanProposed(e) => {
            state.phase = PlanPhase::Proposed;
            state.legacy = true;
            state.bindings = None;
            state.actions.clone_from(&e.actions);
            state.expires_at = e.expires_at;
        }
        RecoveryPlanEvent::RecoveryPlanDrawn(e) => {
            state.phase = PlanPhase::Proposed;
            state.legacy = false;
            state.bindings = Some(e.bindings.clone());
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
        RecoveryPlanEvent::RecoveryActionApplied(e) => evolve_outcome(&mut state, e.outcome),
        RecoveryPlanEvent::RecoveryActionRecorded(e) => {
            if let (Some(write), Some(bindings)) = (&e.write, state.bindings.as_mut())
                && let Some(head) = bindings.heads.get_mut(&write.stream)
                && *head == write.before
            {
                *head = write.after;
            }
            evolve_outcome(&mut state, e.outcome);
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
