//! I28 on the Rust `QuarantineCase` over random command sequences: no case is resolved, and
//! nothing but the escalation level changes, unless an operator commands it.

use mayfly_control_domain::Actor;
use mayfly_control_domain::attempt::QuarantineReason;
use mayfly_control_domain::operations::quarantine::{
    CasePhase, Disposition, EscalationLevel, QuarantineCaseCommand, QuarantineCaseEvent,
    QuarantineCaseState,
};
use proptest::prelude::*;

use super::support::{apply, at, escalate, open, open_with, resolve};

fn body() -> impl Strategy<Value = QuarantineCaseCommand> {
    let disposition = prop_oneof![
        Just(Disposition::DeleteWithProof),
        Just(Disposition::ReleasedObservedAbsent),
        Just(Disposition::Keep),
    ];
    let level = prop_oneof![Just(EscalationLevel::Ticket), Just(EscalationLevel::Page)];
    prop_oneof![
        1 => Just(open()),
        1 => Just(open_with(QuarantineReason::OwnershipDoubt, false)),
        2 => Just(QuarantineCaseCommand::InspectCase),
        3 => level.prop_map(escalate),
        4 => (disposition, any::<bool>()).prop_map(|(d, a)| resolve(d, a)),
    ]
}

fn non_operator() -> impl Strategy<Value = Actor> {
    prop_oneof![
        Just(Actor::Automation {
            policy: "quarantine"
        }),
        Just(Actor::Automation { policy: "repair" }),
        Just(Actor::Startup),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// No timer, cap, policy or the startup resolves or inspects a case; escalation only
    /// raises its level.
    #[test]
    fn should_never_resolve_or_act_when_no_operator_commands(
        steps in proptest::collection::vec((body(), non_operator()), 1..40)
    ) {
        // given
        let mut state = QuarantineCaseState::initial();
        // when
        for (body, actor) in steps {
            let before = state.clone();
            if let Ok((after, events)) = apply(&state, at(1, actor, body)) {
                // then
                for event in &events {
                    match event {
                        QuarantineCaseEvent::QuarantineCaseResolved(_)
                        | QuarantineCaseEvent::QuarantineCaseInspected(_) => {
                            prop_assert!(false, "{event:?} without an operator");
                        }
                        QuarantineCaseEvent::QuarantineCaseEscalated(_) => {
                            prop_assert_eq!(after.phase, before.phase);
                            prop_assert_eq!(&after.case, &before.case);
                        }
                        QuarantineCaseEvent::QuarantineCaseOpened(_)
                        | QuarantineCaseEvent::QuarantineEpochAdvanced(_) => {}
                    }
                }
                let resolved = match after.phase {
                    CasePhase::Resolved(_) => true,
                    CasePhase::NotOpened | CasePhase::Open | CasePhase::Inspected => false,
                };
                prop_assert!(!resolved, "resolved without an operator");
                state = after;
            }
        }
    }
}
