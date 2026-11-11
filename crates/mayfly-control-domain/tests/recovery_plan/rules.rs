//! The recovery plan's rules (`doc/domain-model.md` §6.9): I31's bindings and F45, intent then
//! outcome, actions only from the recovery table's proposals, one operator approval.

use mayfly_control_domain::DeterminismCases;
use mayfly_control_domain::attempt::QuarantineReason;
use mayfly_control_domain::operations::plan::{
    ActionKind, ActionOutcome, Bindings, GithubBinding, OwnWrite, PlanPhase, RecoveryPlan,
    RecoveryPlanCommand, RecoveryPlanEvent, StaleBinding,
};
use mayfly_control_domain::operations::recovery::Proposal;
use mayfly_domain::Epoch;

use super::support::{
    LATER, NOW, OPERATOR, REPAIR, action, all_applied, applied, apply, approve, approved, auto,
    begin, bound, complete, digest, foreign, intent, outcome, outcome_of, own_write, plan, propose,
    reach, stream, when,
};

/// Every way the world can move after the plan was drawn, one binding at a time.
fn moved_worlds() -> Vec<(StaleBinding, Bindings)> {
    let drawn = bound(0);
    vec![
        (
            StaleBinding::JournalHeads,
            foreign(drawn.clone(), "hostrun-host0"),
        ),
        (
            StaleBinding::Inventory,
            Bindings {
                inventory: digest(0x1B),
                ..drawn.clone()
            },
        ),
        (
            StaleBinding::Github,
            Bindings {
                github: GithubBinding::Snapshot(digest(0x6A)),
                ..drawn.clone()
            },
        ),
        (
            StaleBinding::Epoch,
            Bindings {
                epoch: Epoch::new(2),
                ..drawn
            },
        ),
    ]
}

#[test]
fn should_refuse_the_plan_when_it_is_applied_after_the_world_moved() {
    // given: F45, an approved plan, then the world moves before the apply
    let state = reach(approved());
    for (expected, now) in moved_worlds() {
        // when
        let (after, events) =
            apply(&state, auto(begin(0, now))).unwrap_or_else(|r| panic!("{r:?}"));
        // then: refused, and no intent: the action's command never runs
        assert!(
            matches!(
                events.as_slice(),
                [RecoveryPlanEvent::RecoveryPlanRefused(e)] if e.stale == expected
            ),
            "{events:?}"
        );
        assert_eq!(after.phase, PlanPhase::Refused(expected));
        assert_eq!(
            outcome_of(&after, auto(begin(0, bound(0)))),
            "closed",
            "a refused plan is applied no further"
        );
    }
}

#[test]
fn should_refuse_the_plan_when_the_world_moves_between_two_actions() {
    // given: the first action applied, its own heads recorded
    let state = reach(applied());
    let moved = Bindings {
        inventory: digest(0x1B),
        ..bound(1)
    };
    // when
    let own = outcome_of(&state, auto(begin(1, bound(1))));
    let (after, _) = apply(&state, auto(begin(1, moved))).unwrap_or_else(|r| panic!("{r:?}"));
    // then
    assert_eq!(
        own, "accepted",
        "the plan's own outcome moves its heads binding"
    );
    assert_eq!(after.phase, PlanPhase::Refused(StaleBinding::Inventory));
}

#[test]
fn should_refuse_the_plan_when_the_world_moved_before_completion() {
    // given
    let state = reach(all_applied());
    for (expected, now) in moved_worlds() {
        let now = Bindings {
            heads: if expected == StaleBinding::JournalHeads {
                foreign(bound(2), "hostrun-host0").heads
            } else {
                bound(2).heads
            },
            ..now
        };
        // when
        let (after, _) = apply(&state, auto(complete(now))).unwrap_or_else(|r| panic!("{r:?}"));
        // then
        assert_eq!(after.phase, PlanPhase::Refused(expected));
    }
}

#[test]
fn should_refuse_an_outcome_when_its_intent_is_not_recorded() {
    // given
    let approved = reach(approved());
    let between = reach(applied());
    // when
    let outcomes = [
        outcome_of(&approved, auto(outcome(0, ActionOutcome::Applied))),
        outcome_of(&between, auto(outcome(1, ActionOutcome::Applied))),
        outcome_of(&reach(intent()), auto(outcome(1, ActionOutcome::Applied))),
    ];
    // then
    assert_eq!(outcomes, ["no_intent", "no_intent", "no_intent"]);
}

#[test]
fn should_hold_no_destructive_action_when_built_from_proposals() {
    // given: every proposal the recovery table can make
    let reasons = [
        QuarantineReason::OwnershipDoubt,
        QuarantineReason::GuestIdentityRefused,
        QuarantineReason::JitPresumedLeaked,
        QuarantineReason::ProviderIdentityMismatch,
        QuarantineReason::DeleteRefusedRepeatedly,
        QuarantineReason::RedactionViolation,
        QuarantineReason::ForensicRetention,
        QuarantineReason::OutcomeUnprovable,
        QuarantineReason::VmVanishedExternally,
    ];
    let mut proposals = vec![
        Proposal::WaitForInventory,
        Proposal::Adopt,
        Proposal::MarkLost,
        Proposal::Keep,
        Proposal::Report,
        Proposal::Ignore,
    ];
    proposals.extend(reasons.map(Proposal::Quarantine));
    // when
    let kinds: Vec<(Proposal, Option<ActionKind>)> = proposals
        .iter()
        .map(|p| (*p, ActionKind::from_proposal(*p)))
        .collect();
    // then: an incomplete inventory binds no plan, and each kind is one of the six safe ones,
    // matched exhaustively, so a destructive kind added later does not compile here unseen
    for (proposal, kind) in kinds {
        match kind {
            None => assert_eq!(proposal, Proposal::WaitForInventory),
            Some(
                ActionKind::Adopt
                | ActionKind::Quarantine(_)
                | ActionKind::Keep
                | ActionKind::Report
                | ActionKind::Ignore,
            ) => assert!(!kind.is_some_and(ActionKind::irreversible)),
            Some(ActionKind::MarkLost) => {
                assert_eq!(proposal, Proposal::MarkLost);
                assert!(ActionKind::MarkLost.irreversible());
            }
        }
    }
}

#[test]
fn should_refuse_a_plan_when_an_adoption_has_no_evidence() {
    // given
    let unproven = propose(vec![action(1, ActionKind::Adopt, false)]);
    let lost_without_id = propose(vec![action(1, ActionKind::MarkLost, false)]);
    // when
    let outcomes = [
        outcome_of(&reach([]), auto(unproven)),
        outcome_of(&reach([]), auto(lost_without_id)),
    ];
    // then
    assert_eq!(outcomes, ["no_evidence", "accepted"]);
}

#[test]
fn should_approve_once_and_only_by_an_operator_when_any_actor_approves() {
    // given
    let proposed = reach([auto(plan())]);
    // when
    let outcomes = [
        outcome_of(&proposed, auto(RecoveryPlanCommand::ApprovePlan)),
        outcome_of(
            &proposed,
            when(
                1,
                mayfly_control_domain::Actor::Startup,
                NOW,
                RecoveryPlanCommand::ApprovePlan,
            ),
        ),
        outcome_of(
            &proposed,
            when(1, OPERATOR, LATER, RecoveryPlanCommand::ApprovePlan),
        ),
        outcome_of(&proposed, approve()),
        outcome_of(&reach(approved()), approve()),
    ];
    // then
    assert_eq!(
        outcomes,
        [
            "operator_only",
            "operator_only",
            "plan_expired",
            "accepted",
            "already_approved"
        ]
    );
}

#[test]
fn should_resume_at_the_failed_action_when_its_command_was_refused() {
    // given
    let mut path = intent();
    path.push(auto(outcome(0, ActionOutcome::Failed)));
    let failed = reach(path);
    // when
    let skip = outcome_of(&failed, auto(begin(1, bound(0))));
    let complete_early = outcome_of(&failed, auto(complete(bound(0))));
    let (after, events) =
        apply(&failed, auto(begin(0, bound(0)))).unwrap_or_else(|r| panic!("{r:?}"));
    // then
    assert_eq!(
        (skip.as_str(), complete_early.as_str()),
        ("out_of_order", "actions_remaining")
    );
    assert_eq!(after.phase, PlanPhase::Applying);
    assert!(matches!(
        events.as_slice(),
        [RecoveryPlanEvent::RecoveryActionStarted(_)]
    ));
}

#[test]
fn should_complete_only_when_every_action_applied_and_bindings_hold() {
    // given
    let half = reach(applied());
    let done = reach(all_applied());
    // when
    let early = outcome_of(&half, auto(complete(bound(1))));
    let (after, _) = apply(&done, auto(complete(bound(2)))).unwrap_or_else(|r| panic!("{r:?}"));
    // then
    assert_eq!(early, "actions_remaining");
    assert_eq!(after.phase, PlanPhase::Completed);
    assert_eq!(
        outcome_of(&after, when(2, REPAIR, NOW, begin(0, bound(2)))),
        "closed"
    );
}

#[test]
fn should_refuse_the_plan_when_another_stream_moved_before_the_next_action() {
    // given: action 0 applied; another writer appended to attempt 2's stream since
    let state = reach(applied());
    let moved = foreign(bound(1), &stream(2));
    // when
    let (after, events) = apply(&state, auto(begin(1, moved))).unwrap_or_else(|r| panic!("{r:?}"));
    // then: the plan's own write advanced only its own head, so the foreign one shows
    assert!(
        matches!(
            events.as_slice(),
            [RecoveryPlanEvent::RecoveryPlanRefused(e)] if e.stale == StaleBinding::JournalHeads
        ),
        "{events:?}"
    );
    assert_eq!(after.phase, PlanPhase::Refused(StaleBinding::JournalHeads));
}

#[test]
fn should_refuse_the_plan_when_its_own_stream_moved_before_its_write() {
    // given: the intent of action 0; another writer appended to attempt 1's stream before the
    // plan's own write, which therefore started one revision later than bound
    let state = reach(intent());
    let late = RecoveryPlanCommand::RecordOutcome {
        index: 0,
        outcome: ActionOutcome::Applied,
        write: Some(OwnWrite {
            before: 4,
            after: 5,
            ..own_write(0)
        }),
    };
    // when
    let (after, events) = apply(&state, auto(late)).unwrap_or_else(|r| panic!("{r:?}"));
    // then: the outcome is recorded, and the plan refused with it
    assert!(
        matches!(
            events.as_slice(),
            [RecoveryPlanEvent::RecoveryActionRecorded(_), RecoveryPlanEvent::RecoveryPlanRefused(e)]
                if e.stale == StaleBinding::JournalHeads
        ),
        "{events:?}"
    );
    assert_eq!(after.phase, PlanPhase::Refused(StaleBinding::JournalHeads));
}

#[test]
fn should_refuse_an_outcome_when_its_write_is_not_the_action_s_own() {
    // given
    let state = reach(intent());
    let bodies = [
        (ActionOutcome::Applied, Some(own_write(1))),
        (ActionOutcome::Applied, None),
        (ActionOutcome::Failed, Some(own_write(0))),
    ];
    // when
    let outcomes: Vec<String> = bodies
        .into_iter()
        .map(|(outcome, write)| {
            outcome_of(
                &state,
                auto(RecoveryPlanCommand::RecordOutcome {
                    index: 0,
                    outcome,
                    write,
                }),
            )
        })
        .collect();
    // then
    assert_eq!(outcomes, ["wrong_write", "wrong_write", "wrong_write"]);
}

#[test]
fn should_refuse_the_plan_when_the_command_runs_under_another_epoch() {
    // given: approved in run epoch 1
    let state = reach(approved());
    // when: the daemon restarted (epoch 2) and the caller echoes the drawn bindings
    let (after, events) =
        apply(&state, when(2, REPAIR, NOW, begin(0, bound(0)))).unwrap_or_else(|r| panic!("{r:?}"));
    // then: the fenced run epoch decides, not the reported one
    assert!(
        matches!(
            events.as_slice(),
            [RecoveryPlanEvent::RecoveryPlanEpochAdvanced(_), RecoveryPlanEvent::RecoveryPlanRefused(e)]
                if e.stale == StaleBinding::Epoch
        ),
        "{events:?}"
    );
    assert_eq!(after.phase, PlanPhase::Refused(StaleBinding::Epoch));
}

#[test]
fn should_refuse_a_plan_when_its_bindings_do_not_cover_its_run_and_targets() {
    // given
    let empty = reach([]);
    let unbound = RecoveryPlanCommand::ProposePlan {
        bindings: Bindings {
            heads: std::collections::BTreeMap::from([(stream(1), 3)]),
            ..bound(0)
        },
        actions: vec![
            action(1, ActionKind::Adopt, true),
            action(2, ActionKind::MarkLost, false),
        ],
        expires_at: mayfly_domain::Millis::new(10_000),
    };
    // when
    let outcomes = [
        outcome_of(&empty, when(2, REPAIR, NOW, plan())),
        outcome_of(&empty, auto(unbound)),
        outcome_of(&empty, auto(plan())),
    ];
    // then
    assert_eq!(outcomes, ["epoch_mismatch", "unbound_target", "accepted"]);
}

#[test]
fn should_refuse_a_legacy_plan_when_it_is_applied() {
    // given: a plan KER-10 drew, bound by one digest of the heads, then approved
    let legacy = RecoveryPlan::determinism_cases()
        .into_iter()
        .find(|c| c.name == "legacy_plan_is_refused")
        .map(|c| c.history)
        .unwrap_or_else(|| panic!("the legacy case is gone"));
    let state = mayfly_control_domain::replay::<RecoveryPlan>(&legacy);
    // when
    let (after, _) = apply(&state, auto(begin(0, bound(0)))).unwrap_or_else(|r| panic!("{r:?}"));
    // then
    assert_eq!(after.phase, PlanPhase::Refused(StaleBinding::JournalHeads));
}
