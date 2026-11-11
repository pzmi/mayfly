//! The capacity policy (plan §9) over generated inputs: the properties `formal/scheduling.qnt`
//! states of `planOf`, asserted on `capacity::plan`.

use mayfly_control_domain::attempt::GuestOs;
use mayfly_control_domain::capacity::{
    self, CapacityPolicy, CapacityReport, Dimension, Footprint, HoldReason, LeaseInput,
    PlanDecision, PlanInputs, PoolInput, ReportedCapacity,
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
                policy: CapacityPolicy::Exact,
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

/// Each pool's carve-out, by name.
fn carve_by_pool(report: &CapacityReport) -> Vec<(PoolName, u32)> {
    report
        .pools
        .iter()
        .map(|p| (p.pool.clone(), p.carve_out))
        .collect()
}

/// Pool by pool (the same pools in the same order), `less` carves no pool more than `more`.
fn no_pool_more(less: &CapacityReport, more: &CapacityReport) -> bool {
    carve_by_pool(less)
        .iter()
        .zip(carve_by_pool(more).iter())
        .all(|((a, x), (b, y))| a == b && x <= y)
}

/// The oracle of KER-05H's keep (`formal/scheduling.qnt`'s `keepsRoom`): each open pool keeps a
/// whole VM per place in `max_running` minus its live runners, quarantined places included.
fn whole_place_carves(inputs: &PlanInputs, free: Footprint) -> Vec<(PoolName, u32)> {
    let mut pools: Vec<&PoolInput> = inputs.pools.iter().collect();
    pools.sort_by(|a, b| a.pool.cmp(&b.pool));
    pools.dedup_by(|a, b| a.pool == b.pool);
    let count = |pool: &PoolName, quarantined: bool| {
        inputs
            .leases
            .iter()
            .filter(|l| &l.pool == pool && l.quarantined == quarantined)
            .count() as u32
    };
    let times = |vm: Footprint, n: u32| (0..n).fold(Footprint::ZERO, |s, _| s.saturating_add(vm));
    let mut kept = Footprint::ZERO;
    let mut carves = Vec::new();
    for pool in pools {
        let runners = count(&pool.pool, false);
        let headroom = pool
            .max_running
            .saturating_sub(runners + count(&pool.pool, true));
        let vm = Footprint::vm(pool.os, pool.vm);
        let open = inputs.report.is_some() && inputs.admission_open && pool.open;
        let available = free.saturating_sub(kept);
        let mut carve = 0;
        while open && carve < headroom && capacity::fit(available, times(vm, carve), vm).is_ok() {
            carve += 1;
        }
        if open {
            kept = kept.saturating_add(times(vm, pool.max_running.saturating_sub(runners)));
        }
        carves.push((pool.pool.clone(), carve));
    }
    carves
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
    fn should_carve_no_pool_more_when_foreign_consumption_grows_and_inputs_are_random(
        inputs in inputs(),
        more in 1_u64..8,
    ) {
        // given
        let mut grown = inputs.clone();
        if let Some(r) = grown.report.as_mut() {
            r.foreign.memory_bytes += more * GIB;
            r.foreign.disk_bytes += more * GIB;
            r.foreign_vms += 1;
            r.foreign_macos_guests += 1;
        }
        // when
        let (before, _) = capacity::plan(&inputs);
        let (after, _) = capacity::plan(&grown);
        // then: KER-05D, monotone pool by pool, not only in total
        prop_assert!(no_pool_more(&after, &before), "{:?} then {:?}", carve_by_pool(&before), carve_by_pool(&after));
    }

    #[test]
    fn should_carve_no_pool_less_when_a_quarantined_lease_is_released_and_inputs_are_random(
        generated in inputs(),
        extra in lease(),
        which in any::<prop::sample::Index>(),
    ) {
        // given: one of the quarantined leases (at least `extra`) released
        let mut inputs = generated;
        inputs.leases.push(LeaseInput { quarantined: true, ..extra });
        let quarantined: Vec<usize> = inputs
            .leases
            .iter()
            .enumerate()
            .filter_map(|(i, l)| l.quarantined.then_some(i))
            .collect();
        let mut released = inputs.clone();
        released.leases.remove(quarantined[which.index(quarantined.len())]);
        // when
        let (held, _) = capacity::plan(&inputs);
        let (freed, _) = capacity::plan(&released);
        // then: what it held comes back and its pool keeps at most that much more
        prop_assert!(no_pool_more(&held, &freed), "{:?} then {:?}", carve_by_pool(&held), carve_by_pool(&freed));
    }

    #[test]
    fn should_carve_every_pool_at_least_the_whole_place_keep_when_inputs_are_random(
        inputs in inputs(),
    ) {
        // given: `inputs`, generated
        // when
        let (report, _) = capacity::plan(&inputs);
        // then: KER-05D never carves a pool less than KER-05H's keep did
        let oracle = whole_place_carves(&inputs, report.free);
        for ((pool, carve), (oracle_pool, oracle_carve)) in carve_by_pool(&report).iter().zip(&oracle) {
            prop_assert_eq!(pool, oracle_pool);
            prop_assert!(carve >= oracle_carve, "{pool}: {carve} < {oracle_carve}");
        }
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
        policy: CapacityPolicy::Exact,
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
                policy: CapacityPolicy::Exact,
            },
            PoolInput {
                pool: pool("p0"),
                os: GuestOs::Linux,
                vm: Resources::new(15 * GIB, 0),
                open: true,
                max_running: 2,
                wanted: 0,
                policy: CapacityPolicy::Exact,
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

/// KER-05D's hosts: a macOS pool `p0` (15 GiB, 60 GiB of disk, `max_running` 2) before a Linux
/// pool `p1` (4 GiB, 20 GiB, `max_running` 12), with one quarantined macOS lease holding `holds`.
fn quarantined_mac(total_gib: u64, slots: u32, holds: Footprint) -> PlanInputs {
    let mac = Resources::new(15 * GIB, 60 * GIB);
    PlanInputs {
        report: Some(host(total_gib, slots)),
        reserves: Resources::new(0, 0),
        admission_open: true,
        leases: vec![LeaseInput {
            pool: pool("p0"),
            holds,
            quarantined: true,
        }],
        pools: vec![
            PoolInput {
                pool: pool("p0"),
                os: GuestOs::Macos,
                vm: mac,
                open: true,
                max_running: 2,
                wanted: 1,
                policy: CapacityPolicy::Exact,
            },
            demand("p1", GuestOs::Linux, 4, 12, 12),
        ],
    }
}

#[test]
fn should_keep_only_what_a_quarantined_lease_does_not_hold_when_an_earlier_pool_has_one() {
    // given: 64 GiB; a running quarantined macOS VM holds 15 of them (ADR 0025 keeps it until
    // an operator resolves it), so 49 GiB are free and p0 has room for one more VM
    let running = Footprint::vm(GuestOs::Macos, Resources::new(15 * GIB, 60 * GIB));
    let inputs = quarantined_mac(64, 12, running);
    let mut released = inputs.clone();
    released.leases.clear();

    // when
    let (held, _) = capacity::plan(&inputs);
    let (freed, _) = capacity::plan(&released);

    // then: p0 keeps one VM (34 GiB left: eight Linux VMs), not two (KER-05H kept the
    // quarantined VM's 15 GiB a second time: four); releasing it gives p0 its second VM and
    // leaves p1 as it was
    assert_eq!(carve_outs(&held), [("p0", 1), ("p1", 8)]);
    assert_eq!(whole_place_carves(&inputs, held.free)[1].1, 4);
    assert_eq!(carve_outs(&freed), [("p0", 2), ("p1", 8)]);
}

#[test]
fn should_keep_the_memory_a_stopped_quarantined_lease_owes_when_slots_bind() {
    // given: a stopped quarantined macOS VM holds its disk and slots only; 3 slots are free
    let stopped = Footprint {
        resources: Resources::new(0, 60 * GIB),
        ..Footprint::vm(GuestOs::Macos, Resources::new(15 * GIB, 60 * GIB))
    };
    let inputs = quarantined_mac(64, 4, stopped);
    let mut released = inputs.clone();
    released.leases.clear();

    // when
    let (held, _) = capacity::plan(&inputs);
    let (freed, _) = capacity::plan(&released);

    // then: p0 keeps one slot for its headroom and the quarantined place's memory, not a second
    // slot (KER-05H: one Linux VM); releasing it gives p0 its second VM and takes nothing from p1
    assert_eq!(carve_outs(&held), [("p0", 1), ("p1", 2)]);
    assert_eq!(whole_place_carves(&inputs, held.free)[1].1, 1);
    assert_eq!(carve_outs(&freed), [("p0", 2), ("p1", 2)]);
}

// ---- DEC-06: the DEMAND policy (ADR 0069), beside EXACT -----------------------------------

fn policy() -> impl Strategy<Value = CapacityPolicy> {
    prop_oneof![
        Just(CapacityPolicy::Exact),
        (0_u32..6).prop_map(|waiting| CapacityPolicy::Demand { waiting }),
    ]
}

/// Generated inputs whose pools each take a generated policy.
fn mixed_inputs() -> impl Strategy<Value = PlanInputs> {
    (inputs(), prop::collection::vec(policy(), 4)).prop_map(|(mut inputs, policies)| {
        for (pool, policy) in inputs.pools.iter_mut().zip(policies) {
            pool.policy = policy;
        }
        inputs
    })
}

/// The same inputs with every pool `EXACT`.
fn all_exact(inputs: &PlanInputs) -> PlanInputs {
    let mut exact = inputs.clone();
    for pool in &mut exact.pools {
        pool.policy = CapacityPolicy::Exact;
    }
    exact
}

/// The policy `plan` applied to a pool's line: the first input of that name, as `plan` dedups.
fn policy_of(inputs: &PlanInputs, pool: &PoolName) -> CapacityPolicy {
    inputs
        .pools
        .iter()
        .find(|p| &p.pool == pool)
        .map_or(CapacityPolicy::Exact, |p| p.policy)
}

proptest! {
    #[test]
    fn should_fit_and_stay_monotone_when_pools_mix_policies_and_inputs_are_random(
        inputs in mixed_inputs(),
        more in 1_u64..8,
    ) {
        // given
        let mut grown = inputs.clone();
        if let Some(r) = grown.report.as_mut() {
            r.foreign.memory_bytes += more * GIB;
            r.foreign.disk_bytes += more * GIB;
            r.foreign_vms += 1;
            r.foreign_macos_guests += 1;
        }
        // when
        let (report, decisions) = capacity::plan(&inputs);
        let (after, _) = capacity::plan(&grown);
        // then: I24 and I07 under either policy, a DEMAND pool carved only for what it waits
        // for, and monotone pool by pool for a fixed demand (KER-05D's M1)
        prop_assert!(carved(&inputs, &report).fits_within(report.free));
        prop_assert!(report.allocated.saturating_add(reserved(&decisions)).fits_within(
            report.admissible.saturating_add(report.allocated)
        ));
        for line in &report.pools {
            prop_assert!(line.reported_max <= line.deliverable);
            if let CapacityPolicy::Demand { waiting } = policy_of(&inputs, &line.pool) {
                prop_assert!(line.carve_out <= waiting);
            }
        }
        prop_assert!(no_pool_more(&after, &report), "{:?} then {:?}", carve_by_pool(&report), carve_by_pool(&after));
    }

    #[test]
    fn should_carve_no_pool_less_when_a_quarantine_is_released_and_pools_mix_policies(
        generated in mixed_inputs(),
        extra in lease(),
        which in any::<prop::sample::Index>(),
    ) {
        // given: one of the quarantined leases (at least `extra`) released, demand unchanged
        let mut inputs = generated;
        inputs.leases.push(LeaseInput { quarantined: true, ..extra });
        let quarantined: Vec<usize> = inputs
            .leases
            .iter()
            .enumerate()
            .filter_map(|(i, l)| l.quarantined.then_some(i))
            .collect();
        let mut released = inputs.clone();
        released.leases.remove(quarantined[which.index(quarantined.len())]);
        // when
        let (held, _) = capacity::plan(&inputs);
        let (freed, _) = capacity::plan(&released);
        // then: KER-05D's M2 for a fixed demand
        prop_assert!(no_pool_more(&held, &freed), "{:?} then {:?}", carve_by_pool(&held), carve_by_pool(&freed));
    }

    #[test]
    fn should_carve_later_pools_at_least_exact_when_earlier_pools_keep_only_for_demand(
        inputs in mixed_inputs(),
    ) {
        // given: the same inputs with every pool EXACT
        let exact = all_exact(&inputs);
        // when
        let (mixed, _) = capacity::plan(&inputs);
        let (baseline, _) = capacity::plan(&exact);
        // then: a DEMAND pool keeps no more than EXACT would, so every EXACT pool is carved at
        // least as much, whatever the pools before it chose
        for (line, base) in mixed.pools.iter().zip(&baseline.pools) {
            prop_assert_eq!(&line.pool, &base.pool);
            if policy_of(&inputs, &line.pool) == CapacityPolicy::Exact {
                prop_assert!(line.carve_out >= base.carve_out, "{}: {} < {}", line.pool, line.carve_out, base.carve_out);
            }
        }
    }

    #[test]
    fn should_carve_as_exact_when_every_demand_covers_its_headroom(inputs in inputs()) {
        // given: every pool DEMAND, waiting for more than its max_running
        let mut covered = inputs.clone();
        for pool in &mut covered.pools {
            pool.policy = CapacityPolicy::Demand { waiting: pool.max_running.saturating_add(1) };
        }
        // when
        let (exact, exact_decisions) = capacity::plan(&inputs);
        let (demand, demand_decisions) = capacity::plan(&covered);
        // then
        prop_assert_eq!(exact, demand);
        prop_assert_eq!(exact_decisions, demand_decisions);
    }
}

/// The 13 GiB case of ADR 0069 (`formal/scheduling.qnt`'s S1): a macOS pool first in name order
/// whose 15 GiB VM does not fit, a Linux pool after it.
fn thirteen_gib(mac: CapacityPolicy) -> PlanInputs {
    PlanInputs {
        report: Some(host(13, 8)),
        reserves: Resources::new(0, 0),
        admission_open: true,
        leases: Vec::new(),
        pools: vec![
            PoolInput {
                policy: mac,
                ..demand("amac", GuestOs::Macos, 15, 2, 0)
            },
            demand("blin", GuestOs::Linux, 4, 8, 3),
        ],
    }
}

#[test]
fn should_carve_a_later_pool_beside_an_idle_one_when_the_earlier_keeps_only_for_demand() {
    // given
    let exact = thirteen_gib(CapacityPolicy::Exact);
    let idle = thirteen_gib(CapacityPolicy::Demand { waiting: 0 });
    // when
    let (exact_report, _) = capacity::plan(&exact);
    let (demand_report, decisions) = capacity::plan(&idle);
    // then: EXACT keeps the macOS headroom and starves the Linux pool; DEMAND does not
    assert_eq!(carve_outs(&exact_report), [("amac", 0), ("blin", 0)]);
    assert_eq!(carve_outs(&demand_report), [("amac", 0), ("blin", 3)]);
    assert_eq!(demand_report.pools[0].hold, Some(HoldReason::NoDemand));
    assert_eq!(demand_report.pools[0].reported_max, 0);
    assert_eq!(reserved(&decisions).vms, 3);
}

#[test]
fn should_keep_room_for_a_waiting_job_when_the_earlier_pool_keeps_only_for_demand() {
    // given: one macOS job waits (S2)
    let waiting = thirteen_gib(CapacityPolicy::Demand { waiting: 1 });
    // when
    let (report, _) = capacity::plan(&waiting);
    // then: the room for its VM is kept, as EXACT keeps it
    assert_eq!(carve_outs(&report), [("amac", 0), ("blin", 0)]);
    assert_eq!(
        report.pools[0].hold,
        Some(HoldReason::NoCapacity(Dimension::Memory))
    );
}
