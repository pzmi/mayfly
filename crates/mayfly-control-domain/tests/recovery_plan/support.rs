//! Paths through a recovery plan's life, driven through the public `decide` and `evolve`.

use std::collections::BTreeMap;

use mayfly_control_domain::operations::plan::{
    ActionKind, ActionOutcome, Bindings, GithubBinding, OwnWrite, PlanAction, RecoveryPlan,
    RecoveryPlanCommand, RecoveryPlanEvent, RecoveryPlanRejection, RecoveryPlanState,
    attempt_stream,
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

pub fn attempt(n: u128) -> AttemptId {
    AttemptId::from_u128(0xA000 + n)
}

/// The stream of attempt `n`.
pub fn stream(n: u128) -> String {
    attempt_stream(attempt(n))
}

/// The heads after the plan's first `own` writes: attempt 1's stream at 3 (4 after action 0),
/// attempt 2's at 5 (6 after action 1), attempts 3 and 4 at 7, the host run's at 9.
pub fn heads(own: u8) -> BTreeMap<String, u64> {
    BTreeMap::from([
        (stream(1), if own >= 1 { 4 } else { 3 }),
        (stream(2), if own >= 2 { 6 } else { 5 }),
        (stream(3), 7),
        (stream(4), 7),
        ("hostrun-host0".to_owned(), 9),
    ])
}

/// The bindings the plan is drawn from, after its first `own` writes.
pub fn bound(own: u8) -> Bindings {
    Bindings {
        heads: heads(own),
        inventory: digest(0x1A),
        github: GithubBinding::Unreachable,
        epoch: Epoch::new(1),
    }
}

/// `bindings` with `stream`'s head moved on by another writer.
pub fn foreign(bindings: Bindings, stream: &str) -> Bindings {
    let mut moved = bindings;
    if let Some(head) = moved.heads.get_mut(stream) {
        *head += 1;
    }
    moved
}

/// The plan's own write for action `index` (attempt `index + 1`).
pub fn own_write(index: u32) -> OwnWrite {
    let (n, before) = if index == 0 { (1, 3) } else { (2, 5) };
    OwnWrite {
        stream: stream(n),
        before,
        after: before + 1,
    }
}

pub fn action(n: u128, kind: ActionKind, instance: bool) -> PlanAction {
    PlanAction {
        attempt_id: attempt(n),
        instance_id: instance.then(ProviderInstanceId::sample),
        kind,
    }
}

pub fn propose(actions: Vec<PlanAction>) -> RecoveryPlanCommand {
    RecoveryPlanCommand::ProposePlan {
        bindings: bound(0),
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

/// The outcome of action `index`, with the plan's own write when it was applied.
pub fn outcome(index: u32, outcome: ActionOutcome) -> RecoveryPlanCommand {
    RecoveryPlanCommand::RecordOutcome {
        index,
        outcome,
        write: match outcome {
            ActionOutcome::Applied => Some(own_write(index)),
            ActionOutcome::Failed => None,
        },
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
    path.push(auto(begin(0, bound(0))));
    path
}

pub fn applied() -> Vec<Step> {
    let mut path = intent();
    path.push(auto(outcome(0, ActionOutcome::Applied)));
    path
}

pub fn all_applied() -> Vec<Step> {
    let mut path = applied();
    path.extend([
        auto(begin(1, bound(1))),
        auto(outcome(1, ActionOutcome::Applied)),
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
        ..bound(0)
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
            with(intent(), vec![auto(outcome(0, ActionOutcome::Failed))]),
        ),
        (
            "completed",
            with(all_applied(), vec![auto(complete(bound(2)))]),
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
        ..bound(0)
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
        ("begin_first", auto(begin(0, bound(0)))),
        ("begin_first_world_moved", auto(begin(0, moved.clone()))),
        ("begin_second", auto(begin(1, bound(1)))),
        ("outcome_first", auto(outcome(0, ActionOutcome::Applied))),
        ("outcome_second", auto(outcome(1, ActionOutcome::Applied))),
        ("complete_bound", auto(complete(bound(2)))),
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
        ("propose_other_epoch", when(2, REPAIR, NOW, plan())),
        (
            "begin_first_other_run",
            when(2, REPAIR, NOW, begin(0, bound(0))),
        ),
        (
            "outcome_first_after_foreign",
            auto(RecoveryPlanCommand::RecordOutcome {
                index: 0,
                outcome: ActionOutcome::Applied,
                write: Some(OwnWrite {
                    before: 4,
                    after: 5,
                    ..own_write(0)
                }),
            }),
        ),
    ]
}
