//! The recovery plan's `DETERMINISM_CASES` (ver §3.1): accepted cases along a plan's life and
//! one refused case per [`RecoveryPlanRejection`] code. Histories are produced by `decide`
//! itself from command paths (each path is checked to be accepted step by step by
//! `should_accept_every_step_when_a_case_path_is_replayed`).

use std::collections::BTreeMap;

use mayfly_domain::{AttemptId, CommandId, Epoch, Millis, ProviderInstanceId, Sample, StateDigest};

use super::events::{RecoveryPlanApproved, RecoveryPlanEvent, RecoveryPlanProposed};
use super::state::{
    ActionKind, ActionOutcome, Bindings, GithubBinding, LegacyBindings, Observed, OwnWrite,
    PlanAction, RecoveryPlanCommand, RecoveryPlanRejection, RecoveryPlanState, attempt_stream,
};
use super::{RecoveryPlan, decide, evolve};
use crate::aggregate::{DeterminismCase, Expect};
use crate::attempt::QuarantineReason;
use crate::command::{Actor, Command};

type Step = Command<RecoveryPlanCommand>;

const REPAIR: Actor = Actor::Automation { policy: "repair" };
const OPERATOR: Actor = Actor::Operator { uid: 501 };
const NOW: u64 = 1_000;
const EXPIRY: u64 = 10_000;
const LATER: u64 = 20_000;

fn when(run_epoch: u64, actor: Actor, ms: u64, body: RecoveryPlanCommand) -> Step {
    Command::new(
        CommandId::from_u128(0xC1),
        Millis::new(ms),
        Epoch::new(run_epoch),
        actor,
        body,
    )
}

fn auto(body: RecoveryPlanCommand) -> Step {
    when(1, REPAIR, NOW, body)
}

fn digest(n: u128) -> StateDigest {
    StateDigest::from_u128(n)
}

fn attempt(n: u128) -> AttemptId {
    AttemptId::from_u128(0xA000 + n)
}

/// The heads after the plan's first `own` writes: attempt 1's stream at 3 (4 after action 0),
/// attempt 2's at 5 (6 after action 1), attempts 3 and 4 at 7, the host run's at 9.
fn heads(own: u8) -> BTreeMap<String, u64> {
    BTreeMap::from([
        (attempt_stream(attempt(1)), if own >= 1 { 4 } else { 3 }),
        (attempt_stream(attempt(2)), if own >= 2 { 6 } else { 5 }),
        (attempt_stream(attempt(3)), 7),
        (attempt_stream(attempt(4)), 7),
        ("hostrun-host0".to_owned(), 9),
    ])
}

/// The bindings the plan was drawn from, after its first `own` writes.
fn bound(own: u8) -> Bindings {
    Bindings {
        heads: heads(own),
        inventory: digest(0x1A),
        github: GithubBinding::Unreachable,
        epoch: Epoch::new(1),
    }
}

/// The plan's own write for action `index` (attempt `index + 1`).
fn own_write(index: u32) -> OwnWrite {
    let (n, before) = if index == 0 { (1, 3) } else { (2, 5) };
    OwnWrite {
        stream: attempt_stream(attempt(n)),
        before,
        after: before + 1,
    }
}

fn action(n: u128, kind: ActionKind, instance: bool) -> PlanAction {
    PlanAction {
        attempt_id: attempt(n),
        instance_id: instance.then(ProviderInstanceId::sample),
        kind,
    }
}

fn propose(actions: Vec<PlanAction>) -> RecoveryPlanCommand {
    RecoveryPlanCommand::ProposePlan {
        bindings: bound(0),
        actions,
        expires_at: Millis::new(EXPIRY),
    }
}

fn plan() -> RecoveryPlanCommand {
    propose(vec![
        action(1, ActionKind::Adopt, true),
        action(2, ActionKind::MarkLost, false),
    ])
}

/// What a use case observes when the world is `now`: the bound streams read through
/// [`Bindings::read_now`], the inventory and GitHub.
fn observe(now: &Bindings) -> Observed {
    Observed {
        heads: now.read_now(|s| now.heads.get(s).copied().unwrap_or(0)),
        inventory: now.inventory,
        github: now.github,
    }
}

fn begin(index: u32, now: Bindings) -> RecoveryPlanCommand {
    RecoveryPlanCommand::BeginAction {
        index,
        current: observe(&now),
    }
}

fn complete(now: Bindings) -> RecoveryPlanCommand {
    RecoveryPlanCommand::CompletePlan {
        current: observe(&now),
    }
}

fn outcome(index: u32, outcome: ActionOutcome) -> RecoveryPlanCommand {
    RecoveryPlanCommand::RecordOutcome {
        index,
        outcome,
        write: match outcome {
            ActionOutcome::Applied => Some(own_write(index)),
            ActionOutcome::Failed => None,
        },
    }
}

fn approve() -> Step {
    when(1, OPERATOR, NOW, RecoveryPlanCommand::ApprovePlan)
}

fn then(mut path: Vec<Step>, more: impl IntoIterator<Item = RecoveryPlanCommand>) -> Vec<Step> {
    path.extend(more.into_iter().map(auto));
    path
}

fn proposed() -> Vec<Step> {
    vec![auto(plan())]
}

fn approved() -> Vec<Step> {
    let mut path = proposed();
    path.push(approve());
    path
}

/// Approved in run epoch 1, then refused by an intent under run epoch 2: the plan's fence is 2.
fn refused_at_two() -> Vec<Step> {
    let mut path = approved();
    path.push(when(2, REPAIR, NOW, begin(0, bound(0))));
    path
}

fn intent() -> Vec<Step> {
    then(approved(), [begin(0, bound(0))])
}

fn applied() -> Vec<Step> {
    then(intent(), [outcome(0, ActionOutcome::Applied)])
}

fn failed() -> Vec<Step> {
    then(intent(), [outcome(0, ActionOutcome::Failed)])
}

fn all_applied() -> Vec<Step> {
    then(
        applied(),
        [begin(1, bound(1)), outcome(1, ActionOutcome::Applied)],
    )
}

fn completed() -> Vec<Step> {
    then(all_applied(), [complete(bound(2))])
}

/// A plan KER-10 drew, bound by a digest, then approved.
fn legacy() -> Vec<RecoveryPlanEvent> {
    vec![
        RecoveryPlanEvent::RecoveryPlanProposed(RecoveryPlanProposed {
            bindings: LegacyBindings {
                heads: digest(0x10),
                inventory: digest(0x1A),
                github: GithubBinding::Unreachable,
                epoch: Epoch::new(1),
            },
            actions: vec![action(1, ActionKind::Adopt, true)],
            expires_at: Millis::new(EXPIRY),
        }),
        RecoveryPlanEvent::RecoveryPlanApproved(RecoveryPlanApproved { operator_uid: 501 }),
    ]
}

/// Folds `path` through `decide` and `evolve`; the first refused step, if any.
fn replay_path(
    path: Vec<Step>,
) -> (
    Vec<RecoveryPlanEvent>,
    Option<(usize, RecoveryPlanRejection)>,
) {
    let mut state = RecoveryPlanState::initial();
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
) -> DeterminismCase<RecoveryPlan> {
    DeterminismCase {
        name,
        history: replay_path(path).0,
        command,
        expect,
    }
}

/// Every path a case starts from, for the replay check.
#[cfg(test)]
fn paths() -> Vec<Vec<Step>> {
    vec![
        proposed(),
        approved(),
        refused_at_two(),
        intent(),
        applied(),
        failed(),
        all_applied(),
        completed(),
    ]
}

pub(super) fn cases() -> Vec<DeterminismCase<RecoveryPlan>> {
    use Expect::{Accepted, Refused};
    let moved_inventory = Bindings {
        inventory: digest(0x1B),
        ..bound(0)
    };
    let github_answers = Bindings {
        github: GithubBinding::Snapshot(digest(0x6A)),
        ..bound(2)
    };
    let next_run = Bindings {
        epoch: Epoch::new(2),
        ..bound(0)
    };
    vec![
        // accepted
        case("propose_plan", Vec::new(), auto(plan()), Accepted),
        case(
            "propose_quarantine_and_keep",
            Vec::new(),
            auto(propose(vec![
                action(
                    1,
                    ActionKind::Quarantine(QuarantineReason::OwnershipDoubt),
                    true,
                ),
                action(2, ActionKind::Keep, true),
                action(3, ActionKind::Report, true),
                action(4, ActionKind::Ignore, false),
            ])),
            Accepted,
        ),
        case("approve_plan", proposed(), approve(), Accepted),
        case(
            "begin_action",
            approved(),
            auto(begin(0, bound(0))),
            Accepted,
        ),
        case(
            "repeated_intent_resumes",
            intent(),
            auto(begin(0, bound(0))),
            Accepted,
        ),
        case(
            "record_outcome",
            intent(),
            auto(outcome(0, ActionOutcome::Applied)),
            Accepted,
        ),
        case(
            "record_failed_outcome",
            intent(),
            auto(outcome(0, ActionOutcome::Failed)),
            Accepted,
        ),
        case(
            "resume_after_failure",
            failed(),
            auto(begin(0, bound(0))),
            Accepted,
        ),
        case(
            "next_action_after_own_heads",
            applied(),
            auto(begin(1, bound(1))),
            Accepted,
        ),
        case(
            "complete_plan",
            all_applied(),
            auto(complete(bound(2))),
            Accepted,
        ),
        case(
            "world_moved_refuses_plan",
            approved(),
            auto(begin(0, moved_inventory)),
            Accepted,
        ),
        case(
            "github_answer_refuses_completion",
            all_applied(),
            auto(complete(github_answers)),
            Accepted,
        ),
        case(
            "newer_epoch_refuses_plan",
            approved(),
            when(2, REPAIR, NOW, begin(0, next_run)),
            Accepted,
        ),
        case(
            "expire_plan",
            proposed(),
            when(1, REPAIR, LATER, RecoveryPlanCommand::ExpirePlan),
            Accepted,
        ),
        case(
            "foreign_write_before_own_refuses_plan",
            intent(),
            auto(RecoveryPlanCommand::RecordOutcome {
                index: 0,
                outcome: ActionOutcome::Applied,
                write: Some(OwnWrite {
                    before: 4,
                    after: 5,
                    ..own_write(0)
                }),
            }),
            Accepted,
        ),
        case(
            "approval_under_another_epoch_is_refused",
            proposed(),
            when(2, OPERATOR, NOW, RecoveryPlanCommand::ApprovePlan),
            Refused,
        ),
        DeterminismCase {
            name: "legacy_plan_is_refused",
            history: legacy(),
            command: auto(begin(0, bound(0))),
            expect: Accepted,
        },
        // refused, one per code
        case(
            "epoch_mismatch",
            Vec::new(),
            when(2, REPAIR, NOW, plan()),
            Refused,
        ),
        case(
            "unbound_target",
            Vec::new(),
            auto(propose(vec![action(5, ActionKind::Adopt, true)])),
            Refused,
        ),
        case(
            "wrong_write",
            intent(),
            auto(RecoveryPlanCommand::RecordOutcome {
                index: 0,
                outcome: ActionOutcome::Applied,
                write: Some(own_write(1)),
            }),
            Refused,
        ),
        case(
            "stale_run",
            refused_at_two(),
            auto(begin(0, bound(0))),
            Refused,
        ),
        case("not_proposed", Vec::new(), approve(), Refused),
        case("already_proposed", proposed(), auto(plan()), Refused),
        case(
            "duplicate_target",
            Vec::new(),
            auto(propose(vec![
                action(1, ActionKind::Adopt, true),
                action(1, ActionKind::Report, true),
            ])),
            Refused,
        ),
        case(
            "no_evidence",
            Vec::new(),
            auto(propose(vec![action(1, ActionKind::Adopt, false)])),
            Refused,
        ),
        case(
            "operator_only",
            proposed(),
            auto(RecoveryPlanCommand::ApprovePlan),
            Refused,
        ),
        case(
            "plan_expired",
            proposed(),
            when(1, OPERATOR, LATER, RecoveryPlanCommand::ApprovePlan),
            Refused,
        ),
        case("already_approved", approved(), approve(), Refused),
        case(
            "not_approved",
            proposed(),
            auto(begin(0, bound(0))),
            Refused,
        ),
        case(
            "out_of_order",
            approved(),
            auto(begin(1, bound(0))),
            Refused,
        ),
        case(
            "no_intent",
            approved(),
            auto(outcome(0, ActionOutcome::Applied)),
            Refused,
        ),
        case(
            "actions_remaining",
            applied(),
            auto(complete(bound(1))),
            Refused,
        ),
        case(
            "not_expired",
            proposed(),
            auto(RecoveryPlanCommand::ExpirePlan),
            Refused,
        ),
        case(
            "in_progress",
            intent(),
            when(1, REPAIR, LATER, RecoveryPlanCommand::ExpirePlan),
            Refused,
        ),
        case("closed", completed(), approve(), Refused),
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
        let refusals: Vec<(usize, Option<(usize, RecoveryPlanRejection)>)> = paths
            .into_iter()
            .map(|path| replay_path(path).1)
            .enumerate()
            .filter(|(_, refusal)| refusal.is_some())
            .collect();
        // then
        assert!(refusals.is_empty(), "{refusals:?}");
    }
}
