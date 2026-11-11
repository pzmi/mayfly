//! The projection contract suite (ver §5, `doc/event-store.md` §7.1): one set of cases every
//! store that keeps inline projections passes, written once over a [`ProjectionFactory`].
//!
//! Cases are named by their id (`PC-01` …) and hold ES-I9 (inline rows atomic with their
//! unit), ES-I10 (locality) and ES-I11 (rebuild equals live; mayfly I16). They drive the
//! production `AggregateStore` (its inline fold) and `prepare` (the offline rebuild) over the
//! factory's store, with the `Pool` aggregate's `pools` and `recent_failures` projections by
//! default: a pool's rows are created, changed and deleted (a failure leaves the breaker's
//! window, a reset clears it), which is what every case needs. PC-07..PC-09 (the online
//! rebuild) join with ES-12; PC-10..PC-12 (ES-04F) hold that a missed write heals and that the
//! offline rebuild's cutover is guarded against a unit and against a second rebuild.
//! [`rebuild_equals_live_over`] runs "rebuild equals live" over any aggregate's histories
//! ([`pool_history`], [`host_history`], [`attempt_history`]), the property's body on every
//! store.
//!
//! [`check_projection_contract`] returns the failed cases instead of panicking: the suite's own
//! tests show the broken stores, specs and rebuild decisions of §7.6 failing exactly their
//! cases (`crates/mayfly-testkit/tests/projection_contract.rs`). A factory that cannot inject a
//! fault a case needs fails that case: no case is skipped.

use std::collections::BTreeMap;
use std::fmt::{self, Debug, Display};
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use mayfly_control::journal::{
    AggregateName, AppendError, Appended, CommandReceipt, EventLocation, ExpectedRevision,
    InboxKey, Journal, MAX_PAGE, MalformedUnit, NewEvent, Payload, ProjectionName, ProjectionWrite,
    RecordedEvent, RowWrite, StorageError, StoredKind, StreamHead, StreamId, StreamRevision,
    SupportedEpochs, UnitOfWork,
};
use mayfly_control::projection::views::{Attempts, PendingEffects, Pools, RecentFailures, Slots};
use mayfly_control::projection::{
    BuildId, Cutover, EventMeta, Projection, ProjectionRebuild, ProjectionSpec, ProjectionStore,
    Projections, RowChange, Rows, SpecEvent, SpecState, needs_rebuild, prepare, rebuild_projection,
};
use mayfly_control::saga::SAGA;
use mayfly_control::store::{
    AggregateStore, NoNotifier, Outcome, SaveError, SaveRequest, stream_of,
};
use mayfly_control_domain::attempt::{
    Attempt, AttemptCommand, Fact, GuestOs, Observation, RefusalReason, Stage,
};
use mayfly_control_domain::capacity::ReportedCapacity;
use mayfly_control_domain::host::{
    ApplyPoolTerms, Host, HostCommand, ReleaseEvidence, ReserveSlot,
};
use mayfly_control_domain::pool::{ConfigurePool, Pool, PoolCommand, TrustTier};
use mayfly_control_domain::{Actor, Aggregate, Command, ControlAggregates, EventKind, EventSet};
use mayfly_domain::{
    AttemptId, CommandId, Deadline, DirectiveId, Epoch, EventId, Generation, ImageDigest, Millis,
    ObservationId, PoolName, ProviderInstanceId, Resources, Sample,
};
use serde::{Deserialize, Serialize};

use crate::block_on::block_on;
use crate::clock::{FakeClock, SeededRng};
use crate::contracts::journal::InjectedFault;

/// Makes the stores the suite runs against. Every method is required, so an adapter cannot
/// skip a mode by omission.
pub trait ProjectionFactory {
    /// The store under test: the journal whose units write the rows, and the projection ports.
    type Store: Journal + ProjectionStore + ProjectionRebuild + Clone;

    /// A new, empty store.
    fn fresh(&self) -> impl Future<Output = Self::Store>;

    /// The same store, closed and opened again (or itself, for a store with nothing to reopen).
    fn reopen(&self, store: Self::Store) -> impl Future<Output = Self::Store>;

    /// Arms `fault` for the next append on the store; `false` when this factory cannot.
    fn inject(&self, store: &Self::Store, fault: InjectedFault) -> impl Future<Output = bool>;
}

macro_rules! cases {
    ($($variant:ident => $id:literal, $name:literal, $doc:literal;)+) => {
        /// One case of the projection contract, named after its id in `doc/event-store.md` §7.1.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum ProjectionCase {
            $(#[doc = $doc] $variant,)+
        }

        impl ProjectionCase {
            /// Every case, in the order the suite runs them.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// The case's id in `doc/event-store.md` §7.1.
            #[must_use]
            pub const fn id(self) -> &'static str {
                match self { $(Self::$variant => $id,)+ }
            }

            /// The case's `snake_case` name.
            #[must_use]
            pub const fn name(self) -> &'static str {
                match self { $(Self::$variant => $name,)+ }
            }
        }
    };
}

cases! {
    RebuildEqualsLive => "PC-01", "rebuild_equals_live", "PC-01: a rebuild by stream enumeration equals the live rows, each stream's rows are the fold of the stream at the revision applied, and the applied revision is the stream's head (ES-I11, I16).";
    VersionBumpRebuilds => "PC-02", "version_bump_rebuilds", "PC-02: a version bump rebuilds at the next start; until the cutover readers see the old generation only, a rebuild cannot write the active one, and after it the new generation only.";
    InlineRowsCommit => "PC-03", "inline_rows_commit", "PC-03: inline rows commit with their events: after every command each stream's rows (puts and deletes) are the fold at the new head, and survive a reopen (ES-I9).";
    RefusedUnitLeavesNone => "PC-04", "refused_unit_leaves_none", "PC-04: a refused unit (a domain refusal, a stale revision, a store failure) leaves no row and no applied revision (ES-I1, ES-I9).";
    OutsideUnitRefused => "PC-05", "outside_unit_refused", "PC-05: a projection write for a stream the unit does not append to, or a row key of another stream, is Malformed(ProjectionOutsideUnit) and writes nothing (ES-I10).";
    VersionBelowRebuilds => "PC-06", "version_below_rebuilds", "PC-06: a version below the stored one rebuilds too, and so does another build; the same version and build do not (doc/event-store.md section 6.1).";
    MissedRowsHeal => "PC-10", "missed_rows_heal", "PC-10: events appended without their rows (an older build, a store kept without the projection) heal: the stream's next unit puts its whole fold, and a restart under the same build rebuilds every projection with a stream behind its head (ES-04F).";
    RebuildWaitsForUnits => "PC-11", "rebuild_waits_for_units", "PC-11: a cutover is refused, changing nothing, while a stream of the aggregate is missing from the new generation or moved after it was folded (a unit committed during the offline rebuild); the rebuild folds again and its rows equal the fold (ES-04F).";
    OneRebuildWins => "PC-12", "one_rebuild_wins", "PC-12: a second rebuild of a spec claims a new generation and supersedes the first, whose writes and cutover are refused; readers see the old rows until the winner's complete generation is cut over (ES-04F).";
}

impl Display for ProjectionCase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.id(), self.name())
    }
}

/// A failed case and the first check in it that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionFailure {
    /// The case.
    pub case: ProjectionCase,
    /// What was expected and what the store did.
    pub detail: String,
}

/// The outcome of a contract run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectionContractReport {
    /// Every failed case, in run order.
    pub failures: Vec<ProjectionFailure>,
}

impl ProjectionContractReport {
    /// Whether every case passed.
    #[must_use]
    pub fn is_green(&self) -> bool {
        self.failures.is_empty()
    }

    /// The ids of the failed cases, in run order.
    #[must_use]
    pub fn failed_ids(&self) -> Vec<&'static str> {
        self.failures.iter().map(|f| f.case.id()).collect()
    }
}

impl Display for ProjectionContractReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_green() {
            return f.write_str("projection contract: every case passed");
        }
        writeln!(
            f,
            "projection contract: {} case(s) failed",
            self.failures.len()
        )?;
        for failure in &self.failures {
            writeln!(f, "- {}: {}", failure.case, failure.detail)?;
        }
        Ok(())
    }
}

/// How the suite decides which projections a start rebuilds: production's [`needs_rebuild`],
/// or a negative control the suite's own tests run to show PC-02 and PC-06 are not vacuous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SuitePrepare {
    /// [`prepare`], as composition runs it.
    #[default]
    Production,
    /// Rebuilds only when the spec's version is above the stored one (or nothing is stored):
    /// a rolled-back build would read rows a newer spec wrote (PC-06 must catch it).
    UpwardOnly,
    /// Rebuilds only when nothing is stored (PC-02 and PC-06 must catch it).
    VersionBlind,
    /// Rebuilds on another version or build only, blind to a stream behind its head (PC-10
    /// must catch it).
    StaleBlind,
}

/// What the suite runs with: the projections the store keeps for `Pool` streams and how a
/// start decides to rebuild. The default is the control plane's `pools` and `recent_failures`.
#[derive(Debug, Clone)]
pub struct SuiteOptions {
    /// The projections kept inline, all of `Pool` streams.
    pub projections: Projections,
    /// The rebuild decision.
    pub prepare: SuitePrepare,
}

impl Default for SuiteOptions {
    fn default() -> Self {
        Self {
            projections: pool_projections(),
            prepare: SuitePrepare::Production,
        }
    }
}

/// The control plane's projections of `Pool` streams: `pools` and `recent_failures`.
///
/// # Panics
/// Never: both specs have valid names.
#[must_use]
pub fn pool_projections() -> Projections {
    let built = [
        Projection::of::<Pools>(),
        Projection::of::<RecentFailures>(),
    ]
    .into_iter()
    .collect::<Result<Vec<_>, _>>()
    .and_then(Projections::new);
    built.unwrap_or_else(|e| panic!("pool projections: {e}"))
}

/// Runs every case against stores from `factory` with the default options.
pub async fn check_projection_contract<F: ProjectionFactory>(
    factory: &F,
) -> ProjectionContractReport {
    check_projection_contract_with(factory, &SuiteOptions::default()).await
}

/// Runs every case against stores from `factory` with `options`.
pub async fn check_projection_contract_with<F: ProjectionFactory>(
    factory: &F,
    options: &SuiteOptions,
) -> ProjectionContractReport {
    let mut report = ProjectionContractReport::default();
    for &case in ProjectionCase::ALL {
        if let Err(detail) = check_case(factory, case, options).await {
            report.failures.push(ProjectionFailure { case, detail });
        }
    }
    report
}

/// Runs every case and panics with the report unless all pass.
///
/// # Panics
/// When a case fails; the message lists each failed case and why.
pub fn run_projection_contract<F: ProjectionFactory>(factory: &F) {
    let report = block_on(check_projection_contract(factory));
    assert!(report.is_green(), "{report}");
}

/// Runs one case.
///
/// # Errors
/// What was expected and what the store did, at the first check that failed.
pub async fn check_case<F: ProjectionFactory>(
    factory: &F,
    case: ProjectionCase,
    options: &SuiteOptions,
) -> Check {
    match case {
        ProjectionCase::RebuildEqualsLive => rebuild_equals_live(factory, options, 7, 40).await,
        ProjectionCase::VersionBumpRebuilds => version_bump_rebuilds(factory, options).await,
        ProjectionCase::InlineRowsCommit => inline_rows_commit(factory, options).await,
        ProjectionCase::RefusedUnitLeavesNone => refused_unit_leaves_none(factory, options).await,
        ProjectionCase::OutsideUnitRefused => outside_unit_refused(factory, options).await,
        ProjectionCase::VersionBelowRebuilds => version_below_rebuilds(factory, options).await,
        ProjectionCase::MissedRowsHeal => missed_rows_heal(factory, options).await,
        ProjectionCase::RebuildWaitsForUnits => rebuild_waits_for_units(factory, options).await,
        ProjectionCase::OneRebuildWins => one_rebuild_wins(factory, options).await,
    }
}

// ---------------------------------------------------------------------------------------------
// Pool histories

/// One command of a pool history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolStep {
    /// The pool's instance (`a`: stream `pool-a`).
    pub pool: &'static str,
    /// The command.
    pub body: PoolCommand,
    /// Who issues it (a breaker reset needs an operator).
    pub actor: Actor,
}

const POOLS: [&str; 3] = ["c", "a", "b"];

fn digest(n: u8) -> ImageDigest {
    let hex = format!("{n:x}");
    ImageDigest::new(&format!("sha256:{}", hex.repeat(64)))
        .unwrap_or_else(|e| panic!("contract digest: {e}"))
}

fn configure(revision: u64) -> PoolCommand {
    PoolCommand::ConfigurePool(ConfigurePool {
        revision,
        tier: TrustTier::Trusted,
        os: GuestOs::Linux,
        vm_resources: Resources::new(1 << 30, 1 << 30),
        max_running: 4,
        min_prepared: None,
        breaker_threshold: 3,
        breaker_window: Millis::new(100),
    })
}

/// A pool history from `seed`, `len` commands long: every pool configured first, then counted
/// failures over two images at moving times (so the breaker's window drops some and trips),
/// operator resets, pauses and resumes, and commands the pool refuses. Deterministic per seed,
/// so a property test can shrink to one.
#[must_use]
pub fn pool_history(seed: u64, len: usize) -> Vec<PoolStep> {
    let mut state = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    let mut next = |bound: u64| {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) % bound.max(1)
    };
    let mut steps: Vec<PoolStep> = POOLS
        .iter()
        .map(|pool| PoolStep {
            pool,
            body: configure(1),
            actor: Actor::Startup,
        })
        .collect();
    let mut at = 1_000_u64;
    for n in 0..len {
        let pool = POOLS[usize::try_from(next(3)).unwrap_or(0)];
        at += next(60);
        let body = match next(10) {
            0 => PoolCommand::ResetBreaker {
                digest: digest(u8::try_from(next(2)).unwrap_or(0) + 1),
            },
            1 => PoolCommand::PausePool,
            2 => PoolCommand::ResumePool,
            3 => configure(2 + u64::try_from(n).unwrap_or(0)),
            _ => PoolCommand::CountAttemptFailure {
                attempt_id: AttemptId::from_u128(u128::from(next(12)) + 1),
                digest: digest(u8::try_from(next(2)).unwrap_or(0) + 1),
                stage: Stage::Boot,
                at: Millis::new(at),
            },
        };
        let actor = match body {
            PoolCommand::ResetBreaker { .. } => Actor::Operator { uid: 501 },
            _ => Actor::Automation { policy: "contract" },
        };
        steps.push(PoolStep { pool, body, actor });
    }
    steps
}

/// The scripted history every case but PC-01 runs: configured pools, a failure that the window
/// later drops, a tripped breaker and its reset (the deletes of `recent_failures`).
fn scripted() -> Vec<PoolStep> {
    let fail = |pool, attempt: u128, at: u64| PoolStep {
        pool,
        body: PoolCommand::CountAttemptFailure {
            attempt_id: AttemptId::from_u128(attempt),
            digest: digest(1),
            stage: Stage::Clone,
            at: Millis::new(at),
        },
        actor: Actor::Automation { policy: "contract" },
    };
    vec![
        PoolStep {
            pool: "b",
            body: configure(1),
            actor: Actor::Startup,
        },
        PoolStep {
            pool: "a",
            body: configure(1),
            actor: Actor::Startup,
        },
        fail("a", 1, 1_000),
        fail("a", 2, 1_200),
        fail("a", 3, 1_210),
        fail("a", 4, 1_220),
        fail("b", 5, 1_300),
        PoolStep {
            pool: "a",
            body: PoolCommand::ResetBreaker { digest: digest(1) },
            actor: Actor::Operator { uid: 501 },
        },
        PoolStep {
            pool: "b",
            body: PoolCommand::PausePool,
            actor: Actor::Automation { policy: "contract" },
        },
    ]
}

// ---------------------------------------------------------------------------------------------
// Checks and fixtures

/// A case's verdict: what differed at the first failed check.
pub type Check = Result<(), String>;

fn eq<T: PartialEq + Debug>(what: &str, got: &T, want: &T) -> Check {
    if got == want {
        Ok(())
    } else {
        Err(format!("{what}: got {got:?}, want {want:?}"))
    }
}

fn ensure(what: &str, holds: bool) -> Check {
    if holds { Ok(()) } else { Err(what.to_owned()) }
}

fn io<T, E: Display>(what: &str, result: Result<T, E>) -> Result<T, String> {
    result.map_err(|e| format!("{what}: unexpected error: {e}"))
}

fn build(name: &str) -> BuildId {
    BuildId::new(name).unwrap_or_else(|e| panic!("contract build id: {e}"))
}

fn pool_stream(pool: &str) -> StreamId {
    stream_of::<Pool>(pool).unwrap_or_else(|e| panic!("contract stream: {e}"))
}

type Store<S> = AggregateStore<S, FakeClock, SeededRng, NoNotifier>;

fn store_over<S: Journal>(journal: S, projections: &Projections) -> Store<S> {
    AggregateStore::new::<ControlAggregates>(
        journal,
        FakeClock::at(Millis::new(1_700_000_000_000)),
        SeededRng::new(11),
        NoNotifier,
    )
    .unwrap_or_else(|e| panic!("contract store: {e}"))
    .with_projections(projections.clone())
}

/// Saves `step` as command `id`; returns whether `decide` accepted it.
async fn run_step<S: Journal>(store: &Store<S>, id: u128, step: &PoolStep) -> Result<bool, String> {
    run_command::<S, Pool>(store, id, step.pool, step.actor, step.body.clone()).await
}

/// Saves `body` for `instance` of `A` as command `id` from `actor`; returns whether `decide`
/// accepted it.
async fn run_command<S: Journal, A: Aggregate>(
    store: &Store<S>,
    id: u128,
    instance: &str,
    actor: Actor,
    body: A::Body,
) -> Result<bool, String> {
    let command = Command::new(
        CommandId::from_u128(id),
        Millis::new(1_700_000_000_000),
        Epoch::new(1),
        actor,
        body,
    );
    let outcome = io(
        "save",
        store
            .execute::<A>(SaveRequest::new(instance, command))
            .await,
    )?;
    Ok(matches!(outcome, Outcome::Accepted { .. }))
}

/// Starts the store as composition does: [`prepare`] (or the suite's broken decision).
async fn start<S: Journal + ProjectionStore + ProjectionRebuild>(
    store: &S,
    projections: &Projections,
    by: &BuildId,
    how: SuitePrepare,
) -> Result<Vec<String>, String> {
    if how == SuitePrepare::Production {
        let report = io("prepare", prepare(store, store, projections, by).await)?;
        return Ok(report
            .into_iter()
            .filter(|p| p.rebuilt)
            .map(|p| p.spec)
            .collect());
    }
    let stored = io("meta", store.meta().await)?;
    let mut rebuilt = Vec::new();
    for projection in projections.all() {
        let meta = stored.iter().find(|m| m.spec == *projection.name());
        let rebuild = match (how, meta) {
            (_, None) => true,
            (SuitePrepare::UpwardOnly, Some(m)) => projection.version() > m.version,
            (SuitePrepare::VersionBlind, Some(_)) => false,
            (SuitePrepare::Production | SuitePrepare::StaleBlind, Some(_)) => {
                needs_rebuild(projection, meta, by)
            }
        };
        if rebuild {
            io(
                "rebuild",
                rebuild_projection(store, store, projection, by).await,
            )?;
            rebuilt.push(projection.name().as_str().to_owned());
        }
    }
    Ok(rebuilt)
}

/// Every row of `spec` the readers see, by key, paged.
async fn all_rows<S: ProjectionStore>(
    store: &S,
    spec: &ProjectionName,
) -> Result<BTreeMap<String, Payload>, String> {
    let mut out = BTreeMap::new();
    let mut after: Option<String> = None;
    loop {
        let page = io("rows", store.rows(spec, after.as_deref(), MAX_PAGE).await)?;
        let Some((last, _)) = page.last() else {
            break;
        };
        after = Some(last.clone());
        out.extend(page);
    }
    Ok(out)
}

/// What a store shows of every projection: rows by key, and per stream the revision applied.
type Snapshot = BTreeMap<String, (BTreeMap<String, Payload>, BTreeMap<String, Option<u64>>)>;

fn pool_streams() -> Vec<StreamId> {
    POOLS.iter().map(|pool| pool_stream(pool)).collect()
}

/// Every stream of `aggregate`, paged by key.
async fn streams_of<S: Journal>(
    journal: &S,
    aggregate: &AggregateName,
) -> Result<Vec<StreamId>, String> {
    let mut out = Vec::new();
    let mut after: Option<StreamId> = None;
    loop {
        let page = io("streams", journal.streams(after.as_ref(), MAX_PAGE).await)?;
        let Some(last) = page.last() else {
            return Ok(out);
        };
        after = Some(last.stream.clone());
        out.extend(
            page.into_iter()
                .filter(|h| h.aggregate == *aggregate)
                .map(|h| h.stream),
        );
    }
}

async fn snapshot<S: ProjectionStore>(
    store: &S,
    projections: &Projections,
) -> Result<Snapshot, String> {
    snapshot_of(store, projections, &pool_streams()).await
}

async fn snapshot_of<S: ProjectionStore>(
    store: &S,
    projections: &Projections,
    streams: &[StreamId],
) -> Result<Snapshot, String> {
    let mut out = Snapshot::new();
    for projection in projections.all() {
        let rows = all_rows(store, projection.name()).await?;
        let mut applied = BTreeMap::new();
        for s in streams {
            let seen = io("stream_rows", store.stream_rows(projection.name(), s).await)?;
            applied.insert(s.as_str().to_owned(), seen.applied.map(|r| r.get()));
        }
        out.insert(projection.name().as_str().to_owned(), (rows, applied));
    }
    Ok(out)
}

/// Each pool stream's rows are the fold of the stream read now, and their applied revision is
/// its head.
async fn rows_are_folds<S: Journal + ProjectionStore>(
    store: &S,
    projections: &Projections,
    when: &str,
) -> Check {
    rows_are_folds_of(store, projections, &pool_streams(), when).await
}

/// Each of `streams`' rows are the fold of the stream read now, and their applied revision is
/// its head.
async fn rows_are_folds_of<S: Journal + ProjectionStore>(
    store: &S,
    projections: &Projections,
    streams: &[StreamId],
    when: &str,
) -> Check {
    for projection in projections.all() {
        for s in streams {
            let events = io("read_stream", store.read_stream(s, None, None).await)?;
            let want = io(
                "fold",
                projection.fold(s, &events).map_err(|e| e.to_string()),
            )?;
            let seen = io("stream_rows", store.stream_rows(projection.name(), s).await)?;
            eq(
                &format!("{when}: {} rows of {s}", projection.name()),
                &seen.rows,
                &want,
            )?;
            eq(
                &format!("{when}: {} applied revision of {s}", projection.name()),
                &seen.applied,
                &events.last().map(|e| e.revision),
            )?;
        }
    }
    Ok(())
}

/// Checks PC-01 over the pool history of `seed` and `len`: also the "rebuild equals live"
/// property's body (`crates/mayfly-control/tests/store/projections.rs`).
///
/// # Errors
/// What differed.
pub async fn rebuild_equals_live<F: ProjectionFactory>(
    factory: &F,
    options: &SuiteOptions,
    seed: u64,
    len: usize,
) -> Check {
    let steps: Vec<Step<PoolCommand>> = pool_history(seed, len)
        .into_iter()
        .map(|step| Step {
            instance: step.pool.to_owned(),
            body: step.body,
            actor: step.actor,
        })
        .collect();
    rebuild_equals_live_over::<F, Pool>(factory, &options.projections, options.prepare, &steps)
        .await
}

/// One command of a history of `B`'s aggregate: the stream instance, the body and who issues
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step<B> {
    /// The stream's instance (`a`: stream `<aggregate>-a`).
    pub instance: String,
    /// The command.
    pub body: B,
    /// Who issues it.
    pub actor: Actor,
}

/// "Rebuild equals live" over `steps`, commands of `A`'s streams kept with `projections` (all
/// of `A`): after the live run every stream's rows are the fold of the stream with its head as
/// the applied revision, and a rebuild by stream enumeration leaves the readers exactly the
/// rows and revisions they saw (ES-I11, I16). The property over [`pool_history`],
/// [`host_history`] and [`attempt_history`] on memory and SQLite
/// (`crates/mayfly-control/tests/store/projections.rs`,
/// `crates/mayfly-adapter-sqlite/tests/projection_contract.rs`).
///
/// # Errors
/// What differed.
pub async fn rebuild_equals_live_over<F, A>(
    factory: &F,
    projections: &Projections,
    how: SuitePrepare,
    steps: &[Step<A::Body>],
) -> Check
where
    F: ProjectionFactory,
    A: Aggregate,
    A::Body: Clone,
{
    let journal = factory.fresh().await;
    let by = build("contract");
    start(&journal, projections, &by, how).await?;
    let store = store_over(journal.clone(), projections);
    for (n, step) in steps.iter().enumerate() {
        run_command::<_, A>(
            &store,
            1_000 + u128::try_from(n).unwrap_or(0),
            &step.instance,
            step.actor,
            step.body.clone(),
        )
        .await?;
    }
    let aggregate = io("aggregate", AggregateName::new(A::NAME))?;
    let streams = streams_of(&journal, &aggregate).await?;
    let live = snapshot_of(&journal, projections, &streams).await?;
    rows_are_folds_of(&journal, projections, &streams, "live").await?;
    for projection in projections.all() {
        io(
            "rebuild",
            rebuild_projection(&journal, &journal, projection, &by).await,
        )?;
    }
    let rebuilt = snapshot_of(&journal, projections, &streams).await?;
    eq("rows after a rebuild", &rebuilt, &live)
}

/// The control plane's projections of `Host` streams: `slots`.
///
/// # Panics
/// Never: the spec has a valid name.
#[must_use]
pub fn host_projections() -> Projections {
    Projection::of::<Slots>()
        .and_then(|p| Projections::new(vec![p]))
        .unwrap_or_else(|e| panic!("host projections: {e}"))
}

/// The control plane's projections of `Attempt` streams: `attempts` and `pending_effects`.
///
/// # Panics
/// Never: both specs have valid names.
#[must_use]
pub fn attempt_projections() -> Projections {
    [
        Projection::of::<Attempts>(),
        Projection::of::<PendingEffects>(),
    ]
    .into_iter()
    .collect::<Result<Vec<_>, _>>()
    .and_then(Projections::new)
    .unwrap_or_else(|e| panic!("attempt projections: {e}"))
}

/// A deterministic walk from `seed`: the next number below `bound`.
struct Walk(u64);

impl Walk {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % bound.max(1)
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        let n = u64::try_from(items.len()).unwrap_or(1);
        &items[usize::try_from(self.below(n)).unwrap_or(0)]
    }
}

const GIB: u64 = 1 << 30;
const HOSTS: [&str; 2] = ["h2", "h1"];

/// A host history from `seed`, `len` commands long: two hosts registered, reported and opened
/// to one pool, then slots reserved, activated, quarantined (with the VM running or not) and
/// released on each kind of evidence, over eight attempts, many of them refused (an attempt
/// twice, a release of a lease that is not there). The `slots` rows are created, changed and
/// deleted. Deterministic per seed.
#[must_use]
pub fn host_history(seed: u64, len: usize) -> Vec<Step<HostCommand>> {
    let mut walk = Walk(seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(7));
    let pool = PoolName::new("pool0").unwrap_or_else(|e| panic!("contract pool: {e}"));
    let setup = |report_id: u64| HostCommand::RecordCapacityReport {
        report_id,
        capacity: ReportedCapacity {
            total: Resources::new(32 * GIB, 200 * GIB),
            foreign: Resources::new(0, 0),
            base: Resources::new(0, 40 * GIB),
            vm_slots: 4,
            foreign_vms: 0,
            macos_guests: 2,
            foreign_macos_guests: 0,
        },
    };
    let mut steps = Vec::new();
    for host in HOSTS {
        for body in [
            HostCommand::RegisterHost {
                reserves: Resources::new(4 * GIB, 20 * GIB),
            },
            setup(1),
            HostCommand::ApplyPoolTerms(ApplyPoolTerms {
                pool: pool.clone(),
                revision: 1,
                open: true,
                max_running: 4,
                tier: TrustTier::Trusted,
            }),
        ] {
            steps.push(Step {
                instance: host.to_owned(),
                body,
                actor: Actor::Startup,
            });
        }
    }
    for n in 0..len {
        let host = *walk.pick(&HOSTS);
        let attempt_id = AttemptId::from_u128(u128::from(walk.below(8)) + 1);
        let body = match walk.below(9) {
            0..=2 => HostCommand::ReserveSlot(ReserveSlot {
                attempt_id,
                pool: pool.clone(),
                os: GuestOs::Linux,
                resources: Resources::new(2 * GIB, 20 * GIB),
                terms_revision: 1,
            }),
            3 | 4 => HostCommand::GrantActivation { attempt_id },
            5 => HostCommand::QuarantineLease {
                attempt_id,
                vm_running: walk.below(2) == 0,
            },
            6 => setup(2 + u64::try_from(n).unwrap_or(0)),
            _ => HostCommand::ReleaseSlot {
                attempt_id,
                evidence: *walk.pick(&[
                    ReleaseEvidence::VmDeleted,
                    ReleaseEvidence::AttemptLost,
                    ReleaseEvidence::AttemptNeverStarted,
                ]),
            },
        };
        steps.push(Step {
            instance: host.to_owned(),
            body,
            actor: Actor::Automation { policy: "contract" },
        });
    }
    steps
}

/// An attempt history from `seed`, `len` commands long: three attempts started, then
/// directives issued, sent and timed out and observations of their outcomes (cloned, booted,
/// refused, unknown), with directive ids drawn from a few per attempt so that many answer the
/// pending one and many are refused. The `attempts` rows change and the `pending_effects`
/// rows are created and deleted. Deterministic per seed.
///
/// # Panics
/// Never: the attempt numbers fit a slot.
#[must_use]
pub fn attempt_history(seed: u64, len: usize) -> Vec<Step<AttemptCommand>> {
    let mut walk = Walk(
        seed.wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(11),
    );
    let saga = Actor::Automation { policy: SAGA };
    let mut steps: Vec<Step<AttemptCommand>> = (1..=3_u128)
        .map(|n| Step {
            instance: crate::attempts::instance(n),
            body: crate::attempts::start(n),
            actor: saga,
        })
        .collect();
    let deadline = Deadline::at(Millis::new(1_700_000_600_000));
    for n in 0..len {
        let attempt = u128::from(walk.below(3)) + 1;
        let directive_id = DirectiveId::from_u128(attempt * 16 + u128::from(walk.below(4)));
        let observation = |fact| {
            AttemptCommand::RecordObservation(Observation {
                observation_id: ObservationId::from_u128(10_000 + u128::try_from(n).unwrap_or(0)),
                epoch: Epoch::new(1),
                generation: Generation::FIRST,
                fact,
            })
        };
        let body = match walk.below(8) {
            0 | 1 => AttemptCommand::IssueNext {
                directive_id,
                deadline,
            },
            2 => AttemptCommand::RecordDirectiveSent {
                directive_id,
                deadline,
            },
            3 => AttemptCommand::RecordDirectiveTimeout { directive_id },
            4 => observation(Fact::VmCloned {
                directive_id,
                instance_id: ProviderInstanceId::sample(),
            }),
            5 => observation(Fact::VmBooted { directive_id }),
            6 => observation(Fact::DirectiveRefused {
                directive_id,
                reason: RefusalReason::InsufficientResources,
            }),
            _ => observation(Fact::OutcomeUnknown { directive_id }),
        };
        steps.push(Step {
            instance: crate::attempts::instance(attempt),
            body,
            actor: saga,
        });
    }
    steps
}

/// Runs "rebuild equals live" over [`host_history`] with [`host_projections`].
///
/// # Errors
/// What differed.
pub async fn host_rebuild_equals_live<F: ProjectionFactory>(
    factory: &F,
    seed: u64,
    len: usize,
) -> Check {
    rebuild_equals_live_over::<F, Host>(
        factory,
        &host_projections(),
        SuitePrepare::Production,
        &host_history(seed, len),
    )
    .await
}

/// Runs "rebuild equals live" over [`attempt_history`] with [`attempt_projections`].
///
/// # Errors
/// What differed.
pub async fn attempt_rebuild_equals_live<F: ProjectionFactory>(
    factory: &F,
    seed: u64,
    len: usize,
) -> Check {
    rebuild_equals_live_over::<F, Attempt>(
        factory,
        &attempt_projections(),
        SuitePrepare::Production,
        &attempt_history(seed, len),
    )
    .await
}

async fn inline_rows_commit<F: ProjectionFactory>(factory: &F, options: &SuiteOptions) -> Check {
    let journal = factory.fresh().await;
    let by = build("contract");
    start(&journal, &options.projections, &by, options.prepare).await?;
    let store = store_over(journal.clone(), &options.projections);
    let mut most = 0;
    for (n, step) in scripted().iter().enumerate() {
        run_step(&store, 2_000 + u128::try_from(n).unwrap_or(0), step).await?;
        rows_are_folds(&journal, &options.projections, &format!("after step {n}")).await?;
        let total: usize = snapshot(&journal, &options.projections)
            .await?
            .values()
            .map(|(rows, _)| rows.len())
            .sum();
        most = most.max(total);
        if n + 1 == scripted().len() {
            ensure(
                "a reset deleted rows (the history has a delete target)",
                total < most,
            )?;
        }
    }
    let before = snapshot(&journal, &options.projections).await?;
    let reopened = factory.reopen(journal).await;
    eq(
        "rows after a reopen",
        &snapshot(&reopened, &options.projections).await?,
        &before,
    )
}

async fn refused_unit_leaves_none<F: ProjectionFactory>(
    factory: &F,
    options: &SuiteOptions,
) -> Check {
    let journal = factory.fresh().await;
    let by = build("contract");
    start(&journal, &options.projections, &by, options.prepare).await?;
    let store = store_over(journal.clone(), &options.projections);
    let empty = snapshot(&journal, &options.projections).await?;
    let pause = PoolStep {
        pool: "a",
        body: PoolCommand::PausePool,
        actor: Actor::Startup,
    };
    ensure(
        "a pause of an unconfigured pool is refused",
        !run_step(&store, 3_000, &pause).await?,
    )?;
    eq(
        "rows after a domain refusal",
        &snapshot(&journal, &options.projections).await?,
        &empty,
    )?;
    let configure_a = PoolStep {
        pool: "a",
        body: configure(1),
        actor: Actor::Startup,
    };
    ensure(
        "the fault is armed",
        factory
            .inject(&journal, InjectedFault::StorageBeforeCommit)
            .await,
    )?;
    let failed = store
        .save::<Pool>(SaveRequest::new(
            "a",
            Command::new(
                CommandId::from_u128(3_001),
                Millis::new(1),
                Epoch::new(1),
                Actor::Startup,
                configure(1),
            ),
        ))
        .await;
    ensure(
        &format!("a failed commit is a definite error, got {failed:?}"),
        matches!(failed, Err(SaveError::Storage(_))),
    )?;
    eq(
        "rows after a failed commit",
        &snapshot(&journal, &options.projections).await?,
        &empty,
    )?;
    run_step(&store, 3_002, &configure_a).await?;
    let configured = snapshot(&journal, &options.projections).await?;
    let a = pool_stream("a");
    let stale = raw_unit(&a, ExpectedRevision::NoStream, 3_003).with_projection(write(
        options,
        &a,
        a.as_str(),
    ));
    let refused = journal.append(stale).await;
    ensure(
        &format!("a stale unit is refused, got {refused:?}"),
        matches!(refused, Err(AppendError::WrongExpectedRevision { .. })),
    )?;
    eq(
        "rows after a stale unit",
        &snapshot(&journal, &options.projections).await?,
        &configured,
    )
}

fn pool_aggregate() -> AggregateName {
    AggregateName::new("pool").unwrap_or_else(|e| panic!("contract aggregate: {e}"))
}

/// A raw unit appending one opaque event to `s` at `expected`.
fn raw_unit(s: &StreamId, expected: ExpectedRevision, id: u128) -> UnitOfWork {
    UnitOfWork::new(
        Millis::new(9),
        SupportedEpochs::new().with(pool_aggregate(), 1),
    )
    .with_append(
        s.clone(),
        pool_aggregate(),
        expected,
        vec![NewEvent {
            event_id: EventId::from_u128(id),
            kind: StoredKind {
                event_type: "pool_paused".to_owned(),
                schema_version: 1,
            },
            required_epoch: 1,
            payload: Payload::new("{}"),
        }],
    )
}

/// The first projection's write of one row `key` for `stream`.
fn write(options: &SuiteOptions, stream: &StreamId, key: &str) -> ProjectionWrite {
    let projection = options.projections.all().first().map_or_else(
        || ProjectionName::new(Pools::NAME).unwrap_or_else(|e| panic!("{e}")),
        |p| p.name().clone(),
    );
    ProjectionWrite {
        projection,
        stream: stream.clone(),
        rows: vec![RowWrite {
            key: key.to_owned(),
            row: Payload::new("{\"forged\":true}"),
        }],
    }
}

async fn outside_unit_refused<F: ProjectionFactory>(factory: &F, options: &SuiteOptions) -> Check {
    let journal = factory.fresh().await;
    let by = build("contract");
    start(&journal, &options.projections, &by, options.prepare).await?;
    let store = store_over(journal.clone(), &options.projections);
    for (n, step) in scripted().iter().enumerate() {
        run_step(&store, 4_000 + u128::try_from(n).unwrap_or(0), step).await?;
    }
    let before = snapshot(&journal, &options.projections).await?;
    let (b, c) = (pool_stream("b"), pool_stream("c"));
    let units = [
        ("a write for another stream", write(options, &b, b.as_str())),
        (
            "a key of another stream",
            write(options, &c, &format!("{b}/forged")),
        ),
        ("the other stream's own key", write(options, &c, b.as_str())),
    ];
    for (n, (what, forged)) in units.into_iter().enumerate() {
        let unit = raw_unit(
            &c,
            ExpectedRevision::NoStream,
            4_100 + u128::try_from(n).unwrap_or(0),
        )
        .with_projection(forged);
        let refused = journal.append(unit).await;
        eq(
            what,
            &refused.map(|_| ()),
            &Err(AppendError::Malformed(MalformedUnit::ProjectionOutsideUnit)),
        )?;
        eq(
            &format!("{what}: rows"),
            &snapshot(&journal, &options.projections).await?,
            &before,
        )?;
        eq(
            &format!("{what}: head of {c}"),
            &io("head", journal.head(&c).await)?.map(|h| h.revision),
            &None,
        )?;
    }
    Ok(())
}

/// A spec `S` with its version set to `V` and its rows tagged with it, so a reader can tell
/// which version built a row: how PC-02 and PC-06 bump and lower a version.
#[derive(Debug, Clone, Copy, Default)]
pub struct Versioned<S, const V: u32>(std::marker::PhantomData<S>);

/// A row of [`Versioned`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionedRow<R> {
    /// The version that built it.
    pub version: u32,
    /// The row.
    pub row: R,
}

impl<S: ProjectionSpec, const V: u32> ProjectionSpec for Versioned<S, V> {
    const NAME: &'static str = S::NAME;
    const VERSION: u32 = V;
    const SUBSCRIBES: &'static [EventKind] = S::SUBSCRIBES;
    type Source = S::Source;
    type Row = VersionedRow<S::Row>;

    fn apply(
        rows: &Rows<Self::Row>,
        event: &SpecEvent<Self>,
        after: &SpecState<Self>,
        at: EventMeta,
    ) -> Vec<RowChange<Self::Row>> {
        let inner: Rows<S::Row> = rows
            .iter()
            .map(|(k, r)| (k.clone(), r.row.clone()))
            .collect();
        S::apply(&inner, event, after, at)
            .into_iter()
            .map(|change| RowChange {
                suffix: change.suffix,
                row: change.row.map(|row| VersionedRow { version: V, row }),
            })
            .collect()
    }
}

fn versioned<const V: u32>() -> Projections {
    Projection::of::<Versioned<Pools, V>>()
        .and_then(|p| Projections::new(vec![p]))
        .unwrap_or_else(|e| panic!("contract projection: {e}"))
}

/// The version that built every row of `pools` the readers see (`None`: no row).
async fn row_versions<S: ProjectionStore>(store: &S) -> Result<Vec<u32>, String> {
    let spec = ProjectionName::new(Pools::NAME).unwrap_or_else(|e| panic!("{e}"));
    let mut versions: Vec<u32> = all_rows(store, &spec)
        .await?
        .values()
        .map(|p| {
            serde_json::from_str::<VersionedRow<serde_json::Value>>(p.as_str())
                .map_or(0, |r| r.version)
        })
        .collect();
    versions.dedup();
    Ok(versions)
}

/// A store with the scripted history, built and kept by version `V`.
async fn store_at<F: ProjectionFactory, const V: u32>(
    factory: &F,
    by: &BuildId,
    how: SuitePrepare,
) -> Result<F::Store, String> {
    let journal = factory.fresh().await;
    start(&journal, &versioned::<V>(), by, how).await?;
    let store = store_over(journal.clone(), &versioned::<V>());
    for (n, step) in scripted().iter().enumerate() {
        run_step(&store, 5_000 + u128::try_from(n).unwrap_or(0), step).await?;
    }
    eq(
        "rows before the change",
        &row_versions(&journal).await?,
        &vec![V],
    )?;
    Ok(journal)
}

async fn version_bump_rebuilds<F: ProjectionFactory>(factory: &F, options: &SuiteOptions) -> Check {
    let by = build("contract");
    let journal = store_at::<F, 1>(factory, &by, options.prepare).await?;
    let spec = ProjectionName::new(Pools::NAME).unwrap_or_else(|e| panic!("{e}"));
    let (a, b) = (pool_stream("a"), pool_stream("b"));
    let old_a = io("stream_rows", journal.stream_rows(&spec, &a).await)?;
    let generation = io("begin_rebuild", journal.begin_rebuild(&spec).await)?;
    let half = BTreeMap::from([(a.as_str().to_owned(), Payload::new("{\"half\":true}"))]);
    io(
        "apply_stream",
        journal
            .apply_stream(
                &spec,
                generation,
                &a,
                half.clone(),
                old_a.applied.unwrap_or_default(),
            )
            .await,
    )?;
    eq(
        "mid-rebuild: readers see the old generation",
        &io("stream_rows", journal.stream_rows(&spec, &a).await)?,
        &old_a,
    )?;
    let active = io("meta", journal.meta().await)?
        .into_iter()
        .find(|m| m.spec == spec)
        .map(|m| m.active_generation);
    let in_place = match active {
        Some(active) => {
            journal
                .apply_stream(&spec, active, &a, half, old_a.applied.unwrap_or_default())
                .await
        }
        None => Err(mayfly_control::journal::StorageError::new("no_meta")),
    };
    ensure(
        "a rebuild cannot write the active generation",
        in_place.is_err(),
    )?;
    eq(
        "mid-rebuild: readers see the old generation, still",
        &io("stream_rows", journal.stream_rows(&spec, &a).await)?,
        &old_a,
    )?;
    let rebuilt = start(&journal, &versioned::<2>(), &by, options.prepare).await?;
    eq("rebuilt at start", &rebuilt, &vec![Pools::NAME.to_owned()])?;
    eq(
        "rows after the bump",
        &row_versions(&journal).await?,
        &vec![2],
    )?;
    let meta = io("meta", journal.meta().await)?;
    eq(
        "stored version",
        &meta.iter().find(|m| m.spec == spec).map(|m| m.version),
        &Some(2),
    )?;
    let b_rows = io("stream_rows", journal.stream_rows(&spec, &b).await)?;
    eq(
        &format!("{b}: one row, applied at its head"),
        &(b_rows.rows.len(), b_rows.applied),
        &(1, io("head", journal.head(&b).await)?.map(|h| h.revision)),
    )
}

async fn version_below_rebuilds<F: ProjectionFactory>(
    factory: &F,
    options: &SuiteOptions,
) -> Check {
    let by = build("contract");
    let journal = store_at::<F, 2>(factory, &by, options.prepare).await?;
    let same = start(&journal, &versioned::<2>(), &by, options.prepare).await?;
    eq("same version and build: rebuilt", &same, &Vec::new())?;
    let lower = start(&journal, &versioned::<1>(), &by, options.prepare).await?;
    eq(
        "lower version: rebuilt",
        &lower,
        &vec![Pools::NAME.to_owned()],
    )?;
    eq(
        "rows after the lower start",
        &row_versions(&journal).await?,
        &vec![1],
    )?;
    let other = start(
        &journal,
        &versioned::<1>(),
        &build("other"),
        options.prepare,
    )
    .await?;
    eq(
        "another build: rebuilt",
        &other,
        &vec![Pools::NAME.to_owned()],
    )
}

fn fail_a(attempt: u128, at: u64) -> PoolStep {
    PoolStep {
        pool: "a",
        body: PoolCommand::CountAttemptFailure {
            attempt_id: AttemptId::from_u128(attempt),
            digest: digest(1),
            stage: Stage::Clone,
            at: Millis::new(at),
        },
        actor: Actor::Automation { policy: "contract" },
    }
}

async fn missed_rows_heal<F: ProjectionFactory>(factory: &F, options: &SuiteOptions) -> Check {
    let journal = factory.fresh().await;
    let by = build("contract");
    start(&journal, &options.projections, &by, options.prepare).await?;
    let store = store_over(journal.clone(), &options.projections);
    for (n, step) in scripted().iter().enumerate() {
        run_step(&store, 6_000 + u128::try_from(n).unwrap_or(0), step).await?;
    }
    for (n, step) in [fail_a(6, 2_000), fail_a(7, 2_010), fail_a(8, 2_020)]
        .iter()
        .enumerate()
    {
        run_step(&store, 6_100 + u128::try_from(n).unwrap_or(0), step).await?;
    }
    // Missed writes: units of a build that keeps no projection, on both streams, one of them
    // a delete target (a reset clears the failures that tripped the breaker).
    let rowless = store_over(journal.clone(), &Projections::none());
    let missed = [
        PoolStep {
            pool: "a",
            body: PoolCommand::ResetBreaker { digest: digest(1) },
            actor: Actor::Operator { uid: 501 },
        },
        PoolStep {
            pool: "b",
            body: PoolCommand::ResumePool,
            actor: Actor::Automation { policy: "contract" },
        },
    ];
    for (n, step) in missed.iter().enumerate() {
        ensure(
            &format!("missed step {n} is accepted"),
            run_step(&rowless, 6_200 + u128::try_from(n).unwrap_or(0), step).await?,
        )?;
    }
    run_step(&store, 6_300, &fail_a(9, 2_030)).await?;
    let a = pool_stream("a");
    rows_are_folds_of(
        &journal,
        &options.projections,
        std::slice::from_ref(&a),
        "the unit after a missed write",
    )
    .await?;
    let reopened = factory.reopen(journal).await;
    let rebuilt = start(&reopened, &options.projections, &by, options.prepare).await?;
    let names: Vec<String> = options
        .projections
        .all()
        .iter()
        .map(|p| p.name().as_str().to_owned())
        .collect();
    eq("rebuilt at the same build's restart", &rebuilt, &names)?;
    rows_are_folds(&reopened, &options.projections, "after the restart").await?;
    let again = start(&reopened, &options.projections, &by, options.prepare).await?;
    eq("rebuilt at the next restart", &again, &Vec::new())
}

/// A journal that, once, saves a command through the store under test right after it reads a
/// given stream: a unit that commits in the middle of an offline rebuild.
#[derive(Clone)]
struct Interleaving<S> {
    inner: S,
    after_read: StreamId,
    projections: Projections,
    pending: Arc<Mutex<Option<PoolStep>>>,
}

impl<S: Journal + Clone> Journal for Interleaving<S> {
    async fn append(&self, unit: UnitOfWork) -> Result<Appended, AppendError> {
        self.inner.append(unit).await
    }

    async fn head(&self, stream: &StreamId) -> Result<Option<StreamHead>, StorageError> {
        self.inner.head(stream).await
    }

    async fn read_stream(
        &self,
        stream: &StreamId,
        after: Option<StreamRevision>,
        through: Option<StreamRevision>,
    ) -> Result<Vec<RecordedEvent>, StorageError> {
        let read = self.inner.read_stream(stream, after, through).await;
        let step = if *stream == self.after_read {
            self.pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
        } else {
            None
        };
        if let Some(step) = step {
            let store = store_over(self.inner.clone(), &self.projections);
            run_step(&store, 7_900, &step)
                .await
                .map_err(|_| StorageError::new("interleaved_unit_failed"))?;
        }
        read
    }

    async fn lookup_event(&self, event_id: EventId) -> Result<Option<EventLocation>, StorageError> {
        self.inner.lookup_event(event_id).await
    }

    async fn receipt(&self, command_id: CommandId) -> Result<Option<CommandReceipt>, StorageError> {
        self.inner.receipt(command_id).await
    }

    async fn inbox_contains(&self, key: &InboxKey) -> Result<bool, StorageError> {
        self.inner.inbox_contains(key).await
    }

    async fn streams(
        &self,
        after: Option<&StreamId>,
        limit: u32,
    ) -> Result<Vec<StreamHead>, StorageError> {
        self.inner.streams(after, limit).await
    }

    async fn stored_kinds(&self) -> Result<Vec<StoredKind>, StorageError> {
        self.inner.stored_kinds().await
    }

    async fn projection(
        &self,
        projection: &ProjectionName,
        key: &str,
    ) -> Result<Option<Payload>, StorageError> {
        self.inner.projection(projection, key).await
    }
}

/// Folds `streams` into `generation` of `projection`.
async fn fold_into<S: Journal + ProjectionRebuild>(
    journal: &S,
    projection: &Projection,
    generation: u64,
    streams: &[StreamId],
) -> Result<(), StorageError> {
    for s in streams {
        let events = journal.read_stream(s, None, None).await?;
        let Some(applied) = events.last().map(|e| e.revision) else {
            continue;
        };
        let rows = projection
            .fold(s, &events)
            .map_err(|_| StorageError::new("contract_fold"))?;
        journal
            .apply_stream(projection.name(), generation, s, rows, applied)
            .await?;
    }
    Ok(())
}

fn first_projection(options: &SuiteOptions) -> Result<Projection, String> {
    options
        .projections
        .all()
        .first()
        .cloned()
        .ok_or_else(|| "the suite keeps no projection".to_owned())
}

async fn rebuild_waits_for_units<F: ProjectionFactory>(
    factory: &F,
    options: &SuiteOptions,
) -> Check {
    let journal = factory.fresh().await;
    let by = build("contract");
    start(&journal, &options.projections, &by, options.prepare).await?;
    let store = store_over(journal.clone(), &options.projections);
    for (n, step) in scripted().iter().enumerate() {
        run_step(&store, 7_000 + u128::try_from(n).unwrap_or(0), step).await?;
    }
    let projection = first_projection(options)?;
    let spec = projection.name().clone();
    let (a, b) = (pool_stream("a"), pool_stream("b"));
    let before = snapshot(&journal, &options.projections).await?;
    let meta = io("meta", journal.meta().await)?;
    let generation = io("begin_rebuild", journal.begin_rebuild(&spec).await)?;
    io(
        "fold a",
        fold_into(&journal, &projection, generation, std::slice::from_ref(&a)).await,
    )?;
    let cut = |journal: F::Store, generation| {
        let (spec, projection, by) = (spec.clone(), projection.clone(), by.clone());
        async move {
            journal
                .cutover(
                    &spec,
                    projection.aggregate(),
                    generation,
                    projection.version(),
                    &by,
                )
                .await
        }
    };
    eq(
        "a cutover with a stream not folded",
        &io("cutover", cut(journal.clone(), generation).await)?,
        &Cutover::StreamsMoved,
    )?;
    io(
        "fold b",
        fold_into(&journal, &projection, generation, std::slice::from_ref(&b)).await,
    )?;
    run_step(
        &store,
        7_100,
        &PoolStep {
            pool: "a",
            body: PoolCommand::PausePool,
            actor: Actor::Automation { policy: "contract" },
        },
    )
    .await?;
    eq(
        "a cutover after a unit moved a folded stream",
        &io("cutover", cut(journal.clone(), generation).await)?,
        &Cutover::StreamsMoved,
    )?;
    eq(
        "meta after the refused cutovers",
        &io("meta", journal.meta().await)?,
        &meta,
    )?;
    let moved = snapshot(&journal, &options.projections).await?;
    ensure("the unit wrote the active generation", moved != before)?;
    rows_are_folds(&journal, &options.projections, "after the refused cutovers").await?;
    // The whole rebuild, with a unit landing on `a` after the pass folded it.
    let interleaving = Interleaving {
        inner: journal.clone(),
        after_read: b.clone(),
        projections: options.projections.clone(),
        pending: Arc::new(Mutex::new(Some(PoolStep {
            pool: "a",
            body: PoolCommand::ResumePool,
            actor: Actor::Automation { policy: "contract" },
        }))),
    };
    io(
        "rebuild with a unit in the middle",
        rebuild_projection(&interleaving, &journal, &projection, &build("other")).await,
    )?;
    ensure(
        "the unit landed during the rebuild",
        interleaving
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none(),
    )?;
    rows_are_folds(&journal, &options.projections, "after the rebuild").await
}

async fn one_rebuild_wins<F: ProjectionFactory>(factory: &F, options: &SuiteOptions) -> Check {
    let journal = factory.fresh().await;
    let by = build("contract");
    start(&journal, &options.projections, &by, options.prepare).await?;
    let store = store_over(journal.clone(), &options.projections);
    for (n, step) in scripted().iter().enumerate() {
        run_step(&store, 8_000 + u128::try_from(n).unwrap_or(0), step).await?;
    }
    let projection = first_projection(options)?;
    let spec = projection.name().clone();
    let streams = pool_streams();
    let before = snapshot(&journal, &options.projections).await?;
    let first = io("begin_rebuild", journal.begin_rebuild(&spec).await)?;
    io(
        "first rebuild folds a stream",
        fold_into(&journal, &projection, first, &streams[..1]).await,
    )?;
    let second = io("begin_rebuild", journal.begin_rebuild(&spec).await)?;
    ensure(
        &format!("the second rebuild claims its own generation, got {first} twice"),
        second != first,
    )?;
    let late_write = fold_into(&journal, &projection, first, &streams).await;
    ensure(
        &format!("a superseded rebuild's write is refused, got {late_write:?}"),
        late_write.is_err(),
    )?;
    let late_cutover = journal
        .cutover(
            &spec,
            projection.aggregate(),
            first,
            projection.version(),
            &by,
        )
        .await;
    ensure(
        &format!("a superseded rebuild's cutover is refused, got {late_cutover:?}"),
        late_cutover.is_err(),
    )?;
    eq(
        "readers after the superseded rebuild",
        &snapshot(&journal, &options.projections).await?,
        &before,
    )?;
    io(
        "second rebuild folds every stream",
        fold_into(&journal, &projection, second, &streams).await,
    )?;
    eq(
        "the second rebuild's cutover",
        &io(
            "cutover",
            journal
                .cutover(
                    &spec,
                    projection.aggregate(),
                    second,
                    projection.version(),
                    &by,
                )
                .await,
        )?,
        &Cutover::Active,
    )?;
    let active = io("meta", journal.meta().await)?
        .into_iter()
        .find(|m| m.spec == spec)
        .map(|m| m.active_generation);
    eq("the active generation", &active, &Some(second))?;
    rows_are_folds(&journal, &options.projections, "after the winner's cutover").await?;
    eq(
        "readers after the winner's cutover",
        &snapshot(&journal, &options.projections).await?,
        &before,
    )
}

/// Whether `S::SUBSCRIBES` misses a kind its `apply` reacts to: each of `histories` (event
/// sequences of `S::Source`, such as its determinism cases' histories) is folded twice, once
/// calling `apply` on the subscribed kinds only (as the store does) and once on every event;
/// the rows must be equal after every event. Returns the first history index and event where
/// they differ. The declared list is then a contract, not a comment
/// (`ProjectionSpec::SUBSCRIBES`).
///
/// # Errors
/// `(history, event)` of the first divergence.
pub fn check_subscriptions<S: ProjectionSpec>(
    histories: &[Vec<SpecEvent<S>>],
) -> Result<(), (usize, usize)>
where
    SpecEvent<S>: EventSet,
{
    use mayfly_control::journal::StreamRevision;
    use mayfly_control_domain::Aggregate;

    fn apply_all<R>(rows: &mut Rows<R>, changes: Vec<RowChange<R>>) {
        for change in changes {
            match change.row {
                Some(row) => rows.insert(change.suffix, row),
                None => rows.remove(&change.suffix),
            };
        }
    }

    for (h, history) in histories.iter().enumerate() {
        let mut state = <S::Source as Aggregate>::initial();
        let (mut filtered, mut every): (Rows<S::Row>, Rows<S::Row>) = Default::default();
        for (n, event) in history.iter().enumerate() {
            state = <S::Source as Aggregate>::evolve(state, event);
            let at = EventMeta {
                revision: StreamRevision::new(u64::try_from(n).unwrap_or(0)),
                recorded_at: Millis::new(0),
            };
            if S::SUBSCRIBES.contains(&event.kind()) {
                let changes = S::apply(&filtered, event, &state, at);
                apply_all(&mut filtered, changes);
            }
            let changes = S::apply(&every, event, &state, at);
            apply_all(&mut every, changes);
            if filtered != every {
                return Err((h, n));
            }
        }
    }
    Ok(())
}
