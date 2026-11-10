//! `DeliveryWorker` over the store's write path (component level): a saved unit marks the
//! consumer and kicks its worker, a lost kick costs only latency, a failing consumer backs off
//! with jitter from the `Rng` port and starts it again once the stream advances, a journal that
//! ends short of a mark is backed off and reported instead of spun on, and a consumer must be
//! named in `snake_case` (`doc/event-store.md` §3.4, §5.1, §5.5). The ledger's own promises are the delivery
//! contract's (`mayfly_testkit::contracts::delivery`).

use std::sync::{Arc, Mutex, PoisonError};

use mayfly_control::delivery::{
    Consumer, ConsumerError, Delivery, DeliveryConfig, DeliveryError, DeliveryWorker, Kicks,
};
use mayfly_control::journal::{
    AppendError, Appended, CommandReceipt, EventLocation, InboxKey, Journal, Payload,
    ProjectionName, RecordedEvent, StorageError, StoredKind, StreamHead, StreamId, StreamRevision,
    UnitOfWork,
};
use mayfly_control::store::{AggregateStore, Rng, SaveRequest};
use mayfly_domain::{CommandId, EventId, Millis};
use mayfly_testkit::block_on;
use mayfly_testkit::clock::{FakeClock, SeededRng};
use mayfly_testkit::delivery::MemoryLedger;
use mayfly_testkit::journal::MemoryJournal;

use crate::counter::{Counter, CounterBody, Counters, command};

/// Records each delivered `(stream, revision)`, failing every one while `failing` is set, and
/// the revision `fail_at` names.
#[derive(Debug, Clone, Default)]
struct Seen {
    seen: Arc<Mutex<Vec<(StreamId, StreamRevision)>>>,
    failing: Arc<Mutex<bool>>,
    fail_at: Arc<Mutex<Option<StreamRevision>>>,
}

impl Seen {
    fn seen(&self) -> Vec<(StreamId, StreamRevision)> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Consumer for Seen {
    const NAME: &'static str = "seen";

    async fn handle(&self, delivery: Delivery<'_>) -> Result<(), ConsumerError> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((delivery.event.stream.clone(), delivery.event.revision));
        let fail_at = *self.fail_at.lock().unwrap_or_else(PoisonError::into_inner);
        if *self.failing.lock().unwrap_or_else(PoisonError::into_inner)
            || fail_at == Some(delivery.event.revision)
        {
            Err(ConsumerError::new("seen_failing"))
        } else {
            Ok(())
        }
    }
}

struct Misnamed;

impl Consumer for Misnamed {
    const NAME: &'static str = "Not-Snake";

    async fn handle(&self, _delivery: Delivery<'_>) -> Result<(), ConsumerError> {
        Ok(())
    }
}

const SEED: u64 = 9;

struct Harness {
    store: AggregateStore<MemoryJournal, FakeClock, SeededRng, Kicks>,
    worker: DeliveryWorker<MemoryJournal, MemoryLedger<FakeClock>, SeededRng, FakeClock>,
    kicks: Kicks,
    store_clock: FakeClock,
}

fn harness(kick_capacity: usize) -> Harness {
    let journal = MemoryJournal::new();
    let store_clock = FakeClock::at(Millis::new(5_000));
    let kicks = Kicks::new(kick_capacity);
    let store = AggregateStore::new::<Counters>(
        journal.clone(),
        FakeClock::at(Millis::new(1_700_000_000_000)),
        SeededRng::new(1),
        kicks.clone(),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let ledger = MemoryLedger::new(&journal, store_clock.clone());
    let worker = DeliveryWorker::new(
        journal,
        ledger,
        SeededRng::new(SEED),
        store_clock.clone(),
        DeliveryConfig {
            backoff_base: Millis::new(100),
            backoff_cap: Millis::new(400),
            ..DeliveryConfig::default()
        },
    );
    Harness {
        store,
        worker,
        kicks,
        store_clock,
    }
}

fn increment(h: &Harness, id: u128) {
    let request = SaveRequest::new("1", command(id, CounterBody::Increment { by: 1, limit: 9 }));
    block_on(h.store.save::<Counter>(request)).unwrap_or_else(|e| panic!("{e}"));
}

fn counter_1() -> StreamId {
    StreamId::new("counter-1").unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn should_deliver_a_saved_unit_when_its_kick_is_drained() {
    // given
    let h = harness(8);
    let consumer = Seen::default();
    block_on(h.worker.register::<Seen>()).unwrap_or_else(|e| panic!("{e}"));

    // when
    increment(&h, 1);
    let report = block_on(h.worker.drain_kicked(&consumer, &h.kicks));

    // then
    assert_eq!(report.map(|r| r.delivered), Ok(1));
    assert_eq!(consumer.seen(), [(counter_1(), StreamRevision::new(0))]);
    assert_eq!(h.kicks.take(), Vec::<StreamId>::new());
}

#[test]
fn should_lose_only_latency_when_a_kick_is_lost() {
    // given: a kick queue that holds nothing, and a worker that registered before the commit
    let h = harness(0);
    let consumer = Seen::default();
    block_on(h.worker.register::<Seen>()).unwrap_or_else(|e| panic!("{e}"));
    increment(&h, 1);

    // when
    let kicked = block_on(h.worker.drain_kicked(&consumer, &h.kicks));
    let caught_up = block_on(h.worker.catch_up(&consumer));

    // then
    assert_eq!(h.kicks.dropped(), 1);
    assert_eq!(kicked.map(|r| r.delivered), Ok(0));
    assert_eq!(caught_up.map(|r| r.delivered), Ok(1));
    assert_eq!(consumer.seen(), [(counter_1(), StreamRevision::new(0))]);
}

#[test]
fn should_back_off_with_jitter_from_the_rng_when_a_consumer_keeps_failing() {
    // given
    let h = harness(8);
    let consumer = Seen::default();
    *consumer
        .failing
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = true;
    block_on(h.worker.register::<Seen>()).unwrap_or_else(|e| panic!("{e}"));
    increment(&h, 1);

    // when
    let mut backoffs = Vec::new();
    for _ in 0..4 {
        let report = block_on(h.worker.drain(&consumer, None)).unwrap_or_else(|e| panic!("{e}"));
        backoffs.extend(report.failed.into_iter().map(|(_, after)| after));
        if let Some(last) = backoffs.last() {
            h.store_clock.advance(*last);
        }
    }

    // then: full jitter in 1..=d, d doubling from 100 ms to the 400 ms cap, drawn in order
    let rng = SeededRng::new(SEED);
    let expected: Vec<Millis> = [100, 200, 400, 400]
        .into_iter()
        .map(|d| Millis::new(1 + rng.below(d)))
        .collect();
    assert_eq!(backoffs, expected);
    assert_eq!(consumer.seen().len(), 4);
}

#[test]
fn should_not_retry_before_the_backoff_passes_when_a_consumer_fails() {
    // given
    let h = harness(8);
    let consumer = Seen::default();
    *consumer
        .failing
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = true;
    block_on(h.worker.register::<Seen>()).unwrap_or_else(|e| panic!("{e}"));
    increment(&h, 1);
    let first = block_on(h.worker.drain(&consumer, None)).unwrap_or_else(|e| panic!("{e}"));
    let backoff = first.failed.first().map(|(_, after)| *after);

    // when
    let again = block_on(h.worker.drain(&consumer, None)).unwrap_or_else(|e| panic!("{e}"));

    // then
    assert!(backoff.is_some_and(|b| b.get() >= 1), "{first:?}");
    assert_eq!(again.failed, Vec::new());
    assert_eq!(consumer.seen().len(), 1);
}

#[test]
fn should_refuse_a_consumer_when_its_name_is_not_snake_case() {
    // given
    let h = harness(8);

    // when
    let registered = block_on(h.worker.register::<Misnamed>());
    let drained = block_on(h.worker.drain(&Misnamed, None));

    // then
    assert_eq!(registered, Err(DeliveryError::InvalidConsumerName));
    assert_eq!(drained, Err(DeliveryError::InvalidConsumerName));
}

#[test]
fn should_wake_a_waiter_when_a_stream_is_kicked_from_another_thread() {
    // given
    let kicks = Kicks::new(4);
    let stream = counter_1();

    // when
    std::thread::scope(|scope| {
        scope.spawn(|| kicks.kick(&stream));
        block_on(kicks.wait());
    });

    // then
    assert_eq!(kicks.take(), [stream]);
}

#[test]
fn should_restart_the_backoff_from_its_base_when_the_stream_advanced() {
    // given: revision 0 failed once (drawn from 100 ms); then one claim delivers revision 0 and
    // fails at revision 1
    let h = harness(8);
    let consumer = Seen::default();
    block_on(h.worker.register::<Seen>()).unwrap_or_else(|e| panic!("{e}"));
    increment(&h, 1);
    increment(&h, 2);
    fail_at(&consumer, 0);
    let first = block_on(h.worker.drain(&consumer, None)).unwrap_or_else(|e| panic!("{e}"));
    h.store_clock.advance(Millis::new(1_000));
    fail_at(&consumer, 1);

    // when
    let second = block_on(h.worker.drain(&consumer, None)).unwrap_or_else(|e| panic!("{e}"));

    // then: the failure after the advance is the first of its run, drawn from 100 ms, not 200
    let rng = SeededRng::new(SEED);
    let expected: Vec<Millis> = [100, 100]
        .into_iter()
        .map(|d| Millis::new(1 + rng.below(d)))
        .collect();
    let backoffs: Vec<Millis> = first
        .failed
        .iter()
        .chain(&second.failed)
        .map(|(_, after)| *after)
        .collect();
    assert_eq!(backoffs, expected);
    assert_eq!(second.delivered, 1);
}

fn fail_at(consumer: &Seen, revision: u64) {
    *consumer
        .fail_at
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(StreamRevision::new(revision));
}

/// The memory journal, except that `read_stream` returns nothing while `hiding` is set: a store
/// whose marks name events its journal does not return.
#[derive(Debug, Clone)]
struct Hiding {
    inner: MemoryJournal,
    hiding: Arc<Mutex<bool>>,
}

impl Journal for Hiding {
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
        if *self.hiding.lock().unwrap_or_else(PoisonError::into_inner) {
            return Ok(Vec::new());
        }
        self.inner.read_stream(stream, after, through).await
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

#[test]
fn should_back_off_and_report_when_the_journal_ends_short_of_the_mark() {
    // given: a marked stream whose events the worker's journal does not return
    let h = harness(8);
    let journal = Hiding {
        inner: MemoryJournal::new(),
        hiding: Arc::new(Mutex::new(false)),
    };
    let ledger = MemoryLedger::new(&journal.inner, h.store_clock.clone());
    let worker = DeliveryWorker::new(
        journal.clone(),
        ledger,
        SeededRng::new(SEED),
        h.store_clock.clone(),
        DeliveryConfig::default(),
    );
    let consumer = Seen::default();
    block_on(worker.register::<Seen>()).unwrap_or_else(|e| panic!("{e}"));
    let store = AggregateStore::new::<Counters>(
        journal.inner.clone(),
        FakeClock::at(Millis::new(1_700_000_000_000)),
        SeededRng::new(1),
        Kicks::new(1),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let request = SaveRequest::new("1", command(1, CounterBody::Increment { by: 1, limit: 9 }));
    block_on(store.save::<Counter>(request)).unwrap_or_else(|e| panic!("{e}"));
    *journal
        .hiding
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = true;

    // when
    let short = block_on(worker.drain(&consumer, None)).unwrap_or_else(|e| panic!("{e}"));
    let at_once = block_on(worker.drain(&consumer, None)).unwrap_or_else(|e| panic!("{e}"));
    *journal
        .hiding
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = false;
    h.store_clock.advance(Millis::new(60_000));
    let repaired = block_on(worker.drain(&consumer, None)).unwrap_or_else(|e| panic!("{e}"));

    // then: one claim backed off and reported, not a drain spinning to `max_rounds`
    let streams: Vec<StreamId> = short.inconsistent.iter().map(|(s, _)| s.clone()).collect();
    assert_eq!(streams, [counter_1()]);
    assert!(short.inconsistent.iter().all(|(_, after)| after.get() >= 1));
    assert_eq!((short.delivered, short.lost.len()), (0, 0));
    assert_eq!(at_once, mayfly_control::delivery::DrainReport::default());
    assert_eq!(repaired.delivered, 1);
    assert_eq!(consumer.seen(), [(counter_1(), StreamRevision::new(0))]);
}
