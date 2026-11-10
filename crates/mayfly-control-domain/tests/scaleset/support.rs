//! Paths through a scale set's life, driven through its public `decide` and `evolve`.

use std::collections::BTreeSet;

use mayfly_control_domain::attempt::RunnerRequestId;
use mayfly_control_domain::demand::{Capacity, EventName, Offer, Statistics, WireMode};
use mayfly_control_domain::pool::TrustTier;
use mayfly_control_domain::registration::{JobResult, ScaleSetId};
use mayfly_control_domain::scaleset::{
    EnsureScaleSet, JobFact, LossCause, MessageId, MessageItem, RecordMessage, ScaleSet,
    ScaleSetCommand, ScaleSetEvent, ScaleSetRejection, ScaleSetState, SessionId,
};
use mayfly_control_domain::{Actor, Aggregate, Command, Rejection};
use mayfly_domain::{CommandId, Epoch, Millis, PoolName, Sample};

pub type Step = Command<ScaleSetCommand>;

pub const LISTENER: Actor = Actor::Automation {
    policy: "scale_set_listener",
};

/// The first session.
pub const FIRST: SessionId = SessionId { high: 1, low: 1 };
/// A later session.
pub const SECOND: SessionId = SessionId { high: 2, low: 2 };

pub fn at(run_epoch: u64, body: ScaleSetCommand) -> Step {
    Command::new(
        CommandId::from_u128(0x5C),
        Millis::new(1_000),
        Epoch::new(run_epoch),
        LISTENER,
        body,
    )
}

pub fn auto(body: ScaleSetCommand) -> Step {
    at(1, body)
}

pub fn ensure() -> ScaleSetCommand {
    ScaleSetCommand::EnsureScaleSet(EnsureScaleSet {
        pool: PoolName::sample(),
        github_id: ScaleSetId(7),
        tier: TrustTier::Trusted,
    })
}

pub const fn capacity(
    revision: u64,
    deliverable: u32,
    reported_max: u32,
    mode: WireMode,
) -> ScaleSetCommand {
    ScaleSetCommand::ApplyCapacity(Capacity {
        revision,
        deliverable,
        reported_max,
        mode,
    })
}

pub fn assigned(total_assigned_jobs: u32) -> Statistics {
    Statistics {
        total_assigned_jobs,
        ..Statistics::default()
    }
}

pub const fn opened(session_id: SessionId, statistics: Statistics) -> ScaleSetCommand {
    ScaleSetCommand::RecordSessionOpened {
        session_id,
        statistics,
    }
}

pub const fn loss(cause: LossCause) -> ScaleSetCommand {
    ScaleSetCommand::RecordSessionLoss { cause }
}

pub const fn deleted(session_id: SessionId) -> ScaleSetCommand {
    ScaleSetCommand::RecordSessionDeleted { session_id }
}

pub const fn offer(id: u64, event_name: EventName) -> MessageItem {
    MessageItem::Offer(Offer {
        request_id: RunnerRequestId(id),
        event_name,
    })
}

pub const fn push(id: u64) -> MessageItem {
    offer(id, EventName::Push)
}

pub const fn started(id: u64) -> MessageItem {
    MessageItem::Fact(JobFact::Started {
        request_id: RunnerRequestId(id),
        runner_id: None,
        runner_name: None,
    })
}

pub const fn completed(id: u64) -> MessageItem {
    MessageItem::Fact(JobFact::Completed {
        request_id: RunnerRequestId(id),
        result: JobResult::Canceled,
        runner_id: None,
    })
}

pub const fn assigned_fact(id: u64) -> MessageItem {
    MessageItem::Fact(JobFact::Assigned {
        request_id: RunnerRequestId(id),
    })
}

pub const fn message(
    session_id: SessionId,
    id: u64,
    statistics: Option<Statistics>,
    items: Vec<MessageItem>,
) -> ScaleSetCommand {
    ScaleSetCommand::RecordMessage(RecordMessage {
        session_id,
        message_id: MessageId(id),
        statistics,
        items,
    })
}

pub fn ids(ids: &[u64]) -> BTreeSet<RunnerRequestId> {
    ids.iter().copied().map(RunnerRequestId).collect()
}

pub fn result(requested: &[u64], acquired: &[u64]) -> ScaleSetCommand {
    ScaleSetCommand::RecordAcquireResult {
        requested: ids(requested),
        acquired: ids(acquired),
    }
}

/// Decides `step` and folds what it accepted.
pub fn apply(
    state: &ScaleSetState,
    step: Step,
) -> Result<(ScaleSetState, Vec<ScaleSetEvent>), ScaleSetRejection> {
    let events = ScaleSet::decide(state, step)?;
    let next = events.iter().fold(state.clone(), ScaleSet::evolve);
    Ok((next, events))
}

/// The state after `bodies` from `state`, every one accepted.
///
/// # Panics
/// When a step is refused: the path is wrong, not the aggregate under test.
pub fn reach_from(
    state: ScaleSetState,
    bodies: impl IntoIterator<Item = ScaleSetCommand>,
) -> ScaleSetState {
    bodies
        .into_iter()
        .enumerate()
        .fold(state, |state, (index, body)| {
            apply(&state, auto(body))
                .unwrap_or_else(|r| panic!("step {index} of the path refused: {r:?}"))
                .0
        })
}

/// The state after `bodies`, every one accepted.
pub fn reach(bodies: impl IntoIterator<Item = ScaleSetCommand>) -> ScaleSetState {
    reach_from(ScaleSet::initial(), bodies)
}

/// The refusal code of `step` on `state`, or `"accepted"`.
pub fn outcome(state: &ScaleSetState, step: Step) -> String {
    match ScaleSet::decide(state, step) {
        Ok(_) => "accepted".to_owned(),
        Err(rejection) => rejection.code().to_owned(),
    }
}

/// Ensured, capacity 2 deliverable and open, a session opened with nothing assigned.
pub fn active_path() -> Vec<ScaleSetCommand> {
    vec![
        ensure(),
        capacity(1, 2, 2, WireMode::Open),
        ScaleSetCommand::BeginOpen,
        opened(FIRST, assigned(0)),
    ]
}

pub fn active() -> ScaleSetState {
    reach(active_path())
}

fn active_then(more: &[ScaleSetCommand]) -> ScaleSetState {
    let mut path = active_path();
    path.extend_from_slice(more);
    reach(path)
}

/// One state per phase, and the overlays: reservations open, a stale session pending.
pub fn matrix_states() -> Vec<(&'static str, ScaleSetState)> {
    vec![
        ("unconfigured", ScaleSet::initial()),
        ("ensured", reach([ensure()])),
        (
            "opening",
            reach([
                ensure(),
                capacity(1, 2, 2, WireMode::Open),
                ScaleSetCommand::BeginOpen,
            ]),
        ),
        ("active", active()),
        (
            "active_reserved",
            active_then(&[message(FIRST, 1, Some(assigned(0)), vec![push(1), push(2)])]),
        ),
        ("backoff_stale", active_then(&[loss(LossCause::Expired)])),
        (
            "backoff",
            active_then(&[loss(LossCause::Expired), deleted(FIRST)]),
        ),
        (
            "degraded",
            active_then(&[message(
                FIRST,
                1,
                None,
                vec![
                    MessageItem::Unknown,
                    MessageItem::Unknown,
                    MessageItem::Unknown,
                ],
            )]),
        ),
        ("closing", active_then(&[ScaleSetCommand::StartClosing])),
        (
            "closed",
            active_then(&[ScaleSetCommand::StartClosing, deleted(FIRST)]),
        ),
    ]
}

/// One stimulus of every command type (and the variants that decide differently).
pub fn stimuli() -> Vec<(&'static str, Step)> {
    vec![
        ("ensure", auto(ensure())),
        (
            "ensure_other",
            auto(ScaleSetCommand::EnsureScaleSet(EnsureScaleSet {
                pool: PoolName::sample(),
                github_id: ScaleSetId(8),
                tier: TrustTier::Trusted,
            })),
        ),
        ("begin_open", auto(ScaleSetCommand::BeginOpen)),
        ("opened_first", auto(opened(FIRST, assigned(1)))),
        ("opened_second", auto(opened(SECOND, assigned(1)))),
        ("loss_expired", auto(loss(LossCause::Expired))),
        ("loss_conflicted", auto(loss(LossCause::Conflicted))),
        ("deleted_first", auto(deleted(FIRST))),
        ("deleted_second", auto(deleted(SECOND))),
        (
            "message_first",
            auto(message(FIRST, 9, Some(assigned(1)), vec![push(3)])),
        ),
        (
            "message_second",
            auto(message(SECOND, 9, Some(assigned(1)), vec![push(3)])),
        ),
        ("acquire_answer", auto(result(&[1, 2], &[1]))),
        ("acquire_nothing", auto(result(&[], &[]))),
        ("capacity_next", auto(capacity(2, 3, 3, WireMode::Draining))),
        ("capacity_stale", auto(capacity(0, 0, 0, WireMode::Open))),
        ("recovered", auto(ScaleSetCommand::RecordSessionRecovered)),
        ("start_closing", auto(ScaleSetCommand::StartClosing)),
        ("stale_run", at(0, ScaleSetCommand::StartClosing)),
    ]
}
