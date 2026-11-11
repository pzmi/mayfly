//! The recovery plan's `DETERMINISM_CASES` (ver §3.1): accepted cases along a plan's life and
//! one refused case per [`RecoveryPlanRejection`] code. Histories are produced by `decide`
//! itself from command paths (each path is checked to be accepted step by step by
//! `should_accept_every_step_when_a_case_path_is_replayed`).

use mayfly_domain::{AttemptId, CommandId, Epoch, Millis, ProviderInstanceId, Sample, StateDigest};

use super::events::RecoveryPlanEvent;
use super::state::{
    ActionKind, ActionOutcome, Bindings, GithubBinding, PlanAction, RecoveryPlanCommand,
    RecoveryPlanRejection, RecoveryPlanState,
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

/// The bindings the plan was drawn from, with the journal's heads at `heads`.
fn bound(heads: u128) -> Bindings {
    Bindings {
        heads: digest(heads),
        inventory: digest(0x1A),
        github: GithubBinding::Unreachable,
        epoch: Epoch::new(1),
    }
}

fn action(n: u128, kind: ActionKind, instance: bool) -> PlanAction {
    PlanAction {
        attempt_id: AttemptId::from_u128(0xA000 + n),
        instance_id: instance.then(ProviderInstanceId::sample),
        kind,
    }
}

fn propose(actions: Vec<PlanAction>) -> RecoveryPlanCommand {
    RecoveryPlanCommand::ProposePlan {
        bindings: bound(0x10),
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

fn begin(index: u32, current: Bindings) -> RecoveryPlanCommand {
    RecoveryPlanCommand::BeginAction { index, current }
}

fn outcome(index: u32, outcome: ActionOutcome, heads: u128) -> RecoveryPlanCommand {
    RecoveryPlanCommand::RecordOutcome {
        index,
        outcome,
        heads_after: digest(heads),
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

fn approved_at_two() -> Vec<Step> {
    let mut path = proposed();
    path.push(when(2, OPERATOR, NOW, RecoveryPlanCommand::ApprovePlan));
    path
}

fn intent() -> Vec<Step> {
    then(approved(), [begin(0, bound(0x10))])
}

fn applied() -> Vec<Step> {
    then(intent(), [outcome(0, ActionOutcome::Applied, 0x11)])
}

fn failed() -> Vec<Step> {
    then(intent(), [outcome(0, ActionOutcome::Failed, 0x11)])
}

fn all_applied() -> Vec<Step> {
    then(
        applied(),
        [
            begin(1, bound(0x11)),
            outcome(1, ActionOutcome::Applied, 0x12),
        ],
    )
}

fn completed() -> Vec<Step> {
    then(
        all_applied(),
        [RecoveryPlanCommand::CompletePlan {
            current: bound(0x12),
        }],
    )
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
        approved_at_two(),
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
        ..bound(0x10)
    };
    let github_answers = Bindings {
        github: GithubBinding::Snapshot(digest(0x6A)),
        ..bound(0x12)
    };
    let next_run = Bindings {
        epoch: Epoch::new(2),
        ..bound(0x10)
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
            auto(begin(0, bound(0x10))),
            Accepted,
        ),
        case(
            "repeated_intent_resumes",
            intent(),
            auto(begin(0, bound(0x10))),
            Accepted,
        ),
        case(
            "record_outcome",
            intent(),
            auto(outcome(0, ActionOutcome::Applied, 0x11)),
            Accepted,
        ),
        case(
            "record_failed_outcome",
            intent(),
            auto(outcome(0, ActionOutcome::Failed, 0x11)),
            Accepted,
        ),
        case(
            "resume_after_failure",
            failed(),
            auto(begin(0, bound(0x11))),
            Accepted,
        ),
        case(
            "next_action_after_own_heads",
            applied(),
            auto(begin(1, bound(0x11))),
            Accepted,
        ),
        case(
            "complete_plan",
            all_applied(),
            auto(RecoveryPlanCommand::CompletePlan {
                current: bound(0x12),
            }),
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
            auto(RecoveryPlanCommand::CompletePlan {
                current: github_answers,
            }),
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
        // refused, one per code
        case(
            "stale_run",
            approved_at_two(),
            auto(begin(0, bound(0x10))),
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
            auto(begin(0, bound(0x10))),
            Refused,
        ),
        case(
            "out_of_order",
            approved(),
            auto(begin(1, bound(0x10))),
            Refused,
        ),
        case(
            "no_intent",
            approved(),
            auto(outcome(0, ActionOutcome::Applied, 0x11)),
            Refused,
        ),
        case(
            "actions_remaining",
            applied(),
            auto(RecoveryPlanCommand::CompletePlan {
                current: bound(0x11),
            }),
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
