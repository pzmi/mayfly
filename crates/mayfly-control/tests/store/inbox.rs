//! The observation inbox (P8): observations recorded as units keyed by data plane and
//! observation, acked only after they commit, idempotent, a refusal consuming no key.

use mayfly_control::inbox::{Inbox, Recorded, fact_of, inbox_source, refusal, reported_capacity};
use mayfly_control::journal::{InboxKey, Journal, MessageId};
use mayfly_control_domain::Actor;
use mayfly_control_domain::attempt::{
    AttemptCommand, AttemptEvent, Fact, GuestOs, Phase, RefusalReason,
};
use mayfly_control_domain::host::{Host, HostCommand, ReserveSlot};
use mayfly_domain::{
    Epoch, EventId, Generation, Millis, ObservationId, PoolName, Resources, Sample,
};
use mayfly_protocol::{
    AttemptRef, Capacity, CapacityReport, DirectiveRef, Observation, ObservationKind,
    ResourceDimension, UnknownCapacity, UnknownCause,
};
use mayfly_testkit::block_on;
use mayfly_testkit::clock::FakeClock;
use mayfly_testkit::contracts::journal::InjectedFault;
use mayfly_testkit::fleet::{self, GIB, PoolSpec, World, report};

use crate::recovery_kit::{
    BEFORE, NOW, attempt, host, instance, inventory, kit, listed, path, vm_name,
};

fn answer(id: u128, n: u128, k: u128, epoch: Epoch, kind: ObservationKind) -> Observation {
    Observation {
        observation_id: ObservationId::from_u128(id),
        epoch,
        answers: Some(DirectiveRef {
            directive_id: path::directive(n, k),
            attempt: Some(AttemptRef {
                attempt_id: attempt(n),
                generation: Generation::FIRST,
            }),
        }),
        observed_at_dp: Millis::new(5),
        kind,
    }
}

fn cloned(id: u128, n: u128) -> Observation {
    answer(
        id,
        n,
        1,
        BEFORE,
        ObservationKind::VmCloned {
            instance_id: instance(),
        },
    )
}

fn key(id: u128) -> InboxKey {
    InboxKey {
        source: inbox_source(&host()).unwrap_or_else(|e| panic!("{e}")),
        message_id: MessageId::new(&format!(
            "{}/{}",
            BEFORE.get(),
            ObservationId::from_u128(id)
        ))
        .unwrap_or_else(|e| panic!("{e}")),
    }
}

fn clones(events: &[AttemptEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, AttemptEvent::VmCloned(_)))
        .count()
}

#[test]
fn should_record_once_when_an_observation_is_delivered_twice() {
    // given
    let kit = kit();
    kit.drive(1, path::cloning(1));
    let inbox = Inbox::new(&kit.store, host(), BEFORE);

    // when
    let first = block_on(inbox.record(&cloned(0x10, 1))).unwrap_or_else(|e| panic!("{e}"));
    let again = block_on(inbox.record(&cloned(0x10, 1))).unwrap_or_else(|e| panic!("{e}"));

    // then
    assert_eq!(first.accepted, 1);
    assert_eq!(
        again.accepted + again.duplicate,
        1,
        "answered, never recorded twice"
    );
    assert_eq!(clones(&kit.events(1)), 1);
    assert_eq!(kit.state(1).phase, Phase::Booting);
    assert!(block_on(kit.memory.inbox_contains(&key(0x10))).unwrap_or_else(|e| panic!("{e}")));
}

#[test]
fn should_record_the_fact_when_an_observation_id_recurs_under_a_new_epoch() {
    // given an answer recorded under epoch 1, then the attempt adopted by a restart (epoch 2)
    // whose data plane numbers its observations afresh
    let kit = kit();
    kit.drive(1, path::booting(1));
    let inbox = Inbox::new(&kit.store, host(), BEFORE);
    let first = answer(
        0x17,
        1,
        1,
        BEFORE,
        ObservationKind::VmCloned {
            instance_id: instance(),
        },
    );
    let _ = block_on(inbox.record(&first)).unwrap_or_else(|e| panic!("{e}"));
    let run = Epoch::new(2);
    let _ = kit.command(
        1,
        run,
        AttemptCommand::Adopt {
            inventory_ref: EventId::from_u128(3),
        },
    );
    let _ = kit.command(1, run, path::issue(1, 2));
    let reused = answer(0x17, 1, 2, run, ObservationKind::VmBooted);

    // when
    let recorded = block_on(Inbox::new(&kit.store, host(), run).record(&reused))
        .unwrap_or_else(|e| panic!("{e}"));

    // then
    assert_eq!(
        recorded.accepted, 1,
        "a new observation, not a duplicate: {recorded:?}"
    );
    assert_eq!(kit.state(1).phase, Phase::Verifying);
}

#[test]
fn should_decide_a_redelivery_afresh_when_its_refusal_consumed_no_key() {
    // given an answer about an attempt whose stream does not exist yet
    let kit = kit();
    let inbox = Inbox::new(&kit.store, host(), BEFORE);
    let refused = block_on(inbox.record(&cloned(0x11, 1))).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        refused,
        Recorded {
            refused: 1,
            ..Recorded::default()
        }
    );
    assert!(
        !block_on(kit.memory.inbox_contains(&key(0x11))).unwrap_or_else(|e| panic!("{e}")),
        "a refusal consumes no inbox key (JC-34)"
    );

    // when the attempt moved and the observation is delivered again
    kit.drive(1, path::cloning(1));
    let again = block_on(inbox.record(&cloned(0x11, 1))).unwrap_or_else(|e| panic!("{e}"));

    // then
    assert_eq!(
        again.accepted, 1,
        "decided afresh, not dropped as a duplicate"
    );
    assert_eq!(kit.state(1).instance_id, Some(instance()));
}

#[test]
fn should_record_a_stale_answer_as_ignored_when_its_epoch_is_older() {
    // given an attempt adopted under epoch 2
    let kit = kit();
    kit.drive(1, path::cloning(1));
    let _ = kit.command(
        1,
        Epoch::new(2),
        AttemptCommand::Adopt {
            inventory_ref: EventId::from_u128(3),
        },
    );
    let inbox = Inbox::new(&kit.store, host(), Epoch::new(2));

    // when an answer observed in epoch 1 arrives
    let recorded = block_on(inbox.record(&cloned(0x12, 1))).unwrap_or_else(|e| panic!("{e}"));

    // then
    assert_eq!(recorded.accepted, 1);
    assert!(
        kit.events(1)
            .iter()
            .any(|e| matches!(e, AttemptEvent::StaleObservationIgnored(_)))
    );
    assert_eq!(kit.state(1).instance_id, None);
}

#[test]
fn should_ack_only_after_the_record_commits_when_the_store_fails() {
    // given
    let kit = kit();
    kit.drive(1, path::cloning(1));
    let inbox = Inbox::new(&kit.store, host(), BEFORE);
    assert!(kit.memory.inject(InjectedFault::StorageBeforeCommit));

    // when
    let failed = block_on(inbox.record(&cloned(0x13, 1)));
    let redelivered = block_on(inbox.record(&cloned(0x13, 1))).unwrap_or_else(|e| panic!("{e}"));

    // then
    assert!(failed.is_err(), "no licence to ack");
    assert_eq!(redelivered.accepted, 1, "the redelivery records it");
    assert_eq!(clones(&kit.events(1)), 1);
}

#[test]
fn should_record_each_attempt_once_when_an_inventory_is_delivered_twice() {
    // given
    let kit = kit();
    kit.drive(1, path::prepared(1));
    kit.drive(2, path::verifying(2));
    let inbox = Inbox::new(&kit.store, host(), BEFORE);
    let listing = inventory(0x14, BEFORE, vec![listed(1, instance(), true)]);

    // when
    let first = block_on(inbox.record(&listing)).unwrap_or_else(|e| panic!("{e}"));
    let before = (kit.events(1).len(), kit.events(2).len());
    let again = block_on(inbox.record(&listing)).unwrap_or_else(|e| panic!("{e}"));

    // then
    assert_eq!(first.accepted, 2);
    assert_eq!(again.accepted + again.duplicate, 2);
    assert_eq!((kit.events(1).len(), kit.events(2).len()), before);
    assert_eq!(kit.state(1).phase, Phase::Prepared);
    assert_eq!(
        kit.state(2).phase,
        Phase::Lost,
        "its recorded VM vanished (DEC-07)"
    );
}

#[test]
fn should_route_a_runner_exit_by_its_vm_name_when_it_names_no_directive() {
    // given
    let kit = kit();
    kit.drive(1, path::busy(1));
    let inbox = Inbox::new(&kit.store, host(), BEFORE);
    let exited = Observation {
        observation_id: ObservationId::from_u128(0x15),
        epoch: BEFORE,
        answers: None,
        observed_at_dp: Millis::new(5),
        kind: ObservationKind::RunnerExited {
            vm_name: vm_name(1),
            code: 0,
        },
    };

    // when
    let recorded = block_on(inbox.record(&exited)).unwrap_or_else(|e| panic!("{e}"));

    // then
    assert_eq!(recorded.accepted, 1);
    assert_eq!(kit.state(1).phase, Phase::Finished);
}

#[test]
fn should_admit_nothing_in_a_dimension_when_the_report_lists_it_unknown() {
    // given a host whose report admits the pool's VM
    let kit = kit();
    let store = fleet::store(&kit.memory, &FakeClock::at(NOW));
    let mut world = World::new(&store);
    world.configure_pool(PoolSpec {
        name: PoolName::sample(),
        os: GuestOs::Linux,
        memory_gib: 8,
        max_running: 4,
        min_prepared: 0,
        breaker: 3,
    });
    world.register_host(host().as_str(), report(64, 0, 4));
    let capacity = |memory_gib: u64| Capacity {
        vm_slots: 4,
        macos_guests: 2,
        resources: Resources::new(memory_gib * GIB, 1024 * GIB),
    };
    let unknown_memory = Observation {
        observation_id: ObservationId::from_u128(0x16),
        epoch: BEFORE,
        answers: None,
        observed_at_dp: Millis::new(5),
        kind: ObservationKind::CapacityReported(CapacityReport {
            report_id: 50,
            totals: capacity(64),
            used: capacity(0),
            free: Capacity {
                resources: Resources::new(0, 1024 * GIB),
                ..capacity(64)
            },
            foreign: Capacity::default(),
            reasons: std::collections::BTreeSet::new(),
            unknown: vec![UnknownCapacity {
                dimension: ResourceDimension::Memory,
                causes: [UnknownCause::InventoryNotRead].into_iter().collect(),
            }],
        }),
    };

    // when
    let recorded = block_on(Inbox::new(&kit.store, host(), BEFORE).record(&unknown_memory))
        .unwrap_or_else(|e| panic!("{e}"));
    let reserved = world.command::<Host>(
        host().as_str(),
        Actor::Automation {
            policy: "reconciler",
        },
        HostCommand::ReserveSlot(ReserveSlot {
            attempt_id: attempt(1),
            pool: PoolName::sample(),
            os: GuestOs::Linux,
            resources: Resources::new(8 * GIB, 50 * GIB),
            terms_revision: kit.terms_revision(),
        }),
    );

    // then
    assert_eq!(recorded.accepted, 1);
    assert_eq!(
        kit.host_state()
            .capacity
            .map(|c| c.capacity.total.memory_bytes),
        Some(0)
    );
    assert_eq!(
        reserved,
        Err("insufficient_memory"),
        "nothing admitted there"
    );
}

fn about(n: u128, kind: ObservationKind) -> Observation {
    answer(0x20, n, 1, BEFORE, kind)
}

#[test]
fn should_map_each_answer_to_the_attempts_fact_when_it_names_its_directive() {
    // given
    let d = path::directive(1, 1);
    let facts = mayfly_protocol::GuestFacts {
        os: mayfly_protocol::GuestOs::Macos,
        uptime: Millis::new(9),
    };
    let cases = vec![
        (
            ObservationKind::VmBooted,
            Fact::VmBooted { directive_id: d },
        ),
        (
            ObservationKind::GuestVerified { facts },
            Fact::GuestVerified {
                directive_id: d,
                facts: mayfly_control_domain::attempt::GuestFacts {
                    os: GuestOs::Macos,
                    uptime: Millis::new(9),
                },
            },
        ),
        (
            ObservationKind::GuestRejected {
                reason: mayfly_protocol::GuestRejection::AgentMissing,
            },
            Fact::GuestRejected {
                directive_id: d,
                reason: mayfly_control_domain::attempt::GuestRejection::AgentMissing,
            },
        ),
        (
            ObservationKind::RunnerLaunched,
            Fact::RunnerLaunched { directive_id: d },
        ),
        (
            ObservationKind::RunnerLaunchUnknown,
            Fact::RunnerLaunchUnknown { directive_id: d },
        ),
        (
            ObservationKind::DiagnosticsExported {
                bytes: 3,
                sha256: [1; 32],
            },
            Fact::DiagnosticsExported {
                directive_id: d,
                bytes: 3,
                sha256: [1; 32],
            },
        ),
        (
            ObservationKind::DiagnosticsLost {
                reason: mayfly_protocol::DiagnosticsLoss::OverLimit,
            },
            Fact::DiagnosticsLost {
                directive_id: d,
                reason: mayfly_control_domain::attempt::DiagnosticsLoss::OverLimit,
            },
        ),
        (
            ObservationKind::VmStopped,
            Fact::VmStopped { directive_id: d },
        ),
        (
            ObservationKind::VmDeleted,
            Fact::VmDeleted { directive_id: d },
        ),
        (
            ObservationKind::OutcomeUnknown {
                reason: mayfly_protocol::UnknownReason::ProcessLost,
            },
            Fact::OutcomeUnknown { directive_id: d },
        ),
        (
            ObservationKind::DirectiveRefused {
                reason: mayfly_protocol::RefusalReason::SafeMode,
            },
            Fact::DirectiveRefused {
                directive_id: d,
                reason: RefusalReason::SafeMode,
            },
        ),
        (
            ObservationKind::VmObservedStopped {
                vm_name: vm_name(1),
            },
            Fact::VmObservedStopped,
        ),
    ];

    // when
    let mapped: Vec<Option<Fact>> = cases
        .iter()
        .map(|(k, _)| fact_of(&about(1, k.clone())))
        .collect();

    // then
    let expected: Vec<Option<Fact>> = cases.into_iter().map(|(_, f)| Some(f)).collect();
    assert_eq!(mapped, expected);
}

#[test]
fn should_map_every_other_guest_value_when_it_crosses_the_link() {
    // given
    let d = path::directive(1, 1);
    let rejections = [
        mayfly_protocol::GuestRejection::ChallengeFailed,
        mayfly_protocol::GuestRejection::FactsMismatch,
    ];
    let losses = [
        mayfly_protocol::DiagnosticsLoss::Refused,
        mayfly_protocol::DiagnosticsLoss::GuestUnreachable,
    ];

    // when
    let linux = fact_of(&about(
        1,
        ObservationKind::GuestVerified {
            facts: mayfly_protocol::GuestFacts {
                os: mayfly_protocol::GuestOs::Linux,
                uptime: Millis::new(1),
            },
        },
    ));
    let rejected: Vec<_> = rejections
        .into_iter()
        .map(|reason| fact_of(&about(1, ObservationKind::GuestRejected { reason })))
        .collect();
    let lost: Vec<_> = losses
        .into_iter()
        .map(|reason| fact_of(&about(1, ObservationKind::DiagnosticsLost { reason })))
        .collect();

    // then
    assert!(matches!(
        linux,
        Some(Fact::GuestVerified { facts, .. }) if facts.os == GuestOs::Linux
    ));
    assert_eq!(
        rejected,
        vec![
            Some(Fact::GuestRejected {
                directive_id: d,
                reason: mayfly_control_domain::attempt::GuestRejection::ChallengeFailed,
            }),
            Some(Fact::GuestRejected {
                directive_id: d,
                reason: mayfly_control_domain::attempt::GuestRejection::FactsMismatch,
            }),
        ]
    );
    assert_eq!(
        lost,
        vec![
            Some(Fact::DiagnosticsLost {
                directive_id: d,
                reason: mayfly_control_domain::attempt::DiagnosticsLoss::Refused,
            }),
            Some(Fact::DiagnosticsLost {
                directive_id: d,
                reason: mayfly_control_domain::attempt::DiagnosticsLoss::GuestUnreachable,
            }),
        ]
    );
}

#[test]
fn should_read_every_refusal_as_a_definite_no_when_it_is_mapped() {
    // given
    use mayfly_protocol::RefusalReason as Wire;
    let dimension = ResourceDimension::Disk;

    // when
    let mapped: Vec<RefusalReason> = [
        Wire::InsufficientResources { dimension },
        Wire::CapacityUnknown { dimension },
        Wire::ProofMismatch,
        Wire::ProtectedTarget,
        Wire::TargetAbsent,
        Wire::SafeMode,
        Wire::DeadlineExpired,
        Wire::EffectFailed,
    ]
    .into_iter()
    .map(refusal)
    .collect();

    // then
    assert_eq!(
        mapped,
        vec![
            RefusalReason::InsufficientResources,
            RefusalReason::InsufficientResources,
            RefusalReason::ProofMismatch,
            RefusalReason::ProofMismatch,
            RefusalReason::TargetAbsent,
            RefusalReason::SafeMode,
            RefusalReason::EffectFailed,
            RefusalReason::EffectFailed,
        ]
    );
}

#[test]
fn should_deliver_nothing_in_each_dimension_when_the_report_lists_it_unknown() {
    // given
    let capacity = Capacity {
        vm_slots: 4,
        macos_guests: 2,
        resources: Resources::new(64 * GIB, 1024 * GIB),
    };
    let unknown = [
        ResourceDimension::Memory,
        ResourceDimension::Disk,
        ResourceDimension::VmSlots,
        ResourceDimension::MacosGuests,
    ]
    .into_iter()
    .map(|dimension| UnknownCapacity {
        dimension,
        causes: [UnknownCause::InventoryNotRead].into_iter().collect(),
    })
    .collect();

    // when
    let reported = reported_capacity(&CapacityReport {
        report_id: 1,
        totals: capacity,
        used: Capacity::default(),
        free: Capacity::default(),
        foreign: Capacity::default(),
        reasons: std::collections::BTreeSet::new(),
        unknown,
    });

    // then
    assert_eq!(reported.total, Resources::new(0, 0));
    assert_eq!((reported.vm_slots, reported.macos_guests), (0, 0));
}

#[test]
fn should_ack_without_recording_when_the_kind_is_not_an_attempts() {
    // given
    let kit = kit();
    let inbox = Inbox::new(&kit.store, host(), BEFORE);
    let plain = |id: u128, kind: ObservationKind| Observation {
        observation_id: ObservationId::from_u128(id),
        epoch: BEFORE,
        answers: None,
        observed_at_dp: Millis::new(5),
        kind,
    };
    let base = mayfly_protocol::BaseName::new("mfb-base").unwrap_or_else(|e| panic!("{e}"));
    let observations = [
        plain(
            0x30,
            ObservationKind::InventoryIncomplete {
                reason: mayfly_protocol::InventoryGap::Deadline,
            },
        ),
        plain(
            0x31,
            ObservationKind::HostFacts(mayfly_protocol::HostFacts {
                disk_free_bytes: 1,
                disk_low_water: false,
                woke: false,
                tart_version: "2.0.0".to_owned(),
                external_guests: 0,
            }),
        ),
        plain(0x32, ObservationKind::HypervisorLimitHit),
        plain(
            0x33,
            ObservationKind::ImagePulled {
                digest: mayfly_domain::ImageDigest::sample(),
            },
        ),
        plain(
            0x34,
            ObservationKind::ImageMaterialised {
                digest: mayfly_domain::ImageDigest::sample(),
                base_name: base.clone(),
            },
        ),
        plain(0x35, ObservationKind::ImageEvicted { base_name: base }),
        plain(0x36, ObservationKind::VmBooted),
        plain(
            0x37,
            ObservationKind::RunnerExited {
                vm_name: vm_name(7),
                code: 1,
            },
        ),
    ];

    // when
    let routed: Vec<Option<&'static str>> = observations
        .iter()
        .map(|o| {
            block_on(inbox.record(o))
                .unwrap_or_else(|e| panic!("{e}"))
                .not_routed
        })
        .collect();

    // then
    assert_eq!(
        routed,
        vec![
            Some("inventory_incomplete"),
            Some("host_facts"),
            Some("hypervisor_limit_hit"),
            Some("image_pulled"),
            Some("image_materialised"),
            Some("image_evicted"),
            Some("no_attempt"),
            Some("unknown_vm"),
        ]
    );
    assert!(format!("{inbox:?}").contains("Inbox"));
}
