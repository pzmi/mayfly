//! The image's status over random command sequences: it never moves back, and a withdrawn image
//! is never verified (the KER-11 acceptance; `Image` is not modelled in `formal/`, so these
//! properties are its check over long histories, the matrix its check over every pair).

use mayfly_control_domain::image::{ImageEvent, ImageStatus};
use proptest::prelude::*;

use super::support::{apply, matrix_states, stimuli};

/// Indices into [`stimuli`], and a starting state from [`matrix_states`].
fn histories() -> impl Strategy<Value = (usize, Vec<usize>)> {
    (
        0..matrix_states().len(),
        proptest::collection::vec(0..stimuli().len(), 0..32),
    )
}

proptest! {
    #[test]
    fn should_never_move_the_status_back_when_any_commands_are_decided(
        (start, picks) in histories(),
    ) {
        // given
        let stimuli = stimuli();
        let mut state = matrix_states().swap_remove(start).1;
        // when
        for pick in picks {
            let step = stimuli[pick].1.clone();
            if let Ok((after, _)) = apply(&state, step) {
                // then
                prop_assert!(after.status.rank() >= state.status.rank(), "{:?} -> {:?}", state.status, after.status);
                state = after;
            }
        }
    }

    #[test]
    fn should_never_verify_a_withdrawn_image_when_any_commands_are_decided(
        (start, picks) in histories(),
    ) {
        // given
        let stimuli = stimuli();
        let mut state = matrix_states().swap_remove(start).1;
        // when
        for pick in picks {
            let step = stimuli[pick].1.clone();
            let withdrawn = state.status == ImageStatus::Withdrawn;
            let rejected = state.status == ImageStatus::Rejected;
            if let Ok((after, events)) = apply(&state, step) {
                // then
                let verified = events.iter().any(|e| matches!(e, ImageEvent::ImageVerified(_)));
                prop_assert!(!(verified && (withdrawn || rejected)));
                if withdrawn {
                    prop_assert_eq!(after.status, ImageStatus::Withdrawn);
                }
                state = after;
            }
        }
    }
}
