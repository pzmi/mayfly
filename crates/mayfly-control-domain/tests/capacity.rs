//! The capacity policy (plan §9) over generated inputs: the properties `formal/scheduling.qnt`
//! states of `planOf`, asserted on `capacity::plan`.

use mayfly_control_domain::attempt::GuestOs;
use mayfly_control_domain::capacity::{
    self, CapacityReport, Dimension, Footprint, HoldReason, LeaseInput, PlanDecision, PlanInputs,
    PoolInput, ReportedCapacity,
};
use mayfly_domain::{PoolName, Resources};
use proptest::prelude::*;

const GIB: u64 = 1 << 30;

fn pool(name: &str) -> PoolName {
    PoolName::new(name).unwrap_or_else(|e| panic!("{e}"))
}

fn os() -> impl Strategy<Value = GuestOs> {
    prop_oneof![Just(GuestOs::Macos), Just(GuestOs::Linux)]
}

fn pool_name() -> impl Strategy<Value = PoolName> {
    prop_oneof![Just("p0"), Just("p1"), Just("p2")].prop_map(pool)
}

fn report() -> impl Strategy<Value = ReportedCapacity> {
    (
        (0_u64..96, 0_u64..400),
        (0_u64..16, 0_u64..60),
        0_u64..80,
        (0_u32..8, 0_u32..3),
        (0_u32..4, 0_u32..3),
    )
        .prop_map(
            |(
                (memory, disk),
                (foreign_memory, foreign_disk),
                base,
                (slots, foreign_vms),
                (macos, foreign_macos),
            )| {
                ReportedCapacity {
                    total: Resources::new(memory * GIB, disk * GIB),
                    foreign: Resources::new(foreign_memory * GIB, foreign_disk * GIB),
                    base: Resources::new(0, base * GIB),
                    vm_slots: slots,
                    foreign_vms,
                    macos_guests: macos,
                    foreign_macos_guests: foreign_macos,
                }
            },
        )
}

fn lease() -> impl Strategy<Value = LeaseInput> {
    (
        pool_name(),
        os(),
        0_u64..16,
        0_u64..60,
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(pool, os, memory, disk, quarantined, stopped)| {
            let vm = Footprint::vm(os, Resources::new(memory * GIB, disk * GIB));
            let holds = if quarantined && stopped {
                Footprint {
                    resources: Resources::new(0, vm.resources.disk_bytes),
                    ..vm
                }
            } else {
                vm
            };
            LeaseInput {
                pool,
                holds,
                quarantined,
            }
        })
}

fn pool_input() -> impl Strategy<Value = PoolInput> {
    (
        pool_name(),
        os(),
        0_u64..16,
        0_u64..60,
        any::<bool>(),
        0_u32..5,
        0_u32..6,
    )
        .prop_map(
            |(pool, os, memory, disk, open, max_running, wanted)| PoolInput {
                pool,
                os,
                vm: Resources::new(memory * GIB, disk * GIB),
                open,
                max_running,
                wanted,
            },
        )
}

fn inputs() -> impl Strategy<Value = PlanInputs> {
    (
        prop::option::weighted(0.9, report()),
        (0_u64..8, 0_u64..40),
        prop::bool::weighted(0.8),
        prop::collection::vec(lease(), 0..6),
        prop::collection::vec(pool_input(), 0..4),
    )
        .prop_map(
            |(report, (rm, rd), admission_open, leases, pools)| PlanInputs {
                report,
                reserves: Resources::new(rm * GIB, rd * GIB),
                admission_open,
                leases,
                pools,
            },
        )
}

/// What the reservation decisions ask for, summed.
fn reserved(decisions: &[PlanDecision]) -> Footprint {
    decisions.iter().fold(Footprint::ZERO, |sum, d| match d {
        PlanDecision::Reserve { os, resources, .. } => {
            sum.saturating_add(Footprint::vm(*os, *resources))
        }
        PlanDecision::Hold { .. } => sum,
    })
}

fn reserves_any(decisions: &[PlanDecision]) -> bool {
    decisions
        .iter()
        .any(|d| matches!(d, PlanDecision::Reserve { .. }))
}

/// The pools' carve-outs, in VMs, summed as footprints.
fn carved(inputs: &PlanInputs, report: &CapacityReport) -> Footprint {
    report.pools.iter().fold(Footprint::ZERO, |sum, line| {
        let input = inputs
            .pools
            .iter()
            .find(|p| p.pool == line.pool)
            .unwrap_or_else(|| panic!("a line for an unknown pool"));
        let vm = Footprint::vm(input.os, input.vm);
        (0..line.carve_out).fold(sum, |s, _| s.saturating_add(vm))
    })
}

proptest! {
    #[test]
    fn should_admit_within_every_reported_limit_when_inputs_are_random(inputs in inputs()) {
        // given: `inputs`, generated
        // when
        let (report, decisions) = capacity::plan(&inputs);
        // then: what the leases hold plus what is reserved fits the report on every dimension
        let asked = reserved(&decisions);
        let total = report.allocated.saturating_add(asked);
        let limit = report.admissible;
        let used = [
            (asked.vms > 0, total.vms <= limit.vms),
            (asked.macos_guests > 0, total.macos_guests <= limit.macos_guests),
            (asked.resources.memory_bytes > 0, total.resources.memory_bytes <= limit.resources.memory_bytes),
            (asked.resources.disk_bytes > 0, total.resources.disk_bytes <= limit.resources.disk_bytes),
        ];
        for (dimension, (asks, within)) in used.into_iter().enumerate() {
            prop_assert!(!asks || within, "dimension {dimension}: {total:?} beyond {limit:?}");
        }
    }

    #[test]
    fn should_never_admit_a_third_macos_guest_when_inputs_are_random(inputs in inputs()) {
        // given: `inputs`, generated
        // when
        let (report, decisions) = capacity::plan(&inputs);
        // then
        if reserved(&decisions).macos_guests > 0 {
            let foreign = inputs.report.map_or(0, |r| r.foreign_macos_guests);
            let ours = report.allocated.saturating_add(reserved(&decisions)).macos_guests;
            prop_assert!(ours + foreign <= capacity::MACOS_GUESTS_MAX);
        }
    }

    #[test]
    fn should_hold_quarantined_capacity_when_inputs_are_random(inputs in inputs()) {
        // given: the same inputs without the quarantined leases
        let mut released = inputs.clone();
        released.leases.retain(|l| !l.quarantined);
        // when
        let (with_quarantine, _) = capacity::plan(&inputs);
        let (without, _) = capacity::plan(&released);
        // then: quarantined leases are allocated and never give more room
        prop_assert!(with_quarantine.quarantined.fits_within(with_quarantine.allocated));
        prop_assert!(with_quarantine.free.fits_within(without.free));
        let carve = |r: &CapacityReport| r.pools.iter().map(|p| p.carve_out).sum::<u32>();
        prop_assert!(carve(&with_quarantine) <= carve(&without));
    }

    #[test]
    fn should_fit_every_carve_out_together_when_inputs_are_random(inputs in inputs()) {
        // given: `inputs`, generated
        // when
        let (report, decisions) = capacity::plan(&inputs);
        // then: I24, the carve-outs fit the free capacity together; I07, reported max is
        // within what each pool can deliver; decisions never exceed carve-out or demand
        prop_assert!(carved(&inputs, &report).fits_within(report.free));
        for line in &report.pools {
            prop_assert!(line.reported_max <= line.deliverable);
            prop_assert_eq!(line.deliverable, line.runners + line.carve_out);
            let input = inputs.pools.iter().find(|p| p.pool == line.pool);
            let asked = decisions
                .iter()
                .filter(|d| matches!(d, PlanDecision::Reserve { pool, .. } if *pool == line.pool))
                .count();
            prop_assert!(asked as u32 <= line.carve_out);
            prop_assert!(input.is_some_and(|p| asked as u32 <= p.wanted));
        }
    }

    #[test]
    fn should_never_admit_more_when_foreign_consumption_grows(inputs in inputs(), more in 1_u64..8) {
        // given
        let mut grown = inputs.clone();
        if let Some(r) = grown.report.as_mut() {
            r.foreign.memory_bytes += more * GIB;
            r.foreign_vms += 1;
        }
        // when
        let (before, _) = capacity::plan(&inputs);
        let (after, _) = capacity::plan(&grown);
        // then
        prop_assert!(after.admissible.fits_within(before.admissible));
        prop_assert!(after.free.fits_within(before.free));
        let total = |r: &CapacityReport| r.pools.iter().map(|p| p.carve_out).sum::<u32>();
        prop_assert!(total(&after) <= total(&before));
    }

    #[test]
    fn should_admit_nothing_and_report_live_runners_when_admission_is_closed(inputs in inputs()) {
        // given
        let mut closed = inputs;
        closed.admission_open = false;
        // when
        let (report, decisions) = capacity::plan(&closed);
        // then: I08, I26
        prop_assert!(!reserves_any(&decisions));
        for line in &report.pools {
            prop_assert_eq!(line.carve_out, 0);
            prop_assert_eq!(line.reported_max, line.runners);
        }
    }
}

fn host(memory_gib: u64, slots: u32) -> ReportedCapacity {
    ReportedCapacity {
        total: Resources::new(memory_gib * GIB, 400 * GIB),
        foreign: Resources::new(0, 0),
        base: Resources::new(0, 0),
        vm_slots: slots,
        foreign_vms: 0,
        macos_guests: 2,
        foreign_macos_guests: 0,
    }
}

fn demand(name: &str, os: GuestOs, memory_gib: u64, max_running: u32, wanted: u32) -> PoolInput {
    PoolInput {
        pool: pool(name),
        os,
        vm: Resources::new(memory_gib * GIB, 20 * GIB),
        open: true,
        max_running,
        wanted,
    }
}

#[test]
fn should_explain_quarantine_when_quarantined_leases_consume_the_host() {
    // given: 16 GiB, one 8 GiB quarantined lease, a pool wanting two 8 GiB VMs
    let inputs = PlanInputs {
        report: Some(host(16, 4)),
        reserves: Resources::new(0, 0),
        admission_open: true,
        leases: vec![LeaseInput {
            pool: pool("p0"),
            holds: Footprint::vm(GuestOs::Linux, Resources::new(8 * GIB, 20 * GIB)),
            quarantined: true,
        }],
        pools: vec![demand("p1", GuestOs::Linux, 8, 4, 2)],
    };

    // when
    let (report, decisions) = capacity::plan(&inputs);

    // then: one fits, the second is held by the quarantine (ADR 0025's AdmissionExplain)
    assert_eq!(report.pools[0].carve_out, 1);
    assert_eq!(
        decisions,
        [
            PlanDecision::Reserve {
                pool: pool("p1"),
                os: GuestOs::Linux,
                resources: Resources::new(8 * GIB, 20 * GIB),
            },
            PlanDecision::Hold {
                pool: pool("p1"),
                reason: HoldReason::QuarantineHeld,
            },
        ]
    );
}

#[test]
fn should_carve_in_name_order_and_report_only_runners_when_a_pool_is_closed() {
    // given: room for three 8 GiB VMs; p0 wants three, p1 two, p2 is paused with one runner
    let mut paused = demand("p2", GuestOs::Linux, 8, 4, 1);
    paused.open = false;
    let inputs = PlanInputs {
        report: Some(host(32, 8)),
        reserves: Resources::new(8 * GIB, 0),
        admission_open: true,
        leases: vec![LeaseInput {
            pool: pool("p2"),
            holds: Footprint::vm(GuestOs::Linux, Resources::new(0, 20 * GIB)),
            quarantined: false,
        }],
        pools: vec![
            paused,
            demand("p1", GuestOs::Linux, 8, 4, 2),
            demand("p0", GuestOs::Linux, 8, 2, 3),
        ],
    };

    // when
    let (report, decisions) = capacity::plan(&inputs);

    // then
    let lines: Vec<(&str, u32, u32, u32)> = report
        .pools
        .iter()
        .map(|p| (p.pool.as_str(), p.carve_out, p.deliverable, p.reported_max))
        .collect();
    assert_eq!(lines, [("p0", 2, 2, 2), ("p1", 1, 1, 1), ("p2", 0, 1, 1)]);
    assert_eq!(report.pools[2].hold, Some(HoldReason::TermsClosed));
    let holds: Vec<HoldReason> = decisions
        .iter()
        .filter_map(|d| match d {
            PlanDecision::Hold { reason, .. } => Some(*reason),
            PlanDecision::Reserve { .. } => None,
        })
        .collect();
    assert_eq!(
        holds,
        [
            HoldReason::PoolFull,
            HoldReason::NoCapacity(Dimension::Memory),
            HoldReason::TermsClosed
        ]
    );
}

#[test]
fn should_hold_everything_when_no_report_was_applied() {
    // given
    let inputs = PlanInputs {
        report: None,
        reserves: Resources::new(0, 0),
        admission_open: true,
        leases: vec![],
        pools: vec![demand("p0", GuestOs::Macos, 8, 2, 1)],
    };

    // when
    let (report, decisions) = capacity::plan(&inputs);

    // then
    assert_eq!(report.admissible, Footprint::ZERO);
    assert_eq!(
        decisions,
        [PlanDecision::Hold {
            pool: pool("p0"),
            reason: HoldReason::NoReport,
        }]
    );
}

/// The carve-outs of `report`'s pools, in name order.
fn carve_outs(report: &CapacityReport) -> Vec<(&str, u32)> {
    report
        .pools
        .iter()
        .map(|p| (p.pool.as_str(), p.carve_out))
        .collect()
}

#[test]
fn should_carve_no_more_when_foreign_use_leaves_a_large_vm_unfit() {
    // given: KER-05H's shrunk counterexample (seed `cc 9b7106b4…` in
    // `capacity.proptest-regressions`): 28 GiB admissible, 10 GiB held by p0's lease; p0 has
    // room for one more 15 GiB VM, p1 for three 2 GiB VMs
    let inputs = PlanInputs {
        report: Some(ReportedCapacity {
            total: Resources::new(41 * GIB, 145 * GIB),
            foreign: Resources::new(12 * GIB, 0),
            base: Resources::new(0, 0),
            vm_slots: 5,
            foreign_vms: 0,
            macos_guests: 0,
            foreign_macos_guests: 0,
        }),
        reserves: Resources::new(GIB, 0),
        admission_open: true,
        leases: vec![LeaseInput {
            pool: pool("p0"),
            holds: Footprint {
                resources: Resources::new(10 * GIB, 0),
                vms: 1,
                macos_guests: 1,
            },
            quarantined: false,
        }],
        pools: vec![
            PoolInput {
                pool: pool("p1"),
                os: GuestOs::Linux,
                vm: Resources::new(2 * GIB, 0),
                open: true,
                max_running: 3,
                wanted: 0,
            },
            PoolInput {
                pool: pool("p0"),
                os: GuestOs::Linux,
                vm: Resources::new(15 * GIB, 0),
                open: true,
                max_running: 2,
                wanted: 0,
            },
        ],
    };
    let mut grown = inputs.clone();
    if let Some(r) = grown.report.as_mut() {
        r.foreign.memory_bytes += 4 * GIB;
        r.foreign_vms += 1;
    }

    // when
    let (before, _) = capacity::plan(&inputs);
    let (after, _) = capacity::plan(&grown);

    // then: 4 GiB more foreign use no longer fits p0's 15 GiB VM; p0 still keeps its room ahead
    // of p1, so p1 is not handed the 14 GiB left (the greedy leftover carved it three VMs)
    assert_eq!(carve_outs(&before), [("p0", 1), ("p1", 1)]);
    assert_eq!(carve_outs(&after), [("p0", 0), ("p1", 0)]);
}

#[test]
fn should_carve_no_less_when_a_quarantine_of_an_earlier_pool_is_released() {
    // given: p0 (one 16 GiB VM at most) holds a stopped quarantined lease; p1 runs 2 GiB VMs
    let inputs = PlanInputs {
        report: Some(host(32, 20)),
        reserves: Resources::new(0, 0),
        admission_open: true,
        leases: vec![LeaseInput {
            pool: pool("p0"),
            holds: Footprint {
                resources: Resources::new(0, 20 * GIB),
                vms: 1,
                macos_guests: 0,
            },
            quarantined: true,
        }],
        pools: vec![
            demand("p0", GuestOs::Linux, 16, 1, 1),
            demand("p1", GuestOs::Linux, 2, 16, 16),
        ],
    };
    let mut released = inputs.clone();
    released.leases.clear();

    // when
    let (held, _) = capacity::plan(&inputs);
    let (freed, _) = capacity::plan(&released);

    // then: p0's quarantined place stays kept ahead of p1, so releasing it gives p0 its VM and
    // takes nothing from p1 (the greedy leftover carved p1 sixteen VMs, then eight)
    assert_eq!(carve_outs(&held), [("p0", 0), ("p1", 8)]);
    assert_eq!(carve_outs(&freed), [("p0", 1), ("p1", 8)]);
}
