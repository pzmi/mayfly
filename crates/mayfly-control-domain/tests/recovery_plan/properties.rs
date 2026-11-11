//! I31, asserted on the Rust `RecoveryPlan` over random command sequences: a check of the code,
//! where `formal/recovery.qnt`'s `i31PlanBound` checks the model.

use mayfly_control_domain::Actor;
use mayfly_control_domain::operations::plan::{
    ActionOutcome, Bindings, GithubBinding, OwnWrite, PlanPhase, RecoveryPlanCommand,
    RecoveryPlanEvent, RecoveryPlanState, stale,
};
use mayfly_domain::Epoch;
use proptest::prelude::*;

use super::support::{
    EXPIRY, LATER, NOW, OPERATOR, REPAIR, apply, begin, bound, complete, digest, foreign, plan,
    stream, when,
};

/// The bindings a command reports: the plan's own, or one of four moved worlds.
#[derive(Debug, Clone, Copy)]
enum World {
    Same,
    Heads,
    Inventory,
    Github,
    Epoch,
}

fn world(state: &RecoveryPlanState, world: World) -> Bindings {
    let drawn = state.bindings.clone().unwrap_or_else(|| bound(0));
    match world {
        World::Same => drawn,
        World::Heads => foreign(drawn, &stream(2)),
        World::Inventory => Bindings {
            inventory: digest(0x1B),
            ..drawn
        },
        World::Github => Bindings {
            github: GithubBinding::Snapshot(digest(0x6A)),
            ..drawn
        },
        World::Epoch => Bindings {
            epoch: Epoch::new(9),
            ..drawn
        },
    }
}

#[derive(Debug, Clone, Copy)]
enum Choice {
    Propose,
    Approve(bool, bool),
    Begin(u32, World),
    /// Action, applied, and which write: 0 the plan's own, 1 one that started after a foreign
    /// write, 2 on another stream, 3 none.
    Outcome(u32, bool, u8),
    Complete(World),
    Expire(bool),
}

fn choice() -> impl Strategy<Value = Choice> {
    let w = prop_oneof![
        6 => Just(World::Same),
        1 => Just(World::Heads),
        1 => Just(World::Inventory),
        1 => Just(World::Github),
        1 => Just(World::Epoch),
    ];
    prop_oneof![
        1 => Just(Choice::Propose),
        2 => (any::<bool>(), any::<bool>()).prop_map(|(op, late)| Choice::Approve(op, late)),
        4 => (0_u32..3, w.clone()).prop_map(|(i, w)| Choice::Begin(i, w)),
        4 => (0_u32..3, any::<bool>(), 0_u8..4).prop_map(|(i, ok, h)| Choice::Outcome(i, ok, h)),
        2 => w.prop_map(Choice::Complete),
        1 => any::<bool>().prop_map(Choice::Expire),
    ]
}

fn command(state: &RecoveryPlanState, choice: Choice) -> (RecoveryPlanCommand, Actor, u64) {
    match choice {
        Choice::Propose => (plan(), REPAIR, NOW),
        Choice::Approve(op, late) => (
            RecoveryPlanCommand::ApprovePlan,
            if op { OPERATOR } else { REPAIR },
            if late { LATER } else { NOW },
        ),
        Choice::Begin(i, w) => (begin(i, world(state, w)), REPAIR, NOW),
        Choice::Outcome(i, ok, h) => {
            let own = stream(u128::from(i) + 1);
            let head = state
                .bindings
                .as_ref()
                .and_then(|b| b.heads.get(&own).copied())
                .unwrap_or(0);
            let write = |stream: String, before: u64| {
                Some(OwnWrite {
                    stream,
                    before,
                    after: before + 1,
                })
            };
            let write = match h {
                0 => write(own, head),
                1 => write(own, head + 1),
                2 => write(stream(9), head),
                _ => None,
            };
            (
                RecoveryPlanCommand::RecordOutcome {
                    index: i,
                    outcome: if ok {
                        ActionOutcome::Applied
                    } else {
                        ActionOutcome::Failed
                    },
                    write,
                },
                REPAIR,
                NOW,
            )
        }
        Choice::Complete(w) => (complete(world(state, w)), REPAIR, NOW),
        Choice::Expire(late) => (
            RecoveryPlanCommand::ExpirePlan,
            REPAIR,
            if late { LATER } else { EXPIRY - 1 },
        ),
    }
}

const fn closed(phase: PlanPhase) -> bool {
    match phase {
        PlanPhase::Completed | PlanPhase::Refused(_) | PlanPhase::Expired => true,
        PlanPhase::Unproposed
        | PlanPhase::Proposed
        | PlanPhase::Approved
        | PlanPhase::Applying
        | PlanPhase::Failed => false,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// I31, F45: an intent and a completion are recorded only while every binding holds; a
    /// moved one refuses the plan instead.
    #[test]
    fn should_record_no_intent_when_a_binding_moved(
        choices in proptest::collection::vec(choice(), 1..40)
    ) {
        // given
        let mut state = RecoveryPlanState::initial();
        // when
        for choice in choices {
            let (body, actor, ms) = command(&state, choice);
            let current = match &body {
                RecoveryPlanCommand::BeginAction { current, .. }
                | RecoveryPlanCommand::CompletePlan { current } => Some(current.clone()),
                RecoveryPlanCommand::ProposePlan { .. }
                | RecoveryPlanCommand::ApprovePlan
                | RecoveryPlanCommand::RecordOutcome { .. }
                | RecoveryPlanCommand::ExpirePlan => None,
            };
            let drawn = state.bindings.clone();
            let write = match &body {
                RecoveryPlanCommand::RecordOutcome { write, .. } => write.clone(),
                RecoveryPlanCommand::ProposePlan { .. }
                | RecoveryPlanCommand::ApprovePlan
                | RecoveryPlanCommand::BeginAction { .. }
                | RecoveryPlanCommand::CompletePlan { .. }
                | RecoveryPlanCommand::ExpirePlan => None,
            };
            let fresh = current
                .as_ref()
                .zip(drawn.as_ref())
                .map(|(now, d)| stale(d, now).is_none());
            if let Ok((after, events)) = apply(&state, when(1, actor, ms, body)) {
                // then: a bound head moves only by the plan's own write, from where it was bound
                if let (Some(before), Some(now)) = (&drawn, &after.bindings) {
                    for (s, head) in &now.heads {
                        let was = before.heads.get(s).copied();
                        let own = write
                            .as_ref()
                            .is_some_and(|w| &w.stream == s && Some(w.before) == was && w.after == *head);
                        prop_assert!(Some(*head) == was || own, "{s} moved to {head} from {was:?}");
                    }
                }
                for event in &events {
                    match event {
                        RecoveryPlanEvent::RecoveryActionStarted(_)
                        | RecoveryPlanEvent::RecoveryPlanCompleted(_) => {
                            prop_assert_eq!(fresh, Some(true), "acted on a moved world");
                        }
                        RecoveryPlanEvent::RecoveryPlanRefused(_) => {
                            let foreign_write = write.as_ref().is_some_and(|w| {
                                drawn.as_ref().and_then(|d| d.heads.get(&w.stream).copied())
                                    != Some(w.before)
                            });
                            prop_assert!(fresh != Some(true) || foreign_write, "refused a bound plan");
                        }
                        RecoveryPlanEvent::RecoveryPlanEpochAdvanced(_)
                        | RecoveryPlanEvent::RecoveryPlanProposed(_)
                        | RecoveryPlanEvent::RecoveryPlanDrawn(_)
                        | RecoveryPlanEvent::RecoveryPlanApproved(_)
                        | RecoveryPlanEvent::RecoveryActionApplied(_)
                        | RecoveryPlanEvent::RecoveryActionRecorded(_)
                        | RecoveryPlanEvent::RecoveryPlanExpired(_) => {}
                    }
                }
                state = after;
            }
        }
    }

    /// I31: each outcome follows its own intent, actions run in order and after one operator
    /// approval, and a closed plan records nothing more.
    #[test]
    fn should_record_each_outcome_after_its_intent_when_commands_are_random(
        choices in proptest::collection::vec(choice(), 1..40)
    ) {
        // given
        let mut state = RecoveryPlanState::initial();
        let mut open: Option<u32> = None;
        let mut applied: u32 = 0;
        let mut approvals = 0;
        // when
        for choice in choices {
            let (body, actor, ms) = command(&state, choice);
            let was_closed = closed(state.phase);
            if let Ok((after, events)) = apply(&state, when(1, actor, ms, body)) {
                // then
                prop_assert!(!was_closed || events.is_empty(), "a closed plan recorded {events:?}");
                for event in &events {
                    match event {
                        RecoveryPlanEvent::RecoveryPlanApproved(_) => {
                            approvals += 1;
                            let by_operator = matches!(actor, Actor::Operator { .. });
                            prop_assert!(by_operator, "approved by {:?}", actor);
                        }
                        RecoveryPlanEvent::RecoveryActionStarted(e) => {
                            prop_assert_eq!(approvals, 1, "an action before the approval");
                            prop_assert_eq!(e.index, applied, "out of order");
                            open = Some(e.index);
                        }
                        RecoveryPlanEvent::RecoveryActionRecorded(e) => {
                            prop_assert_eq!(open, Some(e.index), "an outcome without its intent");
                            open = None;
                            if e.outcome == ActionOutcome::Applied {
                                applied += 1;
                            }
                        }
                        RecoveryPlanEvent::RecoveryPlanCompleted(_) => {
                            prop_assert_eq!(applied, 2, "completed with actions remaining");
                        }
                        RecoveryPlanEvent::RecoveryPlanEpochAdvanced(_)
                        | RecoveryPlanEvent::RecoveryPlanProposed(_)
                        | RecoveryPlanEvent::RecoveryPlanDrawn(_)
                        | RecoveryPlanEvent::RecoveryActionApplied(_)
                        | RecoveryPlanEvent::RecoveryPlanRefused(_)
                        | RecoveryPlanEvent::RecoveryPlanExpired(_) => {}
                    }
                }
                state = after;
            }
        }
        prop_assert!(approvals <= 1, "approved twice");
    }
}
