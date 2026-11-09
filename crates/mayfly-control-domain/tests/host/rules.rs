//! The host's admission rules, one test per rule of `doc/domain-model.md` §6.3 and per
//! `Accept:` clause of KER-05.

use mayfly_control_domain::attempt::GuestOs;
use mayfly_control_domain::capacity::ReportedCapacity;
use mayfly_control_domain::host::{
    Drain, HostCommand, HostEvent, LeasePhase, ReleaseEvidence, ResourceThreshold,
};
use mayfly_domain::Resources;

use super::support::{
    CAUSES, GIB, RECONCILER, apply, at, auto, drain, grant, open_host, outcome, quarantine, reach,
    record, register, release, report, reserve, reserve_in, terms, with,
};

#[test]
fn should_never_allocate_beyond_the_newer_report_when_a_stale_one_arrives() {
    // given: report 2 leaves 12 GiB (16 minus the 4 GiB reserve); report 1 would leave 60
    let tight = ReportedCapacity {
        total: Resources::new(16 * GIB, 200 * GIB),
        ..report()
    };
    let roomy = ReportedCapacity {
        total: Resources::new(64 * GIB, 200 * GIB),
        ..report()
    };
    let state = reach([register(), record(2, tight), terms("pool0", 1, true, 5)]);

    // when: the older report arrives late
    let (state, events) = apply(&state, auto(record(1, roomy))).unwrap_or_else(|r| panic!("{r:?}"));

    // then: it is ignored, and admission stays within report 2
    assert!(events.is_empty());
    assert_eq!(state.capacity.map(|c| c.report_id), Some(2));
    let (state, _) =
        apply(&state, auto(reserve(1, GuestOs::Linux, 8))).unwrap_or_else(|r| panic!("{r:?}"));
    assert_eq!(
        outcome(&state, auto(reserve(2, GuestOs::Linux, 8))),
        "insufficient_memory"
    );
    assert_eq!(
        outcome(&state, auto(reserve(2, GuestOs::Linux, 4))),
        "accepted"
    );
}

#[test]
fn should_admit_two_macos_guests_at_most_when_foreign_guests_run() {
    // given: the report claims three macOS slots; G3 allows two whatever it says
    let generous = ReportedCapacity {
        macos_guests: 3,
        vm_slots: 8,
        total: Resources::new(128 * GIB, 400 * GIB),
        ..report()
    };
    let state = reach([register(), record(1, generous), terms("pool0", 1, true, 8)]);

    // when
    let two = reach(with(
        vec![register(), record(1, generous), terms("pool0", 1, true, 8)],
        [reserve(1, GuestOs::Macos, 8), reserve(2, GuestOs::Macos, 8)],
    ));

    // then
    assert_eq!(
        outcome(&state, auto(reserve(1, GuestOs::Macos, 8))),
        "accepted"
    );
    assert_eq!(
        outcome(&two, auto(reserve(3, GuestOs::Macos, 8))),
        "macos_guests"
    );
    assert_eq!(
        outcome(&two, auto(reserve(3, GuestOs::Linux, 8))),
        "accepted"
    );
    // and one foreign guest leaves room for one of ours
    let foreign = reach([
        register(),
        record(
            1,
            ReportedCapacity {
                foreign_macos_guests: 1,
                foreign_vms: 1,
                ..generous
            },
        ),
        terms("pool0", 1, true, 8),
        reserve(1, GuestOs::Macos, 8),
    ]);
    assert_eq!(
        outcome(&foreign, auto(reserve(2, GuestOs::Macos, 8))),
        "macos_guests"
    );
}

#[test]
fn should_admit_nothing_when_the_host_drains() {
    // given
    let state = reach(with(open_host(), [reserve(1, GuestOs::Linux, 8), drain()]));

    // when
    let reserve_while_draining = outcome(&state, auto(reserve(2, GuestOs::Linux, 8)));
    let grant_while_draining = outcome(&state, auto(grant(1)));
    let complete_with_a_lease = outcome(&state, auto(HostCommand::CompleteHostDrain));

    // then
    assert_eq!(reserve_while_draining, "draining");
    assert_eq!(grant_while_draining, "draining");
    assert_eq!(complete_with_a_lease, "leases_remaining");
    let (state, _) = apply(&state, auto(release(1, ReleaseEvidence::VmDeleted)))
        .unwrap_or_else(|r| panic!("{r:?}"));
    let (drained, _) =
        apply(&state, auto(HostCommand::CompleteHostDrain)).unwrap_or_else(|r| panic!("{r:?}"));
    assert_eq!(drained.gate.drain, Drain::Drained);
    assert_eq!(
        outcome(&drained, auto(reserve(2, GuestOs::Linux, 8))),
        "draining"
    );
    let (reopened, _) =
        apply(&drained, auto(HostCommand::CancelHostDrain)).unwrap_or_else(|r| panic!("{r:?}"));
    assert_eq!(
        outcome(&reopened, auto(reserve(2, GuestOs::Linux, 8))),
        "accepted"
    );
}

#[test]
fn should_refuse_a_grant_when_any_term_or_cause_is_closed() {
    // given: a reserved lease, then each closing condition on its own
    let lease = with(open_host(), [reserve(1, GuestOs::Linux, 8)]);
    let mut table: Vec<(String, Vec<HostCommand>, &str)> = vec![
        ("open".to_owned(), lease.clone(), "accepted"),
        (
            "disk_low".to_owned(),
            with(
                lease.clone(),
                [HostCommand::RecordResourceThreshold {
                    threshold: ResourceThreshold::DiskLow,
                }],
            ),
            "admission_closed",
        ),
        (
            "drain".to_owned(),
            with(lease.clone(), [drain()]),
            "draining",
        ),
        (
            "terms_closed".to_owned(),
            with(lease.clone(), [terms("pool0", 2, false, 2)]),
            "terms_closed",
        ),
    ];
    for cause in CAUSES {
        table.push((
            format!("{cause:?}"),
            with(lease.clone(), [HostCommand::CloseAdmission { cause }]),
            "admission_closed",
        ));
    }

    // when
    let outcomes: Vec<(String, String)> = table
        .iter()
        .map(|(name, path, _)| (name.clone(), outcome(&reach(path.clone()), auto(grant(1)))))
        .collect();

    // then
    for ((name, _, expected), (_, actual)) in table.iter().zip(&outcomes) {
        assert_eq!(actual, expected, "grant with {name}");
    }
}

#[test]
fn should_hold_capacity_when_a_lease_is_quarantined() {
    // given: two VM slots, two pools; pool0 holds one quarantined, running lease
    let two_slots = ReportedCapacity {
        vm_slots: 2,
        ..report()
    };
    let state = reach([
        register(),
        record(1, two_slots),
        terms("pool0", 1, true, 1),
        terms("pool1", 1, true, 4),
        reserve(1, GuestOs::Linux, 12),
        quarantine(1, true),
    ]);
    let before = state.allocated();

    // when: its VM is stopped
    let (stopped, events) =
        apply(&state, auto(quarantine(1, false))).unwrap_or_else(|r| panic!("{r:?}"));

    // then: memory is freed, disk and the slot are not, and the lease is never reused
    assert_eq!(events.len(), 1);
    assert_eq!(stopped.allocated().resources.memory_bytes, 0);
    assert_eq!(
        stopped.allocated().resources.disk_bytes,
        before.resources.disk_bytes
    );
    assert_eq!(stopped.allocated().vms, 1);
    assert_eq!(
        outcome(&stopped, auto(reserve(2, GuestOs::Linux, 8))),
        "quarantine_held"
    );
    let (other, _) = apply(&stopped, auto(reserve_in("pool1", 2, GuestOs::Linux, 8)))
        .unwrap_or_else(|r| panic!("{r:?}"));
    assert_eq!(
        outcome(&other, auto(reserve_in("pool1", 3, GuestOs::Linux, 8))),
        "no_free_slot"
    );
    // and only the VM's observed absence releases it
    assert_eq!(
        outcome(
            &stopped,
            auto(release(1, ReleaseEvidence::AttemptNeverStarted))
        ),
        "quarantined"
    );
    assert_eq!(outcome(&stopped, auto(grant(1))), "quarantined");
    assert_eq!(
        outcome(&stopped, auto(release(1, ReleaseEvidence::AttemptLost))),
        "accepted"
    );
}

#[test]
fn should_reserve_the_next_generation_when_a_released_slot_is_reused() {
    // given
    let state = reach(with(
        open_host(),
        [
            reserve(1, GuestOs::Linux, 8),
            release(1, ReleaseEvidence::VmDeleted),
        ],
    ));

    // when
    let (state, events) =
        apply(&state, auto(reserve(2, GuestOs::Linux, 8))).unwrap_or_else(|r| panic!("{r:?}"));

    // then
    let reserved: Vec<(u32, u32)> = events
        .iter()
        .filter_map(|e| match e {
            HostEvent::SlotReserved(r) => Some((r.slot.get(), r.generation.get())),
            _ => None,
        })
        .collect();
    assert_eq!(reserved, [(0, 2)]);
    assert!(
        state
            .leases
            .values()
            .all(|l| l.phase == LeasePhase::Reserved)
    );
}

#[test]
fn should_refuse_an_older_run_epoch_when_a_newer_one_was_accepted() {
    // given
    let state = reach(open_host());

    // when
    let (newer, events) = apply(&state, at(2, RECONCILER, reserve(1, GuestOs::Linux, 8)))
        .unwrap_or_else(|r| panic!("{r:?}"));

    // then
    assert!(matches!(
        events.first(),
        Some(HostEvent::HostEpochAdvanced(_))
    ));
    assert_eq!(newer.epoch.get(), 2);
    assert_eq!(outcome(&newer, at(1, RECONCILER, grant(1))), "stale_run");
    assert_eq!(outcome(&newer, at(2, RECONCILER, grant(1))), "accepted");
}

#[test]
fn should_refuse_a_reservation_when_the_terms_revision_moved() {
    // given: the reconciler read revision 1; the pool's terms moved to 2
    let state = reach(with(open_host(), [terms("pool0", 2, true, 2)]));

    // when
    let stale = outcome(&state, auto(reserve(1, GuestOs::Linux, 8)));

    // then
    assert_eq!(stale, "terms_changed");
    assert_eq!(
        outcome(&state, auto(terms("pool0", 2, true, 3))),
        "stale_terms"
    );
}

#[test]
fn should_refuse_to_allocate_blind_when_no_report_was_applied() {
    // given
    let state = reach([register(), terms("pool0", 1, true, 2)]);

    // when
    let blind = outcome(&state, auto(reserve(1, GuestOs::Linux, 1)));

    // then
    assert_eq!(blind, "no_capacity_report");
}

#[test]
fn should_admit_a_linux_vm_when_a_newer_report_shrank_the_macos_slots() {
    // given: one macOS lease, then a report in which two foreign macOS guests run
    let state = reach(with(
        open_host(),
        [
            terms("pool1", 1, true, 4),
            reserve(1, GuestOs::Macos, 8),
            record(
                2,
                ReportedCapacity {
                    foreign_macos_guests: 2,
                    foreign_vms: 2,
                    ..report()
                },
            ),
        ],
    ));

    // when
    let linux = outcome(&state, auto(reserve_in("pool1", 2, GuestOs::Linux, 4)));
    let macos = outcome(&state, auto(reserve_in("pool1", 2, GuestOs::Macos, 4)));

    // then: the over-allocated dimension admits nothing more; the others still admit
    assert_eq!(linux, "accepted");
    assert_eq!(macos, "macos_guests");
}
