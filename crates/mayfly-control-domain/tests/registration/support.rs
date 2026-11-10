//! Paths through a registration's life, driven through its public `decide` and `evolve`.

use mayfly_control_domain::attempt::RunnerRequestId;
use mayfly_control_domain::registration::{
    DeregistrationAnswer, DeregistrationReason, JobResult, LeakReason, RegistrationCommand,
    RegistrationEvent, RegistrationRejection, RegistrationState, RequestRegistration, RunnerId,
    RunnerRegistration, ScaleSetId, SessionHealth,
};
use mayfly_control_domain::{Actor, Aggregate, Command, Rejection};
use mayfly_domain::{
    AttemptId, CommandId, Epoch, EventId, Generation, HostId, Millis, OwnedVmName, PoolName,
    Sample, Slot,
};

pub type Step = Command<RegistrationCommand>;

pub const POLICY: Actor = Actor::Automation {
    policy: "registration_manager",
};

/// The runner id GitHub issues in these paths.
pub const RUNNER: RunnerId = RunnerId(4_242);

pub fn at(run_epoch: u64, body: RegistrationCommand) -> Step {
    Command::new(
        CommandId::from_u128(0xC1),
        Millis::new(1_000),
        Epoch::new(run_epoch),
        POLICY,
        body,
    )
}

pub fn auto(body: RegistrationCommand) -> Step {
    at(1, body)
}

pub fn attempt() -> AttemptId {
    AttemptId::sample()
}

/// The attempt's VM name: the runner name.
pub fn vm_name(attempt: AttemptId) -> OwnedVmName {
    OwnedVmName::new(
        PoolName::sample(),
        HostId::sample(),
        Slot::new(0),
        Generation::FIRST,
        attempt,
    )
}

/// The attempt's first `RetirementRequested` event.
pub fn retirement() -> EventId {
    EventId::from_u128(0xE1)
}

/// A later retirement of the same attempt.
pub fn next_retirement() -> EventId {
    EventId::from_u128(0xE2)
}

pub fn request() -> RegistrationCommand {
    RegistrationCommand::RequestRegistration(RequestRegistration {
        attempt_id: attempt(),
        runner_name: vm_name(attempt()),
        scale_set: ScaleSetId(7),
        session: SessionHealth::Healthy,
    })
}

pub const fn issued() -> RegistrationCommand {
    RegistrationCommand::RecordIssued { runner_id: RUNNER }
}

pub const fn connected(runner_id: RunnerId) -> RegistrationCommand {
    RegistrationCommand::RecordConnected { runner_id }
}

pub const fn consumed(job: u64) -> RegistrationCommand {
    RegistrationCommand::RecordConsumed {
        request_id: RunnerRequestId(job),
    }
}

pub const fn completed() -> RegistrationCommand {
    RegistrationCommand::RecordCompleted {
        result: JobResult::Succeeded,
    }
}

pub const fn leak(reason: LeakReason) -> RegistrationCommand {
    RegistrationCommand::PresumeLeaked { reason }
}

pub const fn deregister_for(retirement: EventId) -> RegistrationCommand {
    RegistrationCommand::RequestDeregistration {
        reason: DeregistrationReason::Retirement { retirement },
    }
}

pub const fn answer(answer: DeregistrationAnswer) -> RegistrationCommand {
    RegistrationCommand::RecordDeregistration { answer }
}

/// Decides `step` and folds what it accepted.
pub fn apply(
    state: &RegistrationState,
    step: Step,
) -> Result<(RegistrationState, Vec<RegistrationEvent>), RegistrationRejection> {
    let events = RunnerRegistration::decide(state, step)?;
    let next = events
        .iter()
        .fold(state.clone(), RunnerRegistration::evolve);
    Ok((next, events))
}

/// The state after `bodies`, every one accepted.
///
/// # Panics
/// When a step is refused: the path is wrong, not the aggregate under test.
pub fn reach(bodies: impl IntoIterator<Item = RegistrationCommand>) -> RegistrationState {
    bodies
        .into_iter()
        .enumerate()
        .fold(RunnerRegistration::initial(), |state, (index, body)| {
            apply(&state, auto(body))
                .unwrap_or_else(|r| panic!("step {index} of the path refused: {r:?}"))
                .0
        })
}

/// The refusal code of `step` on `state`, or `"accepted"`.
pub fn outcome(state: &RegistrationState, step: Step) -> String {
    match RunnerRegistration::decide(state, step) {
        Ok(_) => "accepted".to_owned(),
        Err(rejection) => rejection.code().to_owned(),
    }
}

fn path(more: &[RegistrationCommand]) -> Vec<RegistrationCommand> {
    let mut all = vec![request()];
    all.extend_from_slice(more);
    all
}

/// One state per phase, and the overlays: leaked, a deregistration pending, a busy refusal.
pub fn matrix_states() -> Vec<(&'static str, RegistrationState)> {
    let connected_path = [
        issued(),
        RegistrationCommand::RecordDelivered,
        connected(RUNNER),
    ];
    let with_connected = |more: &[RegistrationCommand]| {
        let mut all = connected_path.to_vec();
        all.extend_from_slice(more);
        reach(path(&all))
    };
    vec![
        ("unrequested", RunnerRegistration::initial()),
        ("requested", reach(path(&[]))),
        (
            "issue_unknown",
            reach(path(&[RegistrationCommand::RecordIssueUnknown])),
        ),
        ("issued", reach(path(&[issued()]))),
        (
            "found_by_name",
            reach(path(&[
                RegistrationCommand::RecordIssueUnknown,
                RegistrationCommand::RecordFoundByName { runner_id: RUNNER },
            ])),
        ),
        (
            "delivered",
            reach(path(&[issued(), RegistrationCommand::RecordDelivered])),
        ),
        ("connected", with_connected(&[])),
        ("consumed", with_connected(&[consumed(1)])),
        ("completed", with_connected(&[consumed(1), completed()])),
        ("leaked", with_connected(&[leak(LeakReason::IdentityDoubt)])),
        (
            "deregistering",
            with_connected(&[deregister_for(retirement())]),
        ),
        (
            "refused_busy",
            with_connected(&[
                deregister_for(retirement()),
                answer(DeregistrationAnswer::Busy),
            ]),
        ),
        (
            "deregistered",
            with_connected(&[
                deregister_for(retirement()),
                answer(DeregistrationAnswer::NotFound),
            ]),
        ),
        ("absent", reach(path(&[RegistrationCommand::RecordAbsent]))),
    ]
}

/// One stimulus of every command type (and the variants that decide differently).
pub fn stimuli() -> Vec<(&'static str, Step)> {
    vec![
        ("request", auto(request())),
        (
            "request_degraded",
            auto(RegistrationCommand::RequestRegistration(
                RequestRegistration {
                    attempt_id: attempt(),
                    runner_name: vm_name(attempt()),
                    scale_set: ScaleSetId(7),
                    session: SessionHealth::Degraded,
                },
            )),
        ),
        (
            "request_other_name",
            auto(RegistrationCommand::RequestRegistration(
                RequestRegistration {
                    attempt_id: attempt(),
                    runner_name: vm_name(AttemptId::from_u128(0xBEEF)),
                    scale_set: ScaleSetId(7),
                    session: SessionHealth::Healthy,
                },
            )),
        ),
        ("issued", auto(issued())),
        (
            "issue_unknown",
            auto(RegistrationCommand::RecordIssueUnknown),
        ),
        ("delivered", auto(RegistrationCommand::RecordDelivered)),
        ("connected_issued_id", auto(connected(RUNNER))),
        ("connected_other_id", auto(connected(RunnerId(1)))),
        ("consumed_job_1", auto(consumed(1))),
        ("consumed_job_2", auto(consumed(2))),
        ("completed", auto(completed())),
        ("presume_leaked", auto(leak(LeakReason::LaunchUnknown))),
        ("deregister_retirement", auto(deregister_for(retirement()))),
        (
            "deregister_next_retirement",
            auto(deregister_for(next_retirement())),
        ),
        (
            "deregister_not_live",
            auto(RegistrationCommand::RequestDeregistration {
                reason: DeregistrationReason::NotLive,
            }),
        ),
        ("removed", auto(answer(DeregistrationAnswer::Removed))),
        ("not_found", auto(answer(DeregistrationAnswer::NotFound))),
        ("busy", auto(answer(DeregistrationAnswer::Busy))),
        (
            "found_by_name",
            auto(RegistrationCommand::RecordFoundByName { runner_id: RUNNER }),
        ),
        ("absent", auto(RegistrationCommand::RecordAbsent)),
        ("stale_run", at(0, RegistrationCommand::RecordDelivered)),
    ]
}
