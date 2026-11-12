//! The `ScaleSetApi` and `RunnerRegistry` contract suite (ver §5, `doc/github-scaleset.md`
//! §2–§7): what every implementation of the two ports does, written once over a
//! [`ScaleSetWorld`] factory and run on a fresh world per case.
//!
//! Implementations run (ver §5): [`crate::github::FakeScaleSetApi`] (this kit's
//! `tests/scaleset_contract.rs`); the GitHub adapter over `mayfly-fake-github` in both flows and
//! over the synthetic recorded fixtures of `tests/fixtures/github-scaleset/`, replayed
//! (`mayfly-adapter-github/tests/client/contract.rs`); a live disposable organisation, owner-gated
//! (HW-02). The deliberately broken [`crate::github::Broken`] fakes fail the cases named for
//! them.
//!
//! | Id | Case |
//! |:--|:--|
//! | SS-01 | ensure: an absent scale set is created, then found by name and by id (the ports have no delete: ADR 0040) |
//! | SS-02 | a session opens with its statistics; an empty hold answers no message, a cursor of 0 included (not sent) |
//! | SS-03 | a second open is a session conflict; after the delete one opens; a gone session's delete is done |
//! | SS-04 | direct assignment (`runnerRequestId` 0) keyed by `(jobId, workflowRunId)`; redelivery until ack; `lastMessageId` passes a delivered message |
//! | SS-05 | a queue token's expiry is refreshed inside the client; an expired session is lost, then a new one opens |
//! | SS-06 | a 429 with `Retry-After` is waited out, as long as it says |
//! | SS-07 | an ack whose answer was lost: acking again is done, and the message is not redelivered |
//! | SS-08 | GHES: offers are listed and acquired; an id not offered is not acquired; a re-acquire answers the id again |
//! | SS-09 | JIT: a runner is registered under its name; a taken name is refused; a throttle registers nothing |
//! | SS-10 | a poll's 200 with an empty body is unavailable and loses nothing |
//! | SS-11 | a scale set GitHub deleted reads as gone to a read and to a session open; deleting its session is done |
//! | SS-12 | a stale session's delete answering 404 is done, and the scale set is not read as gone |
//! | SS-13 | a session the client cannot use (a queue URL a token could leak through) is drift and leaks nothing: the next open succeeds |
//! | RR-01 | a runner is read by id and listed by exact name; an absent one is none |
//! | RR-02 | a removal removes the runner of that id and no other |
//! | RR-03 | removing an absent runner is gone |
//! | RR-04 | a busy runner's removal is refused as busy and the runner stays |
//!
//! [`check_scaleset_contract`] returns a [`Report`]; no case is skipped.

use std::collections::BTreeSet;
use std::fmt::Debug;
use std::future::Future;

use mayfly_control::github::{
    EventName, Failure, GitHubCallError, JobFact, MessageId, MessageItem, PolledMessage,
    RemovalOutcome, RepositoryName, RunnerId, RunnerRegistry, RunnerRequestId, ScaleSetApi,
    ScaleSetId, ScaleSetSpec, WorkflowJobId, WorkflowRunId,
};
use mayfly_domain::Millis;

/// The scale set every case ensures.
pub const SCALE_SET: &str = "mayfly-contract";
/// Its runner group.
pub const GROUP: &str = "Default";
/// Its label, which every queued job asks for.
pub const LABEL: &str = "mayfly-contract";
/// The owner name of every session.
pub const OWNER: &str = "contract-host/contract-pool";
/// The workflow run of the queued jobs.
pub const RUN: u64 = 7;
/// The repository of the queued jobs (`ownerName/repositoryName`).
pub const REPOSITORY: &str = "acme/app";

/// The assignment flow a case needs (spec §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// github.com: `JobAssigned` directly.
    Direct,
    /// GHES: offers and acquire.
    Acquire,
}

/// What a world makes GitHub do next to the scale set (spec §3, §4, §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorldFault {
    /// The live session's queue token stops working (spec §3.3).
    QueueTokenExpired {
        /// The scale set.
        scale_set: ScaleSetId,
    },
    /// The live session is gone (spec §3.1).
    SessionExpired {
        /// The scale set.
        scale_set: ScaleSetId,
    },
    /// The next poll answers 429 with `Retry-After: retry_after_secs` (spec §8).
    PollRateLimited {
        /// The scale set.
        scale_set: ScaleSetId,
        /// Its `Retry-After`.
        retry_after_secs: u64,
    },
    /// The next poll answers 200 with an empty body (spec §4.1).
    PollEmptyOk {
        /// The scale set.
        scale_set: ScaleSetId,
    },
    /// The next ack takes effect and its answer is lost (spec §4.4).
    AckAnswerLost {
        /// The scale set.
        scale_set: ScaleSetId,
    },
    /// JIT minting for the scale set is throttled past the retries (spec §8).
    JitThrottled {
        /// The scale set.
        scale_set: ScaleSetId,
    },
    /// GitHub deletes the scale set as dormant (spec §3.1).
    ScaleSetDeleted {
        /// The scale set.
        scale_set: ScaleSetId,
    },
    /// The next session delete answers 404 (a session the service no longer knows).
    SessionDeleteNotFound {
        /// The scale set.
        scale_set: ScaleSetId,
    },
    /// Sessions opened from now on carry a queue URL a token could leak through (plain http to
    /// a remote host).
    QueueUrlUnusable {
        /// The scale set.
        scale_set: ScaleSetId,
    },
    /// Sessions opened from now on carry a usable queue URL again.
    QueueUrlRestored {
        /// The scale set.
        scale_set: ScaleSetId,
    },
}

/// One world a case runs in: the ports under test and what GitHub's users do to it.
pub trait ScaleSetWorld: Sync {
    /// The scale-set port.
    type Api: ScaleSetApi;
    /// The runner port, over the same GitHub.
    type Registry: RunnerRegistry;

    /// The scale-set port.
    fn api(&self) -> &Self::Api;

    /// The runner port.
    fn registry(&self) -> &Self::Registry;

    /// Queues `count` jobs asking for `label` of workflow run `run_id`, pushed to
    /// [`REPOSITORY`].
    fn queue_jobs(&self, count: usize, label: &str, run_id: u64)
    -> impl Future<Output = ()> + Send;

    /// The runner `runner` takes the assigned job `job`.
    fn start_job(&self, job: &WorkflowJobId, runner: RunnerId) -> impl Future<Output = ()> + Send;

    /// Makes GitHub do `fault` (a recorded world already holds it in its exchanges).
    fn inject(&self, fault: WorldFault) -> impl Future<Output = ()> + Send;

    /// Every wait the implementation took so far, in order.
    fn slept(&self) -> Vec<Millis>;
}

/// One case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScaleSetCase {
    /// Its id.
    pub id: &'static str,
    /// The flow it needs.
    pub flow: Flow,
}

/// Every case, in order.
pub const CASES: [ScaleSetCase; 17] = [
    case("SS-01", Flow::Direct),
    case("SS-02", Flow::Direct),
    case("SS-03", Flow::Direct),
    case("SS-04", Flow::Direct),
    case("SS-05", Flow::Direct),
    case("SS-06", Flow::Direct),
    case("SS-07", Flow::Direct),
    case("SS-08", Flow::Acquire),
    case("SS-09", Flow::Direct),
    case("SS-10", Flow::Direct),
    case("SS-11", Flow::Direct),
    case("SS-12", Flow::Direct),
    case("SS-13", Flow::Direct),
    case("RR-01", Flow::Direct),
    case("RR-02", Flow::Direct),
    case("RR-03", Flow::Direct),
    case("RR-04", Flow::Direct),
];

const fn case(id: &'static str, flow: Flow) -> ScaleSetCase {
    ScaleSetCase { id, flow }
}

/// The outcome of a run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Cases that passed.
    pub passed: Vec<&'static str>,
    /// Cases that failed, with the first broken expectation.
    pub failed: Vec<(&'static str, String)>,
}

impl Report {
    /// The ids of the failed cases.
    #[must_use]
    pub fn failed_ids(&self) -> Vec<&'static str> {
        self.failed.iter().map(|(id, _)| *id).collect()
    }
}

/// Runs every case of [`CASES`], each on a fresh world `world(case)` builds.
pub async fn check_scaleset_contract<W, F, Fut>(world: F) -> Report
where
    W: ScaleSetWorld,
    F: Fn(ScaleSetCase) -> Fut,
    Fut: Future<Output = W>,
{
    let mut report = Report::default();
    for case in CASES {
        let w = world(case).await;
        match run(case.id, &w).await {
            Ok(()) => report.passed.push(case.id),
            Err(why) => report.failed.push((case.id, why)),
        }
    }
    report
}

/// Panics with the failed cases unless every case passed.
pub async fn assert_scaleset_contract<W, F, Fut>(world: F)
where
    W: ScaleSetWorld,
    F: Fn(ScaleSetCase) -> Fut,
    Fut: Future<Output = W>,
{
    let report = check_scaleset_contract(world).await;
    assert!(
        report.failed.is_empty(),
        "contract cases failed: {:#?}",
        report.failed
    );
}

type Outcome = Result<(), String>;

async fn run<W: ScaleSetWorld>(id: &str, w: &W) -> Outcome {
    match id {
        "SS-01" => ensure_case(w).await,
        "SS-02" => open_case(w).await,
        "SS-03" => conflict_case(w).await,
        "SS-04" => direct_case(w).await,
        "SS-05" => expiry_case(w).await,
        "SS-06" => retry_after_case(w).await,
        "SS-07" => lost_ack_case(w).await,
        "SS-08" => acquire_case(w).await,
        "SS-09" => jit_case(w).await,
        "SS-10" => empty_ok_case(w).await,
        "SS-11" => gone_case(w).await,
        "SS-12" => stale_delete_case(w).await,
        "SS-13" => unusable_session_case(w).await,
        "RR-01" => runner_read_case(w).await,
        "RR-02" => remove_by_id_case(w).await,
        "RR-03" => remove_absent_case(w).await,
        "RR-04" => busy_case(w).await,
        _ => Err(format!("no case {id}")),
    }
}

fn spec() -> ScaleSetSpec {
    ScaleSetSpec {
        name: SCALE_SET.to_owned(),
        runner_group: GROUP.to_owned(),
        labels: vec![LABEL.to_owned()],
    }
}

fn expect(holds: bool, what: &str) -> Outcome {
    if holds { Ok(()) } else { Err(what.to_owned()) }
}

fn ok<T>(result: Result<T, GitHubCallError>, what: &str) -> Result<T, String> {
    result.map_err(|e| format!("{what}: {e}"))
}

fn failed<T: Debug>(result: Result<T, GitHubCallError>, failure: Failure, what: &str) -> Outcome {
    match result {
        Err(e) if e.failure == failure => Ok(()),
        other => Err(format!("{what}: expected {failure:?}, got {other:?}")),
    }
}

fn message(polled: Option<PolledMessage>, what: &str) -> Result<PolledMessage, String> {
    polled.ok_or_else(|| format!("{what}: no message"))
}

/// The scale set, created (it was absent).
async fn ensure<W: ScaleSetWorld>(w: &W) -> Result<ScaleSetId, String> {
    let found = ok(w.api().find_scale_set(&spec()).await, "find before create")?;
    expect(found.is_none(), "the scale set existed before the create")?;
    let created = ok(w.api().create_scale_set(&spec()).await, "create")?;
    expect(
        created.name == SCALE_SET,
        "the created scale set has another name",
    )?;
    Ok(created.id)
}

/// The scale set with a session open.
async fn open<W: ScaleSetWorld>(w: &W) -> Result<ScaleSetId, String> {
    let set = ensure(w).await?;
    ok(w.api().create_session(set, OWNER).await, "open")?;
    Ok(set)
}

async fn ensure_case<W: ScaleSetWorld>(w: &W) -> Outcome {
    let set = ensure(w).await?;
    let found = ok(w.api().find_scale_set(&spec()).await, "find after create")?;
    expect(
        found.as_ref().map(|s| s.id) == Some(set),
        "find answers another id",
    )?;
    let read = ok(w.api().get_scale_set(set).await, "get by id")?;
    expect(read.name == SCALE_SET, "get answers another name")?;
    expect(
        read.labels.iter().any(|l| l == LABEL),
        "the scale set lacks its label",
    )
}

async fn open_case<W: ScaleSetWorld>(w: &W) -> Outcome {
    let set = ensure(w).await?;
    let opened = ok(w.api().create_session(set, OWNER).await, "open")?;
    expect(
        opened.statistics.total_assigned_jobs == 0,
        "a new scale set reports assigned jobs",
    )?;
    let polled = ok(w.api().poll(set, None, 0).await, "poll")?;
    expect(polled.is_none(), "an empty hold answered a message")?;
    let zero = ok(
        w.api().poll(set, Some(MessageId(0)), 0).await,
        "poll with cursor 0",
    )?;
    expect(zero.is_none(), "an empty hold answered a message")
}

async fn conflict_case<W: ScaleSetWorld>(w: &W) -> Outcome {
    let set = ensure(w).await?;
    let first = ok(w.api().create_session(set, OWNER).await, "open")?;
    failed(
        w.api().create_session(set, OWNER).await,
        Failure::SessionConflict,
        "a second open",
    )?;
    ok(
        w.api().delete_session(set, first.session_id).await,
        "delete",
    )?;
    ok(
        w.api().create_session(set, OWNER).await,
        "open after the delete",
    )?;
    ok(
        w.api().delete_session(set, first.session_id).await,
        "delete of a gone session",
    )
}

/// The one `JobAssigned` of `m`, checked as spec §4.3 reads github.com's.
fn assigned_job(m: &PolledMessage) -> Result<WorkflowJobId, String> {
    match m.items.as_slice() {
        [
            MessageItem::Fact(JobFact::Assigned {
                request_id: RunnerRequestId(0),
                job: Some(job),
            }),
        ] => {
            expect(job.key.run_id == WorkflowRunId(RUN), "the run id differs")?;
            expect(
                job.repository == Some(RepositoryName(REPOSITORY.to_owned())),
                "the repository differs",
            )?;
            expect(job.event_name == EventName::Push, "the event differs")?;
            expect(job.labels == [LABEL], "the labels differ")?;
            Ok(job.key.job_id.clone())
        }
        other => Err(format!("expected one direct JobAssigned, got {other:?}")),
    }
}

async fn direct_case<W: ScaleSetWorld>(w: &W) -> Outcome {
    let set = open(w).await?;
    w.queue_jobs(2, LABEL, RUN).await;
    let first = message(ok(w.api().poll(set, None, 1).await, "poll")?, "poll")?;
    let first_job = assigned_job(&first)?;
    expect(
        first.statistics.map(|s| s.total_assigned_jobs) == Some(1),
        "capacity 1 assigned other than one job",
    )?;
    let again = message(ok(w.api().poll(set, None, 1).await, "repoll")?, "repoll")?;
    expect(
        again.message_id == first.message_id,
        "an unacked message was not redelivered",
    )?;
    let second = message(
        ok(
            w.api().poll(set, Some(first.message_id), 2).await,
            "poll past the cursor",
        )?,
        "poll past the cursor",
    )?;
    expect(
        second.message_id > first.message_id,
        "lastMessageId did not pass the delivered message",
    )?;
    expect(
        assigned_job(&second)? != first_job,
        "the second job is the first",
    )?;
    ok(w.api().ack(set, first.message_id).await, "ack")?;
    ok(w.api().ack(set, second.message_id).await, "ack")?;
    let after = ok(
        w.api().poll(set, Some(second.message_id), 2).await,
        "poll after acks",
    )?;
    expect(after.is_none(), "an acked message came back")
}

async fn expiry_case<W: ScaleSetWorld>(w: &W) -> Outcome {
    let set = open(w).await?;
    w.inject(WorldFault::QueueTokenExpired { scale_set: set })
        .await;
    let polled = ok(
        w.api().poll(set, None, 0).await,
        "poll on an expired queue token",
    )?;
    expect(polled.is_none(), "an empty hold answered a message")?;
    w.inject(WorldFault::SessionExpired { scale_set: set })
        .await;
    failed(
        w.api().poll(set, None, 0).await,
        Failure::SessionExpired,
        "poll on an expired session",
    )?;
    failed(
        w.api().poll(set, None, 0).await,
        Failure::NoSession,
        "poll after the session was lost",
    )?;
    ok(
        w.api().create_session(set, OWNER).await,
        "open after the loss",
    )
    .map(drop)
}

async fn retry_after_case<W: ScaleSetWorld>(w: &W) -> Outcome {
    let set = open(w).await?;
    w.inject(WorldFault::PollRateLimited {
        scale_set: set,
        retry_after_secs: 7,
    })
    .await;
    let polled = ok(w.api().poll(set, None, 0).await, "poll after a 429")?;
    expect(polled.is_none(), "an empty hold answered a message")?;
    expect(
        w.slept().contains(&Millis::new(7_000)),
        "the Retry-After of 7 s was not waited",
    )
}

async fn lost_ack_case<W: ScaleSetWorld>(w: &W) -> Outcome {
    let set = open(w).await?;
    w.queue_jobs(1, LABEL, RUN).await;
    let m = message(ok(w.api().poll(set, None, 1).await, "poll")?, "poll")?;
    w.inject(WorldFault::AckAnswerLost { scale_set: set }).await;
    let _first = w.api().ack(set, m.message_id).await;
    ok(w.api().ack(set, m.message_id).await, "ack again")?;
    let after = ok(
        w.api().poll(set, Some(m.message_id), 1).await,
        "poll after the ack",
    )?;
    expect(after.is_none(), "the acked message came back")
}

async fn acquire_case<W: ScaleSetWorld>(w: &W) -> Outcome {
    let set = open(w).await?;
    w.queue_jobs(2, LABEL, RUN).await;
    let m = message(ok(w.api().poll(set, None, 2).await, "poll")?, "poll")?;
    let offered: BTreeSet<RunnerRequestId> = m
        .items
        .iter()
        .filter_map(|i| match i {
            MessageItem::Offer(o) if o.request_id.0 > 0 && o.event_name == EventName::Push => {
                Some(o.request_id)
            }
            _ => None,
        })
        .collect();
    expect(offered.len() == 2, "expected two offers with request ids")?;
    ok(w.api().ack(set, m.message_id).await, "ack")?;
    let listed: BTreeSet<RunnerRequestId> = ok(w.api().acquirable_jobs(set).await, "acquirable")?
        .into_iter()
        .map(|o| o.request_id)
        .collect();
    expect(listed == offered, "acquirable jobs differ from the offers")?;
    let not_offered = RunnerRequestId(offered.iter().map(|r| r.0).max().unwrap_or(0) + 1_000_000);
    let mut asked = offered.clone();
    asked.insert(not_offered);
    let acquired = ok(w.api().acquire_jobs(set, &asked).await, "acquire")?;
    expect(
        acquired == offered,
        "the acquired set is not exactly the offered ids",
    )?;
    let one: BTreeSet<RunnerRequestId> = offered.iter().take(1).copied().collect();
    let again = ok(w.api().acquire_jobs(set, &one).await, "re-acquire")?;
    expect(again == one, "a re-acquire did not answer the id")
}

async fn jit_case<W: ScaleSetWorld>(w: &W) -> Outcome {
    let set = ensure(w).await?;
    let jit = ok(
        w.api().generate_jit_config(set, "mf-contract-a", "").await,
        "jit",
    )?;
    expect(
        jit.runner.name == "mf-contract-a",
        "the runner has another name",
    )?;
    expect(!jit.encoded.expose().is_empty(), "the JIT config is empty")?;
    expect(
        !format!("{jit:?}").contains(jit.encoded.expose().as_str()),
        "the JIT config printed through Debug",
    )?;
    failed(
        w.api().generate_jit_config(set, "mf-contract-a", "").await,
        Failure::NameTaken,
        "a taken name",
    )?;
    w.inject(WorldFault::JitThrottled { scale_set: set }).await;
    failed(
        w.api().generate_jit_config(set, "mf-contract-b", "").await,
        Failure::Throttled,
        "a throttled mint",
    )?;
    let minted = ok(w.registry().find_runners("mf-contract-b").await, "find")?;
    expect(minted.is_empty(), "a throttled mint registered a runner")
}

async fn empty_ok_case<W: ScaleSetWorld>(w: &W) -> Outcome {
    let set = open(w).await?;
    w.queue_jobs(1, LABEL, RUN).await;
    w.inject(WorldFault::PollEmptyOk { scale_set: set }).await;
    failed(
        w.api().poll(set, None, 1).await,
        Failure::Unavailable,
        "a 200 with an empty body",
    )?;
    let m = message(
        ok(w.api().poll(set, None, 1).await, "poll")?,
        "poll after it",
    )?;
    assigned_job(&m).map(drop)
}

async fn gone_case<W: ScaleSetWorld>(w: &W) -> Outcome {
    let set = ensure(w).await?;
    let opened = ok(w.api().create_session(set, OWNER).await, "open")?;
    w.inject(WorldFault::ScaleSetDeleted { scale_set: set })
        .await;
    failed(
        w.api().get_scale_set(set).await,
        Failure::ScaleSetGone,
        "get",
    )?;
    failed(
        w.api().create_session(set, OWNER).await,
        Failure::ScaleSetGone,
        "open",
    )?;
    ok(
        w.api().delete_session(set, opened.session_id).await,
        "delete of the gone scale set's session",
    )?;
    let found = ok(w.api().find_scale_set(&spec()).await, "find")?;
    expect(found.is_none(), "a deleted scale set is still found")
}

async fn stale_delete_case<W: ScaleSetWorld>(w: &W) -> Outcome {
    let set = ensure(w).await?;
    let opened = ok(w.api().create_session(set, OWNER).await, "open")?;
    w.inject(WorldFault::SessionDeleteNotFound { scale_set: set })
        .await;
    ok(
        w.api().delete_session(set, opened.session_id).await,
        "a delete answering 404",
    )?;
    ok(
        w.api().get_scale_set(set).await,
        "the scale set after the delete",
    )
    .map(drop)
}

async fn unusable_session_case<W: ScaleSetWorld>(w: &W) -> Outcome {
    let set = ensure(w).await?;
    w.inject(WorldFault::QueueUrlUnusable { scale_set: set })
        .await;
    failed(
        w.api().create_session(set, OWNER).await,
        Failure::ProtocolDrift,
        "an open with an unusable queue URL",
    )?;
    failed(
        w.api().poll(set, None, 0).await,
        Failure::NoSession,
        "a poll after the unusable open",
    )?;
    w.inject(WorldFault::QueueUrlRestored { scale_set: set })
        .await;
    ok(w.api().create_session(set, OWNER).await, "the next open").map(drop)
}

async fn mint<W: ScaleSetWorld>(w: &W, set: ScaleSetId, name: &str) -> Result<RunnerId, String> {
    Ok(ok(w.api().generate_jit_config(set, name, "").await, "jit")?
        .runner
        .id)
}

async fn runner_read_case<W: ScaleSetWorld>(w: &W) -> Outcome {
    let set = ensure(w).await?;
    let id = mint(w, set, "mf-contract-r1").await?;
    let read = ok(w.registry().get_runner(id).await, "get")?;
    expect(
        read.as_ref().map(|r| r.name.as_str()) == Some("mf-contract-r1"),
        "get answers another runner",
    )?;
    let found = ok(w.registry().find_runners("mf-contract-r1").await, "find")?;
    expect(
        found.iter().map(|r| r.id).collect::<Vec<_>>() == [id],
        "find answers other runners",
    )?;
    let none = ok(w.registry().find_runners("mf-contract-none").await, "find")?;
    expect(none.is_empty(), "find answers runners of another name")?;
    let absent = ok(
        w.registry().get_runner(RunnerId(id.0 + 1_000_000)).await,
        "get",
    )?;
    expect(absent.is_none(), "an absent runner reads as present")
}

async fn remove_by_id_case<W: ScaleSetWorld>(w: &W) -> Outcome {
    let set = ensure(w).await?;
    let a = mint(w, set, "mf-contract-r2").await?;
    let b = mint(w, set, "mf-contract-r2b").await?;
    let removed = ok(w.registry().remove_runner(a).await, "remove")?;
    expect(removed == RemovalOutcome::Gone, "a removal was not gone")?;
    let a_after = ok(w.registry().get_runner(a).await, "get")?;
    expect(a_after.is_none(), "the removed runner remains")?;
    let b_after = ok(w.registry().get_runner(b).await, "get")?;
    expect(b_after.is_some(), "a runner of another id was removed")
}

async fn remove_absent_case<W: ScaleSetWorld>(w: &W) -> Outcome {
    let set = ensure(w).await?;
    let a = mint(w, set, "mf-contract-r3").await?;
    ok(w.registry().remove_runner(a).await, "remove")?;
    let again = ok(w.registry().remove_runner(a).await, "remove again")?;
    expect(
        again == RemovalOutcome::Gone,
        "removing an absent runner was not gone",
    )
}

async fn busy_case<W: ScaleSetWorld>(w: &W) -> Outcome {
    let set = open(w).await?;
    w.queue_jobs(1, LABEL, RUN).await;
    let m = message(ok(w.api().poll(set, None, 1).await, "poll")?, "poll")?;
    let job = assigned_job(&m)?;
    ok(w.api().ack(set, m.message_id).await, "ack")?;
    let runner = mint(w, set, "mf-contract-r4").await?;
    w.start_job(&job, runner).await;
    let removed = ok(w.registry().remove_runner(runner).await, "remove")?;
    expect(
        removed == RemovalOutcome::Busy,
        "a busy runner's removal was not busy",
    )?;
    let still = ok(w.registry().get_runner(runner).await, "get")?;
    expect(still.is_some(), "the busy runner was removed")
}
