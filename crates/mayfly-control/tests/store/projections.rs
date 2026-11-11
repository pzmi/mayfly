//! Projections over the in-memory store (component level): the control plane's specs subscribe
//! to every kind they react to, the offline rebuild records the revision it folded, the
//! queries count across streams over stream-local rows, and "rebuild equals live" holds over
//! random pool, host and attempt histories, the last two the views whose rows are deleted
//! (`slots`, `pending_effects`; ES-I11, `doc/event-store.md` §3.6).

use mayfly_control::journal::{
    AggregateName, AppendError, Appended, CommandReceipt, EventLocation, ExpectedRevision,
    InboxKey, Journal, NewEvent, Payload, ProjectionName, RecordedEvent, StorageError, StoredKind,
    StreamHead, StreamId, StreamRevision, SupportedEpochs, UnitOfWork,
};
use mayfly_control::projection::views::{Attempts, PendingEffects, Pools, RecentFailures, Slots};
use mayfly_control::projection::{
    BuildId, ProjectionSpec, ProjectionStore, SpecEvent, control, prepare,
};
use mayfly_control::queries;
use mayfly_control::store::Outcome;
use mayfly_control::store::{AggregateStore, NoNotifier, SaveRequest, stream_of};
use mayfly_control_domain::attempt::{AttemptCommand, GuestOs};
use mayfly_control_domain::capacity::ReportedCapacity;
use mayfly_control_domain::host::{ApplyPoolTerms, Host, HostCommand, ReserveSlot};
use mayfly_control_domain::pool::TrustTier;
use mayfly_control_domain::pool::{Pool, PoolEvent};
use mayfly_control_domain::{Actor, Command, ControlAggregates, DeterminismCases, Expect, replay};
use mayfly_domain::{
    AttemptId, CommandId, Deadline, DirectiveId, Epoch, EventId, Millis, PoolName, Resources,
};
use mayfly_testkit::clock::{FakeClock, SeededRng};
use mayfly_testkit::contracts::projections::{
    SuiteOptions, attempt_rebuild_equals_live, check_subscriptions, host_rebuild_equals_live,
    pool_history, rebuild_equals_live,
};
use mayfly_testkit::journal::{MemoryJournal, MemoryProjectionFactory};
use mayfly_testkit::{attempts, block_on};
use proptest::prelude::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Every case history of `A`, and each accepted case's history with its decided events.
fn histories<A: DeterminismCases>() -> Vec<Vec<A::Event>> {
    A::determinism_cases()
        .into_iter()
        .flat_map(|case| {
            let mut decided = case.history.clone();
            if case.expect == Expect::Accepted
                && let Ok(events) = A::decide(&replay::<A>(case.history.iter()), case.command)
            {
                decided.extend(events);
            }
            [case.history, decided]
        })
        .collect()
}

fn subscriptions<S: ProjectionSpec>() -> Result<(), (usize, usize)>
where
    S::Source: DeterminismCases,
{
    let all: Vec<Vec<SpecEvent<S>>> = histories::<S::Source>();
    check_subscriptions::<S>(&all)
}

#[test]
fn should_subscribe_to_every_kind_a_projection_reacts_to_when_case_histories_are_folded() {
    // given
    let checks = [
        ("attempts", subscriptions::<Attempts>()),
        ("pending_effects", subscriptions::<PendingEffects>()),
        ("slots", subscriptions::<Slots>()),
        ("pools", subscriptions::<Pools>()),
        ("recent_failures", subscriptions::<RecentFailures>()),
    ];

    // when
    let missed: Vec<_> = checks.iter().filter(|(_, r)| r.is_err()).collect();

    // then
    assert!(missed.is_empty(), "{missed:?}");
}

/// A journal that commits a copy of the last event to the stream it is asked to read, right before
/// the read: a head paged by the rebuild is stale by the time the stream is read.
#[derive(Debug, Clone)]
struct MovingJournal(MemoryJournal, Arc<AtomicU64>);

impl Journal for MovingJournal {
    async fn append(&self, unit: UnitOfWork) -> Result<Appended, AppendError> {
        self.0.append(unit).await
    }

    async fn head(&self, stream: &StreamId) -> Result<Option<StreamHead>, StorageError> {
        self.0.head(stream).await
    }

    async fn read_stream(
        &self,
        stream: &StreamId,
        after: Option<StreamRevision>,
        through: Option<StreamRevision>,
    ) -> Result<Vec<RecordedEvent>, StorageError> {
        let history = self.0.read_stream(stream, None, None).await?;
        let head = history.last().map(|e| e.revision);
        let Some(last) = history.last() else {
            return Ok(history);
        };
        let event = NewEvent {
            event_id: EventId::from_u128(u128::from(9_000 + self.1.fetch_add(1, Ordering::SeqCst))),
            kind: last.kind.clone(),
            required_epoch: 1,
            payload: last.payload.clone(),
        };
        let pool = AggregateName::new("pool").unwrap_or_else(|e| panic!("{e}"));
        let unit = UnitOfWork::new(Millis::new(5), SupportedEpochs::new().with(pool.clone(), 1))
            .with_append(
                stream.clone(),
                pool,
                ExpectedRevision::of(head),
                vec![event],
            );
        self.0.append(unit).await.unwrap_or_else(|e| panic!("{e}"));
        self.0.read_stream(stream, after, through).await
    }

    async fn lookup_event(&self, event_id: EventId) -> Result<Option<EventLocation>, StorageError> {
        self.0.lookup_event(event_id).await
    }

    async fn receipt(&self, command_id: CommandId) -> Result<Option<CommandReceipt>, StorageError> {
        self.0.receipt(command_id).await
    }

    async fn inbox_contains(&self, key: &InboxKey) -> Result<bool, StorageError> {
        self.0.inbox_contains(key).await
    }

    async fn streams(
        &self,
        after: Option<&StreamId>,
        limit: u32,
    ) -> Result<Vec<StreamHead>, StorageError> {
        self.0.streams(after, limit).await
    }

    async fn stored_kinds(&self) -> Result<Vec<StoredKind>, StorageError> {
        self.0.stored_kinds().await
    }

    async fn projection(
        &self,
        projection: &ProjectionName,
        key: &str,
    ) -> Result<Option<Payload>, StorageError> {
        self.0.projection(projection, key).await
    }
}

fn store(
    memory: &MemoryJournal,
) -> AggregateStore<MemoryJournal, FakeClock, SeededRng, NoNotifier> {
    AggregateStore::new::<ControlAggregates>(
        memory.clone(),
        FakeClock::at(Millis::new(1_700_000_000_000)),
        SeededRng::new(3),
        NoNotifier,
    )
    .unwrap_or_else(|e| panic!("{e}"))
    .with_projections(control().unwrap_or_else(|e| panic!("{e}")))
}

fn build() -> BuildId {
    BuildId::new("test").unwrap_or_else(|e| panic!("{e}"))
}

fn run_pool_history(memory: &MemoryJournal, seed: u64, len: usize) {
    let store = store(memory);
    for (n, step) in pool_history(seed, len).into_iter().enumerate() {
        let command = Command::new(
            CommandId::from_u128(100 + u128::try_from(n).unwrap_or(0)),
            Millis::new(1),
            Epoch::new(1),
            step.actor,
            step.body,
        );
        block_on(store.execute::<Pool>(SaveRequest::new(step.pool, command)))
            .unwrap_or_else(|e| panic!("{e}"));
    }
}

#[test]
fn should_record_the_revision_folded_when_a_stream_moves_after_its_head_was_paged() {
    // given
    let memory = MemoryJournal::new();
    block_on(prepare(
        &memory,
        &memory,
        &control().unwrap_or_else(|e| panic!("{e}")),
        &build(),
    ))
    .unwrap_or_else(|e| panic!("{e}"));
    run_pool_history(&memory, 1, 0);
    let a = stream_of::<Pool>("a").unwrap_or_else(|e| panic!("{e}"));
    let paged = block_on(memory.head(&a)).unwrap_or_else(|e| panic!("{e}"));
    let spec = ProjectionName::new(Pools::NAME).unwrap_or_else(|e| panic!("{e}"));

    // when
    let rebuilt = block_on(prepare(
        &MovingJournal(memory.clone(), Arc::default()),
        &memory,
        &control().unwrap_or_else(|e| panic!("{e}")),
        &BuildId::new("next").unwrap_or_else(|e| panic!("{e}")),
    ));

    // then
    assert!(rebuilt.is_ok(), "{rebuilt:?}");
    let head = block_on(memory.head(&a)).unwrap_or_else(|e| panic!("{e}"));
    let rows = block_on(memory.stream_rows(&spec, &a)).unwrap_or_else(|e| panic!("{e}"));
    assert_ne!(head, paged);
    assert!(rows.applied.is_some());
    assert!(rows.applied >= paged.map(|h| h.revision));
    let events = block_on(memory.read_stream(&a, None, None)).unwrap_or_else(|e| panic!("{e}"));
    assert!(events.iter().any(|e| Some(e.revision) == rows.applied));
}

#[test]
fn should_count_failures_per_pool_when_queried_across_streams() {
    // given
    let memory = MemoryJournal::new();
    run_pool_history(&memory, 5, 60);

    // when
    let counted = block_on(queries::failures_by_pool(&memory));

    // then
    let counted = counted.unwrap_or_else(|e| panic!("{e}"));
    let state = |pool: &str| {
        let events = block_on(memory.read_stream(
            &stream_of::<Pool>(pool).unwrap_or_else(|e| panic!("{e}")),
            None,
            None,
        ))
        .unwrap_or_else(|e| panic!("{e}"));
        let decoded: Vec<PoolEvent> = events
            .iter()
            .map(|e| {
                mayfly_control::store::decode_event::<PoolEvent>(e)
                    .unwrap_or_else(|e| panic!("{e:?}"))
            })
            .collect();
        replay::<Pool>(decoded.iter())
    };
    for pool in ["a", "b", "c"] {
        let want: usize = state(pool)
            .breakers
            .values()
            .map(|b| b.failures.len())
            .sum();
        let got = counted.get(&format!("pool-{pool}")).map_or(0, Vec::len);
        assert_eq!(got, want, "pool {pool}");
    }
}

const GIB: u64 = 1 << 30;

fn reserve(attempt: u128) -> HostCommand {
    HostCommand::ReserveSlot(ReserveSlot {
        attempt_id: AttemptId::from_u128(attempt),
        pool: PoolName::new("pool0").unwrap_or_else(|e| panic!("{e}")),
        os: GuestOs::Linux,
        resources: Resources::new(2 * GIB, 20 * GIB),
        terms_revision: 1,
    })
}

/// A registered host with a capacity report and open terms for `pool0`.
fn host_setup() -> Vec<HostCommand> {
    vec![
        HostCommand::RegisterHost {
            reserves: Resources::new(4 * GIB, 20 * GIB),
        },
        HostCommand::RecordCapacityReport {
            report_id: 1,
            capacity: ReportedCapacity {
                total: Resources::new(32 * GIB, 200 * GIB),
                foreign: Resources::new(0, 0),
                base: Resources::new(0, 40 * GIB),
                vm_slots: 4,
                foreign_vms: 0,
                macos_guests: 2,
                foreign_macos_guests: 0,
            },
        },
        HostCommand::ApplyPoolTerms(ApplyPoolTerms {
            pool: PoolName::new("pool0").unwrap_or_else(|e| panic!("{e}")),
            revision: 1,
            open: true,
            max_running: 4,
            tier: TrustTier::Trusted,
        }),
    ]
}

#[test]
fn should_list_leases_per_host_when_queried_across_streams() {
    // given
    let memory = MemoryJournal::new();
    let store = store(&memory);
    let setup = host_setup();
    for (host, n) in [("h1", 0_u128), ("h2", 10)] {
        for (k, body) in setup.iter().cloned().enumerate() {
            let command = Command::new(
                CommandId::from_u128(n * 100 + u128::try_from(k).unwrap_or(0)),
                Millis::new(1),
                Epoch::new(1),
                Actor::Startup,
                body,
            );
            block_on(store.execute::<Host>(SaveRequest::new(host, command)))
                .unwrap_or_else(|e| panic!("{e}"));
        }
    }
    for (host, attempt, id) in [("h1", 1, 50), ("h1", 2, 51), ("h2", 3, 1_050)] {
        let command = Command::new(
            CommandId::from_u128(id),
            Millis::new(1),
            Epoch::new(1),
            Actor::Automation { policy: "test" },
            reserve(attempt),
        );
        block_on(store.execute::<Host>(SaveRequest::new(host, command)))
            .unwrap_or_else(|e| panic!("{e}"));
    }

    // when
    let leases = block_on(queries::leases_by_host(&memory)).unwrap_or_else(|e| panic!("{e}"));

    // then
    let counts: Vec<(String, usize)> = leases.iter().map(|(h, l)| (h.clone(), l.len())).collect();
    assert_eq!(
        counts,
        [("host-h1".to_owned(), 2), ("host-h2".to_owned(), 1)]
    );
}

#[test]
fn should_list_pending_effects_and_count_the_unknown_when_queried() {
    // given
    let memory = MemoryJournal::new();
    let store = store(&memory);
    for n in 1..=3_u128 {
        block_on(attempts::save(
            &store,
            n,
            n * 10,
            Epoch::new(1),
            attempts::start(n),
        ));
        block_on(attempts::save(
            &store,
            n,
            n * 10 + 1,
            Epoch::new(1),
            AttemptCommand::IssueNext {
                directive_id: DirectiveId::from_u128(n),
                deadline: Deadline::at(Millis::new(1)),
            },
        ));
    }
    block_on(attempts::save(
        &store,
        2,
        98,
        Epoch::new(1),
        AttemptCommand::RecordDirectiveSent {
            directive_id: DirectiveId::from_u128(2),
            deadline: Deadline::at(Millis::new(1)),
        },
    ));
    let timed_out = block_on(attempts::save(
        &store,
        2,
        99,
        Epoch::new(1),
        AttemptCommand::RecordDirectiveTimeout {
            directive_id: DirectiveId::from_u128(2),
        },
    ));
    assert!(
        matches!(timed_out, Outcome::Accepted { .. }),
        "{timed_out:?}"
    );

    // when
    let pending = block_on(queries::pending_effects(&memory)).unwrap_or_else(|e| panic!("{e}"));

    // then
    assert_eq!((pending.0.len(), pending.1), (3, 1));
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    #[test]
    fn should_equal_the_live_rows_when_rebuilt_over_random_pool_histories(seed in any::<u64>(), len in 0_usize..60) {
        // given
        let options = SuiteOptions::default();

        // when
        let checked = block_on(rebuild_equals_live(&MemoryProjectionFactory, &options, seed, len));

        // then
        prop_assert!(checked.is_ok(), "{checked:?}");
    }

    #[test]
    fn should_equal_the_live_rows_when_rebuilt_over_random_host_histories(seed in any::<u64>(), len in 0_usize..60) {
        // given
        let factory = MemoryProjectionFactory;

        // when
        let checked = block_on(host_rebuild_equals_live(&factory, seed, len));

        // then
        prop_assert!(checked.is_ok(), "{checked:?}");
    }

    #[test]
    fn should_equal_the_live_rows_when_rebuilt_over_random_attempt_histories(seed in any::<u64>(), len in 0_usize..60) {
        // given
        let factory = MemoryProjectionFactory;

        // when
        let checked = block_on(attempt_rebuild_equals_live(&factory, seed, len));

        // then
        prop_assert!(checked.is_ok(), "{checked:?}");
    }
}
