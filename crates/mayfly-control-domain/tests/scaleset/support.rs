//! Paths through a scale set's life, driven through its public `decide` and `evolve`.

use std::collections::BTreeSet;

use mayfly_control_domain::attempt::RunnerRequestId;
use mayfly_control_domain::demand::{
    AssignedJob, Capacity, EventName, JobKey, Offer, RepositoryName, RunFacts, RunLookup,
    Statistics, TrustPolicy, WireMode, WorkflowJobId, WorkflowRunId,
};
use mayfly_control_domain::pool::TrustTier;
use mayfly_control_domain::registration::{JobResult, ScaleSetId};
use mayfly_control_domain::scaleset::{
    CancelAnswer, EnsureScaleSet, JobFact, LossCause, MessageId, MessageItem, RecordMessage,
    ScaleSet, ScaleSetCommand, ScaleSetEvent, ScaleSetRejection, ScaleSetState, SessionId,
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
        job: None,
    })
}

pub const fn completed(id: u64) -> MessageItem {
    MessageItem::Fact(JobFact::Completed {
        request_id: RunnerRequestId(id),
        result: JobResult::Canceled,
        runner_id: None,
        job: None,
    })
}

pub const fn assigned_fact(id: u64) -> MessageItem {
    MessageItem::Fact(JobFact::Assigned {
        request_id: RunnerRequestId(id),
        job: None,
    })
}

// ---- github.com's direct assignment (ADR 0090) ---------------------------------------------

/// The scale set's name, which an assignment routed to it carries among its labels.
pub const SCALE_SET_NAME: &str = "mayfly-linux";
/// The pool's one allowed repository.
pub const REPOSITORY: &str = "octo-org/app";

pub fn repository(name: &str) -> Option<RepositoryName> {
    Some(RepositoryName(name.to_owned()))
}

/// The pool's trust policy: [`REPOSITORY`] and the default events.
pub fn trust(cancel_refused_runs: bool) -> TrustPolicy {
    TrustPolicy {
        scale_set_name: SCALE_SET_NAME.to_owned(),
        allowed_repositories: [RepositoryName(REPOSITORY.to_owned())]
            .into_iter()
            .collect(),
        allowed_events: TrustPolicy::default_events(),
        cancel_refused_runs,
    }
}

pub fn policy(cancel_refused_runs: bool) -> ScaleSetCommand {
    ScaleSetCommand::ApplyTrustPolicy(trust(cancel_refused_runs))
}

pub fn key(n: u64) -> JobKey {
    JobKey {
        job_id: WorkflowJobId(format!("job-{n}")),
        run_id: WorkflowRunId(100 + n),
    }
}

/// A `JobAssigned` of job `n` on github.com (`runnerRequestId` 0), from [`REPOSITORY`].
pub fn assigned_job(n: u64, event_name: EventName) -> AssignedJob {
    AssignedJob {
        key: key(n),
        repository: repository(REPOSITORY),
        event_name,
        labels: vec![SCALE_SET_NAME.to_owned()],
    }
}

pub fn assign(n: u64) -> MessageItem {
    MessageItem::Fact(JobFact::Assigned {
        request_id: RunnerRequestId(0),
        job: Some(assigned_job(n, EventName::Push)),
    })
}

/// A `JobStarted` naming job `n` by its key (`runnerRequestId` 0).
pub fn started_job(n: u64) -> MessageItem {
    MessageItem::Fact(JobFact::Started {
        request_id: RunnerRequestId(0),
        runner_id: None,
        runner_name: None,
        job: Some(key(n)),
    })
}

/// A `JobCompleted` naming job `n` by its key, or naming none (`named` false).
pub fn completed_job(n: u64, named: bool) -> MessageItem {
    MessageItem::Fact(JobFact::Completed {
        request_id: RunnerRequestId(0),
        result: JobResult::Canceled,
        runner_id: None,
        job: named.then(|| key(n)),
    })
}

/// The run facts of a `push` (or any event) from [`REPOSITORY`]'s own branch.
pub fn run(event_name: EventName, head: &str) -> RunLookup {
    RunLookup::Found(RunFacts {
        repository: repository(REPOSITORY),
        head_repository: repository(head),
        event_name,
    })
}

/// The lookup of job `n`: trusted (`true`, a push) or untrusted (a `pull_request_target`).
pub fn lookup(n: u64, trusted: bool) -> ScaleSetCommand {
    let event_name = if trusted {
        EventName::Push
    } else {
        EventName::PullRequestTarget
    };
    ScaleSetCommand::RecordRunLookup {
        job: key(n),
        lookup: run(event_name, REPOSITORY),
    }
}

pub fn failed_lookup(n: u64) -> ScaleSetCommand {
    ScaleSetCommand::RecordRunLookup {
        job: key(n),
        lookup: RunLookup::Failed,
    }
}

/// An open session on github.com's flow, without or with `cancel_refused_runs`.
pub fn direct_path(cancel_refused_runs: bool) -> Vec<ScaleSetCommand> {
    let mut path = active_path();
    path.push(policy(cancel_refused_runs));
    path
}

pub fn direct(cancel_refused_runs: bool) -> ScaleSetState {
    reach(direct_path(cancel_refused_runs))
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

/// One state per phase, and the overlays: reservations open, a reopened session with a floor,
/// a stale session pending.
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
        (
            "active_reopened",
            active_then(&[
                message(FIRST, 3, Some(assigned(0)), vec![]),
                loss(LossCause::Restarted),
                deleted(FIRST),
                ScaleSetCommand::BeginOpen,
                opened(SECOND, assigned(0)),
            ]),
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
        (
            "active_refused",
            active_then(&[
                policy(false),
                message(FIRST, 1, Some(assigned(1)), vec![assign(1)]),
                lookup(1, false),
            ]),
        ),
        (
            "active_sink",
            active_then(&[
                policy(false),
                message(FIRST, 1, Some(assigned(1)), vec![assign(1)]),
                lookup(1, false),
                ScaleSetCommand::StartSink { job: key(1) },
            ]),
        ),
        (
            "active_pending_cancel",
            active_then(&[
                policy(true),
                message(FIRST, 1, Some(assigned(2)), vec![assign(1), assign(2)]),
                lookup(1, false),
            ]),
        ),
        ("closing", active_then(&[ScaleSetCommand::StartClosing])),
        (
            "closed",
            active_then(&[ScaleSetCommand::StartClosing, deleted(FIRST)]),
        ),
        (
            "vanished",
            active_then(&[
                loss(LossCause::Expired),
                deleted(FIRST),
                ScaleSetCommand::RecordScaleSetVanished {
                    github_id: ScaleSetId(7),
                },
            ]),
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
        (
            "message_second_at_floor",
            auto(message(SECOND, 3, Some(assigned(1)), vec![push(3)])),
        ),
        (
            "message_second_below_floor",
            auto(message(SECOND, 1, Some(assigned(1)), vec![push(3)])),
        ),
        ("acquire_answer", auto(result(&[1, 2], &[1]))),
        ("acquire_nothing", auto(result(&[], &[]))),
        ("capacity_next", auto(capacity(2, 3, 3, WireMode::Draining))),
        ("capacity_stale", auto(capacity(0, 0, 0, WireMode::Open))),
        ("recovered", auto(ScaleSetCommand::RecordSessionRecovered)),
        ("start_closing", auto(ScaleSetCommand::StartClosing)),
        ("stale_run", at(0, ScaleSetCommand::StartClosing)),
        ("trust_policy", auto(policy(false))),
        ("lookup_trusted", auto(lookup(2, true))),
        ("lookup_failed", auto(failed_lookup(1))),
        (
            "start_sink",
            auto(ScaleSetCommand::StartSink { job: key(1) }),
        ),
        ("sink_ended", auto(ScaleSetCommand::RecordSinkEnded)),
        (
            "cancel_answer",
            auto(ScaleSetCommand::RecordCancelAnswer {
                job: key(1),
                answer: CancelAnswer::Accepted,
            }),
        ),
        (
            "vanished_same",
            auto(ScaleSetCommand::RecordScaleSetVanished {
                github_id: ScaleSetId(7),
            }),
        ),
        (
            "recreated_new",
            auto(ScaleSetCommand::RecordScaleSetRecreated {
                github_id: ScaleSetId(9),
            }),
        ),
    ]
}
