//! The host's events (`doc/domain-model.md` §6.3), with golden samples under
//! `tests/fixtures/events/<type>/`.

use mayfly_domain::{AttemptId, Deadline, Epoch, Generation, PoolName, Resources, Slot};
use serde::{Deserialize, Serialize};

use super::state::{AdmissionCause, ForceScope, Holds, ReleaseEvidence};
use crate::attempt::GuestOs;
use crate::capacity::ReportedCapacity;
use crate::event::DomainEvent;
use crate::event_set;
use crate::pool::TrustTier;

/// The host accepted a command under a newer run epoch than any before (I18); recorded ahead
/// of that command's events so `evolve` knows the epoch to fence with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HostEpochAdvanced {
    /// The new highest run epoch.
    pub epoch: Epoch,
}

impl DomainEvent for HostEpochAdvanced {
    const EVENT_TYPE: &'static str = "host_epoch_advanced";
    const SCHEMA_VERSION: u32 = 1;
}

/// The host is registered with its operator reserves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HostRegistered {
    /// Held back from admission.
    pub reserves: Resources,
}

impl DomainEvent for HostRegistered {
    const EVENT_TYPE: &'static str = "host_registered";
    const SCHEMA_VERSION: u32 = 1;
}

/// A newer capacity report of the data plane: admission is decided within it from now on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CapacityReportApplied {
    /// The report's id.
    pub report_id: u64,
    /// Its content.
    pub capacity: ReportedCapacity,
}

impl DomainEvent for CapacityReportApplied {
    const EVENT_TYPE: &'static str = "capacity_report_applied";
    const SCHEMA_VERSION: u32 = 1;
}

/// A pool's terms at a newer revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PoolTermsApplied {
    /// The pool.
    pub pool: PoolName,
    /// The pool's terms revision.
    pub revision: u64,
    /// Whether it admits.
    pub open: bool,
    /// Its `max_running`.
    pub max_running: u32,
    /// Its tier.
    pub tier: TrustTier,
}

impl DomainEvent for PoolTermsApplied {
    const EVENT_TYPE: &'static str = "pool_terms_applied";
    const SCHEMA_VERSION: u32 = 1;
}

/// A lease: the slot, its next generation, and what the VM needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SlotReserved {
    /// The attempt.
    pub attempt_id: AttemptId,
    /// The slot.
    pub slot: Slot,
    /// The slot's new generation: higher than any before on this slot (I02).
    pub generation: Generation,
    /// The pool.
    pub pool: PoolName,
    /// The guest OS.
    pub os: GuestOs,
    /// What the VM needs.
    pub resources: Resources,
}

impl DomainEvent for SlotReserved {
    const EVENT_TYPE: &'static str = "slot_reserved";
    const SCHEMA_VERSION: u32 = 1;
}

/// The activation grant (the attempt's `grant_ref` names this event).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LeaseActivated {
    /// The attempt.
    pub attempt_id: AttemptId,
}

impl DomainEvent for LeaseActivated {
    const EVENT_TYPE: &'static str = "lease_activated";
    const SCHEMA_VERSION: u32 = 1;
}

/// A lease is quarantined, or its quarantined VM stopped: it keeps counting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LeaseQuarantined {
    /// The attempt.
    pub attempt_id: AttemptId,
    /// What it holds now.
    pub holds: Holds,
}

impl DomainEvent for LeaseQuarantined {
    const EVENT_TYPE: &'static str = "lease_quarantined";
    const SCHEMA_VERSION: u32 = 1;
}

/// A lease is released on evidence that its VM is gone; the slot's generation stays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SlotReleased {
    /// The attempt.
    pub attempt_id: AttemptId,
    /// The slot.
    pub slot: Slot,
    /// The generation it held.
    pub generation: Generation,
    /// The evidence.
    pub evidence: ReleaseEvidence,
}

impl DomainEvent for SlotReleased {
    const EVENT_TYPE: &'static str = "slot_released";
    const SCHEMA_VERSION: u32 = 1;
}

/// Free disk fell below the low-water mark: admission is closed and `reported_max` drops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DiskLowWaterCrossed {}

impl DomainEvent for DiskLowWaterCrossed {
    const EVENT_TYPE: &'static str = "disk_low_water_crossed";
    const SCHEMA_VERSION: u32 = 1;
}

/// Free disk is back above the low-water mark.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DiskRecovered {}

impl DomainEvent for DiskRecovered {
    const EVENT_TYPE: &'static str = "disk_recovered";
    const SCHEMA_VERSION: u32 = 1;
}

/// Admission closed for a cause.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AdmissionClosed {
    /// The cause.
    pub cause: AdmissionCause,
}

impl DomainEvent for AdmissionClosed {
    const EVENT_TYPE: &'static str = "admission_closed";
    const SCHEMA_VERSION: u32 = 1;
}

/// A cause no longer closes admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AdmissionOpened {
    /// The cause.
    pub cause: AdmissionCause,
}

impl DomainEvent for AdmissionOpened {
    const EVENT_TYPE: &'static str = "admission_opened";
    const SCHEMA_VERSION: u32 = 1;
}

/// The host drains: no reservation and no grant (I08).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HostDrainStarted {
    /// The deadline for Busy attempts.
    pub deadline: Deadline,
}

impl DomainEvent for HostDrainStarted {
    const EVENT_TYPE: &'static str = "host_drain_started";
    const SCHEMA_VERSION: u32 = 1;
}

/// The drain is cancelled; admission reopens (unless another cause holds it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HostDrainCancelled {}

impl DomainEvent for HostDrainCancelled {
    const EVENT_TYPE: &'static str = "host_drain_cancelled";
    const SCHEMA_VERSION: u32 = 1;
}

/// The drain completed: no lease was left.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HostDrainCompleted {}

impl DomainEvent for HostDrainCompleted {
    const EVENT_TYPE: &'static str = "host_drain_completed";
    const SCHEMA_VERSION: u32 = 1;
}

/// An operator forced a drain: the reconciler stops Busy VMs in scope (jobs fail).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DrainForced {
    /// What is forced.
    pub scope: ForceScope,
    /// The operator's uid.
    pub operator_uid: u32,
}

impl DomainEvent for DrainForced {
    const EVENT_TYPE: &'static str = "drain_forced";
    const SCHEMA_VERSION: u32 = 1;
}

event_set! {
    /// Everything that happens to a host.
    pub enum HostEvent {
        /// See [`HostEpochAdvanced`].
        HostEpochAdvanced(HostEpochAdvanced),
        /// See [`HostRegistered`].
        HostRegistered(HostRegistered),
        /// See [`CapacityReportApplied`].
        CapacityReportApplied(CapacityReportApplied),
        /// See [`PoolTermsApplied`].
        PoolTermsApplied(PoolTermsApplied),
        /// See [`SlotReserved`].
        SlotReserved(SlotReserved),
        /// See [`LeaseActivated`].
        LeaseActivated(LeaseActivated),
        /// See [`LeaseQuarantined`].
        LeaseQuarantined(LeaseQuarantined),
        /// See [`SlotReleased`].
        SlotReleased(SlotReleased),
        /// See [`DiskLowWaterCrossed`].
        DiskLowWaterCrossed(DiskLowWaterCrossed),
        /// See [`DiskRecovered`].
        DiskRecovered(DiskRecovered),
        /// See [`AdmissionClosed`].
        AdmissionClosed(AdmissionClosed),
        /// See [`AdmissionOpened`].
        AdmissionOpened(AdmissionOpened),
        /// See [`HostDrainStarted`].
        HostDrainStarted(HostDrainStarted),
        /// See [`HostDrainCancelled`].
        HostDrainCancelled(HostDrainCancelled),
        /// See [`HostDrainCompleted`].
        HostDrainCompleted(HostDrainCompleted),
        /// See [`DrainForced`].
        DrainForced(DrainForced),
    }
}
