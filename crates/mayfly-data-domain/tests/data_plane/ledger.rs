//! The resource ledger's rules: what an applied effect records, what an inventory rebuilds
//! (I25), what a capacity report says (ADR 0024), and what it does not know (ADR 0052).

use std::collections::{BTreeMap, BTreeSet};

use mayfly_data_domain::{BaseUse, Inventory, Ledger, Receipt, Settles, Stimulus, VmUse};
use mayfly_domain::{DirectiveId, Epoch, Millis};
use mayfly_protocol::{
    CapacityConstraint, DirectiveKind, GuestOs, InventoryGap, Mode, ObservationKind, RefusalReason,
    ResourceDimension, UnknownCapacity, UnknownCause, UnknownReason, VmObservation,
};

use super::support::{
    EPOCH, GIB, V1_1, base, capacity, digest, directive, ledger, open, open_at, proof,
    receive_through, roomy, run_options, unlisted, vm,
};

/// A listed VM the provider does not describe.
fn listed(name: String, running: bool) -> VmObservation {
    VmObservation {
        name,
        instance_id: None,
        running,
        guest: None,
        memory_bytes: None,
        disk_bytes: None,
    }
}

/// A listed VM the provider describes completely.
fn described(name: String, running: bool, guest: GuestOs, memory_gib: u64) -> VmObservation {
    VmObservation {
        guest: Some(guest),
        memory_bytes: Some(memory_gib * GIB),
        disk_bytes: Some(80 * GIB),
        ..listed(name, running)
    }
}

/// A stopped VM the ledger knows completely.
fn known(guest: GuestOs) -> VmUse {
    VmUse {
        guest: Some(guest),
        running: false,
        memory_bytes: None,
        disk_bytes: Some(80 * GIB),
    }
}

fn start(n: u128) -> DirectiveKind {
    DirectiveKind::StartVm {
        vm_name: vm(n),
        run_options: run_options(),
    }
}

fn refused(reason: RefusalReason) -> ObservationKind {
    ObservationKind::DirectiveRefused { reason }
}

fn reply_kind(receipt: &Receipt) -> ObservationKind {
    match receipt {
        Receipt::Reply(reply) => reply.kind.clone(),
        other => panic!("expected a reply, got {other:?}"),
    }
}

/// Clones attempt 1's VM and leaves its outcome `Unknown`: a held reservation of one slot.
fn with_unknown_clone() -> mayfly_data_domain::DataPlane {
    let mut dp = open();
    let d = directive(
        1,
        EPOCH,
        DirectiveKind::CloneVm {
            base_name: base(),
            vm_name: vm(1),
        },
    );
    receive_through(&mut dp, &d);
    dp.finish(
        EPOCH,
        d.directive_id,
        Stimulus::Ambiguous(UnknownReason::Deadline),
    );
    dp
}

#[test]
fn should_release_nothing_when_an_inventory_is_incomplete() {
    // given
    let mut dp = with_unknown_clone();
    dp.ledger.commit(
        (EPOCH, DirectiveId::from_u128(2)),
        &DirectiveKind::CloneVm {
            base_name: base(),
            vm_name: vm(2),
        },
    );
    let before = dp.ledger.clone();

    // when
    dp.ledger.apply_inventory(&Inventory::Incomplete {
        reason: InventoryGap::Unparseable,
    });

    // then
    assert_eq!(
        dp.ledger, before,
        "an incomplete inventory is never read as absence (I25)"
    );
    assert_eq!(dp.ledger.used().vm_slots, 2);
}

#[test]
fn should_settle_held_vm_reservations_and_rebuild_vms_when_an_inventory_is_complete() {
    // given
    let mut dp = with_unknown_clone();
    let held = dp.ledger.reservations.len();

    // when
    dp.ledger.apply_inventory(&Inventory::Complete {
        vms: vec![
            listed(vm(1).to_string(), true),
            listed("someone-elses-vm".to_owned(), true),
            listed("mf-not-ours".to_owned(), false),
        ],
    });

    // then
    assert_eq!((held, dp.ledger.reservations.len()), (1, 0));
    assert_eq!(dp.ledger.vms.keys().collect::<Vec<_>>(), [&vm(1)]);
    let adopted = dp.ledger.vms[&vm(1)];
    assert_eq!(
        adopted,
        VmUse {
            guest: None,
            running: true,
            memory_bytes: None,
            disk_bytes: None,
        },
        "what the listing does not say is not known, never assumed (ADR 0052)"
    );
    dp.ledger.apply_inventory(&Inventory::Complete {
        vms: vec![listed(vm(1).to_string(), false)],
    });
    assert!(!dp.ledger.vms[&vm(1)].running);
    assert_eq!(
        dp.ledger.unknown().keys().collect::<Vec<_>>(),
        [&ResourceDimension::Disk],
        "stopped, it takes no memory or guest slot; its disk is still not known"
    );
}

#[test]
fn should_record_what_each_effect_did_when_it_is_applied() {
    // given
    let mut ledger = ledger(roomy());
    ledger.bases.insert(
        base(),
        BaseUse {
            guest: Some(GuestOs::Linux),
            disk_bytes: 60 * GIB,
        },
    );
    let key = |n: u128| (EPOCH, DirectiveId::from_u128(n));
    let kinds = [
        DirectiveKind::PullImage { digest: digest() },
        DirectiveKind::CloneVm {
            base_name: base(),
            vm_name: vm(1),
        },
        DirectiveKind::StartVm {
            vm_name: vm(1),
            run_options: run_options(),
        },
    ];

    // when
    for (n, kind) in (1..).zip(&kinds) {
        ledger.commit(key(n), kind);
    }
    let started = ledger.vms[&vm(1)];
    ledger.commit(
        key(4),
        &DirectiveKind::StopVm {
            grace: Millis::new(1),
            proof: proof(1),
        },
    );
    let stopped = ledger.vms[&vm(1)];
    let stopped_use = ledger.used();
    ledger.commit(key(5), &DirectiveKind::EvictImage { base_name: base() });
    ledger.commit(key(6), &DirectiveKind::DeleteVm { proof: proof(1) });

    // then
    assert_eq!(ledger.images.get(&digest()), Some(&(50 * GIB)));
    assert!(started.running && started.guest == Some(GuestOs::Linux));
    assert_eq!(started.memory_bytes, Some(8 * GIB));
    assert!(!stopped.running);
    assert_eq!(
        stopped_use,
        capacity(1, 0, 0, 190),
        "a stopped VM keeps its slot and disk and frees its memory"
    );
    assert!(ledger.bases.is_empty() && ledger.vms.is_empty());
    assert_eq!(ledger.used(), capacity(0, 0, 0, 50));
}

#[test]
fn should_refuse_capacity_unknown_when_a_start_targets_a_vm_of_unknown_guest() {
    // given: vm(1) cloned from a base whose guest the image store has not named yet, vm(3) not
    // known to the ledger at all, vm(2) a known Linux clone
    let mut ledger = ledger(roomy());
    ledger.bases.insert(
        base(),
        BaseUse {
            guest: None,
            disk_bytes: 60 * GIB,
        },
    );
    ledger.commit(
        (EPOCH, DirectiveId::from_u128(1)),
        &DirectiveKind::CloneVm {
            base_name: base(),
            vm_name: vm(1),
        },
    );
    ledger.vms.insert(vm(2), known(GuestOs::Linux));
    let mut dp = open_at(V1_1, Mode::Normal, ledger);

    // when
    let demands = [1, 3, 2].map(|n| dp.ledger.demand(&start(n)));
    let of_unknown_guest = receive_through(&mut dp, &directive(1, EPOCH, start(1)));
    let of_unknown_vm = receive_through(&mut dp, &directive(3, EPOCH, start(3)));
    let of_linux = receive_through(&mut dp, &directive(2, EPOCH, start(2)));

    // then
    assert_eq!(dp.ledger.vms[&vm(1)].guest, None, "never assumed macOS");
    assert_eq!(
        demands,
        [
            Err(ResourceDimension::MacosGuests),
            Err(ResourceDimension::MacosGuests),
            Ok(capacity(0, 0, 8, 0)),
        ]
    );
    let unknown = refused(RefusalReason::CapacityUnknown {
        dimension: ResourceDimension::MacosGuests,
    });
    assert_eq!(reply_kind(&of_unknown_guest), unknown);
    assert_eq!(reply_kind(&of_unknown_vm), unknown);
    assert_eq!(of_linux, Receipt::Execute, "a known guest starts");
}

#[test]
fn should_refuse_capacity_unknown_and_never_undercount_when_a_running_vm_has_unknown_memory() {
    // given: room for one more 8 GiB VM by what is known; vm(9) runs, adopted, its memory not
    // described; vm(2) is a known stopped Linux clone
    let mut probe = roomy();
    probe.totals = capacity(4, 2, 12, 1000);
    let mut ledger = ledger(probe);
    ledger.vms.insert(vm(2), known(GuestOs::Linux));
    ledger.apply_inventory(&Inventory::Complete {
        vms: vec![
            VmObservation {
                memory_bytes: None,
                ..described(vm(9).to_string(), true, GuestOs::Linux, 0)
            },
            listed(vm(2).to_string(), false),
        ],
    });
    let mut dp = open_at(V1_1, Mode::Normal, ledger);
    let clone = DirectiveKind::CloneVm {
        base_name: base(),
        vm_name: vm(3),
    };

    // when
    let used = dp.ledger.used();
    let report = dp.ledger.report();
    let started = receive_through(&mut dp, &directive(1, EPOCH, start(2)));
    let cloned = receive_through(&mut dp, &directive(2, EPOCH, clone));

    // then
    assert_eq!(
        used.resources.memory_bytes, 0,
        "only what is known is summed: a lower bound"
    );
    assert_eq!(
        report.free.resources.memory_bytes, 0,
        "never reported free: the lower bound would undercount"
    );
    assert_eq!(
        reply_kind(&started),
        refused(RefusalReason::CapacityUnknown {
            dimension: ResourceDimension::Memory,
        })
    );
    assert_eq!(
        cloned,
        Receipt::Execute,
        "a clone needs no memory: it is not held back by it"
    );
}

#[test]
fn should_return_capacity_when_a_complete_listing_describes_every_vm() {
    // given: vm(9) runs, adopted after a restart, nothing described
    let mut ledger = ledger(roomy());
    ledger.apply_inventory(&Inventory::Complete {
        vms: vec![listed(vm(9).to_string(), true)],
    });
    let before = ledger.unknown();

    // when
    ledger.apply_inventory(&Inventory::Incomplete {
        reason: InventoryGap::Deadline,
    });
    let after_incomplete = ledger.unknown();
    ledger.apply_inventory(&Inventory::Complete {
        vms: vec![described(vm(9).to_string(), true, GuestOs::Macos, 16)],
    });

    // then
    let cause = UnknownCause::VmFactUnknown {
        vm_name: vm(9).to_string(),
    };
    assert_eq!(
        before,
        BTreeMap::from([
            (
                ResourceDimension::MacosGuests,
                BTreeSet::from([cause.clone()])
            ),
            (ResourceDimension::Memory, BTreeSet::from([cause.clone()])),
            (ResourceDimension::Disk, BTreeSet::from([cause])),
        ])
    );
    assert_eq!(
        after_incomplete, before,
        "an incomplete listing describes nothing"
    );
    assert!(ledger.unknown().is_empty());
    assert_eq!(ledger.used(), capacity(1, 1, 16, 80));
    assert_eq!(
        ledger.admits(&start(1)),
        Err(RefusalReason::CapacityUnknown {
            dimension: ResourceDimension::MacosGuests,
        }),
        "a VM the ledger does not know has no guest to count"
    );
    ledger.vms.insert(vm(1), known(GuestOs::Macos));
    assert_eq!(ledger.admits(&start(1)), Ok(capacity(0, 1, 8, 0)));
}

#[test]
fn should_refuse_capacity_unknown_for_every_fit_when_nothing_has_been_listed_since_the_start() {
    // given
    let mut ledger = unlisted(roomy());
    let kinds: Vec<DirectiveKind> = super::support::every_kind()
        .into_iter()
        .filter(DirectiveKind::needs_resource_fit)
        .collect();

    // when
    let before: Vec<_> = kinds.iter().map(|k| ledger.admits(k)).collect();
    ledger.apply_inventory(&Inventory::Complete { vms: Vec::new() });
    let without_images: Vec<_> = kinds.iter().map(|k| ledger.admits(k)).collect();
    ledger.apply_images(&BTreeSet::new(), &BTreeMap::new());

    // then
    let unknown = |dimension| Err(RefusalReason::CapacityUnknown { dimension });
    assert_eq!(
        before,
        [
            unknown(ResourceDimension::Disk),
            unknown(ResourceDimension::Disk),
            unknown(ResourceDimension::VmSlots),
            unknown(ResourceDimension::MacosGuests),
        ],
        "our VMs are not known before the first complete inventory"
    );
    assert_eq!(
        without_images[..3],
        [
            unknown(ResourceDimension::Disk),
            unknown(ResourceDimension::Disk),
            unknown(ResourceDimension::Disk),
        ],
        "images and bases are not known before the first image listing"
    );
    assert!(ledger.unknown().is_empty());
}

#[test]
fn should_report_unknown_dimensions_with_free_zero_and_the_vms_to_describe_when_facts_are_missing()
{
    // given
    let mut probe = roomy();
    probe.foreign_unsized = BTreeSet::from([ResourceDimension::Memory]);
    let mut ledger = ledger(probe);
    ledger.apply_inventory(&Inventory::Complete {
        vms: vec![
            listed("mf-not-ours".to_owned(), false),
            described(vm(1).to_string(), true, GuestOs::Linux, 8),
        ],
    });

    // when
    let report = ledger.report();

    // then
    assert_eq!(
        report.unknown,
        vec![
            UnknownCapacity {
                dimension: ResourceDimension::Memory,
                causes: BTreeSet::from([UnknownCause::ForeignVmUnsized]),
            },
            UnknownCapacity {
                dimension: ResourceDimension::Disk,
                causes: BTreeSet::from([UnknownCause::VmFactUnknown {
                    vm_name: "mf-not-ours".to_owned(),
                }]),
            },
        ]
    );
    assert_eq!(
        report.free,
        capacity(2, 2, 0, 0),
        "nothing free where it is not known"
    );
    assert_eq!(report.used, capacity(2, 0, 8, 80), "the known part only");
}

#[test]
fn should_settle_held_image_reservations_when_the_image_store_is_listed() {
    // given
    let mut ledger = ledger(roomy());
    let key = (Epoch::new(5), DirectiveId::from_u128(1));
    ledger
        .reserve(key, &DirectiveKind::PullImage { digest: digest() })
        .expect("room for an image");
    ledger.hold_all();
    ledger.hold((EPOCH, DirectiveId::from_u128(99)));

    // when
    ledger.apply_images(
        &BTreeSet::from([digest()]),
        &BTreeMap::from([(base(), GuestOs::Linux)]),
    );

    // then
    assert!(ledger.reservations.is_empty());
    assert_eq!(ledger.used(), capacity(0, 0, 0, 110));
    assert_eq!(ledger.bases[&base()].guest, Some(GuestOs::Linux));
    assert_eq!(
        ledger
            .reservations
            .values()
            .filter(|r| r.settles == Settles::Images)
            .count(),
        0
    );
}

#[test]
fn should_report_totals_used_free_and_foreign_when_capacity_is_reported() {
    // given
    let mut probe = roomy();
    probe.reserve = capacity(0, 0, 4, 0);
    probe.foreign = capacity(1, 1, 8, 100);
    probe.disk_low_water = true;
    let mut ledger: Ledger = ledger(probe);
    ledger.vms.insert(vm(1), known(GuestOs::Macos));
    ledger.commit(
        (EPOCH, DirectiveId::from_u128(1)),
        &DirectiveKind::StartVm {
            vm_name: vm(1),
            run_options: run_options(),
        },
    );

    // when
    let first = ledger.report();
    let second = ledger.report();

    // then
    assert_eq!((first.report_id, second.report_id), (1, 2));
    assert_eq!(first.totals, capacity(4, 2, 60, 1000));
    assert_eq!(first.used, capacity(2, 2, 16, 180));
    assert_eq!(first.free, capacity(2, 0, 44, 820));
    assert_eq!(first.foreign, capacity(1, 1, 8, 100));
    assert_eq!(
        first.reasons,
        BTreeSet::from([
            CapacityConstraint::DiskLowWater,
            CapacityConstraint::ForeignGuests,
            CapacityConstraint::OperatorReserve,
            CapacityConstraint::HypervisorLimit,
        ])
    );
    assert!(first.unknown.is_empty());
}

#[test]
fn should_count_a_start_at_the_configured_sizing_when_its_run_options_ask_for_less() {
    // given: room for exactly one VM at the configured 8 GiB, and a request for none
    let mut probe = roomy();
    probe.totals = capacity(4, 2, 12, 1000);
    let mut ledger = ledger(probe);
    ledger.vms.insert(vm(1), known(GuestOs::Macos));
    ledger.vms.insert(vm(2), known(GuestOs::Linux));
    let mut dp = super::support::open_with(mayfly_protocol::Mode::Normal, ledger);
    let start = |n: u128, id: u128| {
        directive(
            id,
            EPOCH,
            DirectiveKind::StartVm {
                vm_name: vm(n),
                run_options: mayfly_protocol::RunOptions {
                    resources: mayfly_domain::Resources::new(0, 1),
                },
            },
        )
    };
    let tiny = |n: u128, id: u128| {
        let mut d = start(n, id);
        if let DirectiveKind::StartVm { run_options, .. } = &mut d.kind {
            run_options.resources = mayfly_domain::Resources::new(1, 1);
        }
        d
    };
    let (first, second) = (start(1, 1), tiny(2, 2));

    // when
    let demand = dp.ledger.demand(&first.kind);
    let admitted = receive_through(&mut dp, &first);
    dp.finish(
        EPOCH,
        first.directive_id,
        Stimulus::Applied(mayfly_protocol::ObservationKind::VmBooted),
    );
    let recorded = dp.ledger.vms[&vm(1)];
    let over = receive_through(&mut dp, &second);

    // then
    assert_eq!(
        demand,
        Ok(capacity(0, 1, 8, 0)),
        "the data plane's own sizing"
    );
    assert_eq!(admitted, mayfly_data_domain::Receipt::Execute);
    assert_eq!(
        (recorded.memory_bytes, recorded.disk_bytes),
        (Some(8 * GIB), Some(80 * GIB)),
        "recorded at the sizing, never at what the control plane sent"
    );
    match over {
        mayfly_data_domain::Receipt::Reply(reply) => assert_eq!(
            reply.kind,
            mayfly_protocol::ObservationKind::DirectiveRefused {
                reason: mayfly_protocol::RefusalReason::InsufficientResources {
                    dimension: mayfly_protocol::ResourceDimension::Memory,
                },
            },
            "a one-byte request cannot over-commit memory"
        ),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn should_count_a_start_at_its_run_options_when_they_ask_for_more_than_the_sizing() {
    // given
    let mut ledger = ledger(roomy());
    ledger.vms.insert(vm(1), known(GuestOs::Macos));
    let start = DirectiveKind::StartVm {
        vm_name: vm(1),
        run_options: mayfly_protocol::RunOptions {
            resources: mayfly_domain::Resources::new(16 * GIB, 120 * GIB),
        },
    };

    // when
    let demand = ledger.demand(&start);

    // then
    assert_eq!(
        demand,
        Ok(capacity(0, 1, 16, 40)),
        "the clone's 80 GiB grows to 120"
    );
}

#[test]
fn should_count_a_malformed_mf_vm_when_a_complete_inventory_lists_it() {
    // given
    let mut ledger = ledger(roomy());

    // when
    ledger.apply_inventory(&Inventory::Complete {
        vms: vec![
            described("mf-not-ours".to_owned(), true, GuestOs::Macos, 8),
            described("mf-also-not-ours".to_owned(), false, GuestOs::Linux, 8),
            listed("someone-elses-vm".to_owned(), true),
        ],
    });
    let used = ledger.used();
    let report = ledger.report();
    ledger.apply_inventory(&Inventory::Incomplete {
        reason: InventoryGap::Unparseable,
    });
    let kept = ledger.used();
    ledger.apply_inventory(&Inventory::Complete { vms: Vec::new() });

    // then
    assert!(ledger.vms.is_empty(), "never ours");
    assert_eq!(
        used,
        capacity(2, 1, 8, 160),
        "two slots, one running macOS guest at its listed memory, two clones' disk"
    );
    assert!(report.unknown.is_empty(), "the listing described both");
    assert_eq!(report.foreign, capacity(2, 1, 8, 160));
    assert!(report.reasons.contains(&CapacityConstraint::ForeignGuests));
    assert_eq!(kept, used, "an incomplete inventory releases nothing (I25)");
    assert_eq!(
        ledger.used(),
        capacity(0, 0, 0, 0),
        "gone once a complete listing omits them"
    );
}
