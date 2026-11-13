//! P9's grant from the reconciler over the in-memory store (component level, CP-09): a prepared
//! attempt of a pool with a job waiting is granted by its host and its activation begun, once,
//! only while `runner_may_start` holds and never beyond the shortfall; a pass that read the
//! fleet before another pass's grant committed grants nothing; a grant whose activation was lost
//! is activated by the next pass with the grant's own event; and the registration manager
//! (CP-05) then mints for the activated attempt.

use std::collections::{BTreeMap, BTreeSet};

use mayfly_control::delivery::Kicks;
use mayfly_control::github::{ScaleSetApi as _, ScaleSetId, ScaleSetSpec};
use mayfly_control::projection::views::AttemptRow;
use mayfly_control::queries;
use mayfly_control::reconciler::{
    PassReport, Reconciler, ReconcilerConfig, begin_command_id, grant_command_id,
};
use mayfly_control::registration::{
    JitBroker, RegistrationConfig, RegistrationManager, ScaleSetDirectory, Step, handle_of,
};
use mayfly_control::sessions::{HostSession, HostSessions};
use mayfly_control::store::{Outcome, SaveRequest};
use mayfly_control_domain::attempt::{
    Attempt, AttemptCommand, GuestOs, Jit, Phase, RetireReason, RunnerRequestId,
};
use mayfly_control_domain::host::{AdmissionCause, Host, HostCommand, LeasePhase};
use mayfly_control_domain::reconcile::{Pass, PassCommand, PassInputs, plan_pass};
use mayfly_control_domain::registration::{
    Phase as RegistrationPhase, RunnerRegistration, SessionHealth,
};
use mayfly_control_domain::{Actor, Command};
use mayfly_domain::{AttemptId, EventId, HostId, Millis, PoolName, SecretHandleId};
use mayfly_protocol::Mode;
use mayfly_testkit::block_on;
use mayfly_testkit::clock::FakeClock;
use mayfly_testkit::contracts::scaleset::Flow;
use mayfly_testkit::fleet::{
    EPOCH, FleetStore, LaggedReads, PoolSpec, T0, World, report, simulate_with, store,
};
use mayfly_testkit::github::{DEFAULT_GROUP, FakeScaleSetApi};
use mayfly_testkit::journal::MemoryJournal;

const SECOND: u64 = 1_000;

fn lin() -> PoolName {
    PoolName::new("lin").unwrap_or_else(|e| panic!("{e}"))
}

fn h1() -> HostId {
    HostId::new("h1").unwrap_or_else(|e| panic!("{e}"))
}

/// One 32 GiB host and the `lin` pool keeping `prepared` VMs ahead of demand, each driven to
/// Prepared one second after the one before (P2, P6 and the data plane's answers), in attempt
/// id order: the oldest first.
fn prepared_fleet<'s>(
    store: &'s FleetStore,
    journal: &MemoryJournal,
    clock: &FakeClock,
    prepared: u32,
) -> (World<'s>, Vec<AttemptId>) {
    prepared_on(store, journal, clock, &[("h1", 8)], prepared)
}

/// [`prepared_fleet`] over `hosts`, each 32 GiB with its number of VM slots.
fn prepared_on<'s>(
    store: &'s FleetStore,
    journal: &MemoryJournal,
    clock: &FakeClock,
    hosts: &[(&str, u32)],
    prepared: u32,
) -> (World<'s>, Vec<AttemptId>) {
    let mut world = World::new(store);
    for (host, slots) in hosts {
        world.register_host(host, report(32, 0, *slots));
    }
    world.configure_pool(PoolSpec {
        name: lin(),
        os: GuestOs::Linux,
        memory_gib: 4,
        max_running: 4,
        min_prepared: prepared,
        breaker: 3,
    });
    let first = pass(store, journal);
    assert_eq!(
        reservations(&first.accepted),
        usize::try_from(prepared).unwrap_or(0)
    );
    for (host, attempt) in world.unstarted() {
        assert!(world.start(&host, attempt));
    }
    let mut order = Vec::new();
    for (_, attempt) in world.leased_attempts() {
        for _ in 0..12 {
            if world.attempt(attempt).phase == Phase::Prepared {
                break;
            }
            world.progress(attempt, None);
        }
        assert_eq!(world.attempt(attempt).phase, Phase::Prepared);
        order.push(attempt);
        clock.advance(Millis::new(SECOND));
    }
    (world, order)
}

/// One pass that reads the journal's own rows.
fn pass(store: &FleetStore, journal: &MemoryJournal) -> PassReport {
    let reconciler = Reconciler::new(store, journal, ReconcilerConfig::new(EPOCH), Kicks::new(8));
    block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"))
}

fn reservations(commands: &[PassCommand]) -> usize {
    commands
        .iter()
        .filter(|c| matches!(c, PassCommand::ReserveSlot { .. }))
        .count()
}

fn activated(commands: &[PassCommand]) -> Vec<AttemptId> {
    commands
        .iter()
        .filter_map(|c| match c {
            PassCommand::Activate { attempt_id, .. } => Some(*attempt_id),
            _ => None,
        })
        .collect()
}

fn failed(report: &PassReport) -> Vec<(AttemptId, &'static str)> {
    report
        .failed
        .iter()
        .filter_map(|(c, code)| match c {
            PassCommand::Activate { attempt_id, .. } => Some((*attempt_id, *code)),
            _ => None,
        })
        .collect()
}

fn lease_phase(world: &World<'_>, attempt: AttemptId) -> Option<LeasePhase> {
    world.host(&h1()).leases.get(&attempt).map(|l| l.phase)
}

/// `GrantActivation` of `attempt` under the reconciler's own id, as a pass would save it: the
/// grant's `LeaseActivated` event, from the outcome or, once recorded, from its receipt.
fn grant_as_the_reconciler(store: &FleetStore, attempt: AttemptId) -> EventId {
    let command = Command::new(
        grant_command_id(attempt),
        T0,
        EPOCH,
        Actor::Automation {
            policy: "reconciler",
        },
        HostCommand::GrantActivation {
            attempt_id: attempt,
        },
    );
    match block_on(store.execute::<Host>(SaveRequest::new("h1", command)))
        .unwrap_or_else(|e| panic!("{e}"))
    {
        Outcome::Accepted { appended, .. } => appended
            .streams
            .first()
            .and_then(|s| s.event_ids.first().copied())
            .unwrap_or_else(|| panic!("the grant appended its event")),
        Outcome::Refused { rejection, .. } => panic!("grant refused: {rejection:?}"),
    }
}

#[test]
fn should_grant_and_activate_a_prepared_attempt_once_when_a_trusted_job_waits() {
    // given: one prepared VM, then one job assigned and acquired (classified trusted)
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let (mut world, prepared) = prepared_fleet(&store, &journal, &clock, 1);
    world.statistics(&lin(), 1, 1, false);
    assert!(world.scale_set(&lin()).runner_may_start(0));

    // when
    let first = pass(&store, &journal);
    let again = pass(&store, &journal);

    // then: the host granted the lease and the attempt began its activation under that grant,
    // once; the second pass grants nothing (the runner serves the job)
    assert_eq!(activated(&first.accepted), prepared);
    assert!(
        activated(&again.pass.commands).is_empty(),
        "{:?}",
        again.pass.commands
    );
    assert_eq!(
        lease_phase(&world, prepared[0]),
        Some(LeasePhase::Activated)
    );
    let attempt = world.attempt(prepared[0]);
    assert_eq!(attempt.phase, Phase::Activating);
    assert_eq!(
        attempt.grant_ref,
        Some(grant_as_the_reconciler(&store, prepared[0]))
    );
}

#[test]
fn should_grant_nothing_when_no_runner_may_start() {
    // given: two prepared VMs and two jobs assigned, none classified trusted (no lookup, no
    // acquired offer)
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let (mut world, prepared) = prepared_fleet(&store, &journal, &clock, 2);
    world.statistics(&lin(), 2, 0, false);
    assert!(!world.scale_set(&lin()).runner_may_start(0));

    // when
    let report = pass(&store, &journal);

    // then: both stay prepared, their leases reserved
    assert!(activated(&report.pass.commands).is_empty());
    for attempt in prepared {
        assert_eq!(world.attempt(attempt).phase, Phase::Prepared);
        assert_eq!(lease_phase(&world, attempt), Some(LeasePhase::Reserved));
    }
}

#[test]
fn should_grant_no_more_than_the_shortfall_when_more_attempts_are_prepared() {
    // given: three prepared VMs and one trusted job
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let (mut world, prepared) = prepared_fleet(&store, &journal, &clock, 3);
    world.statistics(&lin(), 1, 1, false);

    // when
    let report = pass(&store, &journal);

    // then: the oldest is granted, the others stay prepared
    assert_eq!(activated(&report.accepted), vec![prepared[0]]);
    assert_eq!(world.attempt(prepared[0]).phase, Phase::Activating);
    for attempt in &prepared[1..] {
        assert_eq!(world.attempt(*attempt).phase, Phase::Prepared);
        assert_eq!(lease_phase(&world, *attempt), Some(LeasePhase::Reserved));
    }
}

/// The pure pass preferring the newest prepared attempt: a pass that read the fleet when
/// another attempt looked the one to grant (stands in for any pass whose read predates the
/// first grant and chose differently).
fn newest_first(inputs: &PassInputs) -> Pass {
    let mut flipped = inputs.clone();
    for lease in flipped.hosts.iter_mut().flat_map(|h| h.leases.iter_mut()) {
        lease.prepared_at = lease.prepared_at.map(|t| Millis::new(u64::MAX - t.get()));
    }
    plan_pass(&flipped)
}

#[test]
fn should_grant_no_second_attempt_when_a_racing_pass_read_before_the_first_grant() {
    // given: two prepared VMs and one trusted job; a second reconciler reads the fleet as it
    // was before the first one's pass (the capture `LaggedReads::new` takes)
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let (mut world, prepared) = prepared_fleet(&store, &journal, &clock, 2);
    world.statistics(&lin(), 1, 1, false);
    let behind = LaggedReads::new(&journal, 1);
    let racing = Reconciler::new(
        &store,
        &behind,
        ReconcilerConfig {
            plan: newest_first,
            ..ReconcilerConfig::new(EPOCH)
        },
        Kicks::new(8),
    );

    // when: the first pass grants the oldest; the racing pass, from its earlier read, the
    // newest
    let first = pass(&store, &journal);
    let raced = block_on(racing.run_pass()).unwrap_or_else(|e| panic!("{e}"));

    // then: the racing pass planned the other attempt and its save was refused: one grant for
    // one job
    assert_eq!(activated(&first.accepted), vec![prepared[0]]);
    assert_eq!(activated(&raced.pass.commands), vec![prepared[1]]);
    assert_eq!(failed(&raced), vec![(prepared[1], "conflict")]);
    assert_eq!(world.attempt(prepared[1]).phase, Phase::Prepared);
    assert_eq!(lease_phase(&world, prepared[1]), Some(LeasePhase::Reserved));
}

#[test]
fn should_grant_no_second_attempt_when_a_racing_pass_on_another_host_read_before_the_first_grant() {
    // given: a prepared VM on each of h1 and h2 (one slot each; h1's the older) and a third
    // host with room, the capacity settled, then one trusted job; a second reconciler reads the
    // fleet as it is now
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let (mut world, prepared) = prepared_on(
        &store,
        &journal,
        &clock,
        &[("h1", 1), ("h2", 1), ("h3", 4)],
        2,
    );
    let settled = pass(&store, &journal);
    assert!(activated(&settled.pass.commands).is_empty());
    world.statistics(&lin(), 1, 1, false);
    let on = |attempt: AttemptId| {
        world
            .attempt(attempt)
            .identity
            .map(|i| i.host_id.to_string())
            .unwrap_or_default()
    };
    assert_eq!(
        (on(prepared[0]), on(prepared[1])),
        ("h1".to_owned(), "h2".to_owned())
    );
    let behind = LaggedReads::new(&journal, 1);
    let racing = Reconciler::new(
        &store,
        &behind,
        ReconcilerConfig {
            plan: newest_first,
            ..ReconcilerConfig::new(EPOCH)
        },
        Kicks::new(8),
    );

    // when: the first pass grants h1's attempt; the racing pass, from its earlier read, h2's
    let first = pass(&store, &journal);
    let raced = block_on(racing.run_pass()).unwrap_or_else(|e| panic!("{e}"));

    // then: only other hosts' streams moved since the racing read (h1's grant, h3's
    // reservation; no capacity, pool or scale-set write and nothing on h2), and the grant on
    // h2, which reads them, is refused: one grant for one job
    assert!(
        !first
            .accepted
            .iter()
            .any(|c| matches!(c, PassCommand::ApplyCapacity { .. })),
        "{:?}",
        first.accepted
    );
    assert_eq!(activated(&first.accepted), vec![prepared[0]]);
    assert_eq!(activated(&raced.pass.commands), vec![prepared[1]]);
    assert_eq!(failed(&raced), vec![(prepared[1], "conflict")]);
    assert_eq!(world.attempt(prepared[1]).phase, Phase::Prepared);
    let h2 = HostId::new("h2").unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        world.host(&h2).leases.get(&prepared[1]).map(|l| l.phase),
        Some(LeasePhase::Reserved)
    );
}

#[test]
fn should_grant_nothing_when_a_job_leaves_the_scale_set_before_the_pass_applies_its_capacity() {
    // given: one prepared VM, one trusted job and a new capacity report; a pass reads the
    // fleet (the capture `LaggedReads::new` takes), then the job leaves the scale set
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let (mut world, prepared) = prepared_fleet(&store, &journal, &clock, 1);
    world.statistics(&lin(), 1, 1, false);
    // the host reports two slots: the pass's capacity input for the scale set changes
    world.report(&h1(), report(32, 0, 2));
    let behind = LaggedReads::new(&journal, 1);
    let stale = Reconciler::new(&store, &behind, ReconcilerConfig::new(EPOCH), Kicks::new(8));
    world.statistics(&lin(), 0, 0, true);
    assert!(!world.scale_set(&lin()).runner_may_start(0));

    // when: the stale pass applies its capacity input (on the scale set as it is now, past
    // the job's leaving) and then plans its grant
    let report = block_on(stale.run_pass()).unwrap_or_else(|e| panic!("{e}"));

    // then: the grant is not decided, since the scale set moved past the pass's read before
    // the pass's own write: no runner for a job that is gone
    assert!(
        report
            .accepted
            .iter()
            .any(|c| matches!(c, PassCommand::ApplyCapacity { .. })),
        "{:?}",
        report.pass.commands
    );
    assert_eq!(failed(&report), vec![(prepared[0], "conflict")]);
    assert_eq!(world.attempt(prepared[0]).phase, Phase::Prepared);
    assert_eq!(lease_phase(&world, prepared[0]), Some(LeasePhase::Reserved));
}

#[test]
fn should_grant_nothing_when_a_retiring_runner_binds_a_job_after_the_read() {
    // given: three prepared VMs; the oldest granted for a job, listening, its retirement
    // requested (not counted as a runner); a second job acquired; a pass reads the fleet (the
    // capture `LaggedReads::new` takes): two jobs, no runner, two prepared VMs
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let (mut world, prepared) = prepared_fleet(&store, &journal, &clock, 3);
    world.statistics(&lin(), 1, 1, false);
    let first = pass(&store, &journal);
    assert_eq!(activated(&first.accepted), vec![prepared[0]]);
    let runner = prepared[0];
    world.progress(runner, None);
    world.progress(runner, None);
    let automation = || Actor::Automation { policy: "test" };
    world
        .command::<Attempt>(
            &runner.to_string(),
            automation(),
            AttemptCommand::RecordJitHandle {
                handle_id: SecretHandleId::from_u128(77),
                registration_ref: EventId::from_u128(78),
            },
        )
        .unwrap_or_else(|e| panic!("{e}"));
    world.progress(runner, None);
    world.progress(runner, None);
    assert_eq!(world.attempt(runner).phase, Phase::Listening);
    world
        .command::<Attempt>(
            &runner.to_string(),
            automation(),
            AttemptCommand::RequestRetirement {
                reason: RetireReason::Surplus,
            },
        )
        .unwrap_or_else(|e| panic!("{e}"));
    world.statistics(&lin(), 2, 1, false);
    let behind = LaggedReads::new(&journal, 1);
    let stale = Reconciler::new(&store, &behind, ReconcilerConfig::new(EPOCH), Kicks::new(8));

    // when: GitHub binds the first job to the retiring runner (its retirement withdrawn), then
    // the stale pass grants for the two jobs it read
    world
        .command::<Attempt>(
            &runner.to_string(),
            automation(),
            AttemptCommand::BindJob {
                request_id: RunnerRequestId(1),
            },
        )
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(world.attempt(runner).phase, Phase::Busy);
    let report = block_on(stale.run_pass()).unwrap_or_else(|e| panic!("{e}"));

    // then: the runner counts again, so one job is left; neither grant planned from the stale
    // read is decided, and both VMs stay prepared
    assert_eq!(
        activated(&report.pass.commands),
        vec![prepared[1], prepared[2]]
    );
    assert_eq!(
        failed(&report),
        vec![(prepared[1], "conflict"), (prepared[2], "conflict")]
    );
    for attempt in &prepared[1..] {
        assert_eq!(world.attempt(*attempt).phase, Phase::Prepared);
        assert_eq!(lease_phase(&world, *attempt), Some(LeasePhase::Reserved));
    }
}

#[test]
fn should_leave_the_attempt_prepared_and_grant_it_later_when_the_host_closes_after_the_read() {
    // given: one prepared VM and one trusted job; a pass reads the fleet (the capture
    // `LaggedReads::new` takes), then the host's admission closes before its save
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let (mut world, prepared) = prepared_fleet(&store, &journal, &clock, 1);
    world.statistics(&lin(), 1, 1, false);
    let behind = LaggedReads::new(&journal, 1);
    let stale = Reconciler::new(&store, &behind, ReconcilerConfig::new(EPOCH), Kicks::new(8));
    let close = |world: &mut World<'_>, body: HostCommand| {
        world
            .command::<Host>("h1", Actor::Automation { policy: "test" }, body)
            .unwrap_or_else(|e| panic!("{e}"));
    };
    close(
        &mut world,
        HostCommand::CloseAdmission {
            cause: AdmissionCause::Woke,
        },
    );

    // when: the stale pass, then a fresh one while closed, then one after it reopens
    let stale_pass = block_on(stale.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    let closed = pass(&store, &journal);
    close(
        &mut world,
        HostCommand::OpenAdmission {
            cause: AdmissionCause::Woke,
        },
    );
    let reopened = pass(&store, &journal);

    // then: the stale grant is not decided, a closed host is granted nothing, and the attempt,
    // still prepared, is granted once the host admits again
    assert_eq!(failed(&stale_pass), vec![(prepared[0], "conflict")]);
    assert!(activated(&closed.pass.commands).is_empty());
    assert_eq!(activated(&reopened.accepted), prepared);
    assert_eq!(world.attempt(prepared[0]).phase, Phase::Activating);
}

#[test]
fn should_activate_a_granted_attempt_when_its_activation_was_lost() {
    // given: a pass granted the lease and died before `BeginActivation`
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let (mut world, prepared) = prepared_fleet(&store, &journal, &clock, 2);
    world.statistics(&lin(), 1, 1, false);
    let grant = grant_as_the_reconciler(&store, prepared[0]);

    // when
    let report = pass(&store, &journal);

    // then: the next pass begins its activation under that grant and grants nothing else: the
    // granted lease already serves the job
    assert_eq!(activated(&report.accepted), vec![prepared[0]]);
    let attempt = world.attempt(prepared[0]);
    assert_eq!(attempt.phase, Phase::Activating);
    assert_eq!(attempt.grant_ref, Some(grant));
    assert_eq!(world.attempt(prepared[1]).phase, Phase::Prepared);
    let begun = Command::new(
        begin_command_id(prepared[0], grant),
        T0,
        EPOCH,
        Actor::Automation {
            policy: "reconciler",
        },
        AttemptCommand::BeginActivation { grant_ref: grant },
    );
    let replayed = block_on(store.execute::<mayfly_control_domain::attempt::Attempt>(
        SaveRequest::new(prepared[0].to_string(), begun),
    ))
    .unwrap_or_else(|e| panic!("{e}"));
    assert!(matches!(replayed, Outcome::Accepted { replayed: true, .. }));
}

/// One scale set serving the pool, healthy.
struct Directory(ScaleSetId);

impl ScaleSetDirectory for Directory {
    fn scale_set(&self, _pool: &PoolName) -> Option<(ScaleSetId, SessionHealth)> {
        Some((self.0, SessionHealth::Healthy))
    }

    fn scale_sets(&self) -> BTreeSet<ScaleSetId> {
        BTreeSet::from([self.0])
    }
}

#[test]
fn should_mint_for_the_attempt_when_the_reconciler_activated_it() {
    // given: a prepared VM granted and activated for a trusted job, its fresh verification
    // answered (the saga's `VerifyGuest` and the data plane's `GuestVerified`)
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let (mut world, prepared) = prepared_fleet(&store, &journal, &clock, 1);
    world.statistics(&lin(), 1, 1, false);
    let report = pass(&store, &journal);
    assert_eq!(activated(&report.accepted), prepared);
    let attempt = prepared[0];
    world.progress(attempt, None);
    world.progress(attempt, None);
    assert!(world.attempt(attempt).fresh_verification);
    let api = FakeScaleSetApi::new(Flow::Direct, FakeClock::at(T0));
    let set = block_on(api.create_scale_set(&ScaleSetSpec {
        name: "mayfly".to_owned(),
        runner_group: DEFAULT_GROUP.to_owned(),
        labels: vec!["mayfly".to_owned()],
    }))
    .unwrap_or_else(|e| panic!("{e}"))
    .id;
    let registry = api.registry();
    let directory = Directory(set);
    let broker = JitBroker::new();
    let sessions = HostSessions::new();
    sessions.open(
        h1(),
        HostSession {
            epoch: EPOCH,
            mode: Mode::Normal,
        },
    );
    let manager = RegistrationManager::new(
        &store,
        sessions,
        &api,
        &registry,
        &directory,
        &broker,
        RegistrationConfig::default(),
    );

    // when
    let step = block_on(manager.step(attempt));

    // then: one registration issued under the VM's name and its handle on the attempt
    assert_eq!(step, Ok(Step::Moved));
    let registration = block_on(store.load::<RunnerRegistration>(&attempt.to_string()))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(registration.phase, RegistrationPhase::Issued);
    assert_eq!(world.attempt(attempt).jit, Jit::Issued(handle_of(attempt)));
}

#[test]
fn should_grant_within_the_demand_when_a_seeded_simulation_reads_behind_the_journal() {
    // given: the CP-03 simulation with acquired jobs, a read two steps behind the journal

    // when
    let sim = simulate_with(
        29,
        600,
        mayfly_testkit::fleet::SimOptions {
            lag: 2,
            ..mayfly_testkit::fleet::SimOptions::default()
        },
    );

    // then: grants happened, none beyond a pool's shortfall or trusted room, none of an
    // attempt that was not prepared (the simulation's P9 check)
    assert!(sim.activated > 0, "no grant in {} passes", sim.passes);
    assert!(sim.violations.is_empty(), "{:#?}", sim.violations);
}

#[test]
fn should_show_when_the_attempt_became_prepared_while_it_is() {
    // given: two VMs prepared a second apart
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let (mut world, prepared) = prepared_fleet(&store, &journal, &clock, 2);
    let rows = || {
        block_on(queries::fleet(&journal))
            .unwrap_or_else(|e| panic!("{e}"))
            .attempts
    };
    let before = rows();

    // when: one of them is activated
    world.statistics(&lin(), 1, 1, false);
    let _ = pass(&store, &journal);
    let after = rows();

    // then: each row shows when it became prepared, and the activated one no longer does
    let at = |rows: &BTreeMap<AttemptId, AttemptRow>, a: &AttemptId| {
        rows.get(a).and_then(|r| r.prepared_at)
    };
    assert_eq!(at(&before, &prepared[0]), Some(T0));
    assert_eq!(
        at(&before, &prepared[1]),
        Some(Millis::new(T0.get() + SECOND))
    );
    assert_eq!(at(&after, &prepared[0]), None);
    assert_eq!(
        at(&after, &prepared[1]),
        Some(Millis::new(T0.get() + SECOND))
    );
}

#[test]
fn should_show_the_trusted_jobs_waiting_on_the_scale_sets_row_when_a_job_is_acquired() {
    // given: the pool's scale set, its session open
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let (mut world, _) = prepared_fleet(&store, &journal, &clock, 0);
    let row = || {
        block_on(queries::fleet(&journal))
            .unwrap_or_else(|e| panic!("{e}"))
            .scale_sets
            .remove("scaleset-lin")
            .unwrap_or_else(|| panic!("no scale_sets row"))
    };
    let idle = row();

    // when: two jobs assigned, one of them acquired
    world.statistics(&lin(), 2, 1, false);
    let busy = row();

    // then
    assert!(idle.active && busy.active);
    assert_eq!(
        (idle.waiting, busy.waiting),
        (0, world.scale_set(&lin()).waiting())
    );
    assert!(busy.waiting > 0);
}
