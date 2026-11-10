//! The registration's `DETERMINISM_CASES` (ver §3.1): accepted cases along the registration's
//! life and one refused case per [`RegistrationRejection`] code. Histories are produced by
//! `decide` itself from command paths (each path is checked to be accepted step by step by
//! `should_accept_every_step_when_a_case_path_is_replayed`).

use mayfly_domain::{
    AttemptId, CommandId, Epoch, EventId, Generation, HostId, Millis, OwnedVmName,
};
use mayfly_domain::{PoolName, Sample, Slot};

use super::events::RegistrationEvent;
use super::state::{
    DeregistrationAnswer, DeregistrationReason, JobResult, LeakReason, RegistrationCommand,
    RegistrationRejection, RegistrationState, RequestRegistration, RunnerId, ScaleSetId,
    SessionHealth,
};
use super::{RunnerRegistration, decide, evolve};
use crate::aggregate::{DeterminismCase, Expect};
use crate::attempt::RunnerRequestId;
use crate::command::{Actor, Command};

type Step = Command<RegistrationCommand>;

const POLICY: Actor = Actor::Automation {
    policy: "registration_manager",
};
const RUNNER: RunnerId = RunnerId(4_242);

fn at(run_epoch: u64, body: RegistrationCommand) -> Step {
    Command::new(
        CommandId::from_u128(0xC1),
        Millis::new(1_000),
        Epoch::new(run_epoch),
        POLICY,
        body,
    )
}

fn auto(body: RegistrationCommand) -> Step {
    at(1, body)
}

fn attempt() -> AttemptId {
    AttemptId::sample()
}

fn retirement() -> EventId {
    EventId::from_u128(0xE1)
}

fn request_as(attempt_id: AttemptId, session: SessionHealth) -> RegistrationCommand {
    RegistrationCommand::RequestRegistration(RequestRegistration {
        attempt_id,
        runner_name: OwnedVmName::new(
            PoolName::sample(),
            HostId::sample(),
            Slot::new(0),
            Generation::FIRST,
            attempt(),
        ),
        scale_set: ScaleSetId(7),
        session,
    })
}

fn request() -> RegistrationCommand {
    request_as(attempt(), SessionHealth::Healthy)
}

fn issued_cmd() -> RegistrationCommand {
    RegistrationCommand::RecordIssued { runner_id: RUNNER }
}

fn connected_cmd(runner_id: RunnerId) -> RegistrationCommand {
    RegistrationCommand::RecordConnected { runner_id }
}

fn consumed_cmd(job: u64) -> RegistrationCommand {
    RegistrationCommand::RecordConsumed {
        request_id: RunnerRequestId(job),
    }
}

fn for_retirement() -> RegistrationCommand {
    RegistrationCommand::RequestDeregistration {
        reason: DeregistrationReason::Retirement {
            retirement: retirement(),
        },
    }
}

fn answer(answer: DeregistrationAnswer) -> RegistrationCommand {
    RegistrationCommand::RecordDeregistration { answer }
}

fn then(mut path: Vec<Step>, more: impl IntoIterator<Item = RegistrationCommand>) -> Vec<Step> {
    path.extend(more.into_iter().map(auto));
    path
}

fn requested() -> Vec<Step> {
    vec![auto(request())]
}

fn issue_unknown() -> Vec<Step> {
    then(requested(), [RegistrationCommand::RecordIssueUnknown])
}

fn issued() -> Vec<Step> {
    then(requested(), [issued_cmd()])
}

fn delivered() -> Vec<Step> {
    then(issued(), [RegistrationCommand::RecordDelivered])
}

fn connected() -> Vec<Step> {
    then(delivered(), [connected_cmd(RUNNER)])
}

fn consumed() -> Vec<Step> {
    then(connected(), [consumed_cmd(1)])
}

fn completed() -> Vec<Step> {
    then(
        consumed(),
        [RegistrationCommand::RecordCompleted {
            result: JobResult::Succeeded,
        }],
    )
}

fn deregistering() -> Vec<Step> {
    then(connected(), [for_retirement()])
}

fn refused_busy() -> Vec<Step> {
    then(deregistering(), [answer(DeregistrationAnswer::Busy)])
}

fn deregistered() -> Vec<Step> {
    then(deregistering(), [answer(DeregistrationAnswer::Removed)])
}

fn leaked() -> Vec<Step> {
    then(
        delivered(),
        [RegistrationCommand::PresumeLeaked {
            reason: LeakReason::NotSeenOnline,
        }],
    )
}

fn at_epoch_two() -> Vec<Step> {
    let mut path = issued();
    path.push(at(2, RegistrationCommand::RecordDelivered));
    path
}

/// Folds `path` through `decide` and `evolve`; the first refused step, if any.
fn replay_path(
    path: Vec<Step>,
) -> (
    Vec<RegistrationEvent>,
    Option<(usize, RegistrationRejection)>,
) {
    let mut state = RegistrationState::initial();
    let mut history = Vec::new();
    for (index, step) in path.into_iter().enumerate() {
        match decide(&state, step) {
            Ok(events) => {
                for event in &events {
                    state = evolve(state, event);
                }
                history.extend(events);
            }
            Err(rejection) => return (history, Some((index, rejection))),
        }
    }
    (history, None)
}

fn case(
    name: &'static str,
    path: Vec<Step>,
    command: Step,
    expect: Expect,
) -> DeterminismCase<RunnerRegistration> {
    DeterminismCase {
        name,
        history: replay_path(path).0,
        command,
        expect,
    }
}

fn accepted(
    name: &'static str,
    path: Vec<Step>,
    body: RegistrationCommand,
) -> DeterminismCase<RunnerRegistration> {
    case(name, path, auto(body), Expect::Accepted)
}

fn refused(
    name: &'static str,
    path: Vec<Step>,
    body: RegistrationCommand,
) -> DeterminismCase<RunnerRegistration> {
    case(name, path, auto(body), Expect::Refused)
}

/// Every path a case starts from, for the replay check.
#[cfg(test)]
fn paths() -> Vec<Vec<Step>> {
    vec![
        requested(),
        issue_unknown(),
        issued(),
        delivered(),
        connected(),
        consumed(),
        completed(),
        deregistering(),
        refused_busy(),
        deregistered(),
        leaked(),
        at_epoch_two(),
    ]
}

pub(super) fn cases() -> Vec<DeterminismCase<RunnerRegistration>> {
    vec![
        // accepted
        accepted("request_registration", Vec::new(), request()),
        accepted("record_issued", requested(), issued_cmd()),
        accepted(
            "record_issue_unknown",
            requested(),
            RegistrationCommand::RecordIssueUnknown,
        ),
        accepted(
            "mint_refused_is_absent",
            requested(),
            RegistrationCommand::RecordAbsent,
        ),
        accepted(
            "reaper_finds_by_name",
            issue_unknown(),
            RegistrationCommand::RecordFoundByName { runner_id: RUNNER },
        ),
        accepted(
            "reaper_finds_nothing",
            issue_unknown(),
            RegistrationCommand::RecordAbsent,
        ),
        accepted(
            "record_delivered",
            issued(),
            RegistrationCommand::RecordDelivered,
        ),
        accepted("confirm_online", delivered(), connected_cmd(RUNNER)),
        accepted("record_consumed", connected(), consumed_cmd(1)),
        accepted("same_job_records_nothing", consumed(), consumed_cmd(1)),
        accepted(
            "record_completed",
            consumed(),
            RegistrationCommand::RecordCompleted {
                result: JobResult::Failed,
            },
        ),
        accepted(
            "not_seen_online_presumed_leaked",
            delivered(),
            RegistrationCommand::PresumeLeaked {
                reason: LeakReason::NotSeenOnline,
            },
        ),
        accepted("deregister_for_retirement", connected(), for_retirement()),
        accepted(
            "deregister_leaked",
            leaked(),
            RegistrationCommand::RequestDeregistration {
                reason: DeregistrationReason::PresumedLeaked,
            },
        ),
        accepted(
            "pending_deregistration_records_nothing",
            deregistering(),
            for_retirement(),
        ),
        accepted(
            "removed",
            deregistering(),
            answer(DeregistrationAnswer::Removed),
        ),
        accepted(
            "not_found",
            deregistering(),
            answer(DeregistrationAnswer::NotFound),
        ),
        accepted(
            "busy_withdraws_the_retirement",
            deregistering(),
            answer(DeregistrationAnswer::Busy),
        ),
        accepted(
            "deregister_after_the_job",
            then(refused_busy(), [consumed_cmd(1)]),
            RegistrationCommand::RequestDeregistration {
                reason: DeregistrationReason::Retirement {
                    retirement: EventId::from_u128(0xE2),
                },
            },
        ),
        case(
            "newer_epoch_is_recorded",
            issued(),
            at(2, RegistrationCommand::RecordDelivered),
            Expect::Accepted,
        ),
        // refused, one per code
        refused("stale_run", at_epoch_two(), connected_cmd(RUNNER)),
        refused(
            "scale_set_degraded",
            Vec::new(),
            request_as(attempt(), SessionHealth::Degraded),
        ),
        refused(
            "runner_name_mismatch",
            Vec::new(),
            request_as(
                AttemptId::from_u128(0xBEEF_0000_0000),
                SessionHealth::Healthy,
            ),
        ),
        refused("already_requested", issue_unknown(), request()),
        refused("not_requested", Vec::new(), issued_cmd()),
        refused(
            "terminal",
            deregistered(),
            answer(DeregistrationAnswer::NotFound),
        ),
        refused("no_issue_pending", issue_unknown(), issued_cmd()),
        refused(
            "issue_not_unknown",
            issued(),
            RegistrationCommand::RecordFoundByName { runner_id: RUNNER },
        ),
        refused(
            "not_issued",
            issue_unknown(),
            RegistrationCommand::RecordDelivered,
        ),
        refused("not_delivered", issued(), connected_cmd(RUNNER)),
        refused(
            "runner_id_mismatch",
            delivered(),
            connected_cmd(RunnerId(1)),
        ),
        refused("no_runner_id", issue_unknown(), for_retirement()),
        refused("already_consumed", completed(), consumed_cmd(2)),
        refused(
            "not_consumed",
            connected(),
            RegistrationCommand::RecordCompleted {
                result: JobResult::Succeeded,
            },
        ),
        refused("busy_refused", refused_busy(), for_retirement()),
        refused(
            "presumed_leaked",
            then(
                issued(),
                [RegistrationCommand::PresumeLeaked {
                    reason: LeakReason::LaunchUnknown,
                }],
            ),
            RegistrationCommand::RecordDelivered,
        ),
        refused(
            "no_deregistration_pending",
            connected(),
            answer(DeregistrationAnswer::Removed),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_accept_every_step_when_a_case_path_is_replayed() {
        // given
        let paths = paths();
        // when
        let refusals: Vec<(usize, Option<(usize, RegistrationRejection)>)> = paths
            .into_iter()
            .map(|path| replay_path(path).1)
            .enumerate()
            .filter(|(_, refusal)| refusal.is_some())
            .collect();
        // then
        assert!(refusals.is_empty(), "{refusals:?}");
    }
}
