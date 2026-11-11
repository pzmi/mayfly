//! The reconciler over the in-memory store (component level, CP-03): a pass reads the fleet
//! through the projections and issues its commands through `AggregateStore`; its timer and
//! kicks, its idempotent ids, its hysteresis, its reading of a data-plane refusal, and the
//! seeded simulation that checks the invariants it must keep.

use mayfly_control::delivery::Kicks;
use mayfly_control::reconciler::{Reconciler, ReconcilerConfig};
use mayfly_control::store::{Clock, stream_of};
use mayfly_control_domain::attempt::{GuestOs, Phase, RefusalReason, RetireReason};
use mayfly_control_domain::demand::WireMode;
use mayfly_control_domain::host::Host;
use mayfly_control_domain::reconcile::PassCommand;
use mayfly_domain::{AttemptId, HostId, Millis, PoolName};
use mayfly_testkit::block_on;
use mayfly_testkit::clock::FakeClock;
use mayfly_testkit::fleet::{EPOCH, FleetStore, PoolSpec, T0, World, report, simulate, store};
use mayfly_testkit::journal::MemoryJournal;

const MINUTE: u64 = 60_000;

fn lin() -> PoolName {
    PoolName::new("lin").unwrap_or_else(|e| panic!("{e}"))
}

fn h1() -> HostId {
    HostId::new("h1").unwrap_or_else(|e| panic!("{e}"))
}

fn spec(min_prepared: u32) -> PoolSpec {
    PoolSpec {
        name: lin(),
        os: GuestOs::Linux,
        memory_gib: 4,
        max_running: 4,
        min_prepared,
        breaker: 3,
    }
}

/// A fleet of one 32 GiB host and the `lin` pool.
fn fleet(store: &FleetStore, min_prepared: u32) -> World<'_> {
    let mut world = World::new(store);
    world.register_host("h1", report(32, 0, 8));
    world.configure_pool(spec(min_prepared));
    world
}

fn reserved(commands: &[PassCommand]) -> Vec<String> {
    commands
        .iter()
        .filter_map(|c| match c {
            PassCommand::ReserveSlot { host_id, pool, .. } => Some(format!("{host_id}/{pool}")),
            _ => None,
        })
        .collect()
}

fn retired(commands: &[PassCommand]) -> Vec<RetireReason> {
    commands
        .iter()
        .filter_map(|c| match c {
            PassCommand::Retire { reason, .. } => Some(*reason),
            _ => None,
        })
        .collect()
}

/// Starts every reservation and drives each attempt to `Prepared` (P2, P6 and the data
/// plane's answers).
fn prepare_all(world: &mut World<'_>) {
    for (host, attempt) in world.unstarted() {
        world.start(&host, attempt);
    }
    for (_, attempt) in world.leased_attempts() {
        for _ in 0..12 {
            if world.attempt(attempt).phase == Phase::Prepared {
                break;
            }
            world.progress(attempt, None);
        }
    }
}

#[test]
fn should_reserve_for_assigned_jobs_when_a_pass_reads_the_fleet() {
    // given
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let mut world = fleet(&store, 0);
    world.statistics(&lin(), 3, 0, false);
    let reconciler = Reconciler::new(
        &store,
        &journal,
        ReconcilerConfig::new(EPOCH),
        Kicks::new(8),
    );

    // when
    let report = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));

    // then: three reservations, and the scale set told the pool's carve-out
    assert_eq!(reserved(&report.accepted), vec!["h1/lin"; 3]);
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    assert_eq!(world.host(&h1()).leases.len(), 3);
    let capacity = world.scale_set(&lin()).capacity;
    assert_eq!((capacity.deliverable, capacity.reported_max), (4, 4));
    assert_eq!(capacity.mode, WireMode::Open);
}

#[test]
fn should_reserve_nothing_twice_when_a_pass_is_repeated_over_the_same_revisions() {
    // given: two stores with the same history
    let reserve_once = || {
        let journal = MemoryJournal::new();
        let clock = FakeClock::at(T0);
        let store = store(&journal, &clock);
        let mut world = fleet(&store, 0);
        world.statistics(&lin(), 2, 0, false);
        let reconciler = Reconciler::new(
            &store,
            &journal,
            ReconcilerConfig::new(EPOCH),
            Kicks::new(8),
        );
        let first = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
        let again = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
        let leases: Vec<AttemptId> = world.host(&h1()).leases.keys().copied().collect();
        (first, again, leases)
    };

    // when
    let (first, again, leases) = reserve_once();
    let (_, _, other) = reserve_once();

    // then: the second pass reserves nothing more, and the same revisions name the same
    // attempts (P5's key: a racing pass is answered by the receipts)
    assert_eq!(reserved(&first.accepted).len(), 2);
    assert!(reserved(&again.pass.commands).is_empty());
    assert_eq!(leases.len(), 2);
    assert_eq!(leases, other);
}

#[test]
fn should_pass_at_most_once_an_interval_when_no_relevant_stream_is_kicked() {
    // given
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let _world = fleet(&store, 0);
    let kicks = Kicks::new(8);
    let reconciler = Reconciler::new(
        &store,
        &journal,
        ReconcilerConfig::new(EPOCH),
        kicks.clone(),
    );
    let host = stream_of::<Host>("h1").unwrap_or_else(|e| panic!("{e}"));
    let attempt =
        stream_of::<mayfly_control_domain::attempt::Attempt>(&AttemptId::from_u128(10).to_string())
            .unwrap_or_else(|e| panic!("{e}"));

    // when
    let first = block_on(reconciler.tick()).unwrap_or_else(|e| panic!("{e}"));
    let idle = block_on(reconciler.tick()).unwrap_or_else(|e| panic!("{e}"));
    kicks.kick(&attempt);
    let not_relevant = block_on(reconciler.tick()).unwrap_or_else(|e| panic!("{e}"));
    kicks.kick(&host);
    let kicked = block_on(reconciler.tick()).unwrap_or_else(|e| panic!("{e}"));
    clock.advance(Millis::new(MINUTE - 1));
    let early = block_on(reconciler.tick()).unwrap_or_else(|e| panic!("{e}"));
    clock.advance(Millis::new(1));
    let due = block_on(reconciler.tick()).unwrap_or_else(|e| panic!("{e}"));

    // then
    assert!(first.is_some());
    assert!(idle.is_none());
    assert!(not_relevant.is_none());
    assert!(kicked.is_some());
    assert!(early.is_none());
    assert!(due.is_some());
    assert_eq!(
        reconciler.next_due(),
        Millis::new(store.clock().now().get() + MINUTE)
    );
}

#[test]
fn should_retire_a_prepared_surplus_only_after_the_hysteresis_when_demand_drops() {
    // given: one job, its VM prepared, then the job is gone
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let mut world = fleet(&store, 0);
    world.statistics(&lin(), 1, 0, false);
    let reconciler = Reconciler::new(
        &store,
        &journal,
        ReconcilerConfig::new(EPOCH),
        Kicks::new(8),
    );
    block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    prepare_all(&mut world);
    world.statistics(&lin(), 0, 0, false);

    // when: passes when the surplus is seen, 4 minutes later and 5 minutes later
    let seen = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    clock.advance(Millis::new(4 * MINUTE));
    let early = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    clock.advance(Millis::new(MINUTE));
    let late = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));

    // then
    assert!(retired(&seen.accepted).is_empty());
    assert!(retired(&early.accepted).is_empty());
    assert_eq!(retired(&late.accepted), vec![RetireReason::Surplus]);
    let (_, attempt) = world.leased_attempts()[0].clone();
    assert_eq!(
        world.attempt(attempt).retire_reason,
        Some(RetireReason::Surplus)
    );
}

#[test]
fn should_admit_nothing_on_the_host_until_it_reports_again_when_the_data_plane_refuses_a_clone() {
    // given: one job reserved; the data plane refuses its clone insufficient_resources (the
    // attempt backs off) while two more jobs arrive
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let mut world = fleet(&store, 0);
    world.statistics(&lin(), 1, 0, false);
    let reconciler = Reconciler::new(
        &store,
        &journal,
        ReconcilerConfig::new(EPOCH),
        Kicks::new(8),
    );
    block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    for (host, attempt) in world.unstarted() {
        world.start(&host, attempt);
    }
    let (_, attempt) = world.leased_attempts()[0].clone();
    world.progress(attempt, None);
    world.progress(attempt, Some(RefusalReason::InsufficientResources));
    world.statistics(&lin(), 3, 0, false);

    // when: a pass reads the refusal, another runs on the same report, then the host reports
    let held = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    let still = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    let told = world.scale_set(&lin()).capacity;
    world.report(&h1(), report(32, 0, 8));
    let reread = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));

    // then: a definite no: nothing reserved and only the live runner told, until a newer
    // report is read
    assert!(matches!(
        world.attempt(attempt).phase,
        Phase::Backoff { .. }
    ));
    assert!(reserved(&held.pass.commands).is_empty());
    assert!(reserved(&still.pass.commands).is_empty());
    assert_eq!((told.deliverable, told.reported_max), (1, 1));
    assert_eq!(reserved(&reread.accepted).len(), 2);
}

#[test]
fn should_never_violate_the_invariants_when_a_seeded_simulation_runs_1000_steps() {
    // given
    let seeds = [1_u64, 2, 3];

    // when
    let runs: Vec<_> = seeds
        .iter()
        .map(|seed| (*seed, simulate(*seed, 1000)))
        .collect();

    // then
    // nothing violated, and every run exercised every kind of command
    for (seed, sim) in runs {
        assert!(
            sim.violations.is_empty(),
            "seed {seed}: {:#?} {sim:?}",
            sim.violations.iter().take(5).collect::<Vec<_>>()
        );
        assert!(sim.passes > 100, "seed {seed}: {sim:?}");
        assert!(sim.reserved > 0, "seed {seed}: {sim:?}");
        assert!(sim.retired > 0, "seed {seed}: {sim:?}");
        assert!(sim.released > 0, "seed {seed}: {sim:?}");
        assert!(sim.capacities > 0, "seed {seed}: {sim:?}");
        assert!(sim.acquired > 0, "seed {seed}: {sim:?}");
    }
}
