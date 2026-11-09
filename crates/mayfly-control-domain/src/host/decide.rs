//! `decide` and `evolve` for the host: every command against the host's state, as explicit
//! match arms with no wildcard (`ARCH-exhaustive-transition`); the outcome of every pair of a
//! named state and a command is pinned by
//! `tests/host/matrix.rs::should_define_every_pair_when_each_state_meets_each_command`.

use mayfly_domain::{AttemptId, Deadline, Epoch, Generation, Slot};

use super::events::{
    AdmissionClosed, AdmissionOpened, CapacityReportApplied, DiskLowWaterCrossed, DiskRecovered,
    DrainForced, HostDrainCancelled, HostDrainCompleted, HostDrainStarted, HostEpochAdvanced,
    HostEvent, HostRegistered, LeaseActivated, LeaseQuarantined, PoolTermsApplied, SlotReleased,
    SlotReserved,
};
use super::state::{
    AppliedReport, ApplyPoolTerms, Drain, ForceScope, Holds, HostCommand, HostPoolTerms,
    HostRejection, HostState, Lease, LeasePhase, ReleaseEvidence, ReserveSlot, ResourceThreshold,
};
use crate::capacity::{self, Dimension, Footprint};
use crate::command::{Actor, Command};

/// What `decide` returns.
pub type Decision = Result<Vec<HostEvent>, HostRejection>;

/// Decides `command` against `state`. An older run epoch is refused (I18); an accepted command
/// under a newer one records `HostEpochAdvanced` ahead of its events.
///
/// # Errors
/// The [`HostRejection`] that names why the command is not allowed.
pub fn decide(state: &HostState, command: Command<HostCommand>) -> Decision {
    let Command {
        run_epoch,
        actor,
        body,
        ..
    } = command;
    if !state.registered {
        return match body {
            HostCommand::RegisterHost { reserves } => Ok(with_epoch(
                state.epoch,
                run_epoch,
                vec![HostEvent::HostRegistered(HostRegistered { reserves })],
            )),
            HostCommand::RecordCapacityReport { .. }
            | HostCommand::ApplyPoolTerms(..)
            | HostCommand::ReserveSlot(..)
            | HostCommand::GrantActivation { .. }
            | HostCommand::QuarantineLease { .. }
            | HostCommand::ReleaseSlot { .. }
            | HostCommand::RecordResourceThreshold { .. }
            | HostCommand::CloseAdmission { .. }
            | HostCommand::OpenAdmission { .. }
            | HostCommand::StartHostDrain { .. }
            | HostCommand::CancelHostDrain
            | HostCommand::CompleteHostDrain
            | HostCommand::ForceDrain { .. } => Err(HostRejection::NotRegistered),
        };
    }
    if run_epoch < state.epoch {
        return Err(HostRejection::StaleRun);
    }
    let events = decide_registered(state, actor, body)?;
    Ok(with_epoch(state.epoch, run_epoch, events))
}

fn with_epoch(stored: Epoch, run_epoch: Epoch, events: Vec<HostEvent>) -> Vec<HostEvent> {
    if events.is_empty() || run_epoch <= stored {
        return events;
    }
    let mut all = Vec::with_capacity(events.len() + 1);
    all.push(HostEvent::HostEpochAdvanced(HostEpochAdvanced {
        epoch: run_epoch,
    }));
    all.extend(events);
    all
}

fn decide_registered(state: &HostState, actor: Actor, body: HostCommand) -> Decision {
    match body {
        HostCommand::RegisterHost { .. } => Err(HostRejection::AlreadyRegistered),
        HostCommand::RecordCapacityReport {
            report_id,
            capacity,
        } => Ok(decide_report(state, report_id, capacity)),
        HostCommand::ApplyPoolTerms(terms) => decide_terms(state, terms),
        HostCommand::ReserveSlot(reserve) => decide_reserve(state, reserve),
        HostCommand::GrantActivation { attempt_id } => decide_grant(state, attempt_id),
        HostCommand::QuarantineLease {
            attempt_id,
            vm_running,
        } => decide_quarantine(state, attempt_id, vm_running),
        HostCommand::ReleaseSlot {
            attempt_id,
            evidence,
        } => decide_release(state, attempt_id, evidence),
        HostCommand::RecordResourceThreshold { threshold } => {
            Ok(decide_threshold(state, threshold))
        }
        HostCommand::CloseAdmission { cause } => Ok(if state.gate.closed.contains(&cause) {
            vec![]
        } else {
            vec![HostEvent::AdmissionClosed(AdmissionClosed { cause })]
        }),
        HostCommand::OpenAdmission { cause } => Ok(if state.gate.closed.contains(&cause) {
            vec![HostEvent::AdmissionOpened(AdmissionOpened { cause })]
        } else {
            vec![]
        }),
        HostCommand::StartHostDrain { deadline } => decide_start_drain(state, deadline),
        HostCommand::CancelHostDrain => match state.gate.drain {
            Drain::Open => Err(HostRejection::NotDraining),
            Drain::Draining { .. } | Drain::Drained => {
                Ok(vec![HostEvent::HostDrainCancelled(HostDrainCancelled {})])
            }
        },
        HostCommand::CompleteHostDrain => match state.gate.drain {
            Drain::Draining { .. } if state.leases.is_empty() => {
                Ok(vec![HostEvent::HostDrainCompleted(HostDrainCompleted {})])
            }
            Drain::Draining { .. } => Err(HostRejection::LeasesRemaining),
            Drain::Open | Drain::Drained => Err(HostRejection::NotDraining),
        },
        HostCommand::ForceDrain { scope } => decide_force(state, actor, scope),
    }
}

/// ADR 0024: a newer report is applied; an older (or the same) `report_id` is ignored, so a
/// stale report never widens what the host allocates within. Pinned by
/// `tests/host/rules.rs::should_never_allocate_beyond_the_newer_report_when_a_stale_one_arrives`.
fn decide_report(
    state: &HostState,
    report_id: u64,
    capacity: capacity::ReportedCapacity,
) -> Vec<HostEvent> {
    if state
        .capacity
        .is_some_and(|applied| report_id <= applied.report_id)
    {
        return vec![];
    }
    vec![HostEvent::CapacityReportApplied(CapacityReportApplied {
        report_id,
        capacity,
    })]
}

fn decide_terms(state: &HostState, terms: ApplyPoolTerms) -> Decision {
    if state
        .pool_terms
        .get(&terms.pool)
        .is_some_and(|applied| terms.revision <= applied.revision)
    {
        return Err(HostRejection::StaleTerms);
    }
    Ok(vec![HostEvent::PoolTermsApplied(PoolTermsApplied {
        pool: terms.pool,
        revision: terms.revision,
        open: terms.open,
        max_running: terms.max_running,
        tier: terms.tier,
    })])
}

/// Whether the drain or another cause closes admission: a drain first (I08), then the rest
/// (I26).
fn gate_refusal(state: &HostState) -> Option<HostRejection> {
    match state.gate.drain {
        Drain::Draining { .. } | Drain::Drained => Some(HostRejection::Draining),
        Drain::Open if !state.gate.is_open() => Some(HostRejection::AdmissionClosed),
        Drain::Open => None,
    }
}

const fn dimension_refusal(dimension: Dimension) -> HostRejection {
    match dimension {
        Dimension::VmSlots => HostRejection::NoFreeSlot,
        Dimension::MacosGuests => HostRejection::MacosGuests,
        Dimension::Memory => HostRejection::InsufficientMemory,
        Dimension::Disk => HostRejection::InsufficientDisk,
    }
}

/// `ReserveSlot`, in order: one lease per attempt; nothing while draining (I08) or closed
/// (I26); nothing without a report (ADR 0024); only under the pool's applied, open terms at the
/// revision the reconciler read; within `max_running`, quarantined leases counted (I28); within
/// the last report on every dimension, every live lease counted (I03, G3); then the lowest free
/// slot under its next generation (I01, I02).
fn decide_reserve(state: &HostState, reserve: ReserveSlot) -> Decision {
    if state.leases.contains_key(&reserve.attempt_id) {
        return Err(HostRejection::AlreadyLeased);
    }
    if let Some(refusal) = gate_refusal(state) {
        return Err(refusal);
    }
    if state.capacity.is_none() {
        return Err(HostRejection::NoCapacityReport);
    }
    let Some(terms) = state.pool_terms.get(&reserve.pool) else {
        return Err(HostRejection::UnknownPool);
    };
    if terms.revision != reserve.terms_revision {
        return Err(HostRejection::TermsChanged);
    }
    if !terms.open {
        return Err(HostRejection::TermsClosed);
    }
    let in_pool: Vec<&Lease> = state
        .leases
        .values()
        .filter(|lease| lease.pool == reserve.pool)
        .collect();
    if u32::try_from(in_pool.len()).unwrap_or(u32::MAX) >= terms.max_running {
        return Err(if in_pool.iter().any(|lease| lease.is_quarantined()) {
            HostRejection::QuarantineHeld
        } else {
            HostRejection::PoolFull
        });
    }
    let demand = Footprint::vm(reserve.os, reserve.resources);
    capacity::fit(state.admissible(), state.allocated(), demand).map_err(dimension_refusal)?;
    let Some((slot, generation)) = free_slot(state) else {
        return Err(HostRejection::NoFreeSlot);
    };
    Ok(vec![HostEvent::SlotReserved(SlotReserved {
        attempt_id: reserve.attempt_id,
        slot,
        generation,
        pool: reserve.pool,
        os: reserve.os,
        resources: reserve.resources,
    })])
}

/// The lowest slot below the report's VM slots that no lease holds and whose generation can
/// still grow, with that next generation. Pinned by
/// `tests/host/properties.rs::should_strictly_increase_generations_per_slot_when_commands_are_random`.
fn free_slot(state: &HostState) -> Option<(Slot, Generation)> {
    let slots = state.capacity.map_or(0, |r| r.capacity.vm_slots);
    (0..slots).map(Slot::new).find_map(|slot| {
        if state.leases.values().any(|lease| lease.slot == slot) {
            return None;
        }
        let next = match state.slots.get(&slot) {
            Some(last) => last.next(),
            None => Some(Generation::FIRST),
        };
        next.map(|generation| (slot, generation))
    })
}

/// I26 and I08: no grant while the gate or the pool's terms are closed, nor after drain start.
/// Pinned by `tests/host/rules.rs::should_refuse_a_grant_when_any_term_or_cause_is_closed`.
fn decide_grant(state: &HostState, attempt_id: AttemptId) -> Decision {
    let Some(lease) = state.leases.get(&attempt_id) else {
        return Err(HostRejection::NoLease);
    };
    match lease.phase {
        LeasePhase::Activated => return Err(HostRejection::AlreadyActivated),
        LeasePhase::Quarantined { .. } => return Err(HostRejection::Quarantined),
        LeasePhase::Reserved => {}
    }
    if let Some(refusal) = gate_refusal(state) {
        return Err(refusal);
    }
    match state.pool_terms.get(&lease.pool) {
        None => Err(HostRejection::UnknownPool),
        Some(HostPoolTerms { open: false, .. }) => Err(HostRejection::TermsClosed),
        Some(HostPoolTerms { open: true, .. }) => {
            Ok(vec![HostEvent::LeaseActivated(LeaseActivated {
                attempt_id,
            })])
        }
    }
}

/// A quarantined lease keeps its slot and resources, shrinking to disk and slots once its VM is
/// stopped (I28); the same holds again record nothing.
fn decide_quarantine(state: &HostState, attempt_id: AttemptId, vm_running: bool) -> Decision {
    let Some(lease) = state.leases.get(&attempt_id) else {
        return Err(HostRejection::NoLease);
    };
    let holds = if vm_running {
        Holds::Full
    } else {
        Holds::DiskAndSlot
    };
    let unchanged = match lease.phase {
        LeasePhase::Quarantined { holds: current } => current == holds,
        LeasePhase::Reserved | LeasePhase::Activated => false,
    };
    if unchanged {
        return Ok(vec![]);
    }
    Ok(vec![HostEvent::LeaseQuarantined(LeaseQuarantined {
        attempt_id,
        holds,
    })])
}

/// A lease is released only on evidence that its VM is gone, never on a timer or a cap (I28);
/// "never started" is evidence only for a lease that was merely reserved.
fn decide_release(state: &HostState, attempt_id: AttemptId, evidence: ReleaseEvidence) -> Decision {
    let Some(lease) = state.leases.get(&attempt_id) else {
        return Err(HostRejection::NoLease);
    };
    match (evidence, lease.phase) {
        (ReleaseEvidence::AttemptNeverStarted, LeasePhase::Activated) => {
            Err(HostRejection::AlreadyActivated)
        }
        (ReleaseEvidence::AttemptNeverStarted, LeasePhase::Quarantined { .. }) => {
            Err(HostRejection::Quarantined)
        }
        (ReleaseEvidence::AttemptNeverStarted, LeasePhase::Reserved)
        | (
            ReleaseEvidence::VmDeleted | ReleaseEvidence::AttemptLost,
            LeasePhase::Reserved | LeasePhase::Activated | LeasePhase::Quarantined { .. },
        ) => Ok(vec![HostEvent::SlotReleased(SlotReleased {
            attempt_id,
            slot: lease.slot,
            generation: lease.generation,
            evidence,
        })]),
    }
}

fn decide_threshold(state: &HostState, threshold: ResourceThreshold) -> Vec<HostEvent> {
    match (threshold, state.gate.disk_low) {
        (ResourceThreshold::DiskLow, false) => {
            vec![HostEvent::DiskLowWaterCrossed(DiskLowWaterCrossed {})]
        }
        (ResourceThreshold::DiskOk, true) => vec![HostEvent::DiskRecovered(DiskRecovered {})],
        (ResourceThreshold::DiskLow, true) | (ResourceThreshold::DiskOk, false) => vec![],
    }
}

fn decide_start_drain(state: &HostState, deadline: Deadline) -> Decision {
    match state.gate.drain {
        Drain::Open => Ok(vec![HostEvent::HostDrainStarted(HostDrainStarted {
            deadline,
        })]),
        Drain::Draining { .. } | Drain::Drained => Err(HostRejection::AlreadyDraining),
    }
}

/// Force is operator-only and never automatic: the host must be draining, or the pool's terms
/// closed.
fn decide_force(state: &HostState, actor: Actor, scope: ForceScope) -> Decision {
    let operator_uid = match actor {
        Actor::Operator { uid } => uid,
        Actor::Automation { .. } | Actor::Startup => return Err(HostRejection::OperatorOnly),
    };
    match &scope {
        ForceScope::Host => match state.gate.drain {
            Drain::Draining { .. } => {}
            Drain::Open | Drain::Drained => return Err(HostRejection::NotDraining),
        },
        ForceScope::Pool { pool } => match state.pool_terms.get(pool) {
            None => return Err(HostRejection::UnknownPool),
            Some(HostPoolTerms { open: true, .. }) => return Err(HostRejection::TermsOpen),
            Some(HostPoolTerms { open: false, .. }) => {}
        },
    }
    Ok(vec![HostEvent::DrainForced(DrainForced {
        scope,
        operator_uid,
    })])
}

/// Applies one recorded event.
#[must_use]
pub fn evolve(mut state: HostState, event: &HostEvent) -> HostState {
    match event {
        HostEvent::HostEpochAdvanced(e) => state.epoch = e.epoch,
        HostEvent::HostRegistered(e) => {
            state.registered = true;
            state.reserves = e.reserves;
        }
        HostEvent::CapacityReportApplied(e) => {
            state.capacity = Some(AppliedReport {
                report_id: e.report_id,
                capacity: e.capacity,
            });
        }
        HostEvent::PoolTermsApplied(e) => {
            state.pool_terms.insert(
                e.pool.clone(),
                HostPoolTerms {
                    revision: e.revision,
                    open: e.open,
                    max_running: e.max_running,
                    tier: e.tier,
                },
            );
        }
        HostEvent::SlotReserved(e) => {
            state.slots.insert(e.slot, e.generation);
            state.leases.insert(
                e.attempt_id,
                Lease {
                    slot: e.slot,
                    generation: e.generation,
                    pool: e.pool.clone(),
                    os: e.os,
                    resources: e.resources,
                    phase: LeasePhase::Reserved,
                },
            );
        }
        HostEvent::LeaseActivated(e) => {
            if let Some(lease) = state.leases.get_mut(&e.attempt_id) {
                lease.phase = LeasePhase::Activated;
            }
        }
        HostEvent::LeaseQuarantined(e) => {
            if let Some(lease) = state.leases.get_mut(&e.attempt_id) {
                lease.phase = LeasePhase::Quarantined { holds: e.holds };
            }
        }
        HostEvent::SlotReleased(e) => {
            state.leases.remove(&e.attempt_id);
        }
        HostEvent::DiskLowWaterCrossed(_) => state.gate.disk_low = true,
        HostEvent::DiskRecovered(_) => state.gate.disk_low = false,
        HostEvent::AdmissionClosed(e) => {
            state.gate.closed.insert(e.cause);
        }
        HostEvent::AdmissionOpened(e) => {
            state.gate.closed.remove(&e.cause);
        }
        HostEvent::HostDrainStarted(e) => {
            state.gate.drain = Drain::Draining {
                deadline: e.deadline,
            };
        }
        HostEvent::HostDrainCancelled(_) => state.gate.drain = Drain::Open,
        HostEvent::HostDrainCompleted(_) => state.gate.drain = Drain::Drained,
        HostEvent::DrainForced(_) => {}
    }
    state
}
