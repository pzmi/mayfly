//! The placement's invariants over random command sequences from every matrix state: one
//! directive at a time (so one pull per host and digest), an eviction only from `Materialised`
//! with nothing pending and zero references, at most two distinct bases with a previous only beside a current one,
//! nothing left after `Evicted`, and an unknown outcome left only on an observation (I10).
//! `ImagePlacement` is not modelled in `formal/`; these properties are its check over long
//! histories, the matrix its check over every pair.

use mayfly_control_domain::placement::{
    PendingOutcome, PlacementCommand, PlacementEvent, PlacementPhase, PlacementState,
};
use proptest::prelude::*;

use super::support::{apply, matrix_states, stimuli};

fn histories() -> impl Strategy<Value = (usize, Vec<usize>)> {
    (
        0..matrix_states().len(),
        proptest::collection::vec(0..stimuli().len(), 0..32),
    )
}

/// Why `before --body--> after` breaks an invariant, if it does.
fn broken(
    before: &PlacementState,
    body: &PlacementCommand,
    events: &[PlacementEvent],
    after: &PlacementState,
) -> Option<&'static str> {
    let issued = events
        .iter()
        .any(|e| matches!(e, PlacementEvent::PlacementDirectiveIssued(_)));
    let eviction = events
        .iter()
        .any(|e| matches!(e, PlacementEvent::EvictionRequested(_)));
    if issued && before.pending.is_some() {
        return Some("a directive issued while one is pending");
    }
    if eviction {
        let unreferenced = matches!(
            body,
            PlacementCommand::RequestEviction { references: 0, .. }
        );
        if before.pending.is_some() {
            return Some("an eviction while a directive is pending");
        }
        if before.phase != PlacementPhase::Materialised || !unreferenced {
            return Some("an eviction not from Materialised with zero references");
        }
    }
    if after.previous.is_some() && (after.base.is_none() || after.previous == after.base) {
        return Some("a previous base without a distinct current one");
    }
    if after.phase == PlacementPhase::Evicted && (after.base.is_some() || after.previous.is_some())
    {
        return Some("a base left after Evicted");
    }
    if after.phase == PlacementPhase::Materialised && after.base.is_none() {
        return Some("Materialised without a base");
    }
    let was_unknown = before
        .pending
        .is_some_and(|p| matches!(p.outcome, PendingOutcome::Unknown { .. }));
    let observation = matches!(
        body,
        PlacementCommand::RecordPulled { .. }
            | PlacementCommand::RecordDrift { .. }
            | PlacementCommand::RecordMaterialised { .. }
            | PlacementCommand::RecordEvicted { .. }
            | PlacementCommand::RecordRefused { .. }
            | PlacementCommand::RecordNotApplied { .. }
    );
    if was_unknown && after.pending != before.pending && !observation {
        return Some("I10: an unknown outcome left without an observation");
    }
    None
}

proptest! {
    #[test]
    fn should_keep_the_placement_invariants_when_any_commands_are_decided(
        (start, picks) in histories(),
    ) {
        // given
        let stimuli = stimuli();
        let mut state = matrix_states().swap_remove(start).1;
        // when
        for pick in picks {
            let step = stimuli[pick].1.clone();
            let body = step.body.clone();
            if let Ok((after, events)) = apply(&state, step) {
                // then
                let problem = broken(&state, &body, &events, &after);
                prop_assert!(problem.is_none(), "{problem:?} after {body:?} from {state:?}");
                state = after;
            }
        }
    }
}
