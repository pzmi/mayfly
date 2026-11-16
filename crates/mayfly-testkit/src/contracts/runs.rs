//! The `WorkflowRuns` contract suite (ver §5, `doc/github-scaleset.md` §2, §6.2, ADR 0090): the
//! workflow-run lookup of an assigned job and the cancel of a refused run, written once over a
//! [`RunsWorld`] factory and run on a fresh world per case.
//!
//! Implementations run (ver §5): [`crate::github::FakeScaleSetApi`] (this kit's
//! `tests/runs_contract.rs`); the GitHub adapter over `mayfly-fake-github` and over the synthetic
//! recorded fixtures of `tests/fixtures/github-scaleset/`, replayed
//! (`mayfly-adapter-github/tests/client/runs.rs` and `replay.rs`); a live disposable
//! organisation, owner-gated (HW-02). The deliberately broken
//! [`crate::github::Broken::ForkBlindRuns`] fails WR-02, and
//! [`crate::github::Broken::RetryingCancelRuns`] CR-02.
//!
//! | Id | Case |
//! |:--|:--|
//! | WR-01 | a pushed run is looked up: its id, its repository as base and head, event, actor |
//! | WR-02 | a fork's pull request is looked up as one: its head repository is not its base |
//! | WR-03 | an absent run, or a run asked under another repository, is a failed lookup |
//! | WR-04 | a lookup that never answers is unavailable within its deadline, never a run |
//! | WR-05 | a run asked under its repository's name in another case is found, named as GitHub's |
//! | CR-01 | a live run's cancel is accepted; asked again once it is over, refused |
//! | CR-02 | a cancel whose answer is lost is an error, and it took effect: asked again, refused |
//! | CR-03 | the cancel of an absent run is refused |
//!
//! A lookup's error is for the caller to read as `RunLookup::Failed`
//! (`mayfly_control::github::run_lookup`), so every failing case here refuses the job.
//! [`check_runs_contract`] returns a [`Report`]; no case is skipped.

use std::fmt::Debug;
use std::future::Future;

use mayfly_control::github::{
    CancelAnswer, EventName, Failure, GitHubCallError, RepositoryName, WorkflowRunId, WorkflowRuns,
};

pub use crate::contracts::scaleset::Report;

/// The base repository of every run.
pub const REPOSITORY: &str = "acme/app";
/// The fork every fork's run comes from.
pub const FORK: &str = "contributor/app";
/// The actor of every run.
pub const ACTOR: &str = "octo";
/// The label every run's job asks for.
pub const LABEL: &str = "mayfly-contract";

/// A workflow run GitHub's users start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSpec {
    /// Its id.
    pub id: u64,
    /// The base repository (`owner/name`), where its jobs are queued.
    pub repository: RepositoryName,
    /// Its head repository: the base's own, or a fork's.
    pub head_repository: RepositoryName,
    /// Its event.
    pub event: EventName,
    /// Its actor's login.
    pub actor: String,
}

impl RunSpec {
    /// A push to [`REPOSITORY`] by [`ACTOR`].
    #[must_use]
    pub fn push(id: u64) -> Self {
        Self {
            id,
            repository: RepositoryName(REPOSITORY.to_owned()),
            head_repository: RepositoryName(REPOSITORY.to_owned()),
            event: EventName::Push,
            actor: ACTOR.to_owned(),
        }
    }

    /// A pull request to [`REPOSITORY`] from [`FORK`].
    #[must_use]
    pub fn fork_pull_request(id: u64) -> Self {
        Self {
            head_repository: RepositoryName(FORK.to_owned()),
            event: EventName::PullRequest,
            actor: "contributor".to_owned(),
            ..Self::push(id)
        }
    }
}

/// What a world makes GitHub do next to a run's REST calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunFault {
    /// Every lookup of the run goes unanswered until the caller's deadline.
    LookupStalls {
        /// The run.
        run: u64,
    },
    /// The run's next cancel takes effect and its answer is lost.
    CancelAnswerLost {
        /// The run.
        run: u64,
    },
}

/// One world a case runs in: the port under test and what GitHub's users do to it.
pub trait RunsWorld: Sync {
    /// The port.
    type Runs: WorkflowRuns;

    /// The port.
    fn runs(&self) -> &Self::Runs;

    /// Starts `run` with one job queued, asking for [`LABEL`].
    fn start_run(&self, run: &RunSpec) -> impl Future<Output = ()> + Send;

    /// Makes GitHub do `fault` (a recorded world already holds it in its exchanges).
    fn inject(&self, fault: RunFault) -> impl Future<Output = ()> + Send;
}

/// One case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunsCase {
    /// Its id.
    pub id: &'static str,
}

/// Every case, in order.
pub const CASES: [RunsCase; 8] = [
    RunsCase { id: "WR-01" },
    RunsCase { id: "WR-02" },
    RunsCase { id: "WR-03" },
    RunsCase { id: "WR-04" },
    RunsCase { id: "WR-05" },
    RunsCase { id: "CR-01" },
    RunsCase { id: "CR-02" },
    RunsCase { id: "CR-03" },
];

/// Runs every case of [`CASES`], each on a fresh world `world(case)` builds.
pub async fn check_runs_contract<W, F, Fut>(world: F) -> Report
where
    W: RunsWorld,
    F: Fn(RunsCase) -> Fut,
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
pub async fn assert_runs_contract<W, F, Fut>(world: F)
where
    W: RunsWorld,
    F: Fn(RunsCase) -> Fut,
    Fut: Future<Output = W>,
{
    let report = check_runs_contract(world).await;
    assert!(
        report.failed.is_empty(),
        "contract cases failed: {:#?}",
        report.failed
    );
}

type Outcome = Result<(), String>;

async fn run<W: RunsWorld>(id: &str, w: &W) -> Outcome {
    match id {
        "WR-01" => push_case(w).await,
        "WR-02" => fork_case(w).await,
        "WR-03" => absent_case(w).await,
        "WR-04" => stalled_case(w).await,
        "WR-05" => case_insensitive_case(w).await,
        "CR-01" => cancel_case(w).await,
        "CR-02" => lost_cancel_case(w).await,
        "CR-03" => absent_cancel_case(w).await,
        _ => Err(format!("no case {id}")),
    }
}

fn expect(holds: bool, what: &str) -> Outcome {
    if holds { Ok(()) } else { Err(what.to_owned()) }
}

fn ok<T>(result: Result<T, GitHubCallError>, what: &str) -> Result<T, String> {
    result.map_err(|e| format!("{what}: {e}"))
}

fn base() -> RepositoryName {
    RepositoryName(REPOSITORY.to_owned())
}

async fn look_up<W: RunsWorld>(
    w: &W,
    repository: &RepositoryName,
    run: u64,
) -> Result<mayfly_control::github::WorkflowRun, GitHubCallError> {
    w.runs().workflow_run(repository, WorkflowRunId(run)).await
}

async fn push_case<W: RunsWorld>(w: &W) -> Outcome {
    let spec = RunSpec::push(11);
    w.start_run(&spec).await;
    let run = ok(look_up(w, &base(), 11).await, "lookup")?;
    expect(
        run.id == WorkflowRunId(11),
        "the lookup answered another run",
    )?;
    expect(run.repository == base(), "the base repository differs")?;
    expect(
        run.head_repository == base(),
        "a pushed run's head is not its base",
    )?;
    expect(run.event == EventName::Push, "the event differs")?;
    expect(
        run.actor.as_deref() == Some(ACTOR),
        "the actor differs or is missing",
    )
}

async fn fork_case<W: RunsWorld>(w: &W) -> Outcome {
    w.start_run(&RunSpec::fork_pull_request(12)).await;
    let run = ok(look_up(w, &base(), 12).await, "lookup")?;
    expect(run.repository == base(), "the base repository differs")?;
    expect(
        run.head_repository == RepositoryName(FORK.to_owned()),
        "a fork's run was not looked up as one: its head is not the fork",
    )?;
    expect(run.event == EventName::PullRequest, "the event differs")
}

async fn absent_case<W: RunsWorld>(w: &W) -> Outcome {
    w.start_run(&RunSpec::push(13)).await;
    let absent = look_up(w, &base(), 14).await;
    expect(absent.is_err(), "an absent run was looked up")?;
    let elsewhere = look_up(w, &RepositoryName("acme/other".to_owned()), 13).await;
    expect(
        elsewhere.is_err(),
        "a run was looked up under a repository it does not belong to",
    )
}

async fn stalled_case<W: RunsWorld>(w: &W) -> Outcome {
    w.start_run(&RunSpec::push(15)).await;
    w.inject(RunFault::LookupStalls { run: 15 }).await;
    failed(
        look_up(w, &base(), 15).await,
        Failure::Unavailable,
        "a stalled lookup",
    )
}

async fn case_insensitive_case<W: RunsWorld>(w: &W) -> Outcome {
    w.start_run(&RunSpec::push(19)).await;
    let asked = RepositoryName("ACME/App".to_owned());
    let run = ok(look_up(w, &asked, 19).await, "lookup in another case")?;
    expect(
        run.repository == base(),
        "the repository is not named as GitHub writes it",
    )
}

fn failed<T: Debug>(result: Result<T, GitHubCallError>, failure: Failure, what: &str) -> Outcome {
    match result {
        Err(e) if e.failure == failure => Ok(()),
        other => Err(format!("{what}: expected {failure:?}, got {other:?}")),
    }
}

async fn cancel<W: RunsWorld>(w: &W, run: u64) -> Result<CancelAnswer, GitHubCallError> {
    w.runs().cancel_run(&base(), WorkflowRunId(run)).await
}

async fn cancel_case<W: RunsWorld>(w: &W) -> Outcome {
    w.start_run(&RunSpec::fork_pull_request(16)).await;
    let first = ok(cancel(w, 16).await, "cancel")?;
    expect(
        first == CancelAnswer::Accepted,
        "a live run's cancel was not accepted",
    )?;
    let again = ok(cancel(w, 16).await, "cancel again")?;
    expect(
        again == CancelAnswer::Refused,
        "a cancelled run's cancel was not refused",
    )
}

async fn lost_cancel_case<W: RunsWorld>(w: &W) -> Outcome {
    w.start_run(&RunSpec::fork_pull_request(17)).await;
    w.inject(RunFault::CancelAnswerLost { run: 17 }).await;
    failed(cancel(w, 17).await, Failure::Unavailable, "a lost cancel")?;
    let again = ok(cancel(w, 17).await, "cancel again")?;
    expect(
        again == CancelAnswer::Refused,
        "the lost cancel did not take effect",
    )
}

async fn absent_cancel_case<W: RunsWorld>(w: &W) -> Outcome {
    let answer = ok(cancel(w, 18).await, "cancel of an absent run")?;
    expect(
        answer == CancelAnswer::Refused,
        "an absent run's cancel was not refused",
    )
}
