//! The reconciler's pass (P5, plan §9): `reconcile::plan_pass` over hand-built and generated
//! inputs: what it reserves, retires, releases and tells each scale set, its hysteresis, its
//! reading of a data-plane refusal, and its determinism.

use std::collections::BTreeMap;

use mayfly_control_domain::attempt::{GuestOs, RetireReason};
use mayfly_control_domain::capacity::{CapacityPolicy, Footprint, ReportedCapacity};
use mayfly_control_domain::demand::{Capacity, WireMode};
use mayfly_control_domain::host::ReleaseEvidence;
use mayfly_control_domain::pool::PoolMode;
use mayfly_control_domain::reconcile::{
    HostReport, HostTerms, HostView, LeaseView, PassCommand, PassInputs, PassMemory, PoolView,
    RunnerStage, ScaleSetView, Settings, exact, plan_pass,
};
use mayfly_domain::{AttemptId, HostId, ImageDigest, Millis, PoolName, Resources, Sample};
use proptest::prelude::*;

const GIB: u64 = 1 << 30;
const MINUTE: u64 = 60_000;

fn pool(name: &str) -> PoolName {
    PoolName::new(name).unwrap_or_else(|e| panic!("{e}"))
}

fn host_id(name: &str) -> HostId {
    HostId::new(name).unwrap_or_else(|e| panic!("{e}"))
}

const fn demand(waiting: u32) -> CapacityPolicy {
    CapacityPolicy::Demand { waiting }
}

fn report(id: u64, memory_gib: u64, slots: u32) -> HostReport {
    HostReport {
        report_id: id,
        capacity: ReportedCapacity {
            total: Resources::new(memory_gib * GIB, 1000 * GIB),
            foreign: Resources::new(0, 0),
            base: Resources::new(0, 0),
            vm_slots: slots,
            foreign_vms: 0,
            macos_guests: 2,
            foreign_macos_guests: 0,
        },
    }
}

fn host(name: &str, pools: &[(&str, u32)]) -> HostView {
    HostView {
        host_id: host_id(name),
        report: Some(report(1, 64, 8)),
        reserves: Resources::new(0, 0),
        admission_open: true,
        draining: false,
        terms: pools
            .iter()
            .map(|(p, max)| {
                (
                    pool(p),
                    HostTerms {
                        revision: 1,
                        open: true,
                        max_running: *max,
                    },
                )
            })
            .collect(),
        placed: [(ImageDigest::sample(), None)].into_iter().collect(),
        leases: Vec::new(),
    }
}

fn linux(name: &str, assigned: Option<u32>) -> PoolView {
    PoolView {
        pool: pool(name),
        os: GuestOs::Linux,
        vm: Resources::new(4 * GIB, 50 * GIB),
        min_prepared: 0,
        mode: PoolMode::Active,
        image_pin: Some(ImageDigest::sample()),
        policy: exact,
        scale_set: Some(ScaleSetView {
            assigned,
            capacity: Capacity::NONE,
        }),
    }
}

fn lease(n: u128, pool_name: &str, stage: RunnerStage) -> LeaseView {
    LeaseView {
        attempt_id: AttemptId::from_u128(n),
        pool: pool(pool_name),
        holds: Footprint::vm(GuestOs::Linux, Resources::new(4 * GIB, 50 * GIB)),
        quarantined: false,
        stage,
        image: Some(ImageDigest::sample()),
        refusal: None,
        retiring: false,
    }
}

fn inputs(hosts: Vec<HostView>, pools: Vec<PoolView>) -> PassInputs {
    PassInputs {
        now: Millis::new(1_700_000_000_000),
        settings: Settings {
            hysteresis: Millis::new(5 * MINUTE),
            start_grace: Millis::new(2 * MINUTE),
        },
        safe_mode: false,
        hosts,
        pools,
        memory: PassMemory::default(),
    }
}

fn reserves(commands: &[PassCommand]) -> Vec<(String, String)> {
    commands
        .iter()
        .filter_map(|c| match c {
            PassCommand::ReserveSlot { host_id, pool, .. } => {
                Some((host_id.to_string(), pool.to_string()))
            }
            _ => None,
        })
        .collect()
}

fn retired(commands: &[PassCommand]) -> Vec<(u128, RetireReason)> {
    commands
        .iter()
        .filter_map(|c| match c {
            PassCommand::Retire { attempt_id, reason } => Some((attempt_id.as_u128(), *reason)),
            _ => None,
        })
        .collect()
}

fn capacity_of(commands: &[PassCommand], name: &str) -> Option<Capacity> {
    commands.iter().find_map(|c| match c {
        PassCommand::ApplyCapacity { pool: p, capacity } if p.as_str() == name => Some(*capacity),
        _ => None,
    })
}

#[test]
fn should_derive_waiting_beyond_live_runners_when_jobs_are_assigned() {
    // given: 5 jobs assigned; 1 busy, 1 listening, 1 prepared, 1 preparing; min_prepared 1
    let mut lin = linux("lin", Some(5));
    lin.min_prepared = 1;
    let mut h = host("h1", &[("lin", 10)]);
    h.leases = vec![
        lease(1, "lin", RunnerStage::Busy),
        lease(2, "lin", RunnerStage::Listening),
        lease(3, "lin", RunnerStage::Prepared),
        lease(4, "lin", RunnerStage::Preparing),
    ];

    // when
    let pass = plan_pass(&inputs(vec![h], vec![lin]));

    // then: shortfall 3, served by 2 prepared/preparing, so 1 for jobs plus 1 for min_prepared
    let need = pass.needs.get(&pool("lin")).copied().unwrap_or_default();
    assert_eq!(need.shortfall, 3);
    assert_eq!(need.waiting, 2);
    assert_eq!(reserves(&pass.commands).len(), 2);
}

#[test]
fn should_carve_a_demand_pool_only_for_assigned_jobs_when_it_precedes_a_busy_pool() {
    // given: a 13 GiB host; "amac" (demand, 8 GiB macOS VMs) waits for nothing, "blin" (4 GiB)
    // has 3 jobs
    let mut mac = linux("amac", Some(0));
    mac.os = GuestOs::Macos;
    mac.vm = Resources::new(8 * GIB, 50 * GIB);
    mac.policy = demand;
    let lin = linux("blin", Some(3));
    let mut h = host("h1", &[("amac", 2), ("blin", 4)]);
    h.report = Some(report(1, 13, 8));

    // when
    let idle = plan_pass(&inputs(vec![h.clone()], vec![mac.clone(), lin.clone()]));
    mac.scale_set = Some(ScaleSetView {
        assigned: Some(1),
        capacity: Capacity::NONE,
    });
    let busy = plan_pass(&inputs(vec![h], vec![mac, lin]));

    // then
    assert_eq!(
        reserves(&idle.commands),
        vec![("h1".to_owned(), "blin".to_owned()); 3]
    );
    assert_eq!(
        capacity_of(&idle.commands, "amac").map(|c| c.reported_max),
        None
    );
    assert_eq!(
        reserves(&busy.commands).first(),
        Some(&("h1".to_owned(), "amac".to_owned()))
    );
    assert_eq!(
        capacity_of(&busy.commands, "amac").map(|c| c.reported_max),
        Some(1)
    );
}

#[test]
fn should_retire_a_surplus_only_after_the_hysteresis_when_prepared_vms_exceed_demand() {
    // given: no job assigned and two prepared VMs, min_prepared 1: a surplus of one
    let mut lin = linux("lin", Some(0));
    lin.min_prepared = 1;
    let mut h = host("h1", &[("lin", 4)]);
    h.leases = vec![
        lease(1, "lin", RunnerStage::Prepared),
        lease(2, "lin", RunnerStage::Prepared),
    ];
    let first = inputs(vec![h], vec![lin]);

    // when: seen now, then 4 minutes later, then 5 minutes after first seen
    let seen = plan_pass(&first);
    let mut early = first.clone();
    early.now = Millis::new(first.now.get() + 4 * MINUTE);
    early.memory = seen.memory.clone();
    let early_pass = plan_pass(&early);
    let mut late = first.clone();
    late.now = Millis::new(first.now.get() + 5 * MINUTE);
    late.memory = early_pass.memory.clone();
    let late_pass = plan_pass(&late);

    // then
    assert!(retired(&seen.commands).is_empty());
    assert!(retired(&early_pass.commands).is_empty());
    assert_eq!(
        retired(&late_pass.commands),
        vec![(2, RetireReason::Surplus)]
    );
}

#[test]
fn should_restart_the_hysteresis_when_the_surplus_disappears_in_between() {
    // given: a surplus seen, then a job takes it, then the surplus is back
    let mut h = host("h1", &[("lin", 4)]);
    h.leases = vec![lease(1, "lin", RunnerStage::Prepared)];
    let first = inputs(vec![h], vec![linux("lin", Some(0))]);
    let seen = plan_pass(&first);
    let mut demanded = first.clone();
    demanded.pools = vec![linux("lin", Some(1))];
    demanded.now = Millis::new(first.now.get() + 3 * MINUTE);
    demanded.memory = seen.memory;
    let gone = plan_pass(&demanded);

    // when: the surplus is back 6 minutes after it was first seen
    let mut back = first.clone();
    back.now = Millis::new(first.now.get() + 6 * MINUTE);
    back.memory = gone.memory;
    let pass = plan_pass(&back);

    // then: its window starts again
    assert!(retired(&pass.commands).is_empty());
    assert_eq!(pass.memory.surplus_since.get(&pool("lin")), Some(&back.now));
}

#[test]
fn should_hold_the_host_until_a_newer_report_when_the_data_plane_refuses_insufficient_resources() {
    // given: two jobs; one attempt's clone was refused insufficient_resources on h1
    let mut h = host("h1", &[("lin", 8)]);
    let mut refused = lease(1, "lin", RunnerStage::Preparing);
    refused.refusal = Some(7);
    h.leases = vec![refused];
    let first = inputs(vec![h.clone()], vec![linux("lin", Some(3))]);

    // when: the refusal is read, a pass runs again on the same report, then on a newer one
    let held = plan_pass(&first);
    let mut again = first.clone();
    again.memory = held.memory.clone();
    let still = plan_pass(&again);
    let mut newer = again.clone();
    newer.hosts[0].report = Some(report(2, 64, 8));
    newer.memory = still.memory.clone();
    let reread = plan_pass(&newer);

    // then: nothing is reserved on the host, its scale set is told only its runners, until a
    // newer report is applied
    assert!(reserves(&held.commands).is_empty());
    assert_eq!(
        capacity_of(&held.commands, "lin").map(|c| c.reported_max),
        Some(1)
    );
    assert!(reserves(&still.commands).is_empty());
    assert_eq!(reserves(&reread.commands).len(), 2);
    assert!(reread.memory.held.is_empty());
}

#[test]
fn should_reserve_on_another_host_when_one_host_is_held_by_a_refusal() {
    // given
    let mut h1 = host("h1", &[("lin", 8)]);
    let mut refused = lease(1, "lin", RunnerStage::Preparing);
    refused.refusal = Some(3);
    h1.leases = vec![refused];
    let h2 = host("h2", &[("lin", 8)]);

    // when
    let pass = plan_pass(&inputs(vec![h1, h2], vec![linux("lin", Some(2))]));

    // then
    assert_eq!(
        reserves(&pass.commands),
        vec![("h2".to_owned(), "lin".to_owned())]
    );
}

#[test]
fn should_plan_capacity_only_when_in_safe_mode() {
    // given
    let mut h = host("h1", &[("lin", 4)]);
    h.leases = vec![
        lease(1, "lin", RunnerStage::Gone(ReleaseEvidence::VmDeleted)),
        lease(2, "lin", RunnerStage::Prepared),
    ];
    h.draining = true;
    let mut safe = inputs(vec![h], vec![linux("lin", Some(4))]);
    safe.safe_mode = true;

    // when
    let pass = plan_pass(&safe);

    // then
    assert_eq!(pass.commands.len(), 1);
    assert_eq!(
        capacity_of(&pass.commands, "lin").map(|c| c.mode),
        Some(WireMode::SafeMode)
    );
}

#[test]
fn should_retire_prepared_and_listening_and_complete_the_drain_when_the_scope_is_empty() {
    // given
    let mut draining = host("h1", &[("lin", 4)]);
    draining.draining = true;
    draining.admission_open = false;
    draining.leases = vec![
        lease(1, "lin", RunnerStage::Prepared),
        lease(2, "lin", RunnerStage::Listening),
        lease(3, "lin", RunnerStage::Busy),
    ];
    let mut empty = host("h2", &[("lin", 4)]);
    empty.draining = true;
    empty.admission_open = false;

    // when
    let pass = plan_pass(&inputs(vec![draining, empty], vec![linux("lin", Some(9))]));

    // then
    assert_eq!(
        retired(&pass.commands),
        vec![(1, RetireReason::Drain), (2, RetireReason::Drain)]
    );
    assert!(pass.commands.contains(&PassCommand::CompleteHostDrain {
        host_id: host_id("h2")
    }));
    assert!(!pass.commands.contains(&PassCommand::CompleteHostDrain {
        host_id: host_id("h1")
    }));
}

#[test]
fn should_not_retire_below_min_prepared_when_a_draining_host_holds_prepared_vms() {
    // given: min_prepared 2, no job; two prepared VMs on a draining host and one elsewhere
    let mut lin = linux("lin", Some(0));
    lin.min_prepared = 2;
    let mut draining = host("h1", &[("lin", 4)]);
    draining.draining = true;
    draining.admission_open = false;
    draining.leases = vec![
        lease(1, "lin", RunnerStage::Prepared),
        lease(2, "lin", RunnerStage::Prepared),
    ];
    let mut other = host("h2", &[("lin", 4)]);
    other.leases = vec![lease(3, "lin", RunnerStage::Prepared)];
    let first = inputs(vec![draining, other], vec![lin]);

    // when: a pass, and another one past the hysteresis
    let pass = plan_pass(&first);
    let mut late = first.clone();
    late.now = Millis::new(first.now.get() + 6 * MINUTE);
    late.memory = pass.memory.clone();
    let late_pass = plan_pass(&late);

    // then: the draining host's two are retired `drain` and replaced; the third is kept (no
    // surplus), and one more is reserved for min_prepared on the other host
    let need = pass.needs.get(&pool("lin")).copied().unwrap_or_default();
    assert_eq!((need.surplus, need.waiting), (0, 1));
    assert_eq!(
        retired(&pass.commands),
        vec![(1, RetireReason::Drain), (2, RetireReason::Drain)]
    );
    assert_eq!(retired(&late_pass.commands), retired(&pass.commands));
    assert_eq!(
        reserves(&pass.commands),
        vec![("h2".to_owned(), "lin".to_owned())]
    );
}

#[test]
fn should_neither_retire_again_nor_count_an_attempt_when_its_retirement_is_requested() {
    // given: min_prepared 1, no job; two prepared VMs, one of them already retiring
    let mut lin = linux("lin", Some(0));
    lin.min_prepared = 1;
    let mut h = host("h1", &[("lin", 4)]);
    let mut retiring = lease(2, "lin", RunnerStage::Prepared);
    retiring.retiring = true;
    h.leases = vec![lease(1, "lin", RunnerStage::Prepared), retiring];
    let first = inputs(vec![h], vec![lin]);

    // when: past the hysteresis
    let seen = plan_pass(&first);
    let mut late = first.clone();
    late.now = Millis::new(first.now.get() + 6 * MINUTE);
    late.memory = seen.memory.clone();
    let pass = plan_pass(&late);

    // then: no surplus: the retiring one is on its way out and the other is min_prepared
    let need = pass.needs.get(&pool("lin")).copied().unwrap_or_default();
    assert_eq!(
        (need.runners.prepared, need.surplus, need.waiting),
        (1, 0, 0)
    );
    assert!(retired(&pass.commands).is_empty());
}

#[test]
fn should_plan_releases_before_reservations_when_a_pass_does_both() {
    // given: a gone VM's lease and two jobs
    let mut h = host("h1", &[("lin", 4)]);
    h.leases = vec![lease(
        1,
        "lin",
        RunnerStage::Gone(ReleaseEvidence::VmDeleted),
    )];

    // when
    let pass = plan_pass(&inputs(vec![h], vec![linux("lin", Some(2))]));

    // then
    let release = pass
        .commands
        .iter()
        .position(|c| matches!(c, PassCommand::ReleaseSlot { .. }));
    let reserve = pass
        .commands
        .iter()
        .position(|c| matches!(c, PassCommand::ReserveSlot { .. }));
    assert!(
        matches!((release, reserve), (Some(a), Some(b)) if a < b),
        "{:?}",
        pass.commands
    );
}

#[test]
fn should_reserve_nothing_on_a_host_when_the_pin_is_not_placed_there() {
    // given: two hosts with open terms; the pool's pin is materialised on h2 only
    let mut h1 = host("h1", &[("lin", 4)]);
    h1.placed.clear();
    let h2 = host("h2", &[("lin", 4)]);
    let unplaced = {
        let mut h = h2.clone();
        h.placed.clear();
        h
    };

    // when
    let pass = plan_pass(&inputs(vec![h1.clone(), h2], vec![linux("lin", Some(2))]));
    let nowhere = plan_pass(&inputs(vec![h1, unplaced], vec![linux("lin", Some(2))]));

    // then: P17, only where the base is; nothing anywhere before, and nothing advertised
    assert_eq!(
        reserves(&pass.commands),
        vec![
            ("h2".to_owned(), "lin".to_owned()),
            ("h2".to_owned(), "lin".to_owned())
        ]
    );
    assert!(reserves(&nowhere.commands).is_empty());
    assert_eq!(
        capacity_of(&nowhere.commands, "lin").map_or(0, |c| c.reported_max),
        0
    );
}

#[test]
fn should_cap_reservations_by_the_hosts_disk_when_the_placed_base_is_measured() {
    // given: 1000 GiB of disk; a pool with no configured disk (`clone_disk_gib` absent) and 4
    // jobs; its base measured at 400 GiB on h1, not measured on h2 (an older data plane)
    let mut lin = linux("lin", Some(4));
    lin.vm = Resources::new(4 * GIB, 0);
    let mut measured = host("h1", &[("lin", 4)]);
    measured
        .placed
        .insert(ImageDigest::sample(), Some(400 * GIB));
    let unmeasured = host("h2", &[("lin", 4)]);

    // when
    let capped = plan_pass(&inputs(vec![measured], vec![lin.clone()]));
    let unknown = plan_pass(&inputs(vec![unmeasured], vec![lin]));

    // then: two 400 GiB clones fit 1000 GiB; an unmeasured base plans none (0, the data plane's
    // gate refuses what does not fit, I33), never a default
    let disks = |pass: &mayfly_control_domain::reconcile::Pass| -> Vec<u64> {
        pass.commands
            .iter()
            .filter_map(|c| match c {
                PassCommand::ReserveSlot { resources, .. } => Some(resources.disk_bytes),
                _ => None,
            })
            .collect()
    };
    assert_eq!(disks(&capped), vec![400 * GIB, 400 * GIB]);
    assert_eq!(disks(&unknown), vec![0, 0, 0, 0]);
}

#[test]
fn should_retire_at_once_when_a_prepared_vm_runs_an_unpinned_image() {
    // given
    let mut h = host("h1", &[("lin", 4)]);
    let mut old = lease(1, "lin", RunnerStage::Prepared);
    old.image = Some(
        ImageDigest::new(&format!("sha256:{}", "b".repeat(64))).unwrap_or_else(|e| panic!("{e}")),
    );
    h.leases = vec![old];

    // when
    let pass = plan_pass(&inputs(vec![h], vec![linux("lin", Some(1))]));

    // then
    assert_eq!(
        retired(&pass.commands),
        vec![(1, RetireReason::ImageReplaced)]
    );
}

#[test]
fn should_release_a_gone_vm_at_once_and_an_unstarted_lease_after_the_grace_when_cleaning_up() {
    // given
    let mut h = host("h1", &[("lin", 4)]);
    let mut quarantined = lease(3, "lin", RunnerStage::Quarantined);
    quarantined.quarantined = true;
    h.leases = vec![
        lease(1, "lin", RunnerStage::Gone(ReleaseEvidence::AttemptLost)),
        lease(2, "lin", RunnerStage::NotStarted),
        quarantined,
    ];
    let first = inputs(vec![h], vec![linux("lin", Some(0))]);

    // when
    let now = plan_pass(&first);
    let mut later = first.clone();
    later.now = Millis::new(first.now.get() + 2 * MINUTE);
    later.memory = now.memory.clone();
    let after_grace = plan_pass(&later);

    // then
    let released = |commands: &[PassCommand]| -> Vec<(u128, ReleaseEvidence)> {
        commands
            .iter()
            .filter_map(|c| match c {
                PassCommand::ReleaseSlot {
                    attempt_id,
                    evidence,
                    ..
                } => Some((attempt_id.as_u128(), *evidence)),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        released(&now.commands),
        vec![(1, ReleaseEvidence::AttemptLost)]
    );
    assert_eq!(
        released(&after_grace.commands),
        vec![
            (1, ReleaseEvidence::AttemptLost),
            (2, ReleaseEvidence::AttemptNeverStarted)
        ]
    );
    assert!(retired(&after_grace.commands).is_empty());
}

#[test]
fn should_plan_nothing_more_when_a_pass_is_repeated_on_its_own_result() {
    // given: two hosts, two pools with jobs
    let hosts = vec![
        host("h1", &[("a", 3), ("b", 3)]),
        host("h2", &[("a", 3), ("b", 3)]),
    ];
    let pools = vec![linux("a", Some(4)), linux("b", Some(2))];
    let mut state = inputs(hosts, pools);

    // when: the first pass's commands are applied as the aggregates would, then a pass again
    let first = plan_pass(&state);
    let mut n = 100;
    for command in &first.commands {
        match command {
            PassCommand::ReserveSlot { host_id, pool, .. } => {
                n += 1;
                if let Some(h) = state.hosts.iter_mut().find(|h| &h.host_id == host_id) {
                    h.leases
                        .push(lease(n, pool.as_str(), RunnerStage::NotStarted));
                }
            }
            PassCommand::ApplyCapacity { pool, capacity } => {
                if let Some(p) = state.pools.iter_mut().find(|p| &p.pool == pool) {
                    p.scale_set = p.scale_set.map(|s| ScaleSetView {
                        capacity: *capacity,
                        ..s
                    });
                }
            }
            _ => {}
        }
    }
    state.memory = first.memory.clone();
    let second = plan_pass(&state);

    // then
    assert_eq!(reserves(&first.commands).len(), 6);
    assert!(second.commands.is_empty(), "{:?}", second.commands);
}

fn generated() -> impl Strategy<Value = (PassInputs, u64)> {
    let lease_strategy = (
        1_u128..40,
        prop_oneof![Just("a"), Just("b"), Just("c")],
        0_usize..8,
        any::<bool>(),
    );
    let host_strategy = (
        prop_oneof![Just("h1"), Just("h2"), Just("h3")],
        8_u64..64,
        1_u32..8,
        any::<bool>(),
        proptest::collection::vec(lease_strategy, 0..6),
    );
    (
        proptest::collection::vec(host_strategy, 1..4),
        proptest::collection::vec((0_u32..6, 0_u32..2, any::<bool>()), 3),
        any::<u64>(),
    )
        .prop_map(|(hosts, pools, seed)| {
            let stages = [
                RunnerStage::NotStarted,
                RunnerStage::Preparing,
                RunnerStage::Prepared,
                RunnerStage::Activating,
                RunnerStage::Listening,
                RunnerStage::Busy,
                RunnerStage::Ending,
                RunnerStage::Gone(ReleaseEvidence::VmDeleted),
            ];
            let names = ["a", "b", "c"];
            let mut seen = std::collections::BTreeSet::new();
            let mut named = std::collections::BTreeSet::new();
            let hosts = hosts
                .into_iter()
                .filter(|(name, ..)| named.insert(*name))
                .map(|(name, memory, slots, open, leases)| {
                    let mut h = host(name, &[("a", 3), ("b", 2), ("c", 4)]);
                    h.report = Some(report(1, memory, slots));
                    h.admission_open = open;
                    h.leases = leases
                        .into_iter()
                        .filter(|(n, ..)| seen.insert(*n))
                        .map(|(n, p, stage, refused)| {
                            let mut l = lease(n, p, stages[stage % stages.len()]);
                            l.refusal = refused.then_some(1);
                            l
                        })
                        .collect();
                    h
                })
                .collect();
            let pools = pools
                .into_iter()
                .zip(names)
                .map(|((assigned, min_prepared, on_demand), name)| {
                    let mut p = linux(name, Some(assigned));
                    p.min_prepared = min_prepared;
                    if on_demand {
                        p.policy = demand;
                    }
                    p
                })
                .collect();
            (inputs(hosts, pools), seed)
        })
}

fn shuffled<T>(mut items: Vec<T>, seed: u64) -> Vec<T> {
    let mut state = seed | 1;
    for i in (1..items.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let j = usize::try_from(state % (i as u64 + 1)).unwrap_or(0);
        items.swap(i, j);
    }
    items
}

proptest! {
    #[test]
    fn should_plan_the_same_pass_when_inputs_are_permuted((inputs, seed) in generated()) {
        // given
        let mut permuted = inputs.clone();
        permuted.hosts = shuffled(permuted.hosts, seed)
            .into_iter()
            .map(|mut h| {
                h.leases = shuffled(h.leases, seed.rotate_left(7));
                h
            })
            .collect();
        permuted.pools = shuffled(permuted.pools, seed.rotate_left(3));

        // when
        let once = plan_pass(&inputs);
        let twice = plan_pass(&inputs);
        let other = plan_pass(&permuted);

        // then
        prop_assert_eq!(&once, &twice);
        prop_assert_eq!(&once, &other);
    }

    #[test]
    fn should_reserve_within_each_hosts_carve_outs_when_inputs_are_random((inputs, _seed) in generated()) {
        // given: random hosts, leases and pools, at least one host
        prop_assume!(!inputs.hosts.is_empty());

        // when
        let pass = plan_pass(&inputs);

        // then: per host and pool, reservations never exceed the carve-out (I24, I03), and a
        // closed or held host gets none (I26)
        let mut per: BTreeMap<(String, String), u32> = BTreeMap::new();
        for (host_id, pool_name) in reserves(&pass.commands) {
            *per.entry((host_id, pool_name)).or_default() += 1;
        }
        for ((host_id, pool_name), n) in per {
            let report = pass
                .reports
                .iter()
                .find(|(h, _)| h.as_str() == host_id)
                .map(|(_, r)| r.clone());
            let carve = report
                .and_then(|r| r.pools.into_iter().find(|l| l.pool.as_str() == pool_name))
                .map_or(0, |l| l.carve_out);
            prop_assert!(n <= carve, "{host_id} {pool_name}: {n} > {carve}");
            let open = inputs
                .hosts
                .iter()
                .find(|h| h.host_id.as_str() == host_id)
                .is_some_and(|h| h.admission_open && h.leases.iter().all(|l| l.refusal.is_none()));
            prop_assert!(open, "{host_id} closed or held");
        }
        for pool_name in pass.needs.keys() {
            if let Some(capacity) = capacity_of(&pass.commands, pool_name.as_str()) {
                prop_assert!(capacity.reported_max <= capacity.deliverable);
            }
        }
    }
}
