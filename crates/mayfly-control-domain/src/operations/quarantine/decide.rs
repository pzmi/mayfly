//! `decide` and `evolve` for the quarantine case: every phase against every command, as
//! explicit match arms with no wildcard (`ARCH-exhaustive-transition`); the outcome of every
//! pair is pinned by
//! `tests/quarantine/matrix.rs::should_define_every_pair_when_each_phase_meets_each_command`.

use mayfly_domain::{AttemptId, Epoch, EventId, Millis, ObservationId, ProviderInstanceId};

use super::events::{
    QuarantineCaseEscalated, QuarantineCaseEvent, QuarantineCaseInspected, QuarantineCaseOpened,
    QuarantineCaseResolved, QuarantineEpochAdvanced,
};
use super::state::{
    Case, CasePhase, Disposition, EscalationLevel, Holds, QuarantineCaseCommand,
    QuarantineCaseRejection, QuarantineCaseState,
};
use crate::attempt::QuarantineReason;
use crate::command::{Actor, Command};

/// What `decide` returns.
pub type Decision = Result<Vec<QuarantineCaseEvent>, QuarantineCaseRejection>;

/// The case as `OpenCase` gives it.
struct Opening {
    attempt_id: AttemptId,
    reason: QuarantineReason,
    instance_id: Option<ProviderInstanceId>,
    holds: Holds,
    evidence: Vec<EventId>,
}

/// Decides `command` against `state`. An older run epoch is refused (I18); an accepted command
/// under a newer one records `QuarantineEpochAdvanced` ahead of its events.
///
/// # Errors
/// The [`QuarantineCaseRejection`] that names why the command is not allowed.
pub fn decide(state: &QuarantineCaseState, command: Command<QuarantineCaseCommand>) -> Decision {
    let Command {
        issued_at,
        run_epoch,
        actor,
        body,
        ..
    } = command;
    if run_epoch < state.epoch {
        return Err(QuarantineCaseRejection::StaleRun);
    }
    let events = match body {
        QuarantineCaseCommand::OpenCase {
            attempt_id,
            reason,
            instance_id,
            holds,
            evidence,
        } => decide_open(
            state,
            Opening {
                attempt_id,
                reason,
                instance_id,
                holds,
                evidence,
            },
            issued_at,
        )?,
        QuarantineCaseCommand::InspectCase => decide_inspect(state, actor)?,
        QuarantineCaseCommand::EscalateCase { level } => decide_escalate(state, level)?,
        QuarantineCaseCommand::ResolveCase {
            disposition,
            absence,
        } => decide_resolve(state, actor, disposition, absence)?,
    };
    Ok(with_epoch(state.epoch, run_epoch, events))
}

fn with_epoch(
    stored: Epoch,
    run_epoch: Epoch,
    events: Vec<QuarantineCaseEvent>,
) -> Vec<QuarantineCaseEvent> {
    if events.is_empty() || run_epoch <= stored {
        return events;
    }
    let mut all = Vec::with_capacity(events.len() + 1);
    all.push(QuarantineCaseEvent::QuarantineEpochAdvanced(
        QuarantineEpochAdvanced { epoch: run_epoch },
    ));
    all.extend(events);
    all
}

/// An open case stays the one case: the same opening again records nothing, another reason is
/// refused (`already_open`); a resolved one may be followed by a new case when the attempt is
/// quarantined again. A stream holds one attempt's cases: another attempt id is refused
/// (`wrong_attempt`); that the id is the stream's own is the store's addressing
/// (`quarantine-<attempt_id>`, the CP-06 card). A vanished VM is not quarantined any more (ADR 0066 as amended
/// 2026-11-11): the reason only decodes. Pinned by
/// `tests/quarantine/rules.rs::should_refuse_to_open_a_case_when_the_vm_vanished`.
fn decide_open(state: &QuarantineCaseState, opening: Opening, at: Millis) -> Decision {
    if let Some(case) = state.case.as_ref()
        && case.attempt_id != opening.attempt_id
    {
        return Err(QuarantineCaseRejection::WrongAttempt);
    }
    match state.phase {
        CasePhase::NotOpened | CasePhase::Resolved(_) => {}
        CasePhase::Open | CasePhase::Inspected => {
            let same = state
                .case
                .as_ref()
                .is_some_and(|case| case.reason == opening.reason);
            return if same {
                Ok(vec![])
            } else {
                Err(QuarantineCaseRejection::AlreadyOpen)
            };
        }
    }
    if opening.reason == QuarantineReason::VmVanishedExternally {
        return Err(QuarantineCaseRejection::VanishedNotQuarantined);
    }
    Ok(vec![QuarantineCaseEvent::QuarantineCaseOpened(
        QuarantineCaseOpened {
            attempt_id: opening.attempt_id,
            reason: opening.reason,
            instance_id: opening.instance_id,
            holds: opening.holds,
            evidence: opening.evidence,
            opened_at: at,
        },
    )])
}

/// The operator's uid, or the refusal of any other actor (I28).
const fn operator(actor: Actor) -> Result<u32, QuarantineCaseRejection> {
    match actor {
        Actor::Operator { uid } => Ok(uid),
        Actor::Automation { .. } | Actor::Startup => Err(QuarantineCaseRejection::OperatorOnly),
    }
}

const fn open(phase: CasePhase) -> Result<(), QuarantineCaseRejection> {
    match phase {
        CasePhase::Open | CasePhase::Inspected => Ok(()),
        CasePhase::NotOpened => Err(QuarantineCaseRejection::NotOpen),
        CasePhase::Resolved(_) => Err(QuarantineCaseRejection::Resolved),
    }
}

fn decide_inspect(state: &QuarantineCaseState, actor: Actor) -> Decision {
    open(state.phase)?;
    let operator_uid = operator(actor)?;
    Ok(vec![QuarantineCaseEvent::QuarantineCaseInspected(
        QuarantineCaseInspected { operator_uid },
    )])
}

/// Escalation only rises, and changes nothing but its level: no phase, hold or disposition.
/// Pinned by `tests/quarantine/properties.rs::should_never_resolve_or_act_when_no_operator_commands`.
fn decide_escalate(state: &QuarantineCaseState, level: EscalationLevel) -> Decision {
    open(state.phase)?;
    if state.escalation.is_some_and(|current| current >= level) {
        return Ok(vec![]);
    }
    Ok(vec![QuarantineCaseEvent::QuarantineCaseEscalated(
        QuarantineCaseEscalated { level },
    )])
}

/// Only an operator resolves a case, a delete only with the recorded instance id, which the
/// resolution records as the proof its `DeleteVm` must carry, and a release only on a complete
/// inventory's absence (I28, ADR 0025). The aggregate checks that an observation id is named,
/// not what it observed: the policy that issues the resolution (CP-06, RT-03) verifies against
/// the stored observation that it is a complete inventory, taken after the case opened, that
/// does not list this VM (its card's obligation). Pinned by
/// `tests/quarantine/rules.rs::should_resolve_only_by_an_operator_when_any_actor_tries` and
/// `tests/quarantine/properties.rs::should_never_resolve_or_act_when_no_operator_commands`.
fn decide_resolve(
    state: &QuarantineCaseState,
    actor: Actor,
    disposition: Disposition,
    absence: Option<ObservationId>,
) -> Decision {
    open(state.phase)?;
    let operator_uid = operator(actor)?;
    let (absence, instance_id) = match disposition {
        Disposition::DeleteWithProof => {
            match state.case.as_ref().and_then(|c| c.instance_id.clone()) {
                Some(instance_id) => (None, Some(instance_id)),
                None => return Err(QuarantineCaseRejection::NoProof),
            }
        }
        Disposition::ReleasedObservedAbsent => match absence {
            Some(observation) => (Some(observation), None),
            None => return Err(QuarantineCaseRejection::AbsenceNotObserved),
        },
        Disposition::Keep => (None, None),
    };
    Ok(vec![QuarantineCaseEvent::QuarantineCaseResolved(
        QuarantineCaseResolved {
            disposition,
            operator_uid,
            absence,
            instance_id,
        },
    )])
}

/// Applies one recorded event.
#[must_use]
pub fn evolve(mut state: QuarantineCaseState, event: &QuarantineCaseEvent) -> QuarantineCaseState {
    match event {
        QuarantineCaseEvent::QuarantineEpochAdvanced(e) => state.epoch = e.epoch,
        QuarantineCaseEvent::QuarantineCaseOpened(e) => {
            state.phase = CasePhase::Open;
            state.escalation = None;
            state.case = Some(Case {
                attempt_id: e.attempt_id,
                reason: e.reason,
                instance_id: e.instance_id.clone(),
                holds: e.holds,
                evidence: e.evidence.clone(),
                opened_at: e.opened_at,
            });
        }
        QuarantineCaseEvent::QuarantineCaseInspected(_) => state.phase = CasePhase::Inspected,
        QuarantineCaseEvent::QuarantineCaseEscalated(e) => state.escalation = Some(e.level),
        QuarantineCaseEvent::QuarantineCaseResolved(e) => {
            state.phase = CasePhase::Resolved(e.disposition);
        }
    }
    state
}
