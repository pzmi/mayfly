//! The delivery contract suite (ver §5, `doc/event-store.md` §7.1): one set of cases every
//! `DeliveryLedger` passes together with the journal whose units mark it, written once over a
//! [`DeliveryFactory`].
//!
//! Cases are named by their id in `doc/event-store.md` §7.1 (`DC-01` …) and hold ES-I14 (durable
//! delivery) and ES-I15 (no skip), the invariants `ES_I14_durableDelivery`, `ES_I15_noSkip` and
//! `directivesFromJournal` of `formal/event_store.qnt`, whose scenarios several cases follow step
//! by step (named on each case). They drive the production `DeliveryWorker` of
//! `mayfly_control::delivery` where a case is about a drain, and the ledger directly where it is
//! about one transaction (a claim, an advance, a release), so a broken worker step can be pinned
//! as well as a broken ledger (`HeadTrustingWorker`, `doc/event-store.md` §7.6); a broken worker
//! the suite can drive itself is a [`SuiteWorker`] (`check_delivery_contract_with`).
//!
//! **Time.** Lease expiry and `retry_after` are judged by the store clock, which the factory
//! owns and the suite advances ([`DeliveryFactory::clock`]); no case sleeps. A lease
//! lives [`TTL`].
//!
//! **Modes.** Every case starts from [`DeliveryFactory::fresh`]; cases reopen the store, take a
//! second handle on it and crash it where they say so, so the suite runs a ledger fresh,
//! reopened, with two writer connections, with two handles on one file, and crash-injected
//! (`crates/mayfly-adapter-sqlite/tests/delivery_contract.rs`). [`check_delivery_contract`]
//! returns the failed cases instead of panicking: the suite's own tests show the broken ledgers
//! of §7.6 failing exactly their cases (`crates/mayfly-testkit/tests/delivery_contract.rs`). A
//! factory that cannot inject a fault a case needs fails that case: no case is skipped.

use std::collections::BTreeSet;
use std::fmt::{self, Debug, Display};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use mayfly_control::delivery::{
    Claim, Consumer, ConsumerError, ConsumerName, Delivery, DeliveryConfig, DeliveryKey,
    DeliveryLedger, DeliveryTimer, DeliveryWorker, DrainReport, Kicks, LeaseError, LeaseFence,
};
use mayfly_control::journal::{
    AggregateName, ExpectedRevision, Journal, NewEvent, Payload, StoredKind, StreamId,
    StreamRevision, SupportedEpochs, UnitOfWork,
};
use mayfly_domain::{EventId, Millis};

use crate::block_on::block_on;
use crate::clock::SeededRng;
use crate::contracts::journal::InjectedFault;

/// How long a lease lives in every case.
pub const TTL: Millis = Millis::new(1_000);

/// A journal and the delivery ledger of the same store, as one handle holds them.
#[derive(Debug, Clone)]
pub struct DeliveryStore<J, L> {
    /// The journal: its units write the dispatch marks.
    pub journal: J,
    /// The ledger.
    pub ledger: L,
}

/// Makes the stores the suite runs against, and the hooks its modes need.
///
/// Every method is required, so an adapter cannot skip a mode by omission.
pub trait DeliveryFactory {
    /// The journal under test.
    type Journal: Journal + Clone;
    /// The ledger under test.
    type Ledger: DeliveryLedger + Clone;

    /// A new, empty store.
    fn fresh(&self) -> impl Future<Output = DeliveryStore<Self::Journal, Self::Ledger>>;

    /// The same store, closed and opened again (or itself, for a store with nothing to reopen).
    fn reopen(
        &self,
        store: DeliveryStore<Self::Journal, Self::Ledger>,
    ) -> impl Future<Output = DeliveryStore<Self::Journal, Self::Ledger>>;

    /// Another handle on the same store, as another process (or another build) would hold.
    fn second_handle(
        &self,
        store: &DeliveryStore<Self::Journal, Self::Ledger>,
    ) -> impl Future<Output = DeliveryStore<Self::Journal, Self::Ledger>>;

    /// Arms `fault` for the next append on the store; `false` when this factory cannot.
    fn inject(
        &self,
        store: &DeliveryStore<Self::Journal, Self::Ledger>,
        fault: InjectedFault,
    ) -> impl Future<Output = bool>;

    /// The process holding the store died; the store, reopened.
    fn crash(
        &self,
        store: DeliveryStore<Self::Journal, Self::Ledger>,
    ) -> impl Future<Output = DeliveryStore<Self::Journal, Self::Ledger>>;

    /// The hand that moves the store clock of every store this factory made forward: a
    /// handle a consumer can hold (DC-23 moves time while an event is handled).
    fn clock(&self) -> ClockHand;
}

/// Moves a factory's store clock forward by the given duration.
pub type ClockHand = Arc<dyn Fn(Millis) + Send + Sync>;

/// Moves `factory`'s store clock forward by `by`.
fn tick<F: DeliveryFactory>(factory: &F, by: Millis) {
    (factory.clock())(by);
}

macro_rules! cases {
    ($($variant:ident => $id:literal, $name:literal, $doc:literal;)+) => {
        /// One case of the delivery contract, named after its id in `doc/event-store.md` §7.1.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum DeliveryCase {
            $(#[doc = $doc] $variant,)+
        }

        impl DeliveryCase {
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
    NoConsumerNoMark => "DC-01", "no_consumer_no_mark", "DC-01: with no consumer registered a unit writes no mark.";
    EveryConsumerMarked => "DC-02", "every_consumer_marked", "DC-02: each registered consumer is marked at the new head; a mark rises with the head.";
    RefusedUnitNoMark => "DC-03", "refused_unit_no_mark", "DC-03: a refused, failed or crashed unit leaves no mark; a committed one always does (ES-I1).";
    BootstrapMarksExisting => "DC-04", "bootstrap_marks_existing", "DC-04: bootstrap marks every existing stream, page by page by key (model `bootstrapAfterWrites`).";
    RegisterTwiceIsOne => "DC-05", "register_twice_is_one", "DC-05: registering twice is one consumer.";
    RetiredNotMarked => "DC-06", "retired_not_marked", "DC-06: a retired consumer is no longer marked and its rows are gone; registered again it starts over under a higher fence (model `retireThenRegister`).";
    DrainOnceInOrder => "DC-07", "drain_once_in_order", "DC-07: a drain delivers every event once, in revision order.";
    MarkDuringProcessing => "DC-08", "mark_during_processing", "DC-08: a mark set during processing survives the checkpoint move (model `markDuringProcessing`, `commitBetweenFetchAndApply`).";
    LiveLeaseNotClaimable => "DC-09", "live_lease_not_claimable", "DC-09: a live lease is not claimable, from any handle.";
    KickClaimsOnlyItsStream => "DC-10", "kick_claims_only_its_stream", "DC-10: a kick claims only its stream.";
    StaleAdvanceRefused => "DC-11", "stale_advance_refused", "DC-11: a stale worker's advance after a takeover is refused (model `staleWorkerFenced`).";
    ExpiredAdvanceRefused => "DC-12", "expired_advance_refused", "DC-12: an advance after expiry without a takeover is refused.";
    CheckpointNeverBack => "DC-13", "checkpoint_never_back", "DC-13: the checkpoint never moves back.";
    DeadBeforeEffect => "DC-14", "dead_before_effect", "DC-14: a worker dead before its effect: the event is delivered again.";
    DeadAfterEffect => "DC-15", "dead_after_effect", "DC-15: dead after the effect, before the advance: delivered again, and the delivery key absorbs it (model `workerCrashRedelivers`).";
    AdvancedBeforeDeath => "DC-16", "advanced_before_death", "DC-16: advanced before death: not delivered again.";
    DeadWorkerWaitsForLease => "DC-17", "dead_worker_waits_for_lease", "DC-17: a dead worker's stream waits for its lease to end, then is delivered.";
    FailingConsumerNoSkip => "DC-18", "failing_consumer_no_skip", "DC-18: a failing consumer gets the same event again, never the next (ES-I15; model `failingConsumerNoSkip`).";
    LongDownCatchesUp => "DC-19", "long_down_catches_up", "DC-19: a long-down worker catches up every stream in order.";
    TakeoverHigherFence => "DC-20", "takeover_higher_fence", "DC-20: a takeover, and every later claim, gets a higher fence.";
    DeliveryCarriesFence => "DC-21", "delivery_carries_fence", "DC-21: each delivery carries its claim's fence.";
    ExpiredRenewRefused => "DC-22", "expired_renew_refused", "DC-22: renewing an expired lease is refused; a renewal in time extends it.";
    LostStreamOthersGoOn => "DC-23", "lost_stream_others_go_on", "DC-23: a slow consumer that lost one stream goes on draining others.";
    ForeignConsumerMarked => "DC-24", "foreign_consumer_marked", "DC-24: a consumer registered by another build is marked by this one.";
    RetryAfterHonoured => "DC-25", "retry_after_honoured", "DC-25: a release with `retry_after` is not claimable before the store clock passes it.";
    SlowHandleCut => "DC-26", "slow_handle_cut", "DC-26: a `handle` slower than the lease is cut at its deadline and released with a backoff, never redelivered round after round under a dead lease.";
    ClaimedWhenReached => "DC-27", "claimed_when_reached", "DC-27: a drain claims each stream when it reaches it: slow streams early in a round waste no later claim.";
    FailingStreamNoStarvation => "DC-28", "failing_stream_no_starvation", "DC-28: a stream that keeps failing is handled once per drain and never keeps a later stream waiting: discovery walks the stream space round-robin from the last claim.";
}

impl Display for DeliveryCase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.id(), self.name())
    }
}

/// A failed case and the first check in it that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryFailure {
    /// The case.
    pub case: DeliveryCase,
    /// What was expected and what the store did.
    pub detail: String,
}

/// The outcome of a contract run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeliveryContractReport {
    /// Every failed case, in run order.
    pub failures: Vec<DeliveryFailure>,
}

impl DeliveryContractReport {
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

impl Display for DeliveryContractReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_green() {
            return f.write_str("delivery contract: every case passed");
        }
        writeln!(
            f,
            "delivery contract: {} case(s) failed",
            self.failures.len()
        )?;
        for failure in &self.failures {
            writeln!(f, "- {}: {}", failure.case, failure.detail)?;
        }
        Ok(())
    }
}

/// The worker the suite drives: the production `DeliveryWorker`, or a negative control the
/// suite's own tests run to show a case is not vacuous (`doc/event-store.md` §7.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SuiteWorker {
    /// The production worker, with its deadline on each `handle`.
    #[default]
    Production,
    /// A worker without the deadline on `handle`: its deadline never fires, so a `handle`
    /// slower than the lease runs to its end and its advance finds the lease gone (the worker
    /// before ES-05F; DC-26 must catch it).
    WithoutHandleDeadline,
}

/// Runs every case against stores from `factory`.
pub async fn check_delivery_contract<F: DeliveryFactory>(factory: &F) -> DeliveryContractReport {
    check_delivery_contract_with(factory, SuiteWorker::Production).await
}

/// Runs every case against stores from `factory`, draining with `worker`.
pub async fn check_delivery_contract_with<F: DeliveryFactory>(
    factory: &F,
    worker: SuiteWorker,
) -> DeliveryContractReport {
    let mut report = DeliveryContractReport::default();
    for &case in DeliveryCase::ALL {
        if let Err(detail) = check_case_with(factory, case, worker).await {
            report.failures.push(DeliveryFailure { case, detail });
        }
    }
    report
}

/// Runs every case and panics with the report unless all pass.
///
/// # Panics
/// When a case fails; the message lists each failed case and why.
pub fn run_delivery_contract<F: DeliveryFactory>(factory: &F) {
    let report = block_on(check_delivery_contract(factory));
    assert!(report.is_green(), "{report}");
}

/// Runs one case.
///
/// # Errors
/// What was expected and what the store did, at the first check that failed.
pub async fn check_case<F: DeliveryFactory>(factory: &F, case: DeliveryCase) -> Check {
    check_case_with(factory, case, SuiteWorker::Production).await
}

/// Runs one case, draining with `worker`.
///
/// # Errors
/// What was expected and what the store did, at the first check that failed.
pub async fn check_case_with<F: DeliveryFactory>(
    factory: &F,
    case: DeliveryCase,
    worker: SuiteWorker,
) -> Check {
    match case {
        DeliveryCase::NoConsumerNoMark => no_consumer_no_mark(factory).await,
        DeliveryCase::EveryConsumerMarked => every_consumer_marked(factory).await,
        DeliveryCase::RefusedUnitNoMark => refused_unit_no_mark(factory).await,
        DeliveryCase::BootstrapMarksExisting => bootstrap_marks_existing(factory).await,
        DeliveryCase::RegisterTwiceIsOne => register_twice_is_one(factory).await,
        DeliveryCase::RetiredNotMarked => retired_not_marked(factory).await,
        DeliveryCase::DrainOnceInOrder => drain_once_in_order(factory).await,
        DeliveryCase::MarkDuringProcessing => mark_during_processing(factory).await,
        DeliveryCase::LiveLeaseNotClaimable => live_lease_not_claimable(factory).await,
        DeliveryCase::KickClaimsOnlyItsStream => kick_claims_only_its_stream(factory).await,
        DeliveryCase::StaleAdvanceRefused => stale_advance_refused(factory).await,
        DeliveryCase::ExpiredAdvanceRefused => expired_advance_refused(factory).await,
        DeliveryCase::CheckpointNeverBack => checkpoint_never_back(factory).await,
        DeliveryCase::DeadBeforeEffect => dead_before_effect(factory).await,
        DeliveryCase::DeadAfterEffect => dead_after_effect(factory).await,
        DeliveryCase::AdvancedBeforeDeath => advanced_before_death(factory).await,
        DeliveryCase::DeadWorkerWaitsForLease => dead_worker_waits_for_lease(factory).await,
        DeliveryCase::FailingConsumerNoSkip => failing_consumer_no_skip(factory).await,
        DeliveryCase::LongDownCatchesUp => long_down_catches_up(factory).await,
        DeliveryCase::TakeoverHigherFence => takeover_higher_fence(factory).await,
        DeliveryCase::DeliveryCarriesFence => delivery_carries_fence(factory).await,
        DeliveryCase::ExpiredRenewRefused => expired_renew_refused(factory).await,
        DeliveryCase::LostStreamOthersGoOn => lost_stream_others_go_on(factory).await,
        DeliveryCase::ForeignConsumerMarked => foreign_consumer_marked(factory).await,
        DeliveryCase::RetryAfterHonoured => retry_after_honoured(factory).await,
        DeliveryCase::SlowHandleCut => slow_handle_cut(factory, worker).await,
        DeliveryCase::ClaimedWhenReached => claimed_when_reached(factory).await,
        DeliveryCase::FailingStreamNoStarvation => failing_stream_no_starvation(factory).await,
    }
}

// ---------------------------------------------------------------------------------------------
// Checks and fixtures

type Check = Result<(), String>;

type Store<F> = DeliveryStore<<F as DeliveryFactory>::Journal, <F as DeliveryFactory>::Ledger>;

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

fn stream(name: &str) -> StreamId {
    StreamId::new(name).unwrap_or_else(|e| panic!("contract stream id {name}: {e}"))
}

fn consumer(name: &str) -> ConsumerName {
    ConsumerName::new(name).unwrap_or_else(|e| panic!("contract consumer {name}: {e}"))
}

fn rev(n: u64) -> StreamRevision {
    StreamRevision::new(n)
}

fn attempt() -> AggregateName {
    AggregateName::new("attempt").unwrap_or_else(|e| panic!("contract aggregate: {e}"))
}

/// A unit appending `ids` to `s` after `expected` (`None`: a new stream).
fn unit(s: &StreamId, expected: Option<u64>, ids: &[u128]) -> UnitOfWork {
    let expected = expected.map_or(ExpectedRevision::NoStream, |r| {
        ExpectedRevision::Exactly(rev(r))
    });
    UnitOfWork::new(Millis::new(7), SupportedEpochs::new().with(attempt(), 1)).with_append(
        s.clone(),
        attempt(),
        expected,
        ids.iter()
            .map(|&n| NewEvent {
                event_id: EventId::from_u128(n),
                kind: StoredKind {
                    event_type: "contract_fact_recorded".to_owned(),
                    schema_version: 1,
                },
                required_epoch: 1,
                payload: Payload::new(format!("{{\"n\":{n}}}")),
            })
            .collect(),
    )
}

async fn append<J: Journal>(
    journal: &J,
    s: &StreamId,
    expected: Option<u64>,
    ids: &[u128],
) -> Check {
    io("append", journal.append(unit(s, expected, ids)).await).map(|_| ())
}

async fn claim<L: DeliveryLedger>(
    ledger: &L,
    c: &ConsumerName,
    only: Option<&StreamId>,
) -> Result<Vec<Claim>, String> {
    io("claim", ledger.claim(c, TTL, 100, only).await)
}

/// The streams `claims` name, with their checkpoint and mark.
fn claimed(claims: &[Claim]) -> Vec<(StreamId, Option<StreamRevision>, StreamRevision)> {
    claims
        .iter()
        .map(|c| (c.stream.clone(), c.checkpoint, c.marked))
        .collect()
}

async fn claim_one<L: DeliveryLedger>(
    ledger: &L,
    c: &ConsumerName,
    s: &StreamId,
) -> Result<Claim, String> {
    let mut claims = claim(ledger, c, Some(s)).await?;
    match (claims.pop(), claims.is_empty()) {
        (Some(one), true) => Ok(one),
        (got, _) => Err(format!(
            "claim of {s}: expected exactly one claim, got {got:?} and {claims:?}"
        )),
    }
}

/// Releases every claim of `claims` (with no backoff).
async fn release_all<L: DeliveryLedger>(ledger: &L, claims: &[Claim]) -> Check {
    for c in claims {
        io("release", ledger.release(c, None).await)?;
    }
    Ok(())
}

fn lost(what: &str, result: Result<(), LeaseError>) -> Check {
    eq(what, &result, &Err(LeaseError::Lost))
}

/// The recorder's consumer name.
const RECORDER: &str = "dc_recorder";

/// One delivery a [`Recorder`] saw.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    stream: StreamId,
    revision: StreamRevision,
    fence: LeaseFence,
    key: DeliveryKey,
}

/// A consumer that records every delivery, failing (after recording it) the events listed in
/// `failing`. Clones share both.
#[derive(Debug, Clone, Default)]
struct Recorder {
    seen: Arc<Mutex<Vec<Seen>>>,
    failing: Arc<Mutex<BTreeSet<(StreamId, StreamRevision)>>>,
}

impl Recorder {
    fn seen(&self) -> Vec<Seen> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn revisions(&self, s: &StreamId) -> Vec<u64> {
        self.seen()
            .into_iter()
            .filter(|d| d.stream == *s)
            .map(|d| d.revision.get())
            .collect()
    }

    fn fail(&self, s: &StreamId, revision: u64, failing: bool) {
        let mut set = self.failing.lock().unwrap_or_else(PoisonError::into_inner);
        if failing {
            set.insert((s.clone(), rev(revision)));
        } else {
            set.remove(&(s.clone(), rev(revision)));
        }
    }

    fn record(&self, delivery: Delivery<'_>) -> Result<(), ConsumerError> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Seen {
                stream: delivery.event.stream.clone(),
                revision: delivery.event.revision,
                fence: delivery.fence,
                key: delivery.key.clone(),
            });
        let failing = self
            .failing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&(delivery.event.stream.clone(), delivery.event.revision));
        if failing {
            Err(ConsumerError::new("dc_failing"))
        } else {
            Ok(())
        }
    }
}

impl Consumer for Recorder {
    const NAME: &'static str = RECORDER;

    async fn handle(&self, delivery: Delivery<'_>) -> Result<(), ConsumerError> {
        self.record(delivery)
    }
}

fn config(claim_limit: u32) -> DeliveryConfig {
    DeliveryConfig {
        lease_ttl: TTL,
        handle_deadline: TTL,
        claim_limit,
        bootstrap_page: 2,
        backoff_base: Millis::new(100),
        backoff_cap: Millis::new(400),
        max_rounds: 64,
    }
}

/// The deadline timer of the suite's workers: `Never` for every case but DC-26, so a `handle`
/// that waits on a real store's I/O is never cut; `Hand` in DC-26, where the deadline passes
/// the store clock by the time it waited and fires at once.
#[derive(Clone)]
enum Timer {
    /// Never fires.
    Never,
    /// Moves the store clock by the duration, then fires.
    Hand(ClockHand),
}

impl DeliveryTimer for Timer {
    async fn sleep(&self, duration: Millis) {
        match self {
            Self::Never => std::future::pending::<()>().await,
            Self::Hand(hand) => hand(duration),
        }
    }
}

type Worker<F> = DeliveryWorker<
    <F as DeliveryFactory>::Journal,
    <F as DeliveryFactory>::Ledger,
    SeededRng,
    Timer,
>;

fn worker<F: DeliveryFactory>(store: &Store<F>, claim_limit: u32) -> Worker<F> {
    timed_worker::<F>(store, claim_limit, Timer::Never)
}

fn timed_worker<F: DeliveryFactory>(store: &Store<F>, claim_limit: u32, timer: Timer) -> Worker<F> {
    DeliveryWorker::new(
        store.journal.clone(),
        store.ledger.clone(),
        SeededRng::new(5),
        timer,
        config(claim_limit),
    )
}

/// A store with the recorder registered (and bootstrapped).
async fn registered<F: DeliveryFactory>(factory: &F) -> Result<(Store<F>, ConsumerName), String> {
    let store = factory.fresh().await;
    let name = io(
        "register",
        worker::<F>(&store, 16).register::<Recorder>().await,
    )?;
    Ok((store, name))
}

async fn drain<F: DeliveryFactory>(
    store: &Store<F>,
    recorder: &Recorder,
) -> Result<DrainReport, String> {
    io("drain", worker::<F>(store, 16).drain(recorder, None).await)
}

/// The time just past a lease taken now.
fn past_ttl() -> Millis {
    Millis::new(TTL.get() + 1)
}

// ---------------------------------------------------------------------------------------------
// The cases

async fn no_consumer_no_mark<F: DeliveryFactory>(factory: &F) -> Check {
    let store = factory.fresh().await;
    let s1 = stream("attempt-dc1");
    eq(
        "consumers of a new store",
        &io("registered", store.ledger.registered().await)?,
        &Vec::new(),
    )?;
    append(&store.journal, &s1, None, &[101]).await?;
    let store = factory.reopen(store).await;
    let c = consumer("dc_late");
    io("register", store.ledger.register(&c).await)?;
    eq(
        "claims of a consumer registered after the unit, before its bootstrap",
        &claimed(&claim(&store.ledger, &c, None).await?),
        &Vec::new(),
    )
}

async fn every_consumer_marked<F: DeliveryFactory>(factory: &F) -> Check {
    let store = factory.fresh().await;
    let (a, b) = (consumer("dc_one"), consumer("dc_two"));
    io("register", store.ledger.register(&a).await)?;
    io("register", store.ledger.register(&b).await)?;
    let (s1, s2) = (stream("attempt-dc1"), stream("attempt-dc2"));
    append(&store.journal, &s1, None, &[201, 202]).await?;
    append(&store.journal, &s2, None, &[203]).await?;
    let store = factory.reopen(store).await;
    let want = vec![(s1.clone(), None, rev(1)), (s2.clone(), None, rev(0))];
    for c in [&a, &b] {
        let claims = claim(&store.ledger, c, None).await?;
        eq(&format!("claims of {c}"), &claimed(&claims), &want)?;
        release_all(&store.ledger, &claims).await?;
    }
    append(&store.journal, &s1, Some(1), &[204]).await?;
    eq(
        "the mark after a later unit",
        &claimed(&claim(&store.ledger, &a, Some(&s1)).await?),
        &vec![(s1, None, rev(2))],
    )
}

async fn refused_unit_no_mark<F: DeliveryFactory>(factory: &F) -> Check {
    let store = factory.fresh().await;
    let c = consumer("dc_one");
    io("register", store.ledger.register(&c).await)?;
    let (s1, s2, s3) = (
        stream("attempt-dc1"),
        stream("attempt-dc2"),
        stream("attempt-dc3"),
    );
    append(&store.journal, &s1, None, &[301]).await?;
    let first = claim_one(&store.ledger, &c, &s1).await?;
    io("advance", store.ledger.advance(&first, rev(0), TTL).await)?;
    io("release", store.ledger.release(&first, None).await)?;
    // A stale expected revision, and a duplicate event id on a new stream: both refused.
    ensure(
        "a stale expected revision is refused",
        store.journal.append(unit(&s1, None, &[302])).await.is_err(),
    )?;
    ensure(
        "a duplicate event id is refused",
        store.journal.append(unit(&s2, None, &[301])).await.is_err(),
    )?;
    // A store failure before the commit.
    ensure(
        "the factory cannot inject a storage failure (the case never skips)",
        factory
            .inject(&store, InjectedFault::StorageBeforeCommit)
            .await,
    )?;
    ensure(
        "an injected failure refuses the unit",
        store.journal.append(unit(&s3, None, &[303])).await.is_err(),
    )?;
    eq(
        "claims after refused units",
        &claimed(&claim(&store.ledger, &c, None).await?),
        &Vec::new(),
    )?;
    // A crash after each statement of a unit: its events and its mark together, or neither.
    let mut store = store;
    for n in 0..64_u32 {
        let s = stream(&format!("attempt-dc-crash{n}"));
        if !factory
            .inject(&store, InjectedFault::CrashAfterStatement(n))
            .await
        {
            ensure(
                "the factory cannot inject a crash after the first statement (the case never skips)",
                n > 0,
            )?;
            break;
        }
        let _ = store
            .journal
            .append(unit(&s, None, &[400 + u128::from(n)]))
            .await;
        store = factory.crash(store).await;
        let head = io("head", store.journal.head(&s).await)?;
        let marked = claim(&store.ledger, &c, Some(&s)).await?;
        eq(
            &format!("the mark after a crash at statement {n}, beside the unit's head"),
            &claimed(&marked),
            &head
                .map(|h| vec![(s.clone(), None, h.revision)])
                .unwrap_or_default(),
        )?;
        release_all(&store.ledger, &marked).await?;
    }
    Ok(())
}

async fn bootstrap_marks_existing<F: DeliveryFactory>(factory: &F) -> Check {
    let store = factory.fresh().await;
    let (s1, s2, s3) = (
        stream("attempt-dc1"),
        stream("attempt-dc2"),
        stream("attempt-dc3"),
    );
    append(&store.journal, &s3, None, &[501, 502]).await?;
    append(&store.journal, &s1, None, &[503]).await?;
    append(&store.journal, &s2, None, &[504]).await?;
    let c = consumer("dc_one");
    io("register", store.ledger.register(&c).await)?;
    let store = factory.reopen(store).await;
    let pages = [
        io("bootstrap", store.ledger.bootstrap(&c, None, 2).await)?,
        io("bootstrap", store.ledger.bootstrap(&c, Some(&s2), 2).await)?,
    ];
    eq("the pages' last keys", &pages, &[Some(s2.clone()), None])?;
    let state = io("registered", store.ledger.registered().await)?;
    ensure(
        "the consumer is recorded as bootstrapped",
        state.iter().any(|r| r.name == c && r.bootstrapped),
    )?;
    let again = io("bootstrap", store.ledger.bootstrap(&c, None, 10).await)?;
    eq("a repeated bootstrap", &again, &None)?;
    eq(
        "claims after the bootstrap",
        &claimed(&claim(&store.ledger, &c, None).await?),
        &vec![(s1, None, rev(0)), (s2, None, rev(0)), (s3, None, rev(1))],
    )
}

async fn register_twice_is_one<F: DeliveryFactory>(factory: &F) -> Check {
    let store = factory.fresh().await;
    let c = consumer("dc_one");
    io("register", store.ledger.register(&c).await)?;
    let other = factory.second_handle(&store).await;
    io("register", other.ledger.register(&c).await)?;
    let names: Vec<ConsumerName> = io("registered", store.ledger.registered().await)?
        .into_iter()
        .map(|r| r.name)
        .collect();
    eq("registered consumers", &names, &vec![c.clone()])?;
    let s1 = stream("attempt-dc1");
    append(&store.journal, &s1, None, &[601]).await?;
    eq(
        "claims of the consumer registered twice",
        &claimed(&claim(&store.ledger, &c, None).await?),
        &vec![(s1, None, rev(0))],
    )
}

async fn retired_not_marked<F: DeliveryFactory>(factory: &F) -> Check {
    let store = factory.fresh().await;
    let (a, b) = (consumer("dc_one"), consumer("dc_two"));
    io("register", store.ledger.register(&a).await)?;
    io("register", store.ledger.register(&b).await)?;
    let s1 = stream("attempt-dc1");
    append(&store.journal, &s1, None, &[701]).await?;
    let held = claim_one(&store.ledger, &a, &s1).await?;
    io("advance", store.ledger.advance(&held, rev(0), TTL).await)?;
    io("retire", store.ledger.retire(&a).await)?;
    let names: Vec<ConsumerName> = io("registered", store.ledger.registered().await)?
        .into_iter()
        .map(|r| r.name)
        .collect();
    eq("registered after the retirement", &names, &vec![b.clone()])?;
    append(&store.journal, &s1, Some(0), &[702]).await?;
    lost(
        "the retired consumer's held claim advancing",
        store.ledger.advance(&held, rev(1), TTL).await,
    )?;
    lost(
        "the retired consumer's held claim releasing",
        store.ledger.release(&held, None).await,
    )?;
    let store = factory.reopen(store).await;
    io("register", store.ledger.register(&a).await)?;
    eq(
        "claims of the consumer registered again, before its bootstrap",
        &claimed(&claim(&store.ledger, &a, None).await?),
        &Vec::new(),
    )?;
    while io("bootstrap", store.ledger.bootstrap(&a, None, 10).await)?.is_some() {}
    let again = claim_one(&store.ledger, &a, &s1).await?;
    eq(
        "the claim after the bootstrap (no checkpoint survived)",
        &claimed(std::slice::from_ref(&again)),
        &vec![(s1.clone(), None, rev(1))],
    )?;
    ensure(
        &format!(
            "the fence after the retirement ({}) is above the retired one's ({})",
            again.fence, held.fence
        ),
        again.fence > held.fence,
    )?;
    eq(
        "the other consumer's claim",
        &claimed(&claim(&store.ledger, &b, None).await?),
        &vec![(s1, None, rev(1))],
    )
}

async fn drain_once_in_order<F: DeliveryFactory>(factory: &F) -> Check {
    let (store, _) = registered(factory).await?;
    let (s1, s2) = (stream("attempt-dc1"), stream("attempt-dc2"));
    append(&store.journal, &s1, None, &[801, 802]).await?;
    append(&store.journal, &s2, None, &[803, 804]).await?;
    append(&store.journal, &s1, Some(1), &[805]).await?;
    let store = factory.reopen(store).await;
    let recorder = Recorder::default();
    let report = drain::<F>(&store, &recorder).await?;
    eq("delivered", &report.delivered, &5)?;
    eq("revisions of s1", &recorder.revisions(&s1), &vec![0, 1, 2])?;
    eq("revisions of s2", &recorder.revisions(&s2), &vec![0, 1])?;
    let ids: Vec<EventId> = recorder
        .seen()
        .into_iter()
        .map(|d| d.key.event_id)
        .collect();
    let mut want: Vec<EventId> = [801, 802, 805, 803, 804]
        .into_iter()
        .map(EventId::from_u128)
        .collect();
    let mut got = ids;
    got.sort();
    want.sort();
    eq("the events delivered", &got, &want)?;
    let store = factory.reopen(store).await;
    let again = drain::<F>(&store, &recorder).await?;
    eq("a second drain", &again, &DrainReport::default())
}

async fn mark_during_processing<F: DeliveryFactory>(factory: &F) -> Check {
    let (store, c) = registered(factory).await?;
    let s1 = stream("attempt-dc1");
    append(&store.journal, &s1, None, &[901]).await?;
    let first = claim_one(&store.ledger, &c, &s1).await?;
    // The model's `markDuringProcessing`: a unit commits after the claim, before the advance.
    append(&store.journal, &s1, Some(0), &[902]).await?;
    io("advance", store.ledger.advance(&first, rev(0), TTL).await)?;
    io("release", store.ledger.release(&first, None).await)?;
    let next = claim_one(&store.ledger, &c, &s1).await?;
    eq(
        "the claim after the advance",
        &claimed(std::slice::from_ref(&next)),
        &vec![(s1.clone(), Some(rev(0)), rev(1))],
    )?;
    io("release", store.ledger.release(&next, None).await)?;
    // Through the worker: a unit committed while an event is handled is delivered too.
    let recorder = Recorder::default();
    let report = drain::<F>(&store, &recorder).await?;
    eq(
        "revisions delivered after the advance",
        &recorder.revisions(&s1),
        &vec![1],
    )?;
    eq("delivered", &report.delivered, &1)
}

async fn live_lease_not_claimable<F: DeliveryFactory>(factory: &F) -> Check {
    let (store, c) = registered(factory).await?;
    let s1 = stream("attempt-dc1");
    append(&store.journal, &s1, None, &[1001]).await?;
    let held = claim_one(&store.ledger, &c, &s1).await?;
    let other = factory.second_handle(&store).await;
    eq(
        "claims from another handle while the lease lives",
        &claimed(&claim(&other.ledger, &c, None).await?),
        &Vec::new(),
    )?;
    eq(
        "a kicked claim while the lease lives",
        &claimed(&claim(&store.ledger, &c, Some(&s1)).await?),
        &Vec::new(),
    )?;
    tick(factory, Millis::new(TTL.get() - 1));
    eq(
        "claims just before the lease ends",
        &claimed(&claim(&other.ledger, &c, None).await?),
        &Vec::new(),
    )?;
    tick(factory, Millis::new(1));
    let taken = claim_one(&other.ledger, &c, &s1).await?;
    ensure("the expired lease is taken over", taken.fence > held.fence)
}

async fn kick_claims_only_its_stream<F: DeliveryFactory>(factory: &F) -> Check {
    let (store, c) = registered(factory).await?;
    let (s1, s2) = (stream("attempt-dc1"), stream("attempt-dc2"));
    append(&store.journal, &s1, None, &[1101]).await?;
    append(&store.journal, &s2, None, &[1102, 1103]).await?;
    let kicked = claim(&store.ledger, &c, Some(&s2)).await?;
    eq(
        "a claim for one stream",
        &claimed(&kicked),
        &vec![(s2.clone(), None, rev(1))],
    )?;
    release_all(&store.ledger, &kicked).await?;
    let kicks = Kicks::new(8);
    kicks.kick(&s2);
    let recorder = Recorder::default();
    let report = io(
        "drain_kicked",
        worker::<F>(&store, 16)
            .drain_kicked(&recorder, &kicks)
            .await,
    )?;
    eq(
        "revisions of the kicked stream",
        &recorder.revisions(&s2),
        &vec![0, 1],
    )?;
    eq(
        "revisions of the other stream",
        &recorder.revisions(&s1),
        &Vec::new(),
    )?;
    eq("delivered", &report.delivered, &2)?;
    eq("kicks left", &kicks.take(), &Vec::new())
}

async fn stale_advance_refused<F: DeliveryFactory>(factory: &F) -> Check {
    let (store, c) = registered(factory).await?;
    let s1 = stream("attempt-dc1");
    append(&store.journal, &s1, None, &[1201]).await?;
    // The model's `staleWorkerFenced`: claim, expire, claim by another worker.
    let stale = claim_one(&store.ledger, &c, &s1).await?;
    tick(factory, past_ttl());
    let other = factory.second_handle(&store).await;
    let fresh = claim_one(&other.ledger, &c, &s1).await?;
    lost(
        "the stale worker's advance",
        store.ledger.advance(&stale, rev(0), TTL).await,
    )?;
    lost(
        "the stale worker's renewal",
        store.ledger.renew(&stale, TTL).await,
    )?;
    lost(
        "the stale worker's release",
        store.ledger.release(&stale, None).await,
    )?;
    eq(
        "the new holder's release (the stale steps changed nothing)",
        &other.ledger.release(&fresh, None).await,
        &Ok(()),
    )?;
    eq(
        "the claim after the stale steps",
        &claimed(&claim(&store.ledger, &c, Some(&s1)).await?),
        &vec![(s1, None, rev(0))],
    )
}

async fn expired_advance_refused<F: DeliveryFactory>(factory: &F) -> Check {
    let (store, c) = registered(factory).await?;
    let s1 = stream("attempt-dc1");
    append(&store.journal, &s1, None, &[1301]).await?;
    let held = claim_one(&store.ledger, &c, &s1).await?;
    tick(factory, Millis::new(TTL.get()));
    lost(
        "an advance at the lease's end",
        store.ledger.advance(&held, rev(0), TTL).await,
    )?;
    eq(
        "the claim after the refused advance",
        &claimed(&claim(&store.ledger, &c, Some(&s1)).await?),
        &vec![(s1, None, rev(0))],
    )
}

async fn checkpoint_never_back<F: DeliveryFactory>(factory: &F) -> Check {
    let (store, c) = registered(factory).await?;
    let s1 = stream("attempt-dc1");
    append(&store.journal, &s1, None, &[1401, 1402, 1403]).await?;
    let held = claim_one(&store.ledger, &c, &s1).await?;
    io("advance", store.ledger.advance(&held, rev(1), TTL).await)?;
    io("advance", store.ledger.advance(&held, rev(0), TTL).await)?;
    io("release", store.ledger.release(&held, None).await)?;
    let store = factory.reopen(store).await;
    let next = claim_one(&store.ledger, &c, &s1).await?;
    eq(
        "the checkpoint after an advance to a lower revision",
        &next.checkpoint,
        &Some(rev(1)),
    )?;
    io("advance", store.ledger.advance(&next, rev(2), TTL).await)?;
    io("release", store.ledger.release(&next, None).await)?;
    append(&store.journal, &s1, Some(2), &[1404]).await?;
    let last = claim_one(&store.ledger, &c, &s1).await?;
    eq("the checkpoint later", &last.checkpoint, &Some(rev(2)))
}

async fn dead_before_effect<F: DeliveryFactory>(factory: &F) -> Check {
    let (store, c) = registered(factory).await?;
    let s1 = stream("attempt-dc1");
    append(&store.journal, &s1, None, &[1501]).await?;
    let dead = claim_one(&store.ledger, &c, &s1).await?;
    io("renew", store.ledger.renew(&dead, TTL).await)?;
    // The worker dies here, before its effect; its lease runs out.
    tick(factory, past_ttl());
    let recorder = Recorder::default();
    let report = drain::<F>(&store, &recorder).await?;
    eq(
        "revisions delivered after the death",
        &recorder.revisions(&s1),
        &vec![0],
    )?;
    eq("delivered", &report.delivered, &1)
}

async fn dead_after_effect<F: DeliveryFactory>(factory: &F) -> Check {
    let (store, c) = registered(factory).await?;
    let s1 = stream("attempt-dc1");
    append(&store.journal, &s1, None, &[1601]).await?;
    let recorder = Recorder::default();
    // The model's `workerCrashRedelivers`: claim, effect, the worker crashes, the lease expires.
    let dead = claim_one(&store.ledger, &c, &s1).await?;
    let events = io(
        "read_stream",
        store
            .journal
            .read_stream(&s1, dead.checkpoint, Some(dead.marked))
            .await,
    )?;
    for event in &events {
        let key = DeliveryKey {
            consumer: c.clone(),
            event_id: event.event_id,
        };
        io(
            "handle",
            recorder
                .handle(Delivery {
                    event,
                    fence: dead.fence,
                    key: &key,
                })
                .await,
        )?;
    }
    tick(factory, past_ttl());
    let report = drain::<F>(&store, &recorder).await?;
    eq("revisions seen", &recorder.revisions(&s1), &vec![0, 0])?;
    let keys: BTreeSet<DeliveryKey> = recorder.seen().into_iter().map(|d| d.key).collect();
    eq("distinct delivery keys", &keys.len(), &1)?;
    eq("delivered by the drain", &report.delivered, &1)
}

async fn advanced_before_death<F: DeliveryFactory>(factory: &F) -> Check {
    let (store, c) = registered(factory).await?;
    let s1 = stream("attempt-dc1");
    append(&store.journal, &s1, None, &[1701]).await?;
    let dead = claim_one(&store.ledger, &c, &s1).await?;
    io("advance", store.ledger.advance(&dead, rev(0), TTL).await)?;
    tick(factory, past_ttl());
    let store = factory.reopen(store).await;
    let recorder = Recorder::default();
    let report = drain::<F>(&store, &recorder).await?;
    eq(
        "revisions delivered after the death",
        &recorder.revisions(&s1),
        &Vec::new(),
    )?;
    eq("the drain", &report, &DrainReport::default())
}

async fn dead_worker_waits_for_lease<F: DeliveryFactory>(factory: &F) -> Check {
    let (store, c) = registered(factory).await?;
    let s1 = stream("attempt-dc1");
    append(&store.journal, &s1, None, &[1801]).await?;
    let _dead = claim_one(&store.ledger, &c, &s1).await?;
    let other = factory.second_handle(&store).await;
    let recorder = Recorder::default();
    let early = drain::<F>(&other, &recorder).await?;
    eq(
        "a drain while the dead worker's lease lives",
        &early,
        &DrainReport::default(),
    )?;
    tick(factory, Millis::new(TTL.get() - 1));
    let still = drain::<F>(&other, &recorder).await?;
    eq(
        "a drain just before the lease ends",
        &still,
        &DrainReport::default(),
    )?;
    tick(factory, Millis::new(1));
    let after = drain::<F>(&other, &recorder).await?;
    eq(
        "revisions delivered once the lease ended",
        &recorder.revisions(&s1),
        &vec![0],
    )?;
    eq("delivered", &after.delivered, &1)
}

async fn failing_consumer_no_skip<F: DeliveryFactory>(factory: &F) -> Check {
    let (store, c) = registered(factory).await?;
    let s1 = stream("attempt-dc1");
    append(&store.journal, &s1, None, &[1901, 1902, 1903]).await?;
    let recorder = Recorder::default();
    recorder.fail(&s1, 1, true);
    let worker = worker::<F>(&store, 16);
    for round in 0..4 {
        let report = io("drain", worker.drain(&recorder, None).await)?;
        ensure(
            &format!("round {round}: the failing stream is reported failed: {report:?}"),
            report.failed.iter().any(|(s, _)| *s == s1),
        )?;
        let (_, backoff) = report
            .failed
            .last()
            .cloned()
            .ok_or_else(|| format!("round {round}: no backoff reported"))?;
        tick(factory, backoff);
    }
    eq(
        "revisions seen while revision 1 fails",
        &recorder.revisions(&s1),
        &vec![0, 1, 1, 1, 1],
    )?;
    let held = claim_one(&store.ledger, &c, &s1).await?;
    eq(
        "the checkpoint while revision 1 fails",
        &held.checkpoint,
        &Some(rev(0)),
    )?;
    io("release", store.ledger.release(&held, None).await)?;
    recorder.fail(&s1, 1, false);
    io("drain", worker.drain(&recorder, None).await)?;
    eq(
        "revisions seen once it is fixed",
        &recorder.revisions(&s1),
        &vec![0, 1, 1, 1, 1, 1, 2],
    )
}

async fn long_down_catches_up<F: DeliveryFactory>(factory: &F) -> Check {
    let (store, _) = registered(factory).await?;
    let streams: Vec<StreamId> = (0..5).map(|n| stream(&format!("attempt-dc{n}"))).collect();
    for round in 0..3_u64 {
        for (n, s) in (0_u128..).zip(&streams) {
            let id = 2000 + n * 10 + u128::from(round);
            append(&store.journal, s, round.checked_sub(1), &[id]).await?;
        }
    }
    tick(factory, Millis::new(10 * TTL.get()));
    let store = factory.crash(store).await;
    let recorder = Recorder::default();
    let report = io("catch_up", worker::<F>(&store, 2).catch_up(&recorder).await)?;
    eq("delivered", &report.delivered, &15)?;
    for s in &streams {
        eq(
            &format!("revisions of {s}"),
            &recorder.revisions(s),
            &vec![0, 1, 2],
        )?;
    }
    Ok(())
}

async fn takeover_higher_fence<F: DeliveryFactory>(factory: &F) -> Check {
    let (store, c) = registered(factory).await?;
    let s1 = stream("attempt-dc1");
    append(&store.journal, &s1, None, &[2101]).await?;
    let first = claim_one(&store.ledger, &c, &s1).await?;
    tick(factory, past_ttl());
    let store = factory.reopen(store).await;
    let second = claim_one(&store.ledger, &c, &s1).await?;
    io("release", store.ledger.release(&second, None).await)?;
    let third = claim_one(&store.ledger, &c, &s1).await?;
    ensure(
        &format!(
            "fences rise with each claim: {}, {}, {}",
            first.fence, second.fence, third.fence
        ),
        first.fence < second.fence && second.fence < third.fence,
    )
}

async fn delivery_carries_fence<F: DeliveryFactory>(factory: &F) -> Check {
    let (store, c) = registered(factory).await?;
    let s1 = stream("attempt-dc1");
    append(&store.journal, &s1, None, &[2201, 2202]).await?;
    let abandoned = claim_one(&store.ledger, &c, &s1).await?;
    tick(factory, past_ttl());
    let recorder = Recorder::default();
    drain::<F>(&store, &recorder).await?;
    append(&store.journal, &s1, Some(1), &[2203]).await?;
    drain::<F>(&store, &recorder).await?;
    let fences: Vec<(u64, LeaseFence)> = recorder
        .seen()
        .into_iter()
        .map(|d| (d.revision.get(), d.fence))
        .collect();
    let next = LeaseFence::new(abandoned.fence.get() + 1);
    let last = LeaseFence::new(abandoned.fence.get() + 2);
    eq(
        "each delivery's fence",
        &fences,
        &vec![(0, next), (1, next), (2, last)],
    )
}

async fn expired_renew_refused<F: DeliveryFactory>(factory: &F) -> Check {
    let (store, c) = registered(factory).await?;
    let s1 = stream("attempt-dc1");
    append(&store.journal, &s1, None, &[2301]).await?;
    let held = claim_one(&store.ledger, &c, &s1).await?;
    tick(factory, Millis::new(TTL.get() - 1));
    io("renew", store.ledger.renew(&held, TTL).await)?;
    tick(factory, Millis::new(TTL.get() - 1));
    let other = factory.second_handle(&store).await;
    eq(
        "claims after a renewal in time",
        &claimed(&claim(&other.ledger, &c, None).await?),
        &Vec::new(),
    )?;
    tick(factory, Millis::new(1));
    lost(
        "a renewal at the renewed lease's end",
        store.ledger.renew(&held, TTL).await,
    )
}

/// A consumer that, while handling the first event of `slow`, lets the store clock pass its
/// lease and has another handle take the stream over: a slow consumer that loses its claim.
struct Slow<L> {
    recorder: Recorder,
    slow: StreamId,
    rival: L,
    clock: ClockHand,
    taken: Mutex<Option<Claim>>,
}

impl<L: DeliveryLedger> Consumer for Slow<L> {
    const NAME: &'static str = RECORDER;

    async fn handle(&self, delivery: Delivery<'_>) -> Result<(), ConsumerError> {
        let first = self
            .taken
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none();
        if delivery.event.stream == self.slow && first {
            (self.clock)(past_ttl());
            let mut taken = self
                .rival
                .claim(&delivery.key.consumer, TTL, 1, Some(&self.slow))
                .await
                .map_err(|_| ConsumerError::new("dc_rival_failed"))?;
            *self.taken.lock().unwrap_or_else(PoisonError::into_inner) = taken.pop();
        }
        self.recorder.record(delivery)
    }
}

async fn lost_stream_others_go_on<F: DeliveryFactory>(factory: &F) -> Check {
    let (store, _) = registered(factory).await?;
    let (s1, s2) = (stream("attempt-dc1"), stream("attempt-dc2"));
    append(&store.journal, &s1, None, &[2401, 2402]).await?;
    append(&store.journal, &s2, None, &[2403]).await?;
    let rival = factory.second_handle(&store).await;
    let slow = Slow {
        recorder: Recorder::default(),
        slow: s1.clone(),
        rival: rival.ledger.clone(),
        clock: factory.clock(),
        taken: Mutex::new(None),
    };
    // One stream a round: the slow stream is claimed first (by key), lost while handled, and
    // the drain goes on to the other in its next round.
    let report = io("drain", worker::<F>(&store, 1).drain(&slow, None).await)?;
    eq("streams lost", &report.lost, &vec![s1.clone()])?;
    eq(
        "revisions of the other stream",
        &slow.recorder.revisions(&s2),
        &vec![0],
    )?;
    eq(
        "revisions of the lost stream",
        &slow.recorder.revisions(&s1),
        &vec![0],
    )?;
    eq("delivered", &report.delivered, &1)?;
    let taken = slow
        .taken
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .ok_or("the rival took no claim")?;
    eq(
        "the rival's claim of the lost stream (nothing advanced by the slow worker)",
        &claimed(std::slice::from_ref(&taken)),
        &vec![(s1, None, rev(1))],
    )
}

async fn foreign_consumer_marked<F: DeliveryFactory>(factory: &F) -> Check {
    let store = factory.fresh().await;
    let other_build = factory.second_handle(&store).await;
    let foreign = consumer("dc_foreign");
    io("register", other_build.ledger.register(&foreign).await)?;
    let store = factory.reopen(store).await;
    let s1 = stream("attempt-dc1");
    append(&store.journal, &s1, None, &[2501]).await?;
    eq(
        "claims of a consumer this build has no code for",
        &claimed(&claim(&other_build.ledger, &foreign, None).await?),
        &vec![(s1, None, rev(0))],
    )
}

async fn retry_after_honoured<F: DeliveryFactory>(factory: &F) -> Check {
    let (store, c) = registered(factory).await?;
    let s1 = stream("attempt-dc1");
    append(&store.journal, &s1, None, &[2601]).await?;
    let held = claim_one(&store.ledger, &c, &s1).await?;
    io(
        "release",
        store.ledger.release(&held, Some(Millis::new(500))).await,
    )?;
    eq(
        "claims right after the release",
        &claimed(&claim(&store.ledger, &c, None).await?),
        &Vec::new(),
    )?;
    tick(factory, Millis::new(499));
    let store = factory.reopen(store).await;
    eq(
        "claims before the retry time",
        &claimed(&claim(&store.ledger, &c, None).await?),
        &Vec::new(),
    )?;
    tick(factory, Millis::new(1));
    eq(
        "claims at the retry time",
        &claimed(&claim(&store.ledger, &c, None).await?),
        &vec![(s1, None, rev(0))],
    )
}

/// Pending once (waking itself), then ready: a call that does not finish on its first poll.
#[derive(Default)]
struct Yield {
    yielded: bool,
}

impl Future for Yield {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.yielded {
            return Poll::Ready(());
        }
        self.yielded = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

/// A consumer whose `handle` of `slow`, while `slow_now` holds, outlives the lease: it does
/// not finish on its first poll, and when it does it has let the store clock pass the lease.
struct Sluggish {
    recorder: Recorder,
    slow: StreamId,
    slow_now: Mutex<bool>,
    clock: ClockHand,
}

impl Consumer for Sluggish {
    const NAME: &'static str = RECORDER;

    async fn handle(&self, delivery: Delivery<'_>) -> Result<(), ConsumerError> {
        let slow = delivery.event.stream == self.slow
            && *self.slow_now.lock().unwrap_or_else(PoisonError::into_inner);
        self.recorder.record(delivery)?;
        if slow {
            Yield::default().await;
            (self.clock)(past_ttl());
        }
        Ok(())
    }
}

async fn slow_handle_cut<F: DeliveryFactory>(factory: &F, under_test: SuiteWorker) -> Check {
    let (store, c) = registered(factory).await?;
    let (s1, s2) = (stream("attempt-dc1"), stream("attempt-dc2"));
    append(&store.journal, &s1, None, &[2701]).await?;
    append(&store.journal, &s2, None, &[2702]).await?;
    let sluggish = Sluggish {
        recorder: Recorder::default(),
        slow: s1.clone(),
        slow_now: Mutex::new(true),
        clock: factory.clock(),
    };
    let timer = match under_test {
        SuiteWorker::Production => Timer::Hand(factory.clock()),
        SuiteWorker::WithoutHandleDeadline => Timer::Never,
    };
    let worker = timed_worker::<F>(&store, 16, timer);
    let report = io("drain", worker.drain(&sluggish, None).await)?;
    let failed: Vec<StreamId> = report.failed.iter().map(|(s, _)| s.clone()).collect();
    eq(
        "streams failed (cut at the deadline)",
        &failed,
        &vec![s1.clone()],
    )?;
    eq("streams lost", &report.lost, &Vec::new())?;
    eq(
        "deliveries of the slow stream in one drain",
        &sluggish.recorder.revisions(&s1),
        &vec![0],
    )?;
    eq(
        "revisions of the other stream",
        &sluggish.recorder.revisions(&s2),
        &vec![0],
    )?;
    let backoff = report.failed.first().map_or(Millis::new(0), |(_, b)| *b);
    eq(
        "claims of the cut stream before its backoff passed",
        &claimed(&claim(&store.ledger, &c, Some(&s1)).await?),
        &Vec::new(),
    )?;
    tick(factory, backoff);
    *sluggish
        .slow_now
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = false;
    let again = io("drain", worker.drain(&sluggish, None).await)?;
    eq("delivered once the handle is quick", &again.delivered, &1)?;
    eq(
        "deliveries of the slow stream",
        &sluggish.recorder.revisions(&s1),
        &vec![0, 0],
    )?;
    let store = factory.reopen(store).await;
    eq(
        "claims once delivered",
        &claimed(&claim(&store.ledger, &c, None).await?),
        &Vec::new(),
    )
}

/// A consumer that lets the store clock pass `step` per delivery.
struct Ticking {
    recorder: Recorder,
    step: Millis,
    clock: ClockHand,
}

impl Consumer for Ticking {
    const NAME: &'static str = RECORDER;

    async fn handle(&self, delivery: Delivery<'_>) -> Result<(), ConsumerError> {
        (self.clock)(self.step);
        self.recorder.record(delivery)
    }
}

async fn claimed_when_reached<F: DeliveryFactory>(factory: &F) -> Check {
    let (store, _) = registered(factory).await?;
    let streams = [
        stream("attempt-dc1"),
        stream("attempt-dc2"),
        stream("attempt-dc3"),
    ];
    for (s, id) in streams.iter().zip([2801, 2802, 2803]) {
        append(&store.journal, s, None, &[id]).await?;
    }
    // Each delivery takes three quarters of a lease: within its own lease, but a claim of the
    // third stream taken with the first would run out before the drain reached it.
    let ticking = Ticking {
        recorder: Recorder::default(),
        step: Millis::new(TTL.get() * 3 / 4),
        clock: factory.clock(),
    };
    let report = io("drain", worker::<F>(&store, 16).drain(&ticking, None).await)?;
    eq("streams lost", &report.lost, &Vec::new())?;
    eq("delivered", &report.delivered, &3)?;
    for s in &streams {
        eq(
            &format!("revisions of {s}"),
            &ticking.recorder.revisions(s),
            &vec![0],
        )?;
    }
    Ok(())
}

async fn failing_stream_no_starvation<F: DeliveryFactory>(factory: &F) -> Check {
    let (store, _) = registered(factory).await?;
    let (s1, s2, s3) = (
        stream("attempt-dc1"),
        stream("attempt-dc2"),
        stream("attempt-dc3"),
    );
    for (s, id) in [(&s1, 2901), (&s2, 2902), (&s3, 2903)] {
        append(&store.journal, s, None, &[id]).await?;
    }
    // The lowest stream always fails, and each delivery lets the store clock pass more than the
    // largest backoff (400 ms) but less than a lease: the failing stream is claimable again
    // after every later delivery. A discovery that restarts from the lowest key would hand it
    // back before the third stream; a worker that took it again would handle it every time
    // the walk came round.
    let ticking = Ticking {
        recorder: Recorder::default(),
        step: Millis::new(TTL.get() / 2),
        clock: factory.clock(),
    };
    ticking.recorder.fail(&s1, 0, true);
    let report = io("drain", worker::<F>(&store, 16).drain(&ticking, None).await)?;
    eq(
        "revisions of the later streams",
        &(
            ticking.recorder.revisions(&s2),
            ticking.recorder.revisions(&s3),
        ),
        &(vec![0], vec![0]),
    )?;
    eq(
        "deliveries of the failing stream in one drain",
        &ticking.recorder.revisions(&s1),
        &vec![0],
    )?;
    let failed: Vec<StreamId> = report.failed.iter().map(|(s, _)| s.clone()).collect();
    eq("streams failed", &failed, &vec![s1])?;
    eq("streams lost", &report.lost, &Vec::new())?;
    eq("delivered", &report.delivered, &2)
}
