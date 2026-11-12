//! The image placement manager (P17, P18; CP-07) at component level, over the fleet kit's real
//! `AggregateStore` and projections on `MemoryJournal`: a pin placed through the placement relay
//! and the inbox until the reconciler admits the pool on the host, the clone's base read from
//! the materialised placement, withdrawal refused while pinned, an unknown materialisation
//! resolved only by its base's absence, and the runner version's expiry tripping the breaker.

use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use mayfly_control::delivery::{
    DeliveryConfig, DeliveryWorker, PlacementRelay, PlacementRelayConfig, compose_placement,
};
use mayfly_control::inbox::Inbox;
use mayfly_control::placement::{
    PlacementConfig, PlacementIndex, PlacementManager, WithdrawError, image_instance,
    withdraw_image,
};
use mayfly_control::reconciler::{Reconciler, ReconcilerConfig};
use mayfly_control::sessions::{HostSession, HostSessions};
use mayfly_control_domain::Actor;
use mayfly_control_domain::attempt::GuestOs;
use mayfly_control_domain::host::{Host, HostCommand, ReserveSlot};
use mayfly_control_domain::image::{
    Image, ImageCommand, ImageManifest, RunnerVersion, Verification,
};
use mayfly_control_domain::placement::policy::PlacementAction;
use mayfly_control_domain::placement::{
    ImagePlacement, PendingOutcome, PlacementDirective, PlacementPhase, placement_instance,
};
use mayfly_control_domain::pool::{ConfigurePool, Pool, PoolCommand, TrustTier, terms};
use mayfly_control_domain::reconcile::PassCommand;
use mayfly_domain::{
    CommandId, DirectiveId, EventId, HostId, ImageDigest, Millis, ObservationId, PoolName,
    Resources, Sample,
};
use mayfly_protocol::{
    BaseName, Directive, DirectiveKind, DirectiveRef, LinkError, Mode, Observation,
    ObservationKind, VmObservation,
};
use mayfly_testkit::block_on;
use mayfly_testkit::clock::{FakeClock, SeededRng};
use mayfly_testkit::delivery::MemoryLedger;
use mayfly_testkit::fleet::{EPOCH, FleetStore, PoolSpec, T0, World, report, store};
use mayfly_testkit::journal::MemoryJournal;

use crate::recovery_kit::{kit, stream};
use crate::scripted::BeforeAppend;

const BASE: &str = "mfb-000000000000-r2-330-0";

/// The link's sending half: records each frame.
#[derive(Debug, Clone, Default)]
struct Link {
    sent: Arc<Mutex<Vec<Directive>>>,
}

impl Link {
    fn take(&self) -> Vec<Directive> {
        std::mem::take(&mut *self.sent.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl mayfly_protocol::DirectiveSink for Link {
    fn send(&self, directive: Directive) -> impl Future<Output = Result<(), LinkError>> + Send {
        let sent = Arc::clone(&self.sent);
        async move {
            sent.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(directive);
            Ok(())
        }
    }
}

type Relay<'s> = PlacementRelay<
    's,
    MemoryJournal,
    FakeClock,
    SeededRng,
    mayfly_control::store::NoNotifier,
    Link,
    FakeClock,
>;
type Worker = DeliveryWorker<MemoryJournal, MemoryLedger<FakeClock>, SeededRng, FakeClock>;

fn pool() -> PoolName {
    PoolName::new("lin").unwrap_or_else(|e| panic!("{e}"))
}

fn other_image() -> ImageDigest {
    ImageDigest::new(&format!("sha256:{}", "b".repeat(64))).unwrap_or_else(|e| panic!("{e}"))
}

/// A world with the sample host and a Linux pool pinned to the sample image, registered and
/// verified with runner 2.330.0, placed on no host yet; two jobs are assigned.
fn world(store: &FleetStore) -> World<'_> {
    let mut world = World::new(store);
    world.register_host(HostId::sample().as_str(), report(64, 0, 8));
    for digest in [ImageDigest::sample(), other_image()] {
        let _ = world.command::<Image>(
            image_instance(&digest),
            Actor::Startup,
            ImageCommand::RegisterImage {
                digest: digest.clone(),
                manifest: ImageManifest {
                    os: GuestOs::Linux,
                    runner_version: RunnerVersion::new(2, 330, 0),
                    revision: 1,
                },
            },
        );
        let _ = world.command::<Image>(
            image_instance(&digest),
            Actor::Startup,
            ImageCommand::RecordVerification {
                outcome: Verification::Passed,
            },
        );
    }
    world.configure_unplaced(PoolSpec {
        name: pool(),
        os: GuestOs::Linux,
        memory_gib: 4,
        max_running: 2,
        min_prepared: 0,
        breaker: 3,
    });
    world.statistics(&pool(), 2, 0, false);
    world
}

fn sessions() -> HostSessions {
    let sessions = HostSessions::new();
    sessions.open(
        HostId::sample(),
        HostSession {
            epoch: EPOCH,
            mode: Mode::Normal,
        },
    );
    sessions
}

fn worker(journal: &MemoryJournal) -> Worker {
    let store_clock = FakeClock::at(Millis::new(5_000));
    DeliveryWorker::new(
        journal.clone(),
        MemoryLedger::new(journal, store_clock.clone()),
        SeededRng::new(3),
        store_clock,
        DeliveryConfig::default(),
    )
}

fn answer(n: u128, directive_id: DirectiveId, kind: ObservationKind) -> Observation {
    Observation {
        observation_id: ObservationId::from_u128(0x0b00 + n),
        epoch: EPOCH,
        answers: Some(DirectiveRef {
            directive_id,
            attempt: None,
        }),
        observed_at_dp: Millis::new(0),
        kind,
    }
}

fn reservations(store: &FleetStore, journal: &MemoryJournal) -> usize {
    let reconciler = Reconciler::new(
        store,
        journal,
        ReconcilerConfig::new(EPOCH),
        mayfly_control::delivery::Kicks::new(8),
    );
    let report = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    report
        .accepted
        .iter()
        .filter(|c| matches!(c, PassCommand::ReserveSlot { .. }))
        .count()
}

#[test]
fn should_place_the_pin_and_admit_the_pool_when_the_base_is_materialised() {
    // given: a pinned pool with open terms, its image on no host
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let _world = world(&store);
    let index = PlacementIndex::default();
    let manager = PlacementManager::new(
        &store,
        &journal,
        PlacementConfig::new(EPOCH),
        mayfly_control::delivery::Kicks::new(8),
        index.clone(),
    );
    let link = Link::default();
    let relay: Relay<'_> = PlacementRelay::new(
        &store,
        link.clone(),
        FakeClock::at(Millis::new(0)),
        sessions(),
        PlacementRelayConfig::default(),
        &DeliveryConfig::default(),
        index.clone(),
    );
    let worker = worker(&journal);
    let inbox = Inbox::new(&store, HostId::sample(), EPOCH).with_placements(index);
    let before = reservations(&store, &journal);

    // when: the manager requests and pulls; the relay sends; the data plane answers
    let requested = block_on(manager.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    let issued = block_on(manager.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    block_on(worker.catch_up(&relay)).unwrap_or_else(|e| panic!("{e:?}"));
    let pull = link.take();
    let pull_id = pull.first().map(|d| d.directive_id);
    let pulled = answer(
        1,
        pull_id.unwrap_or(DirectiveId::from_u128(0)),
        ObservationKind::ImagePulled {
            digest: ImageDigest::sample(),
        },
    );
    block_on(inbox.record(&pulled)).unwrap_or_else(|e| panic!("{e}"));
    let _ = block_on(manager.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    block_on(worker.catch_up(&relay)).unwrap_or_else(|e| panic!("{e:?}"));
    let materialise = link.take();
    let materialised = answer(
        2,
        materialise
            .first()
            .map_or(DirectiveId::from_u128(0), |d| d.directive_id),
        ObservationKind::ImageMaterialised {
            digest: ImageDigest::sample(),
            base_name: BaseName::new(BASE).unwrap_or_else(|e| panic!("{e}")),
            disk_bytes: None,
        },
    );
    block_on(inbox.record(&materialised)).unwrap_or_else(|e| panic!("{e}"));
    let placed = block_on(manager.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    let after = reservations(&store, &journal);

    // then: nothing admitted before the base, one placement per host, a pull by digest and a
    // materialisation to a named local base, never an OCI reference (ADR 0039)
    assert_eq!(before, 0, "no reservation before the base is materialised");
    assert!(matches!(
        requested.accepted.as_slice(),
        [PlacementAction::Request { .. }]
    ));
    assert!(matches!(
        issued.accepted.as_slice(),
        [PlacementAction::Issue { .. }]
    ));
    assert_eq!(
        pull.iter()
            .map(|d| (&d.kind, d.epoch, d.attempt))
            .collect::<Vec<_>>(),
        vec![(
            &DirectiveKind::PullImage {
                digest: ImageDigest::sample()
            },
            EPOCH,
            None
        )]
    );
    assert_eq!(
        materialise.iter().map(|d| &d.kind).collect::<Vec<_>>(),
        vec![&DirectiveKind::MaterialiseImage {
            digest: ImageDigest::sample(),
            base_name: BaseName::new(BASE).unwrap_or_else(|e| panic!("{e}")),
            runner_version: "2.330.0".to_owned(),
        }]
    );
    assert_eq!(
        placed.bases.get(&(HostId::sample(), ImageDigest::sample())),
        Some(&BASE.to_owned())
    );
    assert!(placed.actions.is_empty(), "{:?}", placed.actions);
    assert_eq!(
        after, 2,
        "the pool is admitted once its base is materialised"
    );
}

#[test]
fn should_send_only_named_local_bases_when_placement_directives_are_relayed() {
    // given
    let digest = ImageDigest::sample();
    let runner = RunnerVersion::new(2, 330, 0);

    // when
    let kinds: Vec<DirectiveKind> = [
        PlacementDirective::PullImage,
        PlacementDirective::MaterialiseImage {
            runner_version: runner,
        },
        PlacementDirective::EvictImage {
            runner_version: runner,
        },
    ]
    .into_iter()
    .map(|d| compose_placement(&digest, d).unwrap_or_else(|e| panic!("{e}")))
    .collect();

    // then: a pull names the digest only; a base is an `mfb-*` name built from the digest and
    // the runner version, which no OCI reference can be (ADR 0039)
    let base = BaseName::new(BASE).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        kinds,
        vec![
            DirectiveKind::PullImage {
                digest: digest.clone()
            },
            DirectiveKind::MaterialiseImage {
                digest,
                base_name: base.clone(),
                runner_version: "2.330.0".to_owned(),
            },
            DirectiveKind::EvictImage { base_name: base },
        ]
    );
    assert!(BaseName::new("ghcr.io/example/image@sha256:00").is_err());
}

#[test]
fn should_refuse_a_withdrawal_when_a_pool_pins_the_image() {
    // given
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let mut world = world(&store);

    // when: withdrawn while pinned, then once the pool pins another image
    let pinned = block_on(withdraw_image(
        &store,
        &journal,
        &ImageDigest::sample(),
        501,
        CommandId::from_u128(0x3d01),
        EPOCH,
    ));
    let _ = world.command::<Pool>(
        pool().as_str(),
        Actor::Operator { uid: 501 },
        PoolCommand::PinImage {
            digest: other_image(),
            verified_ref: Some(EventId::from_u128(8)),
        },
    );
    let unpinned = block_on(withdraw_image(
        &store,
        &journal,
        &ImageDigest::sample(),
        501,
        CommandId::from_u128(0x3d02),
        EPOCH,
    ));

    // then: P17
    assert_eq!(pinned, Err(WithdrawError::Pinned(pool())));
    assert_eq!(unpinned, Ok(()));
}

#[test]
fn should_resolve_an_unknown_materialisation_only_when_its_base_is_absent() {
    // given: a pull answered, a materialisation sent and past its deadline unanswered
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let _world = world(&store);
    let index = PlacementIndex::default();
    let config = PlacementConfig::new(EPOCH);
    let manager = PlacementManager::new(
        &store,
        &journal,
        config,
        mayfly_control::delivery::Kicks::new(8),
        index.clone(),
    );
    let link = Link::default();
    let relay: Relay<'_> = PlacementRelay::new(
        &store,
        link.clone(),
        FakeClock::at(Millis::new(0)),
        sessions(),
        PlacementRelayConfig::default(),
        &DeliveryConfig::default(),
        index.clone(),
    );
    let worker = worker(&journal);
    let inbox = Inbox::new(&store, HostId::sample(), EPOCH).with_placements(index);
    let _ = block_on(manager.run_pass());
    let _ = block_on(manager.run_pass());
    block_on(worker.catch_up(&relay)).unwrap_or_else(|e| panic!("{e:?}"));
    let pull = link
        .take()
        .first()
        .map_or(DirectiveId::from_u128(0), |d| d.directive_id);
    let pulled = answer(
        1,
        pull,
        ObservationKind::ImagePulled {
            digest: ImageDigest::sample(),
        },
    );
    block_on(inbox.record(&pulled)).unwrap_or_else(|e| panic!("{e}"));
    let _ = block_on(manager.run_pass());
    block_on(worker.catch_up(&relay)).unwrap_or_else(|e| panic!("{e:?}"));
    clock.advance(Millis::new(
        config.timeouts.materialise.get() + config.reply_grace.get() + 1,
    ));
    let _ = block_on(manager.run_pass());
    let instance = placement_instance(&HostId::sample(), &ImageDigest::sample());
    let unknown =
        block_on(store.load::<ImagePlacement>(&instance)).unwrap_or_else(|e| panic!("{e}"));
    let listing = |n: u128, names: &[&str]| Observation {
        observation_id: ObservationId::from_u128(0x0c00 + n),
        epoch: EPOCH,
        answers: None,
        observed_at_dp: Millis::new(0),
        kind: ObservationKind::Inventory {
            vms: names
                .iter()
                .map(|name| VmObservation {
                    name: (*name).to_owned(),
                    instance_id: None,
                    running: false,
                    guest: None,
                    memory_bytes: None,
                    disk_bytes: None,
                })
                .collect(),
        },
    };

    // when: a listing shows the base (half made, perhaps), then one does not
    block_on(inbox.record(&listing(1, &[BASE]))).unwrap_or_else(|e| panic!("{e}"));
    let still = block_on(store.load::<ImagePlacement>(&instance)).unwrap_or_else(|e| panic!("{e}"));
    block_on(inbox.record(&listing(2, &[]))).unwrap_or_else(|e| panic!("{e}"));
    let resolved =
        block_on(store.load::<ImagePlacement>(&instance)).unwrap_or_else(|e| panic!("{e}"));

    // then: unknown until an observation; a listed base proves nothing (ADR 0025), an absent one
    // shows the materialisation did not apply, and it is issued again
    assert!(matches!(
        unknown.pending.map(|p| p.outcome),
        Some(PendingOutcome::Unknown { .. })
    ));
    assert_eq!(still.pending, unknown.pending);
    assert_eq!(resolved.pending, None);
    assert_eq!(resolved.phase, PlacementPhase::Pulled);
    let again = block_on(manager.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    assert!(matches!(
        again.accepted.as_slice(),
        [PlacementAction::Issue { .. }]
    ));
}

#[test]
fn should_trip_the_breaker_when_the_pinned_runner_version_expires() {
    // given
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let mut world = world(&store);
    let manager = PlacementManager::new(
        &store,
        &journal,
        PlacementConfig::new(EPOCH),
        mayfly_control::delivery::Kicks::new(8),
        PlacementIndex::default(),
    );
    let open = terms(&world.pool(&pool())).is_some_and(|t| t.open);

    // when: GitHub no longer accepts runner 2.330.0
    let _ = world.command::<Image>(
        image_instance(&ImageDigest::sample()),
        Actor::Startup,
        ImageCommand::RecordRunnerVersionExpired {
            minimum: RunnerVersion::new(2, 331, 0),
        },
    );
    let first = block_on(manager.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    let second = block_on(manager.run_pass()).unwrap_or_else(|e| panic!("{e}"));

    // then: P18, the pin's breaker trips once with a named reason, closing the pool's terms
    let state = world.pool(&pool());
    assert!(open);
    assert!(
        first
            .accepted
            .iter()
            .any(|a| matches!(a, PlacementAction::Trip { .. }))
    );
    assert!(
        !second
            .actions
            .iter()
            .any(|a| matches!(a, PlacementAction::Trip { .. }))
    );
    assert!(
        state
            .breakers
            .get(&ImageDigest::sample())
            .is_some_and(|b| b.tripped_at.is_some())
    );
    assert!(terms(&state).is_some_and(|t| !t.open));
}

/// Places the sample image on the sample host through the manager, the relay and the inbox,
/// the data plane answering the materialisation with `disk_bytes`.
fn place_through_the_link(store: &FleetStore, journal: &MemoryJournal, disk_bytes: Option<u64>) {
    let index = PlacementIndex::default();
    let manager = PlacementManager::new(
        store,
        journal,
        PlacementConfig::new(EPOCH),
        mayfly_control::delivery::Kicks::new(8),
        index.clone(),
    );
    let link = Link::default();
    let relay: Relay<'_> = PlacementRelay::new(
        store,
        link.clone(),
        FakeClock::at(Millis::new(0)),
        sessions(),
        PlacementRelayConfig::default(),
        &DeliveryConfig::default(),
        index.clone(),
    );
    let worker = worker(journal);
    let inbox = Inbox::new(store, HostId::sample(), EPOCH).with_placements(index);
    let _ = block_on(manager.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    let _ = block_on(manager.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    block_on(worker.catch_up(&relay)).unwrap_or_else(|e| panic!("{e:?}"));
    let pull = link
        .take()
        .first()
        .map_or(DirectiveId::from_u128(0), |d| d.directive_id);
    let pulled = answer(
        11,
        pull,
        ObservationKind::ImagePulled {
            digest: ImageDigest::sample(),
        },
    );
    block_on(inbox.record(&pulled)).unwrap_or_else(|e| panic!("{e}"));
    let _ = block_on(manager.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    block_on(worker.catch_up(&relay)).unwrap_or_else(|e| panic!("{e:?}"));
    let made = link
        .take()
        .first()
        .map_or(DirectiveId::from_u128(0), |d| d.directive_id);
    let materialised = answer(
        12,
        made,
        ObservationKind::ImageMaterialised {
            digest: ImageDigest::sample(),
            base_name: BaseName::new(BASE).unwrap_or_else(|e| panic!("{e}")),
            disk_bytes,
        },
    );
    block_on(inbox.record(&materialised)).unwrap_or_else(|e| panic!("{e}"));
}

/// Reconfigures the pool with no disk (`clone_disk_gib` absent) and four VMs, and assigns four
/// jobs.
fn unsized_pool(world: &mut World<'_>) {
    let _ = world.command::<Pool>(
        pool().as_str(),
        Actor::Operator { uid: 501 },
        PoolCommand::ConfigurePool(ConfigurePool {
            revision: 2,
            tier: TrustTier::Trusted,
            os: GuestOs::Linux,
            vm_resources: Resources::new(4 << 30, 0),
            max_running: 4,
            min_prepared: Some(0),
            breaker_threshold: 3,
            breaker_window: Millis::new(30 * 60_000),
        }),
    );
    world.apply_terms(&pool());
    world.statistics(&pool(), 4, 0, false);
}

fn reserved_disks(store: &FleetStore, journal: &MemoryJournal) -> Vec<u64> {
    let reconciler = Reconciler::new(
        store,
        journal,
        ReconcilerConfig::new(EPOCH),
        mayfly_control::delivery::Kicks::new(8),
    );
    let report = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    report
        .accepted
        .iter()
        .filter_map(|c| match c {
            PassCommand::ReserveSlot { resources, .. } => Some(resources.disk_bytes),
            _ => None,
        })
        .collect()
}

#[test]
fn should_cap_reservations_by_the_hosts_disk_when_the_placed_base_reports_its_disk() {
    // given: 1 TiB of disk on the host; a pool with no configured disk wanting four VMs
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let mut world = world(&store);
    unsized_pool(&mut world);

    // when: the data plane reports the base at 400 GiB (protocol 1.4)
    place_through_the_link(&store, &journal, Some(400 << 30));
    let disks = reserved_disks(&store, &journal);

    // then: each clone is planned at its base's disk, so two fit, not four (CP-07, F3)
    assert_eq!(disks, vec![400 << 30, 400 << 30]);
}

#[test]
fn should_plan_no_disk_when_the_materialised_answer_carries_none() {
    // given: the same pool, its base materialised by a data plane of protocol 1.3 or older
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let mut world = world(&store);
    unsized_pool(&mut world);
    let older = ObservationKind::ImageMaterialised {
        digest: ImageDigest::sample(),
        base_name: BaseName::new(BASE).unwrap_or_else(|e| panic!("{e}")),
        disk_bytes: Some(400 << 30),
    }
    .written_in(mayfly_protocol::GUEST_IDENTITY);
    let ObservationKind::ImageMaterialised { disk_bytes, .. } = older else {
        panic!("{older:?}");
    };

    // when
    place_through_the_link(&store, &journal, disk_bytes);
    let disks = reserved_disks(&store, &journal);
    let instance = placement_instance(&HostId::sample(), &ImageDigest::sample());
    let state = block_on(store.load::<ImagePlacement>(&instance)).unwrap_or_else(|e| panic!("{e}"));

    // then: unknown, never a default; every clone planned with no disk, the data plane's gate
    // (I33) refusing what does not fit, as before CP-07
    assert_eq!(state.disk_bytes, None);
    assert_eq!(state.phase, PlacementPhase::Materialised);
    assert_eq!(disks, vec![0, 0, 0, 0]);
}

/// The pool moved off the sample image to `other_image()`, both placed on the sample host:
/// the sample image's base is materialised, unpinned and, with no lease, unreferenced.
fn repinned(world: &mut World<'_>) {
    world.place(&ImageDigest::sample());
    let _ = world.command::<Pool>(
        pool().as_str(),
        Actor::Operator { uid: 501 },
        PoolCommand::PinImage {
            digest: other_image(),
            verified_ref: Some(EventId::from_u128(8)),
        },
    );
    world.place(&other_image());
    world.apply_terms(&pool());
}

/// A second writer on `memory` saving `body` on `A`'s `instance`.
fn command_on<A: mayfly_control_domain::Aggregate>(
    memory: &MemoryJournal,
    id: u128,
    instance: &str,
    actor: Actor,
    body: A::Body,
) where
    A::Body: Clone,
{
    let store = store(memory, &FakeClock::at(T0));
    let command =
        mayfly_control_domain::Command::new(CommandId::from_u128(id), T0, EPOCH, actor, body);
    match block_on(store.execute::<A>(mayfly_control::store::SaveRequest::new(instance, command)))
        .unwrap_or_else(|e| panic!("{e}"))
    {
        mayfly_control::store::Outcome::Accepted { .. } => {}
        mayfly_control::store::Outcome::Refused { rejection, .. } => {
            panic!(
                "refused: {}",
                mayfly_control_domain::Rejection::code(&rejection)
            )
        }
    }
}

fn evictions(report: &mayfly_control::placement::PlacementReport) -> usize {
    report
        .actions
        .iter()
        .filter(|a| matches!(a, PlacementAction::Evict { .. }))
        .count()
}

#[test]
fn should_refuse_the_eviction_when_a_lease_is_reserved_between_the_read_and_the_save() {
    // given: the old base is evictable when the manager reads, and a lease is reserved on its
    // host right before the eviction commits
    let kit = kit();
    let fleet_store = store(&kit.memory, &FakeClock::at(T0));
    let mut world = world(&fleet_store);
    repinned(&mut world);
    let terms_revision = world
        .host(&HostId::sample())
        .pool_terms
        .get(&pool())
        .map_or(0, |t| t.revision);
    let memory = kit.memory.clone();
    let old = placement_instance(&HostId::sample(), &ImageDigest::sample());
    kit.journal.before_next_append(BeforeAppend::new(
        stream(&format!("placement-{old}")),
        move || {
            command_on::<Host>(
                &memory,
                0xE51,
                HostId::sample().as_str(),
                Actor::Automation {
                    policy: "reconciler",
                },
                HostCommand::ReserveSlot(ReserveSlot {
                    attempt_id: mayfly_domain::AttemptId::from_u128(0xA1),
                    pool: pool(),
                    os: GuestOs::Linux,
                    resources: Resources::new(4 << 30, 50 << 30),
                    terms_revision,
                }),
            );
        },
    ));
    let manager = PlacementManager::new(
        &kit.store,
        &kit.memory,
        PlacementConfig::new(EPOCH),
        mayfly_control::delivery::Kicks::new(8),
        PlacementIndex::default(),
    );

    // when
    let raced = block_on(manager.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    let next = block_on(manager.run_pass()).unwrap_or_else(|e| panic!("{e}"));

    // then: the unit read the moved host stream and was refused; the next pass counts the new,
    // unstarted lease as referencing every base on the host and plans no eviction
    let state = block_on(kit.store.load::<ImagePlacement>(&old)).unwrap_or_else(|e| panic!("{e}"));
    assert!(
        raced
            .failed
            .iter()
            .any(|(a, code)| matches!(a, PlacementAction::Evict { .. }) && *code == "conflict"),
        "{:?}",
        raced.failed
    );
    assert_eq!(state.phase, PlacementPhase::Materialised);
    assert_eq!(evictions(&next), 0);
}

#[test]
fn should_evict_the_unpinned_base_when_nothing_references_it() {
    // given
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let mut world = world(&store);
    repinned(&mut world);
    let manager = PlacementManager::new(
        &store,
        &journal,
        PlacementConfig::new(EPOCH),
        mayfly_control::delivery::Kicks::new(8),
        PlacementIndex::default(),
    );

    // when
    let report = block_on(manager.run_pass()).unwrap_or_else(|e| panic!("{e}"));

    // then: P17, the old image's base only
    assert!(matches!(
        report.accepted.as_slice(),
        [PlacementAction::Evict { digest, .. }] if *digest == ImageDigest::sample()
    ));
}

#[test]
fn should_keep_every_base_on_a_host_when_a_lease_there_has_no_attempt_yet() {
    // given: the old base is unpinned and no attempt runs it, but a lease on its host is not
    // started (its attempt row does not exist)
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let mut world = world(&store);
    repinned(&mut world);
    let terms_revision = world
        .host(&HostId::sample())
        .pool_terms
        .get(&pool())
        .map_or(0, |t| t.revision);
    world
        .command::<Host>(
            HostId::sample().as_str(),
            Actor::Automation {
                policy: "reconciler",
            },
            HostCommand::ReserveSlot(ReserveSlot {
                attempt_id: mayfly_domain::AttemptId::from_u128(0xA2),
                pool: pool(),
                os: GuestOs::Linux,
                resources: Resources::new(4 << 30, 50 << 30),
                terms_revision,
            }),
        )
        .unwrap_or_else(|e| panic!("{e}"));
    let manager = PlacementManager::new(
        &store,
        &journal,
        PlacementConfig::new(EPOCH),
        mayfly_control::delivery::Kicks::new(8),
        PlacementIndex::default(),
    );

    // when
    let report = block_on(manager.run_pass()).unwrap_or_else(|e| panic!("{e}"));

    // then: fail closed, nothing evicted under a lease whose image is not known yet
    assert_eq!(evictions(&report), 0);
}

#[test]
fn should_refuse_a_withdrawal_when_a_pin_commits_between_the_check_and_the_save() {
    // given: the pool pins another image when the withdrawal checks, and pins this one again
    // right before the withdrawal commits
    let kit = kit();
    let fleet_store = store(&kit.memory, &FakeClock::at(T0));
    let mut world = world(&fleet_store);
    repinned(&mut world);
    let memory = kit.memory.clone();
    kit.journal.before_next_append(BeforeAppend::new(
        stream(&format!("image-{}", image_instance(&ImageDigest::sample()))),
        move || {
            command_on::<Pool>(
                &memory,
                0xE52,
                pool().as_str(),
                Actor::Operator { uid: 501 },
                PoolCommand::PinImage {
                    digest: ImageDigest::sample(),
                    verified_ref: Some(EventId::from_u128(9)),
                },
            );
        },
    ));

    // when
    let withdrawn = block_on(withdraw_image(
        &kit.store,
        &kit.memory,
        &ImageDigest::sample(),
        501,
        CommandId::from_u128(0x3d03),
        EPOCH,
    ));

    // then: the unit read the moved pool stream and was refused; the retry found the pin
    let image = block_on(
        kit.store
            .load::<Image>(image_instance(&ImageDigest::sample())),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(withdrawn, Err(WithdrawError::Pinned(pool())));
    assert_ne!(
        image.status,
        mayfly_control_domain::image::ImageStatus::Withdrawn
    );
}

#[test]
fn should_record_only_timeouts_when_the_manager_is_in_safe_mode() {
    // given: a pinned image on no host yet
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let _world = world(&store);
    let manager = PlacementManager::new(
        &store,
        &journal,
        PlacementConfig::new(EPOCH),
        mayfly_control::delivery::Kicks::new(8),
        PlacementIndex::default(),
    );

    // when: a pass in safe mode, then one after it
    manager.set_safe_mode(true);
    let safe = block_on(manager.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    manager.set_safe_mode(false);
    let normal = block_on(manager.run_pass()).unwrap_or_else(|e| panic!("{e}"));

    // then: I29, nothing decided while safe; the request once normal
    assert!(safe.actions.is_empty(), "{:?}", safe.actions);
    assert!(matches!(
        normal.accepted.as_slice(),
        [PlacementAction::Request { .. }]
    ));
}

#[test]
fn should_leave_the_breaker_reset_when_an_operator_resets_it_after_the_expiry() {
    // given: the pin's runner version expired and the manager tripped the breaker
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let mut world = world(&store);
    let manager = PlacementManager::new(
        &store,
        &journal,
        PlacementConfig::new(EPOCH),
        mayfly_control::delivery::Kicks::new(8),
        PlacementIndex::default(),
    );
    let _ = world.command::<Image>(
        image_instance(&ImageDigest::sample()),
        Actor::Startup,
        ImageCommand::RecordRunnerVersionExpired {
            minimum: RunnerVersion::new(2, 331, 0),
        },
    );
    let _ = block_on(manager.run_pass()).unwrap_or_else(|e| panic!("{e}"));

    // when: an operator resets it, and the manager passes again over the same expiry
    world
        .command::<Pool>(
            pool().as_str(),
            Actor::Operator { uid: 501 },
            PoolCommand::ResetBreaker {
                digest: ImageDigest::sample(),
            },
        )
        .unwrap_or_else(|e| panic!("{e}"));
    let _ = block_on(manager.run_pass()).unwrap_or_else(|e| panic!("{e}"));

    // then: the trip is answered by its receipt; the operator's reset stands
    let state = world.pool(&pool());
    assert!(
        state
            .breakers
            .get(&ImageDigest::sample())
            .is_none_or(|b| b.tripped_at.is_none())
    );
}
