//! The reconciler over the in-memory store (component level, CP-03): a pass reads the fleet
//! through the projections and issues its commands through `AggregateStore`; its timer and
//! kicks, its idempotent ids, its hysteresis, its reading of a data-plane refusal, P2's starts
//! (CP-10), and the seeded simulation that checks the invariants it must keep.

use mayfly_control::delivery::Kicks;
use mayfly_control::journal::{Appended, AppendedStream, Journal as _, StoredKind, StreamRevision};
use mayfly_control::projection::views::Attempts;
use mayfly_control::queries;
use mayfly_control::reconciler::{Reconciler, ReconcilerConfig, kick_prepared};
use mayfly_control::store::{Clock, Committed, stream_of};
use mayfly_control_domain::Actor;
use mayfly_control_domain::attempt::{Attempt, GuestOs, Phase, RefusalReason, RetireReason};
use mayfly_control_domain::demand::WireMode;
use mayfly_control_domain::host::{Host, ReleaseEvidence};
use mayfly_control_domain::pool::{Pool, PoolCommand};
use mayfly_control_domain::reconcile::{Pass, PassCommand, PassInputs, plan_pass};
use mayfly_domain::{AttemptId, CommandId, EventId, HostId, ImageDigest, Millis, PoolName, Sample};
use mayfly_testkit::block_on;
use mayfly_testkit::clock::FakeClock;
use mayfly_testkit::fleet::{
    EPOCH, FleetStore, LaggedReads, PoolSpec, SimOptions, T0, World, report, simulate,
    simulate_with, store,
};
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

/// The image the pool is repinned to, materialised on no host until placed.
fn other_image() -> ImageDigest {
    ImageDigest::new(&format!("sha256:{}", "b".repeat(64))).unwrap_or_else(|e| panic!("{e}"))
}

/// Pins the pool to [`other_image`] (its base on no host) and applies its terms.
fn repin_unplaced(world: &mut World<'_>) {
    let _ = world.command::<Pool>(
        lin().as_str(),
        Actor::Automation { policy: "image" },
        PoolCommand::PinImage {
            digest: other_image(),
            verified_ref: Some(EventId::from_u128(8)),
        },
    );
    world.apply_terms(&lin());
}

/// Pauses the pool, so a pass starts none of its reservations (CP-10), and applies its terms.
fn pause(world: &mut World<'_>) {
    let _ = world.command::<Pool>(
        lin().as_str(),
        Actor::Operator { uid: 501 },
        PoolCommand::PausePool,
    );
    world.apply_terms(&lin());
}

fn started(commands: &[PassCommand]) -> Vec<AttemptId> {
    commands
        .iter()
        .filter_map(|c| match c {
            PassCommand::Start { attempt_id, .. } => Some(*attempt_id),
            _ => None,
        })
        .collect()
}

fn released(commands: &[PassCommand]) -> Vec<AttemptId> {
    commands
        .iter()
        .filter_map(|c| match c {
            PassCommand::ReleaseSlot { attempt_id, .. } => Some(*attempt_id),
            _ => None,
        })
        .collect()
}

/// How many `AttemptStarted` the attempt's stream holds.
fn starts_recorded(journal: &MemoryJournal, attempt: AttemptId) -> usize {
    let stream = stream_of::<Attempt>(&attempt.to_string()).unwrap_or_else(|e| panic!("{e}"));
    block_on(journal.read_stream(&stream, None, None))
        .unwrap_or_default()
        .iter()
        .filter(|e| e.kind.event_type == "attempt_started")
        .count()
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
fn should_pass_at_once_when_an_attempt_becomes_prepared_and_not_on_its_other_events() {
    // given: a reconciler woken by its prepared kicks, fed as the composition root feeds them
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let _world = fleet(&store, 0);
    let kicks = Kicks::new(8);
    let prepared = Kicks::new(8);
    let reconciler = Reconciler::new(
        &store,
        &journal,
        ReconcilerConfig::new(EPOCH),
        kicks.clone(),
    )
    .with_prepared(prepared.clone());
    let attempt = stream_of::<Attempt>(&AttemptId::from_u128(10).to_string())
        .unwrap_or_else(|e| panic!("{e}"));
    let commit = |kind: &str| Committed {
        command_id: CommandId::from_u128(1),
        appended: Appended {
            streams: vec![AppendedStream {
                stream: attempt.clone(),
                first: StreamRevision::FIRST,
                last: StreamRevision::FIRST,
                event_ids: vec![EventId::from_u128(1)],
            }],
        },
        kinds: vec![StoredKind {
            event_type: kind.to_owned(),
            schema_version: 1,
        }],
    };
    let first = block_on(reconciler.tick()).unwrap_or_else(|e| panic!("{e}"));

    // when: an attempt records a directive, then becomes Prepared
    let issued = commit("directive_issued");
    kicks.kick(&attempt);
    kick_prepared(&prepared, &issued);
    let on_directive = block_on(reconciler.tick()).unwrap_or_else(|e| panic!("{e}"));
    let became_prepared = commit("attempt_prepared");
    kicks.kick(&attempt);
    kick_prepared(&prepared, &became_prepared);
    let on_prepared = block_on(reconciler.tick()).unwrap_or_else(|e| panic!("{e}"));
    let after = block_on(reconciler.tick()).unwrap_or_else(|e| panic!("{e}"));

    // then: only the Prepared event makes a pass due before the interval, once
    assert!(first.is_some());
    assert!(on_directive.is_none());
    assert!(on_prepared.is_some());
    assert!(after.is_none());
}

#[test]
fn should_start_a_reserved_lease_when_the_next_pass_runs() {
    // given: one job, reserved by a pass
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
    let reserving = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    let (_, attempt) = world.unstarted()[0].clone();

    // when
    let next = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    let again = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));

    // then: started from the host's lease on the pool's pin with the configured retry budget,
    // once; the pass that reserved it did not start it (it read no lease), the one after it
    // plans nothing more
    assert!(started(&reserving.pass.commands).is_empty());
    assert_eq!(started(&next.accepted), vec![attempt]);
    assert!(
        next.refused.is_empty() && next.failed.is_empty(),
        "{next:?}"
    );
    let identity = world
        .attempt(attempt)
        .identity
        .unwrap_or_else(|| panic!("not started"));
    assert_eq!(
        (
            identity.host_id,
            identity.image_digest,
            identity.retry_budget
        ),
        (h1(), ImageDigest::sample(), 2)
    );
    assert!(started(&again.pass.commands).is_empty());
    assert!(world.unstarted().is_empty());
    assert_eq!(starts_recorded(&journal, attempt), 1);
}

#[test]
fn should_start_once_when_a_stale_read_plans_the_start_again() {
    // given: a reservation the reconciler reads unstarted through projections it refreshes only
    // when told, started by a pass
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let mut world = fleet(&store, 0);
    world.statistics(&lin(), 1, 0, false);
    let reads = LaggedReads::new(&journal, 1);
    let reconciler = Reconciler::new(&store, &reads, ReconcilerConfig::new(EPOCH), Kicks::new(8));
    block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    reads.capture();
    let (_, attempt) = world.unstarted()[0].clone();
    let first = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));

    // when: a pass over the same stale read plans the start again
    let again = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));

    // then: the receipt answers it; the attempt started once
    assert_eq!(started(&first.accepted), vec![attempt]);
    assert_eq!(started(&again.pass.commands), vec![attempt]);
    assert_eq!(started(&again.accepted), vec![attempt]);
    assert_eq!(starts_recorded(&journal, attempt), 1);
}

#[test]
fn should_start_a_lease_once_its_placement_materialises_when_its_pin_was_not_placed() {
    // given: a reservation read unstarted on the pin then placed, and the pool repinned to an
    // image whose base is on no host before the pass that would start it
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let mut world = fleet(&store, 0);
    world.statistics(&lin(), 1, 0, false);
    let reads = LaggedReads::new(&journal, 1);
    let reconciler = Reconciler::new(&store, &reads, ReconcilerConfig::new(EPOCH), Kicks::new(8));
    block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    reads.capture();
    let (_, attempt) = world.unstarted()[0].clone();
    repin_unplaced(&mut world);

    // when: a pass plans the start from the stale read, a pass reads the new pin unplaced, and
    // a pass runs after the new pin's base is materialised
    let stale = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    reads.capture();
    let unplaced = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    world.place(&other_image());
    reads.capture();
    let placed = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));

    // then: P2 started nothing on the unplaced pin and the lease was neither released nor
    // retried; the pass after the placement started it on the new pin
    let start = PassCommand::Start {
        host_id: h1(),
        attempt_id: attempt,
    };
    assert!(
        stale.failed.contains(&(start.clone(), "not_placed")),
        "{stale:?}"
    );
    assert!(started(&unplaced.pass.commands).is_empty());
    for pass in [&stale, &unplaced] {
        assert!(released(&pass.pass.commands).is_empty(), "{pass:?}");
    }
    assert!(placed.accepted.contains(&start), "{placed:?}");
    assert_eq!(
        world.attempt(attempt).identity.map(|i| i.image_digest),
        Some(other_image())
    );
    assert!(world.host(&h1()).leases.contains_key(&attempt));
}

#[test]
fn should_release_a_lease_after_the_start_grace_when_its_pin_is_never_placed() {
    // given: a reservation, then the pool repinned to an image whose base is on no host
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
    let (_, attempt) = world.unstarted()[0].clone();
    repin_unplaced(&mut world);

    // when: a pass within the start grace, and one past it
    let within = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    clock.advance(Millis::new(3 * MINUTE));
    let past = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));

    // then: never started; held within the grace, released `attempt_never_started` after it
    assert!(started(&within.pass.commands).is_empty());
    assert!(released(&within.pass.commands).is_empty());
    assert!(past.accepted.contains(&PassCommand::ReleaseSlot {
        host_id: h1(),
        attempt_id: attempt,
        evidence: ReleaseEvidence::AttemptNeverStarted,
    }));
    assert!(started(&past.pass.commands).is_empty());
    assert!(!world.host(&h1()).leases.contains_key(&attempt));
    assert_eq!(starts_recorded(&journal, attempt), 0);
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
fn should_keep_the_slot_when_the_attempt_starts_between_the_read_and_the_release() {
    // given: a reservation the reconciler reads unstarted, through projections it refreshes
    // only when told (a read model behind the journal)
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let mut world = fleet(&store, 0);
    world.statistics(&lin(), 1, 0, false);
    let reads = LaggedReads::new(&journal, 1);
    let reconciler = Reconciler::new(&store, &reads, ReconcilerConfig::new(EPOCH), Kicks::new(8));
    block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    // the pool paused, so the passes do not start the reservation themselves (CP-10)
    pause(&mut world);
    reads.capture();
    block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    clock.advance(Millis::new(3 * MINUTE));

    // when: the attempt starts after that read, and a pass past the start grace plans its
    // release from the stale read
    let (host, attempt) = world.unstarted()[0].clone();
    assert!(world.start(&host, attempt));
    let pass = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));

    // then: the release was planned, refused at the save, and the lease is still held
    let release = PassCommand::ReleaseSlot {
        host_id: h1(),
        attempt_id: attempt,
        evidence: ReleaseEvidence::AttemptNeverStarted,
    };
    assert!(pass.pass.commands.contains(&release), "{:?}", pass.pass);
    assert!(
        pass.failed.contains(&(release.clone(), "conflict")),
        "{pass:?}"
    );
    assert!(!pass.accepted.contains(&release));
    assert!(world.host(&h1()).leases.contains_key(&attempt));
}

#[test]
fn should_release_an_unstarted_lease_and_refuse_its_late_start_when_the_release_commits_first() {
    // given: a reservation never started past the start grace
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
    let (host, attempt) = world.unstarted()[0].clone();
    world.statistics(&lin(), 0, 0, false);
    // the pool paused, so the passes do not start the reservation themselves (CP-10)
    pause(&mut world);
    block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    clock.advance(Millis::new(3 * MINUTE));

    // when: the release commits, then P2's start (which read the host stream before it)
    let pass = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    let started = world.start(&host, attempt);

    // then
    assert!(pass.accepted.contains(&PassCommand::ReleaseSlot {
        host_id: h1(),
        attempt_id: attempt,
        evidence: ReleaseEvidence::AttemptNeverStarted,
    }));
    assert!(!started);
    assert!(world.attempt(attempt).identity.is_none());
}

#[test]
fn should_hold_the_host_until_a_newer_report_when_the_attempt_retries_before_the_pass() {
    // given: one job reserved and started; its clone is refused insufficient_resources and the
    // saga retries before any pass runs, while two more jobs arrive
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
    world.progress(attempt, None);
    world.statistics(&lin(), 3, 0, false);
    let row = block_on(queries::all_rows::<Attempts, _>(&journal))
        .unwrap_or_else(|e| panic!("{e}"))
        .into_iter()
        .map(|(_, row)| row)
        .find(|row| row.attempt_id == attempt)
        .unwrap_or_else(|| panic!("no attempts row"));

    // when: a pass, another on the same report, then one after a newer report
    let held = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    let still = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    world.report(&h1(), report(32, 0, 8));
    let reread = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));

    // then: the retry cleared `refused`, yet the host is held until the newer report
    assert_eq!(row.refused, None);
    assert_eq!(row.phase, "cloning");
    assert!(reserved(&held.pass.commands).is_empty());
    assert!(reserved(&still.pass.commands).is_empty());
    assert_eq!(reserved(&reread.accepted).len(), 2);
}

/// A pass that ignores admission: every host open, no hold, no refusal read.
fn ignoring_admission(inputs: &PassInputs) -> Pass {
    let mut open = inputs.clone();
    for host in &mut open.hosts {
        host.admission_open = true;
        for lease in &mut host.leases {
            lease.refusal = None;
        }
    }
    open.memory.held.clear();
    plan_pass(&open)
}

#[test]
fn should_find_violations_when_a_plan_ignores_admission() {
    // given: the negative control: the simulation's checks against a broken pass
    let options = SimOptions {
        plan: ignoring_admission,
        ..SimOptions::default()
    };

    // when
    let sim = simulate_with(1, 1000, options);

    // then: the checks are not vacuous
    assert!(!sim.violations.is_empty(), "{sim:?}");
}

#[test]
fn should_never_violate_the_invariants_when_the_reconciler_reads_projections_behind_the_journal() {
    // given: projections read 3 steps behind the journal
    let options = SimOptions {
        lag: 3,
        ..SimOptions::default()
    };

    // when
    let runs: Vec<_> = [4_u64, 5]
        .iter()
        .map(|seed| (*seed, simulate_with(*seed, 1000, options)))
        .collect();

    // then
    for (seed, sim) in runs {
        assert!(
            sim.violations.is_empty(),
            "seed {seed}: {:#?} {sim:?}",
            sim.violations.iter().take(5).collect::<Vec<_>>()
        );
        assert!(sim.reserved > 0 && sim.released > 0, "seed {seed}: {sim:?}");
    }
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
        assert!(sim.refusals > 0, "seed {seed}: {sim:?}");
    }
}
