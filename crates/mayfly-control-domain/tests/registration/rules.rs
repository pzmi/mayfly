//! The rules of `doc/domain-model.md` §6.5 and P10–P14, each a test: the name issued once, the
//! unknown issue left only on the reaper's observation, deregister before stop and the busy
//! race (I21, mutant 21), journaled ids only (I19), the online confirmation (ADR 0038), the
//! leak path (P13) and the epoch fence (I18).

use mayfly_control_domain::registration::{
    DeregistrationAnswer, DeregistrationReason, LeakReason, Phase, RegistrationCommand,
    RegistrationEvent, RegistrationState, RetirementStep, RunnerId, RunnerRegistration,
    retirement_step,
};
use mayfly_control_domain::{Aggregate, EventSet};

use super::support::{
    RUNNER, answer, apply, at, auto, completed, connected, consumed, deregister_for, issued, leak,
    matrix_states, next_retirement, outcome, reach, request, retirement, stimuli,
};

/// The registration holds a runner id GitHub may still list, with its config possibly in the VM.
const fn may_be_registered(state: &RegistrationState) -> bool {
    let holding = match state.phase {
        Phase::Issued
        | Phase::Delivered
        | Phase::Connected
        | Phase::Consumed
        | Phase::Completed => true,
        Phase::Unrequested
        | Phase::Requested
        | Phase::IssueUnknown
        | Phase::Deregistered
        | Phase::Absent => false,
    };
    holding && state.runner_id.is_some()
}

fn names_issued(events: &[RegistrationEvent]) -> usize {
    events
        .iter()
        .filter(|e| e.kind().event_type == "registration_requested")
        .count()
}

fn connected_path() -> Vec<RegistrationCommand> {
    vec![
        request(),
        issued(),
        RegistrationCommand::RecordDelivered,
        connected(RUNNER),
    ]
}

fn after(more: impl IntoIterator<Item = RegistrationCommand>) -> RegistrationState {
    let mut path = connected_path();
    path.extend(more);
    reach(path)
}

#[test]
fn should_never_issue_a_runner_name_twice_when_any_command_follows() {
    // given
    let states = matrix_states();
    let mut requested_again = Vec::new();
    // when
    for (name, start) in &states {
        for (first, step) in stimuli() {
            let Ok((middle, events)) = apply(start, step) else {
                continue;
            };
            let first_names = names_issued(&events);
            if start.phase != Phase::Unrequested && first_names > 0 {
                requested_again.push(format!("{name} x {first}"));
            }
            for (second, step) in stimuli() {
                let Ok((_, more)) = apply(&middle, step) else {
                    continue;
                };
                if first_names + names_issued(&more) > 1 {
                    requested_again.push(format!("{name} x {first} x {second}"));
                }
            }
        }
    }
    // then
    assert!(requested_again.is_empty(), "{requested_again:#?}");
    for (name, state) in states.iter().filter(|(_, s)| s.phase != Phase::Unrequested) {
        assert_eq!(
            outcome(state, auto(request())),
            "already_requested",
            "{name}"
        );
    }
}

#[test]
fn should_refuse_a_runner_name_when_it_is_not_the_attempts_vm_name() {
    // given
    let state = RunnerRegistration::initial();
    let (_, other) = stimuli()
        .into_iter()
        .find(|(name, _)| *name == "request_other_name")
        .unwrap_or_else(|| panic!("the stimulus exists"));
    // when
    let refusal = outcome(&state, other);
    // then
    assert_eq!(refusal, "runner_name_mismatch");
}

#[test]
fn should_refuse_a_registration_when_the_scale_set_is_degraded() {
    // given
    let state = RunnerRegistration::initial();
    let (_, degraded) = stimuli()
        .into_iter()
        .find(|(name, _)| *name == "request_degraded")
        .unwrap_or_else(|| panic!("the stimulus exists"));
    // when
    let refusal = outcome(&state, degraded);
    // then
    assert_eq!(refusal, "scale_set_degraded");
}

#[test]
fn should_leave_an_unknown_issue_only_on_the_reapers_observation_when_every_command_is_tried() {
    // given
    let unknown = reach([request(), RegistrationCommand::RecordIssueUnknown]);
    let mut left = Vec::new();
    // when
    for (name, step) in stimuli() {
        let reaper = matches!(
            step.body,
            RegistrationCommand::RecordFoundByName { .. } | RegistrationCommand::RecordAbsent
        );
        if let Ok((next, _)) = apply(&unknown, step)
            && next.phase != Phase::IssueUnknown
        {
            left.push((name, reaper, next.phase));
        }
    }
    // then
    assert_eq!(
        left,
        vec![
            ("found_by_name", true, Phase::Issued),
            ("absent", true, Phase::Absent),
        ]
    );
    assert_eq!(outcome(&unknown, auto(issued())), "no_issue_pending");
}

#[test]
fn should_presume_leaked_and_journal_the_id_when_the_reaper_finds_the_name() {
    // given
    let unknown = reach([request(), RegistrationCommand::RecordIssueUnknown]);
    // when
    let found = apply(
        &unknown,
        auto(RegistrationCommand::RecordFoundByName { runner_id: RUNNER }),
    )
    .map(|(state, _)| state);
    // then
    let found = found.unwrap_or_else(|r| panic!("refused: {r:?}"));
    assert_eq!(found.runner_id, Some(RUNNER));
    assert_eq!(found.leaked, Some(LeakReason::IssueUnknown));
    assert_eq!(
        retirement_step(&found, retirement()),
        RetirementStep::Deregister
    );
}

#[test]
fn should_never_confirm_a_retirement_when_a_runner_may_still_be_registered() {
    // given
    let states = matrix_states();
    // when
    let confirmed: Vec<&str> = states
        .iter()
        .filter(|(_, state)| may_be_registered(state))
        .filter(|(_, state)| retirement_step(state, retirement()) == RetirementStep::Confirm)
        .map(|(name, _)| *name)
        .collect();
    // then
    assert!(
        confirmed.is_empty(),
        "stopped before deregistering: {confirmed:?}"
    );
    assert_eq!(
        retirement_step(&after([]), retirement()),
        RetirementStep::Deregister
    );
    assert_eq!(
        retirement_step(&after([deregister_for(retirement())]), retirement()),
        RetirementStep::Wait
    );
    assert_eq!(
        retirement_step(
            &after([
                deregister_for(retirement()),
                answer(DeregistrationAnswer::Removed)
            ]),
            retirement()
        ),
        RetirementStep::Confirm
    );
}

#[test]
fn should_confirm_when_no_runner_can_be_in_the_vm() {
    // given
    let unrequested = RunnerRegistration::initial();
    let requested = reach([request()]);
    let unknown = reach([request(), RegistrationCommand::RecordIssueUnknown]);
    let absent = reach([request(), RegistrationCommand::RecordAbsent]);
    // when
    let steps = [
        retirement_step(&unrequested, retirement()),
        retirement_step(&requested, retirement()),
        retirement_step(&unknown, retirement()),
        retirement_step(&absent, retirement()),
    ];
    // then
    assert_eq!(
        steps,
        [
            RetirementStep::Confirm,
            RetirementStep::Wait,
            RetirementStep::Confirm,
            RetirementStep::Confirm,
        ]
    );
}

#[test]
fn should_withdraw_the_retirement_when_github_answered_busy_for_it() {
    // given
    let busy = after([
        deregister_for(retirement()),
        consumed(1),
        answer(DeregistrationAnswer::Busy),
    ]);
    // when
    let finished = apply(&busy, auto(completed())).map(|(state, _)| state);
    // then
    let finished = finished.unwrap_or_else(|r| panic!("refused: {r:?}"));
    assert_eq!(
        retirement_step(&busy, retirement()),
        RetirementStep::Withdraw
    );
    assert_eq!(
        retirement_step(&finished, retirement()),
        RetirementStep::Withdraw,
        "a job finishing after the busy answer does not revive that retirement"
    );
    assert_eq!(
        retirement_step(&finished, next_retirement()),
        RetirementStep::Deregister
    );
}

#[test]
fn should_refuse_the_same_retirement_when_github_answered_busy() {
    // given
    let busy = after([
        deregister_for(retirement()),
        answer(DeregistrationAnswer::Busy),
    ]);
    // when
    let same = outcome(&busy, auto(deregister_for(retirement())));
    let next = outcome(&busy, auto(deregister_for(next_retirement())));
    let leaked = outcome(
        &busy,
        auto(RegistrationCommand::RequestDeregistration {
            reason: DeregistrationReason::PresumedLeaked,
        }),
    );
    // then
    assert_eq!(same, "busy_refused");
    assert_eq!(next, "accepted");
    assert_eq!(leaked, "accepted");
    assert_eq!(busy.deregistration, None);
}

#[test]
fn should_deregister_and_confirm_when_the_busy_race_ends_with_the_job() {
    // given
    let state = after([
        deregister_for(retirement()),
        consumed(1),
        answer(DeregistrationAnswer::Busy),
        completed(),
        deregister_for(next_retirement()),
    ]);
    // when
    let done = apply(&state, auto(answer(DeregistrationAnswer::NotFound))).map(|(s, _)| s);
    // then
    let done = done.unwrap_or_else(|r| panic!("refused: {r:?}"));
    assert_eq!(done.phase, Phase::Deregistered);
    assert_eq!(
        retirement_step(&done, next_retirement()),
        RetirementStep::Confirm
    );
}

#[test]
fn should_refuse_a_deregistration_when_no_runner_id_is_journaled() {
    // given
    let without_id = [
        reach([request()]),
        reach([request(), RegistrationCommand::RecordIssueUnknown]),
    ];
    let reasons = [
        DeregistrationReason::Retirement {
            retirement: retirement(),
        },
        DeregistrationReason::PresumedLeaked,
        DeregistrationReason::NotLive,
    ];
    // when
    let outcomes: Vec<String> = without_id
        .iter()
        .flat_map(|state| {
            reasons.iter().map(|reason| {
                outcome(
                    state,
                    auto(RegistrationCommand::RequestDeregistration { reason: *reason }),
                )
            })
        })
        .collect();
    // then
    assert!(outcomes.iter().all(|o| o == "no_runner_id"), "{outcomes:?}");
}

#[test]
fn should_confirm_only_the_issued_runner_id_when_a_runner_is_seen_online() {
    // given
    let delivered = reach([request(), issued(), RegistrationCommand::RecordDelivered]);
    let issued_only = reach([request(), issued()]);
    // when
    let other = outcome(&delivered, auto(connected(RunnerId(1))));
    let before_delivery = outcome(&issued_only, auto(connected(RUNNER)));
    let confirmed = apply(&delivered, auto(connected(RUNNER))).map(|(state, _)| state.phase);
    // then
    assert_eq!(other, "runner_id_mismatch");
    assert_eq!(before_delivery, "not_delivered");
    assert_eq!(confirmed, Ok(Phase::Connected));
}

#[test]
fn should_stay_leaked_and_deregister_when_a_delivery_is_presumed_leaked() {
    // given
    let delivered = reach([request(), issued(), RegistrationCommand::RecordDelivered]);
    // when
    let leaked = apply(&delivered, auto(leak(LeakReason::NotSeenOnline)))
        .and_then(|(state, _)| apply(&state, auto(leak(LeakReason::IdentityDoubt))))
        .map(|(state, events)| (state, events.len()));
    // then
    let (leaked, again) = leaked.unwrap_or_else(|r| panic!("refused: {r:?}"));
    assert_eq!(leaked.leaked, Some(LeakReason::NotSeenOnline));
    assert_eq!(again, 0, "presumed leaked once");
    assert_eq!(
        outcome(
            &leaked,
            auto(RegistrationCommand::RequestDeregistration {
                reason: DeregistrationReason::PresumedLeaked
            })
        ),
        "accepted"
    );
    assert_eq!(
        outcome(&reach([request()]), auto(leak(LeakReason::LaunchUnknown))),
        "not_issued"
    );
}

#[test]
fn should_refuse_a_delivery_when_the_registration_is_presumed_leaked() {
    // given
    let launch_unknown = reach([request(), issued(), leak(LeakReason::LaunchUnknown)]);
    let found = reach([
        request(),
        RegistrationCommand::RecordIssueUnknown,
        RegistrationCommand::RecordFoundByName { runner_id: RUNNER },
    ]);
    // when
    let outcomes = [
        outcome(&launch_unknown, auto(RegistrationCommand::RecordDelivered)),
        outcome(&found, auto(RegistrationCommand::RecordDelivered)),
    ];
    // then
    assert_eq!(outcomes, ["presumed_leaked", "presumed_leaked"]);
}

#[test]
fn should_bind_one_job_when_a_second_is_reported() {
    // given
    let consumed_once = after([consumed(1)]);
    // when
    let second = outcome(&consumed_once, auto(consumed(2)));
    let same = apply(&consumed_once, auto(consumed(1))).map(|(_, events)| events.len());
    // then
    assert_eq!(second, "already_consumed");
    assert_eq!(same, Ok(0));
}

#[test]
fn should_refuse_an_older_run_epoch_when_a_newer_one_was_accepted() {
    // given
    let state = reach([request(), issued()]);
    let (newer, events) = apply(&state, at(2, RegistrationCommand::RecordDelivered))
        .unwrap_or_else(|r| panic!("refused: {r:?}"));
    // when
    let older = outcome(&newer, at(1, connected(RUNNER)));
    // then
    assert_eq!(events[0].kind().event_type, "registration_epoch_advanced");
    assert_eq!(newer.epoch.get(), 2);
    assert_eq!(older, "stale_run");
}

#[test]
fn should_refuse_everything_when_the_registration_is_terminal() {
    // given
    let deregistered = after([
        deregister_for(retirement()),
        answer(DeregistrationAnswer::Removed),
    ]);
    let absent = reach([request(), RegistrationCommand::RecordAbsent]);
    // when
    let accepted: Vec<String> = [("deregistered", &deregistered), ("absent", &absent)]
        .iter()
        .flat_map(|(name, state)| {
            stimuli()
                .into_iter()
                .filter(|(_, step)| RunnerRegistration::decide(state, step.clone()).is_ok())
                .map(move |(stimulus, _)| format!("{name} x {stimulus}"))
        })
        .collect();
    // then
    assert!(accepted.is_empty(), "{accepted:?}");
}
