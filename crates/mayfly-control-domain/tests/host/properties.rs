//! The invariants of `formal/scheduling.qnt`, asserted on the Rust `Host` over random command
//! sequences: a check of the code, where the model's invariants check the model.

use std::collections::BTreeMap;

use mayfly_control_domain::attempt::GuestOs;
use mayfly_control_domain::capacity::{
    self, CapacityPolicy, Footprint, PlanDecision, ReportedCapacity,
};
use mayfly_control_domain::host::{
    Drain, HostCommand, HostEvent, HostState, PoolDemand, ReleaseEvidence, ReserveSlot,
    ResourceThreshold,
};
use mayfly_domain::{Resources, Slot};
use proptest::prelude::*;

use super::support::{
    CAUSES, GIB, apply, attempt, auto, drain, grant, pool, quarantine, reach, record, register,
    release, terms,
};

/// One random host command over a small space: two pools, six attempt ids, a few reports.
fn command() -> impl Strategy<Value = HostCommand> {
    let report = (1_u64..6, 4_u64..64, 0_u32..6, 0_u32..4, 0_u32..3, 0_u64..8).prop_map(
        |(id, memory, slots, macos, foreign_macos, foreign_memory)| {
            record(
                id,
                ReportedCapacity {
                    total: Resources::new(memory * GIB, 300 * GIB),
                    foreign: Resources::new(foreign_memory * GIB, 0),
                    base: Resources::new(0, 40 * GIB),
                    vm_slots: slots,
                    foreign_vms: foreign_macos,
                    macos_guests: macos,
                    foreign_macos_guests: foreign_macos,
                },
            )
        },
    );
    let pools = prop_oneof![Just("pool0"), Just("pool1")];
    let os = prop_oneof![Just(GuestOs::Macos), Just(GuestOs::Linux)];
    let reserve = (pools.clone(), 0_u128..6, os, 1_u64..16, 0_u64..4).prop_map(
        |(name, n, os, memory, revision)| {
            HostCommand::ReserveSlot(ReserveSlot {
                attempt_id: attempt(n),
                pool: pool(name),
                os,
                resources: Resources::new(memory * GIB, 20 * GIB),
                terms_revision: revision,
            })
        },
    );
    let evidence = prop_oneof![
        Just(ReleaseEvidence::VmDeleted),
        Just(ReleaseEvidence::AttemptLost),
        Just(ReleaseEvidence::AttemptNeverStarted),
    ];
    prop_oneof![
        3 => report,
        2 => (pools, 0_u64..4, any::<bool>(), 0_u32..4)
            .prop_map(|(name, revision, open, max)| terms(name, revision, open, max)),
        6 => reserve,
        2 => (0_u128..6).prop_map(grant),
        2 => (0_u128..6, any::<bool>()).prop_map(|(n, running)| quarantine(n, running)),
        3 => (0_u128..6, evidence).prop_map(|(n, e)| release(n, e)),
        1 => Just(drain()),
        1 => Just(HostCommand::CancelHostDrain),
        1 => Just(HostCommand::CompleteHostDrain),
        1 => (0_usize..4).prop_map(|i| HostCommand::CloseAdmission { cause: CAUSES[i] }),
        1 => (0_usize..4).prop_map(|i| HostCommand::OpenAdmission { cause: CAUSES[i] }),
        1 => prop_oneof![Just(ResourceThreshold::DiskLow), Just(ResourceThreshold::DiskOk)]
            .prop_map(|threshold| HostCommand::RecordResourceThreshold { threshold }),
    ]
}

fn commands() -> impl Strategy<Value = Vec<HostCommand>> {
    prop::collection::vec(command(), 0..60)
}

/// Whether every dimension `demand` uses is within the last report after the reservation (a
/// dimension it does not use may already be over-allocated by a shrunk report).
fn within_used(after: &HostState, demand: Footprint) -> bool {
    let total = after.allocated();
    let limit = after.admissible();
    (demand.vms == 0 || total.vms <= limit.vms)
        && (demand.macos_guests == 0 || total.macos_guests <= limit.macos_guests)
        && (demand.resources.memory_bytes == 0
            || total.resources.memory_bytes <= limit.resources.memory_bytes)
        && (demand.resources.disk_bytes == 0
            || total.resources.disk_bytes <= limit.resources.disk_bytes)
}

/// Why `before --events--> after` breaks an invariant of the scheduling model, if it does.
fn broken(
    before: &HostState,
    events: &[HostEvent],
    after: &HostState,
    generations: &mut BTreeMap<Slot, u32>,
) -> Option<String> {
    for event in events {
        match event {
            HostEvent::SlotReserved(r) => {
                let last = generations.insert(r.slot, r.generation.get()).unwrap_or(0);
                if r.generation.get() <= last {
                    return Some(format!(
                        "I02: slot {:?} reserved at {:?} after {last}",
                        r.slot, r.generation
                    ));
                }
                if !before.gate.is_open() {
                    return Some("I08/I26: a reservation with the gate closed".to_owned());
                }
                if !within_used(after, Footprint::vm(r.os, r.resources)) {
                    return Some("I03: a reservation beyond the last report".to_owned());
                }
                if !before.pool_terms.get(&r.pool).is_some_and(|t| t.open) {
                    return Some("I26: a reservation under closed terms".to_owned());
                }
            }
            HostEvent::LeaseActivated(a) => {
                let open = before
                    .leases
                    .get(&a.attempt_id)
                    .and_then(|l| before.pool_terms.get(&l.pool))
                    .is_some_and(|t| t.open);
                if !before.gate.is_open() || !open {
                    return Some("I26: a grant while closed".to_owned());
                }
            }
            HostEvent::CapacityReportApplied(r) => {
                if before.capacity.is_some_and(|c| r.report_id <= c.report_id) {
                    return Some("allocationWithinReport: an older report applied".to_owned());
                }
            }
            HostEvent::HostDrainCompleted(_) => {
                if !before.leases.is_empty() {
                    return Some("I08: drain completed with leases".to_owned());
                }
            }
            HostEvent::SlotReleased(r) => {
                let was_quarantined = before
                    .leases
                    .get(&r.attempt_id)
                    .is_some_and(|l| l.is_quarantined());
                if was_quarantined && r.evidence == ReleaseEvidence::AttemptNeverStarted {
                    return Some(
                        "I28: a quarantined lease released without its VM's absence".to_owned(),
                    );
                }
            }
            HostEvent::HostEpochAdvanced(_)
            | HostEvent::HostRegistered(_)
            | HostEvent::PoolTermsApplied(_)
            | HostEvent::LeaseQuarantined(_)
            | HostEvent::DiskLowWaterCrossed(_)
            | HostEvent::DiskRecovered(_)
            | HostEvent::AdmissionClosed(_)
            | HostEvent::AdmissionOpened(_)
            | HostEvent::HostDrainStarted(_)
            | HostEvent::HostDrainCancelled(_)
            | HostEvent::DrainForced(_) => {}
        }
    }
    let mut slots: Vec<Slot> = after.leases.values().map(|l| l.slot).collect();
    slots.sort();
    slots.dedup();
    if slots.len() != after.leases.len() {
        return Some("I01: two leases on one slot".to_owned());
    }
    if after
        .leases
        .values()
        .any(|l| after.slots.get(&l.slot) != Some(&l.generation))
    {
        return Some(
            "generationsMonotonic: a lease's generation is not its slot's last".to_owned(),
        );
    }
    None
}

proptest! {
    #[test]
    fn should_strictly_increase_generations_per_slot_when_commands_are_random(cmds in commands()) {
        // given
        let mut state = reach([register()]);
        let mut generations = BTreeMap::new();
        // when
        for body in cmds {
            if let Ok((after, events)) = apply(&state, auto(body)) {
                // then
                let problem = broken(&state, &events, &after, &mut generations);
                prop_assert!(problem.is_none(), "{problem:?}");
                state = after;
            }
        }
    }

    #[test]
    fn should_allocate_within_the_last_report_when_commands_are_random(cmds in commands()) {
        // given
        let mut state = reach([register()]);
        // when
        for body in cmds {
            let reserving = matches!(body, HostCommand::ReserveSlot(_));
            if let Ok((after, _)) = apply(&state, auto(body)) {
                // then: a reservation keeps every live lease within the last report, and at
                // most two macOS guests with the foreign ones
                if reserving {
                    let lease = after.leases.values().count().checked_sub(state.leases.len());
                    prop_assert_eq!(lease, Some(1));
                    let added = after
                        .leases
                        .iter()
                        .find(|(id, _)| !state.leases.contains_key(id))
                        .map(|(_, l)| Footprint::vm(l.os, l.resources));
                    prop_assert!(added.is_some_and(|demand| within_used(&after, demand)));
                    if added.is_some_and(|demand| demand.macos_guests > 0) {
                        let foreign = after.capacity.map_or(0, |c| c.capacity.foreign_macos_guests);
                        prop_assert!(after.allocated().macos_guests + foreign <= 2);
                    }
                }
                if !matches!(after.gate.drain, Drain::Open) {
                    prop_assert!(!reserving || after.leases.len() == state.leases.len());
                }
                state = after;
            }
        }
    }

    #[test]
    fn should_admit_every_planned_reservation_when_the_host_decides_them(
        cmds in commands(),
        wanted in (0_u32..4, 0_u32..4),
    ) {
        // given: any reachable host, and demand in both pools
        let mut state = reach([register()]);
        for body in cmds {
            if let Ok((after, _)) = apply(&state, auto(body)) {
                state = after;
            }
        }
        let demand = [
            PoolDemand { pool: pool("pool0"), os: GuestOs::Macos, vm: Resources::new(8 * GIB, 20 * GIB), wanted: wanted.0, policy: CapacityPolicy::Exact },
            PoolDemand { pool: pool("pool1"), os: GuestOs::Linux, vm: Resources::new(4 * GIB, 20 * GIB), wanted: wanted.1, policy: CapacityPolicy::Exact },
        ];
        // when: the policy's reservations are decided one by one
        let (_, decisions) = capacity::plan(&state.plan_inputs(&demand));
        let mut next_attempt = 100;
        for decision in decisions {
            if let PlanDecision::Reserve { pool, os, resources } = decision {
                let revision = state.pool_terms.get(&pool).map_or(0, |t| t.revision);
                let body = HostCommand::ReserveSlot(ReserveSlot {
                    attempt_id: attempt(next_attempt),
                    pool,
                    os,
                    resources,
                    terms_revision: revision,
                });
                next_attempt += 1;
                // then: the host admits each one (carveOutAdmissible)
                let result = apply(&state, auto(body));
                prop_assert!(result.is_ok(), "{result:?}");
                if let Ok((after, _)) = result {
                    state = after;
                }
            }
        }
    }
}
