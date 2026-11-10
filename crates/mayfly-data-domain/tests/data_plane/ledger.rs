//! The resource ledger's rules: what an applied effect records, what an inventory rebuilds
//! (I25), what a capacity report says (ADR 0024).

use std::collections::{BTreeMap, BTreeSet};

use mayfly_data_domain::{Inventory, Ledger, Settles, Stimulus};
use mayfly_domain::{DirectiveId, Epoch, Millis};
use mayfly_protocol::{
    CapacityConstraint, DirectiveKind, GuestOs, InventoryGap, UnknownReason, VmObservation,
};

use super::support::{
    EPOCH, GIB, NOW, V1_0, base, capacity, digest, directive, ledger, open, proof, roomy,
    run_options, vm,
};

fn listed(name: String, running: bool) -> VmObservation {
    VmObservation {
        name,
        instance_id: None,
        running,
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
    dp.receive(V1_0, &d, NOW);
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
    assert!(adopted.running && adopted.guest == GuestOs::Macos);
    assert_eq!(
        adopted.memory_bytes,
        8 * GIB,
        "unknown memory is counted the larger way"
    );
    dp.ledger.apply_inventory(&Inventory::Complete {
        vms: vec![listed(vm(1).to_string(), false)],
    });
    assert_eq!(dp.ledger.vms[&vm(1)].memory_bytes, 0);
}

#[test]
fn should_record_what_each_effect_did_when_it_is_applied() {
    // given
    let mut ledger = ledger(roomy());
    ledger.bases.insert(
        base(),
        mayfly_data_domain::BaseUse {
            guest: GuestOs::Linux,
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
    ledger.commit(key(5), &DirectiveKind::EvictImage { base_name: base() });
    ledger.commit(key(6), &DirectiveKind::DeleteVm { proof: proof(1) });

    // then
    assert_eq!(ledger.images.get(&digest()), Some(&(50 * GIB)));
    assert!(started.running && started.guest == GuestOs::Linux);
    assert_eq!(started.memory_bytes, 8 * GIB);
    assert!(!stopped.running && stopped.memory_bytes == 0);
    assert!(ledger.bases.is_empty() && ledger.vms.is_empty());
    assert_eq!(ledger.used(), capacity(0, 0, 0, 50));
}

#[test]
fn should_count_a_start_of_an_unknown_vm_as_a_macos_guest_when_its_guest_is_not_known() {
    // given
    let mut ledger = ledger(roomy());
    let start = DirectiveKind::StartVm {
        vm_name: vm(3),
        run_options: run_options(),
    };

    // when
    let demand = ledger.demand(&start);
    ledger.commit((EPOCH, DirectiveId::from_u128(1)), &start);

    // then
    assert_eq!(demand, capacity(0, 1, 8, 80));
    assert_eq!(ledger.used(), capacity(1, 1, 8, 80));
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
    assert_eq!(ledger.bases[&base()].guest, GuestOs::Linux);
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
    let mut ledger = Ledger::new(super::support::sizing(), probe);
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
}
