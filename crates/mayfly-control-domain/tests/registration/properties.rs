//! The invariants of `formal/registration.qnt`, asserted on the Rust aggregate over every
//! accepted transition two stimuli deep from every matrix state: a check of the code, where the
//! model's invariants check the model.

use mayfly_control_domain::registration::{
    Phase, RegistrationCommand, RegistrationEvent, RegistrationState, RetirementStep,
    retirement_step,
};

use super::support::{apply, matrix_states, retirement, stimuli};

/// Why the transition `before --stimulus--> after` breaks an invariant, if it does.
fn broken(
    before: &RegistrationState,
    stimulus: &RegistrationCommand,
    events: &[RegistrationEvent],
    after: &RegistrationState,
) -> Option<&'static str> {
    let reaper = matches!(
        stimulus,
        RegistrationCommand::RecordFoundByName { .. } | RegistrationCommand::RecordAbsent
    );
    let requested = events
        .iter()
        .any(|e| matches!(e, RegistrationEvent::RegistrationRequested(..)));
    let delivered = events
        .iter()
        .any(|e| matches!(e, RegistrationEvent::RegistrationDelivered(..)));
    let deregistration = events
        .iter()
        .any(|e| matches!(e, RegistrationEvent::DeregistrationRequested(..)));
    let connected = events.iter().find_map(|e| match e {
        RegistrationEvent::RegistrationConnected(c) => Some(c.runner_id),
        _ => None,
    });
    if requested && before.phase != Phase::Unrequested {
        return Some("I19: a runner name issued twice");
    }
    if before.phase == Phase::IssueUnknown && after.phase != Phase::IssueUnknown && !reaper {
        return Some("noRemintAfterUnknown: an unknown issue left without the reaper");
    }
    if delivered && before.leaked.is_some() {
        return Some("I22: a delivery once presumed leaked (deliveredWhileLeaked)");
    }
    if deregistration && before.runner_id.is_none() {
        return Some("I19: a deregistration without a journaled runner id");
    }
    if connected.is_some() && connected != before.runner_id {
        return Some("I22: an online confirmation of another runner id");
    }
    if before.leaked.is_some() && after.leaked.is_none() {
        return Some("I22: a presumed leak forgotten");
    }
    let holding =
        after.runner_id.is_some() && !matches!(after.phase, Phase::Deregistered | Phase::Absent);
    if holding && retirement_step(after, retirement()) == RetirementStep::Confirm {
        return Some("I21: a retirement confirmed while the runner may be registered");
    }
    if before.busy_refused.contains(&retirement())
        && retirement_step(after, retirement()) != RetirementStep::Withdraw
    {
        return Some("I21: a busy answer did not cancel its retirement");
    }
    None
}

#[test]
fn should_keep_the_registration_invariants_when_any_two_stimuli_are_accepted() {
    // given
    let starts = matrix_states();
    let mut checked = 0;
    let mut failures = Vec::new();
    // when
    for (name, start) in &starts {
        for (first, step) in stimuli() {
            let body = step.body.clone();
            let Ok((middle, events)) = apply(start, step) else {
                continue;
            };
            checked += 1;
            if let Some(why) = broken(start, &body, &events, &middle) {
                failures.push(format!("{name} x {first}: {why}"));
            }
            for (second, step) in stimuli() {
                let body = step.body.clone();
                let Ok((end, events)) = apply(&middle, step) else {
                    continue;
                };
                checked += 1;
                if let Some(why) = broken(&middle, &body, &events, &end) {
                    failures.push(format!("{name} x {first} x {second}: {why}"));
                }
            }
        }
    }
    // then
    assert!(checked > 300, "{checked}");
    assert!(failures.is_empty(), "{failures:#?}");
}
