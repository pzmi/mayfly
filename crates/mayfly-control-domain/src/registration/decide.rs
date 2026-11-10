//! `decide` and `evolve` for the registration: every phase against every command, as explicit
//! match arms with no wildcard (`ARCH-exhaustive-transition`); the outcome of every pair is
//! pinned by `tests/registration/matrix.rs::should_define_every_pair_when_each_phase_meets_each_command`.

use mayfly_domain::Epoch;

use super::events::{
    DeregistrationRefusedBusy, DeregistrationRequested, RegistrationAbsent, RegistrationCompleted,
    RegistrationConnected, RegistrationConsumed, RegistrationDelivered, RegistrationDeregistered,
    RegistrationEpochAdvanced, RegistrationEvent, RegistrationFoundByName,
    RegistrationIssueUnknown, RegistrationIssued, RegistrationPresumedLeaked,
    RegistrationRequested,
};
use super::state::{
    DeregistrationAnswer, DeregistrationReason, JobResult, LeakReason, Phase, RegistrationCommand,
    RegistrationRejection, RegistrationState, Removal, RequestRegistration, RunnerId,
    SessionHealth,
};
use crate::attempt::RunnerRequestId;
use crate::command::Command;

/// What `decide` returns.
pub type Decision = Result<Vec<RegistrationEvent>, RegistrationRejection>;

/// Decides `command` against `state`. An older run epoch is refused (I18); an accepted command
/// under a newer one records `RegistrationEpochAdvanced` ahead of its events.
///
/// # Errors
/// The [`RegistrationRejection`] that names why the command is not allowed.
pub fn decide(state: &RegistrationState, command: Command<RegistrationCommand>) -> Decision {
    let Command {
        run_epoch, body, ..
    } = command;
    if run_epoch < state.epoch {
        return Err(RegistrationRejection::StaleRun);
    }
    let events = decide_body(state, body)?;
    Ok(with_epoch(state.epoch, run_epoch, events))
}

fn with_epoch(
    stored: Epoch,
    run_epoch: Epoch,
    events: Vec<RegistrationEvent>,
) -> Vec<RegistrationEvent> {
    if events.is_empty() || run_epoch <= stored {
        return events;
    }
    let mut all = Vec::with_capacity(events.len() + 1);
    all.push(RegistrationEvent::RegistrationEpochAdvanced(
        RegistrationEpochAdvanced { epoch: run_epoch },
    ));
    all.extend(events);
    all
}

fn decide_body(state: &RegistrationState, body: RegistrationCommand) -> Decision {
    match body {
        RegistrationCommand::RequestRegistration(request) => decide_request(state, request),
        RegistrationCommand::RecordIssued { runner_id } => decide_issued(state, runner_id),
        RegistrationCommand::RecordIssueUnknown => decide_issue_unknown(state),
        RegistrationCommand::RecordDelivered => decide_delivered(state),
        RegistrationCommand::RecordConnected { runner_id } => decide_connected(state, runner_id),
        RegistrationCommand::RecordConsumed { request_id } => decide_consumed(state, request_id),
        RegistrationCommand::RecordCompleted { result } => decide_completed(state, result),
        RegistrationCommand::PresumeLeaked { reason } => decide_presume_leaked(state, reason),
        RegistrationCommand::RequestDeregistration { reason } => {
            decide_request_deregistration(state, reason)
        }
        RegistrationCommand::RecordDeregistration { answer } => {
            decide_deregistration_answer(state, answer)
        }
        RegistrationCommand::RecordFoundByName { runner_id } => {
            decide_found_by_name(state, runner_id)
        }
        RegistrationCommand::RecordAbsent => decide_absent(state),
    }
}

/// I19: the stream asks for its registration once, ever, under the attempt's own VM name; so a
/// runner name is issued at most once (pinned by
/// `tests/registration/rules.rs::should_never_issue_a_runner_name_twice_when_any_command_follows`).
fn decide_request(state: &RegistrationState, request: RequestRegistration) -> Decision {
    match state.phase {
        Phase::Unrequested => {}
        Phase::Requested
        | Phase::IssueUnknown
        | Phase::Issued
        | Phase::Delivered
        | Phase::Connected
        | Phase::Consumed
        | Phase::Completed
        | Phase::Deregistered
        | Phase::Absent => return Err(RegistrationRejection::AlreadyRequested),
    }
    match request.session {
        SessionHealth::Healthy => {}
        SessionHealth::Degraded => return Err(RegistrationRejection::ScaleSetDegraded),
    }
    if request.runner_name.attempt_short() != request.attempt_id.short() {
        return Err(RegistrationRejection::RunnerNameMismatch);
    }
    Ok(vec![RegistrationEvent::RegistrationRequested(
        RegistrationRequested {
            attempt_id: request.attempt_id,
            runner_name: request.runner_name,
            scale_set: request.scale_set,
        },
    )])
}

fn decide_issued(state: &RegistrationState, runner_id: RunnerId) -> Decision {
    match state.phase {
        Phase::Requested => Ok(vec![RegistrationEvent::RegistrationIssued(
            RegistrationIssued { runner_id },
        )]),
        Phase::Unrequested => Err(RegistrationRejection::NotRequested),
        Phase::Deregistered | Phase::Absent => Err(RegistrationRejection::Terminal),
        Phase::IssueUnknown
        | Phase::Issued
        | Phase::Delivered
        | Phase::Connected
        | Phase::Consumed
        | Phase::Completed => Err(RegistrationRejection::NoIssuePending),
    }
}

/// Entering `IssueUnknown` is a record of ignorance; leaving it takes the reaper's observation
/// (`RecordFoundByName`, `RecordAbsent`), never a new mint (`noRemintAfterUnknown`, pinned by
/// `tests/registration/rules.rs::should_leave_an_unknown_issue_only_on_the_reapers_observation_when_every_command_is_tried`).
fn decide_issue_unknown(state: &RegistrationState) -> Decision {
    match state.phase {
        Phase::Requested => Ok(vec![RegistrationEvent::RegistrationIssueUnknown(
            RegistrationIssueUnknown {},
        )]),
        Phase::Unrequested => Err(RegistrationRejection::NotRequested),
        Phase::Deregistered | Phase::Absent => Err(RegistrationRejection::Terminal),
        Phase::IssueUnknown
        | Phase::Issued
        | Phase::Delivered
        | Phase::Connected
        | Phase::Consumed
        | Phase::Completed => Err(RegistrationRejection::NoIssuePending),
    }
}

/// I22: no delivery once presumed leaked (after `RecordFoundByName` or `PresumeLeaked`), as the
/// model's `canDeliver` and its `deliveredWhileLeaked` ghost state; the runner is deregistered
/// instead (pinned by
/// `tests/registration/rules.rs::should_refuse_a_delivery_when_the_registration_is_presumed_leaked`).
fn decide_delivered(state: &RegistrationState) -> Decision {
    match state.phase {
        Phase::Issued if state.leaked.is_some() => Err(RegistrationRejection::PresumedLeaked),
        Phase::Issued => Ok(vec![RegistrationEvent::RegistrationDelivered(
            RegistrationDelivered {},
        )]),
        Phase::Delivered | Phase::Connected | Phase::Consumed | Phase::Completed => Ok(vec![]),
        Phase::Unrequested => Err(RegistrationRejection::NotRequested),
        Phase::Deregistered | Phase::Absent => Err(RegistrationRejection::Terminal),
        Phase::Requested | Phase::IssueUnknown => Err(RegistrationRejection::NotIssued),
    }
}

/// ADR 0038: after delivery the confirmation names the runner id GitHub shows online, and only
/// the issued one confirms (pinned by
/// `tests/registration/rules.rs::should_confirm_only_the_issued_runner_id_when_a_runner_is_seen_online`).
fn decide_connected(state: &RegistrationState, runner_id: RunnerId) -> Decision {
    let matches = state.runner_id == Some(runner_id);
    match state.phase {
        Phase::Requested | Phase::IssueUnknown => Err(RegistrationRejection::NoRunnerId),
        Phase::Issued => Err(RegistrationRejection::NotDelivered),
        Phase::Delivered if matches => Ok(vec![RegistrationEvent::RegistrationConnected(
            RegistrationConnected { runner_id },
        )]),
        Phase::Connected | Phase::Consumed | Phase::Completed if matches => Ok(vec![]),
        Phase::Delivered | Phase::Connected | Phase::Consumed | Phase::Completed => {
            Err(RegistrationRejection::RunnerIdMismatch)
        }
        Phase::Unrequested => Err(RegistrationRejection::NotRequested),
        Phase::Deregistered | Phase::Absent => Err(RegistrationRejection::Terminal),
    }
}

/// One runner, one job: a second job is refused, the same one again records nothing. A job on
/// a registration still `Issued` is a fact too: a launch of unknown outcome may have delivered it.
fn decide_consumed(state: &RegistrationState, request_id: RunnerRequestId) -> Decision {
    match state.phase {
        Phase::Requested | Phase::IssueUnknown => Err(RegistrationRejection::NoRunnerId),
        Phase::Issued | Phase::Delivered | Phase::Connected => {
            Ok(vec![RegistrationEvent::RegistrationConsumed(
                RegistrationConsumed { request_id },
            )])
        }
        Phase::Consumed | Phase::Completed => {
            if state.request_id == Some(request_id) {
                Ok(vec![])
            } else {
                Err(RegistrationRejection::AlreadyConsumed)
            }
        }
        Phase::Unrequested => Err(RegistrationRejection::NotRequested),
        Phase::Deregistered | Phase::Absent => Err(RegistrationRejection::Terminal),
    }
}

fn decide_completed(state: &RegistrationState, result: JobResult) -> Decision {
    match state.phase {
        Phase::Consumed => Ok(vec![RegistrationEvent::RegistrationCompleted(
            RegistrationCompleted { result },
        )]),
        Phase::Completed => Ok(vec![]),
        Phase::Unrequested => Err(RegistrationRejection::NotRequested),
        Phase::Deregistered | Phase::Absent => Err(RegistrationRejection::Terminal),
        Phase::Requested
        | Phase::IssueUnknown
        | Phase::Issued
        | Phase::Delivered
        | Phase::Connected => Err(RegistrationRejection::NotConsumed),
    }
}

/// P13: an issued registration may be presumed leaked at any later phase; once is enough.
fn decide_presume_leaked(state: &RegistrationState, reason: LeakReason) -> Decision {
    match state.phase {
        Phase::Issued
        | Phase::Delivered
        | Phase::Connected
        | Phase::Consumed
        | Phase::Completed => match state.leaked {
            Some(_) => Ok(vec![]),
            None => Ok(vec![RegistrationEvent::RegistrationPresumedLeaked(
                RegistrationPresumedLeaked { reason },
            )]),
        },
        Phase::Unrequested => Err(RegistrationRejection::NotRequested),
        Phase::Deregistered | Phase::Absent => Err(RegistrationRejection::Terminal),
        Phase::Requested | Phase::IssueUnknown => Err(RegistrationRejection::NotIssued),
    }
}

/// I19: a deregistration names a journaled runner id, so a registration without one refuses it
/// (pinned by
/// `tests/registration/rules.rs::should_refuse_a_deregistration_when_no_runner_id_is_journaled`).
/// I21: a retirement GitHub answered busy for is never retried (pinned by
/// `tests/registration/rules.rs::should_refuse_the_same_retirement_when_github_answered_busy`).
fn decide_request_deregistration(
    state: &RegistrationState,
    reason: DeregistrationReason,
) -> Decision {
    match state.phase {
        Phase::Requested | Phase::IssueUnknown => Err(RegistrationRejection::NoRunnerId),
        Phase::Issued
        | Phase::Delivered
        | Phase::Connected
        | Phase::Consumed
        | Phase::Completed => {
            if state.deregistration.is_some() {
                return Ok(vec![]);
            }
            if let Some(retirement) = reason.retirement()
                && state.busy_refused.contains(&retirement)
            {
                return Err(RegistrationRejection::BusyRefused);
            }
            Ok(vec![RegistrationEvent::DeregistrationRequested(
                DeregistrationRequested { reason },
            )])
        }
        Phase::Unrequested => Err(RegistrationRejection::NotRequested),
        Phase::Deregistered | Phase::Absent => Err(RegistrationRejection::Terminal),
    }
}

/// 204 and 404 deregister; busy refuses and cancels the retirement it served (I21).
fn decide_deregistration_answer(
    state: &RegistrationState,
    answer: DeregistrationAnswer,
) -> Decision {
    match state.phase {
        Phase::Unrequested => return Err(RegistrationRejection::NotRequested),
        Phase::Deregistered | Phase::Absent => return Err(RegistrationRejection::Terminal),
        Phase::Requested
        | Phase::IssueUnknown
        | Phase::Issued
        | Phase::Delivered
        | Phase::Connected
        | Phase::Consumed
        | Phase::Completed => {}
    }
    let Some(reason) = state.deregistration else {
        return Err(RegistrationRejection::NoDeregistrationPending);
    };
    let event = match answer {
        DeregistrationAnswer::Removed => {
            RegistrationEvent::RegistrationDeregistered(RegistrationDeregistered {
                answer: Removal::Removed,
            })
        }
        DeregistrationAnswer::NotFound => {
            RegistrationEvent::RegistrationDeregistered(RegistrationDeregistered {
                answer: Removal::NotFound,
            })
        }
        DeregistrationAnswer::Busy => {
            RegistrationEvent::DeregistrationRefusedBusy(DeregistrationRefusedBusy {
                retirement: reason.retirement(),
            })
        }
    };
    Ok(vec![event])
}

fn decide_found_by_name(state: &RegistrationState, runner_id: RunnerId) -> Decision {
    match state.phase {
        Phase::IssueUnknown => Ok(vec![RegistrationEvent::RegistrationFoundByName(
            RegistrationFoundByName { runner_id },
        )]),
        Phase::Unrequested => Err(RegistrationRejection::NotRequested),
        Phase::Deregistered | Phase::Absent => Err(RegistrationRejection::Terminal),
        Phase::Requested
        | Phase::Issued
        | Phase::Delivered
        | Phase::Connected
        | Phase::Consumed
        | Phase::Completed => Err(RegistrationRejection::IssueNotUnknown),
    }
}

/// A definite refusal of the mint (`Requested`) or the reaper's empty listing (`IssueUnknown`).
fn decide_absent(state: &RegistrationState) -> Decision {
    match state.phase {
        Phase::Requested | Phase::IssueUnknown => Ok(vec![RegistrationEvent::RegistrationAbsent(
            RegistrationAbsent {},
        )]),
        Phase::Unrequested => Err(RegistrationRejection::NotRequested),
        Phase::Deregistered | Phase::Absent => Err(RegistrationRejection::Terminal),
        Phase::Issued
        | Phase::Delivered
        | Phase::Connected
        | Phase::Consumed
        | Phase::Completed => Err(RegistrationRejection::IssueNotUnknown),
    }
}

/// Applies one recorded event.
#[must_use]
pub fn evolve(mut state: RegistrationState, event: &RegistrationEvent) -> RegistrationState {
    match event {
        RegistrationEvent::RegistrationEpochAdvanced(e) => state.epoch = e.epoch,
        RegistrationEvent::RegistrationRequested(e) => {
            state.phase = Phase::Requested;
            state.attempt_id = Some(e.attempt_id);
            state.runner_name = Some(e.runner_name.clone());
            state.scale_set = Some(e.scale_set);
        }
        RegistrationEvent::RegistrationIssued(e) => {
            state.phase = Phase::Issued;
            state.runner_id = Some(e.runner_id);
        }
        RegistrationEvent::RegistrationIssueUnknown(_) => state.phase = Phase::IssueUnknown,
        RegistrationEvent::RegistrationFoundByName(e) => {
            state.phase = Phase::Issued;
            state.runner_id = Some(e.runner_id);
            state.leaked = Some(LeakReason::IssueUnknown);
        }
        RegistrationEvent::RegistrationDelivered(_) => state.phase = Phase::Delivered,
        RegistrationEvent::RegistrationConnected(_) => state.phase = Phase::Connected,
        RegistrationEvent::RegistrationConsumed(e) => {
            state.phase = Phase::Consumed;
            state.request_id = Some(e.request_id);
        }
        RegistrationEvent::RegistrationCompleted(e) => {
            state.phase = Phase::Completed;
            state.result = Some(e.result);
        }
        RegistrationEvent::RegistrationPresumedLeaked(e) => state.leaked = Some(e.reason),
        RegistrationEvent::DeregistrationRequested(e) => state.deregistration = Some(e.reason),
        RegistrationEvent::RegistrationDeregistered(_) => {
            state.phase = Phase::Deregistered;
            state.deregistration = None;
        }
        RegistrationEvent::DeregistrationRefusedBusy(e) => {
            state.deregistration = None;
            state.busy_refused.extend(e.retirement);
        }
        RegistrationEvent::RegistrationAbsent(_) => state.phase = Phase::Absent,
    }
    state
}
