//! Paths through a recovery plan's life, driven through the public `decide` and `evolve`.

use mayfly_control_domain::operations::plan::{
    ActionKind, ActionOutcome, Bindings, GithubBinding, PlanAction, RecoveryPlan,
    RecoveryPlanCommand, RecoveryPlanEvent, RecoveryPlanRejection, RecoveryPlanState,
};
use mayfly_control_domain::{Actor, Aggregate, Command, Rejection};
use mayfly_domain::{AttemptId, CommandId, Epoch, Millis, ProviderInstanceId, Sample, StateDigest};

pub type Step = Command<RecoveryPlanCommand>;

pub const REPAIR: Actor = Actor::Automation { policy: "repair" };
pub const OPERATOR: Actor = Actor::Operator { uid: 501 };
pub const NOW: u64 = 1_000;
pub const EXPIRY: u64 = 10_000;
pub const LATER: u64 = 20_000;

pub fn when(run_epoch: u64, actor: Actor, ms: u64, body: RecoveryPlanCommand) -> Step {
    Command::new(
        CommandId::from_u128(0xC1),
        Millis::new(ms),
        Epoch::new(run_epoch),
        actor,
        body,
    )
}

pub fn auto(body: RecoveryPlanCommand) -> Step {
    when(1, REPAIR, NOW, body)
}

pub fn digest(n: u128) -> StateDigest {
    StateDigest::from_u128(n)
}

/// The bindings the plan is drawn from, with the journal's heads at `heads`.
pub fn bound(heads: u128) -> Bindings {
    Bindings {
        heads: digest(heads),
        inventory: digest(0x1A),
        github: GithubBinding::Unreachable,
        epoch: Epoch::new(1),
    }
}

pub fn action(n: u128, kind: ActionKind, instance: bool) -> PlanAction {
    PlanAction {
        attempt_id: AttemptId::from_u128(0xA000 + n),
        instance_id: instance.then(ProviderInstanceId::sample),
        kind,
    }
}

pub fn propose(actions: Vec<PlanAction>) -> RecoveryPlanCommand {
    RecoveryPlanCommand::ProposePlan {
        bindings: bound(0x10),
        actions,
        expires_at: Millis::new(EXPIRY),
    }
}

/// Adopt attempt 1, mark attempt 2 lost.
pub fn plan() -> RecoveryPlanCommand {
    propose(vec![
        action(1, ActionKind::Adopt, true),
        action(2, ActionKind::MarkLost, false),
    ])
}

pub fn approve() -> Step {
    when(1, OPERATOR, NOW, RecoveryPlanCommand::ApprovePlan)
}

pub fn begin(index: u32, current: Bindings) -> RecoveryPlanCommand {
    RecoveryPlanCommand::BeginAction { index, current }
}

pub fn outcome(index: u32, outcome: ActionOutcome, heads: u128) -> RecoveryPlanCommand {
    RecoveryPlanCommand::RecordOutcome {
        index,
        outcome,
        heads_after: digest(heads),
    }
}

pub fn complete(current: Bindings) -> RecoveryPlanCommand {
    RecoveryPlanCommand::CompletePlan { current }
}

/// Decides `step` and folds what it accepted.
pub fn apply(
    state: &RecoveryPlanState,
    step: Step,
) -> Result<(RecoveryPlanState, Vec<RecoveryPlanEvent>), RecoveryPlanRejection> {
    let events = RecoveryPlan::decide(state, step)?;
    let next = events.iter().fold(state.clone(), RecoveryPlan::evolve);
    Ok((next, events))
}

/// The state after `steps`, every one accepted.
///
/// # Panics
/// When a step is refused: the path is wrong, not the aggregate under test.
pub fn reach(steps: impl IntoIterator<Item = Step>) -> RecoveryPlanState {
    steps
        .into_iter()
        .enumerate()
        .fold(RecoveryPlan::initial(), |state, (index, step)| {
            apply(&state, step)
                .unwrap_or_else(|r| panic!("step {index} of the path refused: {r:?}"))
                .0
        })
}

/// The refusal code of `step` on `state`, or `"accepted"`.
pub fn outcome_of(state: &RecoveryPlanState, step: Step) -> String {
    match RecoveryPlan::decide(state, step) {
        Ok(_) => "accepted".to_owned(),
        Err(rejection) => rejection.code().to_owned(),
    }
}

pub fn approved() -> Vec<Step> {
    vec![auto(plan()), approve()]
}

pub fn intent() -> Vec<Step> {
    let mut path = approved();
    path.push(auto(begin(0, bound(0x10))));
    path
}

pub fn applied() -> Vec<Step> {
    let mut path = intent();
    path.push(auto(outcome(0, ActionOutcome::Applied, 0x11)));
    path
}

pub fn all_applied() -> Vec<Step> {
    let mut path = applied();
    path.extend([
        auto(begin(1, bound(0x11))),
        auto(outcome(1, ActionOutcome::Applied, 0x12)),
    ]);
    path
}

/// One state per phase, and the applying variants that decide differently.
pub fn matrix_states() -> Vec<(&'static str, RecoveryPlanState)> {
    let with = |base: Vec<Step>, more: Vec<Step>| {
        let mut path = base;
        path.extend(more);
        reach(path)
    };
    let moved = Bindings {
        inventory: digest(0x1B),
        ..bound(0x10)
    };
    vec![
        ("unproposed", RecoveryPlan::initial()),
        ("proposed", reach([auto(plan())])),
        ("approved", reach(approved())),
        ("applying/intent", reach(intent())),
        ("applying/between", reach(applied())),
        ("applying/done", reach(all_applied())),
        (
            "failed",
            with(
                intent(),
                vec![auto(outcome(0, ActionOutcome::Failed, 0x11))],
            ),
        ),
        (
            "completed",
            with(all_applied(), vec![auto(complete(bound(0x12)))]),
        ),
        ("refused", with(approved(), vec![auto(begin(0, moved))])),
        (
            "expired",
            with(
                vec![auto(plan())],
                vec![when(1, REPAIR, LATER, RecoveryPlanCommand::ExpirePlan)],
            ),
        ),
    ]
}

/// One stimulus of every command type (and the variants that decide differently).
pub fn stimuli() -> Vec<(&'static str, Step)> {
    let moved = Bindings {
        inventory: digest(0x1B),
        ..bound(0x10)
    };
    vec![
        ("propose", auto(plan())),
        (
            "propose_duplicate",
            auto(propose(vec![
                action(1, ActionKind::Adopt, true),
                action(1, ActionKind::Keep, true),
            ])),
        ),
        (
            "propose_unproven_adopt",
            auto(propose(vec![action(1, ActionKind::Adopt, false)])),
        ),
        ("approve_by_operator", approve()),
        (
            "approve_by_automation",
            auto(RecoveryPlanCommand::ApprovePlan),
        ),
        (
            "approve_after_expiry",
            when(1, OPERATOR, LATER, RecoveryPlanCommand::ApprovePlan),
        ),
        ("begin_first", auto(begin(0, bound(0x10)))),
        ("begin_first_world_moved", auto(begin(0, moved))),
        ("begin_second", auto(begin(1, bound(0x11)))),
        (
            "outcome_first",
            auto(outcome(0, ActionOutcome::Applied, 0x11)),
        ),
        (
            "outcome_second",
            auto(outcome(1, ActionOutcome::Applied, 0x12)),
        ),
        ("complete_bound", auto(complete(bound(0x12)))),
        ("complete_world_moved", auto(complete(moved))),
        (
            "expire_later",
            when(1, REPAIR, LATER, RecoveryPlanCommand::ExpirePlan),
        ),
        ("expire_now", auto(RecoveryPlanCommand::ExpirePlan)),
        (
            "stale_run",
            when(0, REPAIR, NOW, RecoveryPlanCommand::ExpirePlan),
        ),
    ]
}
