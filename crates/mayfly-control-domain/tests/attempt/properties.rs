//! The invariants of `formal/lifecycle.qnt`, asserted on the Rust aggregate over every
//! accepted transition two stimuli deep from every matrix state: a check of the code, where the
//! model's invariants check the model.

use mayfly_control_domain::attempt::{
    AttemptCommand, AttemptEvent, AttemptState, DirectiveKind, DirectiveTag, Jit, Outcome, Phase,
};

use super::support::{apply, matrix_states, stimuli};

/// Why the transition `before --stimulus--> after` breaks an invariant, if it does.
fn broken(
    before: &AttemptState,
    stimulus: &AttemptCommand,
    events: &[AttemptEvent],
    after: &AttemptState,
) -> Option<&'static str> {
    let was_unknown = matches!(
        before.pending.map(|p| p.outcome),
        Some(Outcome::Unknown { .. })
    );
    let is_unknown = matches!(
        after.pending.map(|p| p.outcome),
        Some(Outcome::Unknown { .. })
    );
    let observed = matches!(stimulus, AttemptCommand::RecordObservation(..));
    let issued: Vec<&DirectiveKind> = events
        .iter()
        .filter_map(|e| match e {
            AttemptEvent::DirectiveIssued(issued) => Some(&issued.kind),
            _ => None,
        })
        .collect();
    let minted = events
        .iter()
        .any(|e| matches!(e, AttemptEvent::JitHandleIssued(..)));
    let bound = events
        .iter()
        .any(|e| matches!(e, AttemptEvent::JobBound(..)));
    let served = matches!(
        before.phase,
        Phase::Listening | Phase::Busy | Phase::Finished
    );
    if was_unknown && !is_unknown && !observed {
        return Some("I10: Unknown left without an observation");
    }
    if !issued.is_empty() && before.pending.is_some() {
        return Some("I13: a directive issued while another is pending");
    }
    if minted && !(before.grant_ref.is_some() && before.fresh_verification) {
        return Some("I22: a JIT handle without a grant and a fresh verification");
    }
    if issued
        .iter()
        .any(|k| matches!(k, DirectiveKind::LaunchRunner { .. }))
        && !matches!(before.jit, Jit::Issued(..))
    {
        return Some("I22: a launch of a handle that is not freshly issued");
    }
    if bound && !matches!(before.jit, Jit::Taken(..)) {
        return Some("I22: a job bound to a runner whose JIT is presumed leaked");
    }
    if bound && before.job_binding.is_some() {
        return Some("I14: a second job");
    }
    if served && after.phase == Phase::Prepared {
        return Some("busyNeverWarm: back to Prepared");
    }
    if before.identity.is_some() && after.identity != before.identity {
        return Some("retryKeepsNameAndGeneration: the identity changed");
    }
    // I21 (KER-04G): a runner of the registration may be registered in the VM (a launch was
    // answered or is of unknown outcome) and was not seen exiting: retiring the running VM
    // (which commits to its stop) waits for the deregistration's `ConfirmRetirement`
    let retired = events
        .iter()
        .any(|e| matches!(e, AttemptEvent::AttemptRetired(..)));
    let runner_may_live = matches!(before.jit, Jit::Taken(..))
        || (matches!(before.jit, Jit::Orphaned(..))
            && !matches!(
                before.pending.map(|p| p.tag),
                Some(DirectiveTag::LaunchRunner)
            ));
    if retired
        && runner_may_live
        && !before.runner_exited
        && after.vm_running
        && !matches!(before.phase, Phase::Quarantined { .. })
        && !matches!(stimulus, AttemptCommand::ConfirmRetirement)
    {
        return Some("I21: retired before the deregistration answered");
    }
    if matches!(before.phase, Phase::Quarantined { .. })
        && issued
            .iter()
            .any(|k| matches!(k, DirectiveKind::DeleteVm { .. }))
    {
        return Some("quarantineNeverAutoDeleted: a delete while quarantined");
    }
    None
}

#[test]
fn should_keep_the_lifecycle_invariants_when_any_two_stimuli_are_accepted() {
    // given
    let starts = matrix_states();
    let mut checked = 0;
    let mut failures = Vec::new();
    // when
    for (name, start) in &starts {
        for (first, step) in stimuli(start) {
            let body = step.body.clone();
            let Ok((middle, events)) = apply(start, step) else {
                continue;
            };
            checked += 1;
            if let Some(why) = broken(start, &body, &events, &middle) {
                failures.push(format!("{name} x {first}: {why}"));
            }
            for (second, step) in stimuli(&middle) {
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
    assert!(checked > 1_000, "{checked}");
    assert!(failures.is_empty(), "{failures:#?}");
}
