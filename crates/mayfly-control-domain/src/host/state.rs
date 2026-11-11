//! The host's folded state, its commands and its refusals.

use std::collections::{BTreeMap, BTreeSet};

use mayfly_domain::{AttemptId, Deadline, Epoch, Generation, PoolName, Resources, Slot};
use serde::{Deserialize, Serialize};

use crate::aggregate::{CommandBody, Rejection};
use crate::attempt::GuestOs;
use crate::capacity::{
    self, CapacityPolicy, Footprint, LeaseInput, PlanInputs, PoolInput, ReportedCapacity,
};
use crate::pool::TrustTier;

/// What a quarantined lease holds (plan §9): everything while its VM may run, disk and slots
/// once it is stopped. Never nothing: a quarantined lease is released only on the VM's observed
/// absence (I28, ADR 0025).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Holds {
    /// Memory, disk and slots: the VM runs (or is not known to be stopped).
    Full,
    /// Disk and slots: the VM is stopped.
    DiskAndSlot,
}

/// A lease's phase (`doc/domain-model.md` §5.2); a released lease is gone from the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeasePhase {
    /// `SlotReserved`: the attempt is preparing (or prepared).
    Reserved,
    /// `LeaseActivated`: the attempt was granted activation.
    Activated,
    /// `LeaseQuarantined`: held for an operator, never reused.
    Quarantined {
        /// What it holds.
        holds: Holds,
    },
}

/// The host's record that an attempt holds a slot and a resource vector (allocation
/// bookkeeping; the data plane's ledger is the truth, ADR 0024).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    /// The slot.
    pub slot: Slot,
    /// The slot's generation at the reservation: part of the VM name and the ownership proof.
    pub generation: Generation,
    /// The pool.
    pub pool: PoolName,
    /// The guest OS.
    pub os: GuestOs,
    /// What the VM needs.
    pub resources: Resources,
    /// The phase.
    pub phase: LeasePhase,
}

impl Lease {
    /// What the lease counts against capacity: a VM's slot (and macOS-guest slot), its
    /// resources, and only its disk once it is a stopped quarantined lease.
    #[must_use]
    pub const fn holds(&self) -> Footprint {
        let vm = Footprint::vm(self.os, self.resources);
        match self.phase {
            LeasePhase::Reserved
            | LeasePhase::Activated
            | LeasePhase::Quarantined { holds: Holds::Full } => vm,
            LeasePhase::Quarantined {
                holds: Holds::DiskAndSlot,
            } => Footprint {
                resources: Resources::new(0, self.resources.disk_bytes),
                ..vm
            },
        }
    }

    /// Whether the lease is quarantined.
    #[must_use]
    pub const fn is_quarantined(&self) -> bool {
        match self.phase {
            LeasePhase::Reserved | LeasePhase::Activated => false,
            LeasePhase::Quarantined { .. } => true,
        }
    }
}

/// A pool's terms as applied on this host (`PoolTermsApplied`, policy P4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostPoolTerms {
    /// The pool's terms revision; only a higher one replaces it.
    pub revision: u64,
    /// Whether the pool admits.
    pub open: bool,
    /// Its `max_running`.
    pub max_running: u32,
    /// Its tier.
    pub tier: TrustTier,
}

/// The last capacity report applied (`CapacityReportApplied`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedReport {
    /// The data plane's report id; an older one is ignored.
    pub report_id: u64,
    /// Its content.
    pub capacity: ReportedCapacity,
}

/// A cause that closes the host's admission, other than a drain and low disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionCause {
    /// The installed Tart is not one mayfly supports.
    TartUnsupported,
    /// The host woke from sleep; prepared VMs are re-verified first.
    Woke,
    /// Startup recovery has not completed.
    Recovering,
    /// The last inventory was incomplete (I25).
    InventoryIncomplete,
}

/// The host's drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drain {
    /// No drain.
    Open,
    /// Draining: no reservation and no grant (I08); completes with no lease left.
    Draining {
        /// The operator's deadline for Busy attempts (force is a separate, explicit command).
        deadline: Deadline,
    },
    /// Drained: closed until the drain is cancelled.
    Drained,
}

/// The host's admission gate (`doc/domain-model.md` §6.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gate {
    /// The drain.
    pub drain: Drain,
    /// Free disk is below the low-water mark (F17).
    pub disk_low: bool,
    /// The other closing causes.
    pub closed: BTreeSet<AdmissionCause>,
}

impl Gate {
    /// Open: no drain, disk not low, no closing cause.
    #[must_use]
    pub fn is_open(&self) -> bool {
        let drained = match self.drain {
            Drain::Open => false,
            Drain::Draining { .. } | Drain::Drained => true,
        };
        !drained && !self.disk_low && self.closed.is_empty()
    }
}

/// The state of one host stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostState {
    /// `HostRegistered` was recorded.
    pub registered: bool,
    /// The highest run epoch accepted (I18).
    pub epoch: Epoch,
    /// The operator's reserves, held back from admission.
    pub reserves: Resources,
    /// The last capacity report applied.
    pub capacity: Option<AppliedReport>,
    /// The last generation handed out per slot: it only grows (I02).
    pub slots: BTreeMap<Slot, Generation>,
    /// The live leases.
    pub leases: BTreeMap<AttemptId, Lease>,
    /// Pool terms as applied here.
    pub pool_terms: BTreeMap<PoolName, HostPoolTerms>,
    /// The admission gate.
    pub gate: Gate,
}

impl HostState {
    /// The state of a stream with no events.
    #[must_use]
    pub const fn initial() -> Self {
        Self {
            registered: false,
            epoch: Epoch::new(0),
            reserves: Resources::new(0, 0),
            capacity: None,
            slots: BTreeMap::new(),
            leases: BTreeMap::new(),
            pool_terms: BTreeMap::new(),
            gate: Gate {
                drain: Drain::Open,
                disk_low: false,
                closed: BTreeSet::new(),
            },
        }
    }

    /// What every live lease holds, quarantined ones included (I03).
    #[must_use]
    pub fn allocated(&self) -> Footprint {
        self.leases.values().fold(Footprint::ZERO, |sum, lease| {
            sum.saturating_add(lease.holds())
        })
    }

    /// The capacity policy's inputs for this host: its report, reserves, gate and leases, and
    /// per pool its applied terms (an unknown pool is closed) with the shape and the number of
    /// new attempts the caller wants (`capacity::plan` reads nothing else).
    #[must_use]
    pub fn plan_inputs(&self, wanted: &[PoolDemand]) -> PlanInputs {
        PlanInputs {
            report: self.capacity.map(|r| r.capacity),
            reserves: self.reserves,
            admission_open: self.gate.is_open(),
            leases: self
                .leases
                .values()
                .map(|lease| LeaseInput {
                    pool: lease.pool.clone(),
                    holds: lease.holds(),
                    quarantined: lease.is_quarantined(),
                })
                .collect(),
            pools: wanted
                .iter()
                .map(|demand| {
                    let terms = self.pool_terms.get(&demand.pool);
                    PoolInput {
                        pool: demand.pool.clone(),
                        os: demand.os,
                        vm: demand.vm,
                        open: terms.is_some_and(|t| t.open),
                        max_running: terms.map_or(0, |t| t.max_running),
                        wanted: demand.wanted,
                        policy: demand.policy,
                    }
                })
                .collect(),
        }
    }

    /// What the last report admits, or nothing without one.
    #[must_use]
    pub fn admissible(&self) -> Footprint {
        self.capacity.map_or(Footprint::ZERO, |r| {
            capacity::admissible(&r.capacity, self.reserves)
        })
    }
}

/// A pool's shape and the new attempts wanted for it, for [`HostState::plan_inputs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolDemand {
    /// The pool.
    pub pool: PoolName,
    /// Its guest OS.
    pub os: GuestOs,
    /// What one VM needs.
    pub vm: Resources,
    /// New attempts wanted.
    pub wanted: u32,
    /// The pool's capacity policy (ADR 0069, DEC-06).
    pub policy: CapacityPolicy,
}

/// Evidence that a lease's VM is gone, the only reason a slot is released (I28).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseEvidence {
    /// The attempt observed its `VmDeleted`.
    VmDeleted,
    /// The attempt was found lost: a complete inventory did not list the VM.
    AttemptLost,
    /// No `AttemptStarted` appeared for the reservation: nothing was ever cloned. Refused for a
    /// lease that is not merely reserved.
    AttemptNeverStarted,
}

/// A resource threshold the data plane crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ResourceThreshold {
    /// Free disk fell below the low-water mark: admission closes (F17).
    DiskLow,
    /// Free disk is back above it.
    DiskOk,
}

/// What a forced drain stops (operator only; jobs fail).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForceScope {
    /// Every attempt on the host.
    Host,
    /// One pool's attempts on the host.
    Pool {
        /// The pool.
        pool: PoolName,
    },
}

/// `ReserveSlot`: a lease for an attempt the reconciler minted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReserveSlot {
    /// The attempt; the same id starts the attempt's stream.
    pub attempt_id: AttemptId,
    /// The pool.
    pub pool: PoolName,
    /// The guest OS.
    pub os: GuestOs,
    /// What the VM needs.
    pub resources: Resources,
    /// The pool terms revision the reconciler read; a different one is refused.
    pub terms_revision: u64,
}

/// `ApplyPoolTerms`: a pool's terms at a revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApplyPoolTerms {
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

/// The commands of `doc/domain-model.md` §6.3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum HostCommand {
    /// Register the host with its operator reserves.
    RegisterHost {
        /// Held back from admission.
        reserves: Resources,
    },
    /// Apply the data plane's capacity report; an older `report_id` is ignored.
    RecordCapacityReport {
        /// The report's id, increasing per data-plane session.
        report_id: u64,
        /// Its content.
        capacity: ReportedCapacity,
    },
    /// Apply a pool's terms (policy P4).
    ApplyPoolTerms(ApplyPoolTerms),
    /// Reserve a slot.
    ReserveSlot(ReserveSlot),
    /// Grant an activation (policy P9).
    GrantActivation {
        /// The attempt.
        attempt_id: AttemptId,
    },
    /// Quarantine a lease, or tell the host its quarantined VM stopped.
    QuarantineLease {
        /// The attempt.
        attempt_id: AttemptId,
        /// Whether the VM may still run.
        vm_running: bool,
    },
    /// Release a lease on evidence that its VM is gone.
    ReleaseSlot {
        /// The attempt.
        attempt_id: AttemptId,
        /// The evidence.
        evidence: ReleaseEvidence,
    },
    /// A resource threshold was crossed.
    RecordResourceThreshold {
        /// Which.
        threshold: ResourceThreshold,
    },
    /// Close admission for a cause.
    CloseAdmission {
        /// The cause.
        cause: AdmissionCause,
    },
    /// Lift a cause.
    OpenAdmission {
        /// The cause.
        cause: AdmissionCause,
    },
    /// Start draining the host.
    StartHostDrain {
        /// The deadline for Busy attempts.
        deadline: Deadline,
    },
    /// Cancel the drain (or reopen a drained host).
    CancelHostDrain,
    /// The drain policy saw no lease left.
    CompleteHostDrain,
    /// An operator forces a drain: Busy VMs are stopped, jobs fail.
    ForceDrain {
        /// What is forced.
        scope: ForceScope,
    },
}

impl CommandBody for HostCommand {
    const TYPES: &'static [&'static str] = &[
        "register_host",
        "record_capacity_report",
        "apply_pool_terms",
        "reserve_slot",
        "grant_activation",
        "quarantine_lease",
        "release_slot",
        "record_resource_threshold",
        "close_admission",
        "open_admission",
        "start_host_drain",
        "cancel_host_drain",
        "complete_host_drain",
        "force_drain",
    ];

    fn command_type(&self) -> &'static str {
        match self {
            Self::RegisterHost { .. } => "register_host",
            Self::RecordCapacityReport { .. } => "record_capacity_report",
            Self::ApplyPoolTerms(_) => "apply_pool_terms",
            Self::ReserveSlot(_) => "reserve_slot",
            Self::GrantActivation { .. } => "grant_activation",
            Self::QuarantineLease { .. } => "quarantine_lease",
            Self::ReleaseSlot { .. } => "release_slot",
            Self::RecordResourceThreshold { .. } => "record_resource_threshold",
            Self::CloseAdmission { .. } => "close_admission",
            Self::OpenAdmission { .. } => "open_admission",
            Self::StartHostDrain { .. } => "start_host_drain",
            Self::CancelHostDrain => "cancel_host_drain",
            Self::CompleteHostDrain => "complete_host_drain",
            Self::ForceDrain { .. } => "force_drain",
        }
    }
}

/// A refusal of the `Host` aggregate. Its serde form is its code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostRejection {
    /// The stream has no `HostRegistered`.
    NotRegistered,
    /// `RegisterHost` on a registered host.
    AlreadyRegistered,
    /// The command's run epoch is older than the host's (I18).
    StaleRun,
    /// Pool terms at a revision not higher than the applied one.
    StaleTerms,
    /// No terms were applied for the pool.
    UnknownPool,
    /// The reservation was decided on another terms revision than the applied one.
    TermsChanged,
    /// The pool's terms are closed (I26).
    TermsClosed,
    /// The pool's terms are open (a pool-scoped force needs them closed).
    TermsOpen,
    /// The host's gate is closed by low disk or a cause (I26).
    AdmissionClosed,
    /// The host drains or is drained (I08).
    Draining,
    /// No capacity report was applied: nothing is allocated blind (ADR 0024).
    NoCapacityReport,
    /// The attempt already holds a lease.
    AlreadyLeased,
    /// The pool runs `max_running` attempts.
    PoolFull,
    /// The pool's `max_running` is consumed by quarantined leases among others (I28).
    QuarantineHeld,
    /// No free VM slot in the last report.
    NoFreeSlot,
    /// No macOS-guest slot: two at most, foreign guests included (G3).
    MacosGuests,
    /// Not enough memory in the last report.
    InsufficientMemory,
    /// Not enough disk in the last report.
    InsufficientDisk,
    /// The attempt holds no lease.
    NoLease,
    /// A grant for a lease that is already activated.
    AlreadyActivated,
    /// The lease is quarantined.
    Quarantined,
    /// A drain is started or done.
    AlreadyDraining,
    /// No drain to cancel, complete or force.
    NotDraining,
    /// The drain completes only with no lease left (I08).
    LeasesRemaining,
    /// Only an operator forces a drain.
    OperatorOnly,
}

impl Rejection for HostRejection {
    const CODES: &'static [&'static str] = &[
        "not_registered",
        "already_registered",
        "stale_run",
        "stale_terms",
        "unknown_pool",
        "terms_changed",
        "terms_closed",
        "terms_open",
        "admission_closed",
        "draining",
        "no_capacity_report",
        "already_leased",
        "pool_full",
        "quarantine_held",
        "no_free_slot",
        "macos_guests",
        "insufficient_memory",
        "insufficient_disk",
        "no_lease",
        "already_activated",
        "quarantined",
        "already_draining",
        "not_draining",
        "leases_remaining",
        "operator_only",
    ];

    fn code(&self) -> &'static str {
        match self {
            Self::NotRegistered => "not_registered",
            Self::AlreadyRegistered => "already_registered",
            Self::StaleRun => "stale_run",
            Self::StaleTerms => "stale_terms",
            Self::UnknownPool => "unknown_pool",
            Self::TermsChanged => "terms_changed",
            Self::TermsClosed => "terms_closed",
            Self::TermsOpen => "terms_open",
            Self::AdmissionClosed => "admission_closed",
            Self::Draining => "draining",
            Self::NoCapacityReport => "no_capacity_report",
            Self::AlreadyLeased => "already_leased",
            Self::PoolFull => "pool_full",
            Self::QuarantineHeld => "quarantine_held",
            Self::NoFreeSlot => "no_free_slot",
            Self::MacosGuests => "macos_guests",
            Self::InsufficientMemory => "insufficient_memory",
            Self::InsufficientDisk => "insufficient_disk",
            Self::NoLease => "no_lease",
            Self::AlreadyActivated => "already_activated",
            Self::Quarantined => "quarantined",
            Self::AlreadyDraining => "already_draining",
            Self::NotDraining => "not_draining",
            Self::LeasesRemaining => "leases_remaining",
            Self::OperatorOnly => "operator_only",
        }
    }
}
