//! The capacity policy (plan §9, ADR 0024): pure arithmetic over the data plane's last
//! capacity report and the `Host`'s leases. The data plane accounts for and enforces the host's
//! resources; this module only decides what the control plane **allocates** within what was
//! reported, and what each pool's scale set is told it may run.
//!
//! - [`admissible`]: what a report admits before any lease: its totals minus foreign
//!   consumption, our own non-VM use (image bases) and the operator's reserves, the VM slots
//!   foreign VMs leave, and G3's two macOS guests minus the foreign ones.
//! - [`fit`]: whether a demand fits that limit on top of what the leases already hold. `Host`
//!   decides `ReserveSlot` with it, and [`plan`] carves with it, so the two never disagree.
//! - [`plan`]: the per-host [`CapacityReport`] (admissible, allocated, quarantined holds, free,
//!   and per pool the live runners, carve-out, deliverable and `reported_max`) and the
//!   reservations the reconciler may ask the `Host` for.
//!
//! The model is `formal/scheduling.qnt` (`planOf`, `fitsReport`); its invariants and the tests
//! that pin them here: admitted within every reported limit (I03,
//! `tests/capacity.rs::should_admit_within_every_reported_limit_when_inputs_are_random`), at most
//! two macOS guests (`should_never_admit_a_third_macos_guest_when_inputs_are_random`), nothing
//! while admission is closed (I08, I26,
//! `should_admit_nothing_and_report_live_runners_when_admission_is_closed`), quarantined leases
//! held (I28, `should_hold_quarantined_capacity_when_inputs_are_random`), the carve-outs fit the
//! host together (I07, I24, `should_fit_every_carve_out_together_when_inputs_are_random`) and
//! more foreign consumption never admits more
//! (`should_never_admit_more_when_foreign_consumption_grows`).

use mayfly_domain::{PoolName, Resources};
use serde::{Deserialize, Serialize};

use crate::attempt::GuestOs;

/// G3: at most two running macOS guests per host, foreign ones included, whatever a report
/// says.
pub const MACOS_GUESTS_MAX: u32 = 2;

/// The content of the data plane's `CapacityReported` (ADR 0024), as `Host` applies it: the
/// ledger's totals and the consumption that is not one of our leases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReportedCapacity {
    /// What the ledger may give VMs on this host (after its own headroom).
    pub total: Resources,
    /// External consumption: what foreign VMs and processes use.
    pub foreign: Resources,
    /// Our own use outside any VM: image bases and pulls in flight.
    pub base: Resources,
    /// The host's VM slots.
    pub vm_slots: u32,
    /// Slots held by foreign VMs.
    pub foreign_vms: u32,
    /// The macOS-guest slots the host offers (at most [`MACOS_GUESTS_MAX`] count).
    pub macos_guests: u32,
    /// Running foreign macOS guests.
    pub foreign_macos_guests: u32,
}

/// A point in the four dimensions admission counts: memory and disk, VM slots and macOS-guest
/// slots. Used for limits, allocations and demands alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Footprint {
    /// Memory and disk.
    pub resources: Resources,
    /// VM slots.
    pub vms: u32,
    /// macOS-guest slots.
    pub macos_guests: u32,
}

impl Footprint {
    /// Nothing.
    pub const ZERO: Self = Self {
        resources: Resources::new(0, 0),
        vms: 0,
        macos_guests: 0,
    };

    /// One VM of `os` with `resources`: a slot, and a macOS-guest slot for macOS.
    #[must_use]
    pub const fn vm(os: GuestOs, resources: Resources) -> Self {
        Self {
            resources,
            vms: 1,
            macos_guests: match os {
                GuestOs::Macos => 1,
                GuestOs::Linux => 0,
            },
        }
    }

    /// The sum, saturating on every dimension (capacity arithmetic never wraps).
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self {
            resources: Resources::new(
                self.resources
                    .memory_bytes
                    .saturating_add(other.resources.memory_bytes),
                self.resources
                    .disk_bytes
                    .saturating_add(other.resources.disk_bytes),
            ),
            vms: self.vms.saturating_add(other.vms),
            macos_guests: self.macos_guests.saturating_add(other.macos_guests),
        }
    }

    /// The difference, floored at zero on every dimension.
    #[must_use]
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self {
            resources: Resources::new(
                self.resources
                    .memory_bytes
                    .saturating_sub(other.resources.memory_bytes),
                self.resources
                    .disk_bytes
                    .saturating_sub(other.resources.disk_bytes),
            ),
            vms: self.vms.saturating_sub(other.vms),
            macos_guests: self.macos_guests.saturating_sub(other.macos_guests),
        }
    }

    /// Whether every dimension is within `limit`.
    #[must_use]
    pub const fn fits_within(self, limit: Self) -> bool {
        first_exceeded(self, limit).is_none()
    }
}

/// A dimension of admission, named in a refusal and an explanation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Dimension {
    /// VM slots.
    VmSlots,
    /// macOS-guest slots (G3).
    MacosGuests,
    /// Memory.
    Memory,
    /// Disk.
    Disk,
}

/// The first dimension (in [`Dimension`] order) on which `amount` exceeds `limit`.
const fn first_exceeded(amount: Footprint, limit: Footprint) -> Option<Dimension> {
    if amount.vms > limit.vms {
        Some(Dimension::VmSlots)
    } else if amount.macos_guests > limit.macos_guests {
        Some(Dimension::MacosGuests)
    } else if amount.resources.memory_bytes > limit.resources.memory_bytes {
        Some(Dimension::Memory)
    } else if amount.resources.disk_bytes > limit.resources.disk_bytes {
        Some(Dimension::Disk)
    } else {
        None
    }
}

/// What `report` admits before any lease: totals minus foreign consumption, our non-VM use and
/// the operator's `reserves`; slots minus foreign VMs; G3's macOS guests (never more than
/// [`MACOS_GUESTS_MAX`], whatever the report says) minus foreign guests. Every subtraction floors
/// at zero, so more external consumption never admits more (`formal/scheduling.qnt`'s
/// `externalConsumptionShrinksAdmissible`).
#[must_use]
pub const fn admissible(report: &ReportedCapacity, reserves: Resources) -> Footprint {
    let consumed = Footprint {
        resources: Resources::new(
            report
                .foreign
                .memory_bytes
                .saturating_add(report.base.memory_bytes)
                .saturating_add(reserves.memory_bytes),
            report
                .foreign
                .disk_bytes
                .saturating_add(report.base.disk_bytes)
                .saturating_add(reserves.disk_bytes),
        ),
        vms: report.foreign_vms,
        macos_guests: report.foreign_macos_guests,
    };
    let macos = if report.macos_guests < MACOS_GUESTS_MAX {
        report.macos_guests
    } else {
        MACOS_GUESTS_MAX
    };
    Footprint {
        resources: report.total,
        vms: report.vm_slots,
        macos_guests: macos,
    }
    .saturating_sub(consumed)
}

/// Whether `demand` fits `limit` on top of `allocated`, on every dimension the demand uses; the
/// first dimension that does not, otherwise.
///
/// A dimension the demand does not use is not checked: a Linux VM is admitted while the
/// macOS-guest slots are over-allocated because a newer report shrank them, since it takes
/// none of them. A dimension it uses must hold `allocated + demand` within the limit, so an
/// over-allocated dimension admits nothing more of it (pinned by
/// `tests/host/rules.rs::should_admit_a_linux_vm_when_a_newer_report_shrank_the_macos_slots`).
///
/// # Errors
/// The first [`Dimension`] (in its order) on which the demand does not fit.
pub const fn fit(
    limit: Footprint,
    allocated: Footprint,
    demand: Footprint,
) -> Result<(), Dimension> {
    const fn exceeds(allocated: u64, demand: u64, limit: u64) -> bool {
        demand > 0 && allocated.saturating_add(demand) > limit
    }
    if exceeds(allocated.vms as u64, demand.vms as u64, limit.vms as u64) {
        Err(Dimension::VmSlots)
    } else if exceeds(
        allocated.macos_guests as u64,
        demand.macos_guests as u64,
        limit.macos_guests as u64,
    ) {
        Err(Dimension::MacosGuests)
    } else if exceeds(
        allocated.resources.memory_bytes,
        demand.resources.memory_bytes,
        limit.resources.memory_bytes,
    ) {
        Err(Dimension::Memory)
    } else if exceeds(
        allocated.resources.disk_bytes,
        demand.resources.disk_bytes,
        limit.resources.disk_bytes,
    ) {
        Err(Dimension::Disk)
    } else {
        Ok(())
    }
}

/// One lease as the policy sees it: its pool, what it holds, and whether it is quarantined
/// (held, never a runner).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseInput {
    /// The pool it was reserved for.
    pub pool: PoolName,
    /// What it holds now (a stopped quarantined lease holds disk and slots only).
    pub holds: Footprint,
    /// Quarantined: it counts against capacity and its pool's `max_running`, and is no runner.
    pub quarantined: bool,
}

/// One pool's admission inputs: its applied terms on the host, its VM shape, and how many new
/// attempts the reconciler wants for it (demand plus the `min_prepared` deficit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolInput {
    /// The pool.
    pub pool: PoolName,
    /// Its guest OS.
    pub os: GuestOs,
    /// What one of its VMs needs.
    pub vm: Resources,
    /// Whether its terms on this host are open.
    pub open: bool,
    /// Its `max_running` on this host.
    pub max_running: u32,
    /// New attempts wanted now.
    pub wanted: u32,
}

/// Everything [`plan`] reads: plain data, so the policy is a pure function of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanInputs {
    /// The last capacity report the host applied, if any.
    pub report: Option<ReportedCapacity>,
    /// The operator's reserves.
    pub reserves: Resources,
    /// Whether the host's gate is open: no drain, no closed cause.
    pub admission_open: bool,
    /// Every live lease (reserved, activated, quarantined).
    pub leases: Vec<LeaseInput>,
    /// The pools served.
    pub pools: Vec<PoolInput>,
}

/// Why a pool is given no new reservation (`AdmissionExplain`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldReason {
    /// No capacity report has been applied: nothing is allocated blind.
    NoReport,
    /// The host's gate is closed (drain, low disk, recovery, ...).
    AdmissionClosed,
    /// The pool's terms are closed (paused, draining, breaker tripped, no pin).
    TermsClosed,
    /// The pool runs `max_running` attempts.
    PoolFull,
    /// Quarantined leases consume what the pool would need (I28, ADR 0025).
    QuarantineHeld,
    /// The host's free capacity does not fit one more VM of the pool.
    NoCapacity(Dimension),
}

/// What the policy says to do for a pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanDecision {
    /// Ask the `Host` for one `ReserveSlot` of this shape.
    Reserve {
        /// The pool.
        pool: PoolName,
        /// Its guest OS.
        os: GuestOs,
        /// What the VM needs.
        resources: Resources,
    },
    /// The pool wanted more and gets nothing now.
    Hold {
        /// The pool.
        pool: PoolName,
        /// Why.
        reason: HoldReason,
    },
}

/// One pool's line of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolCapacity {
    /// The pool.
    pub pool: PoolName,
    /// Its live, unquarantined leases: runners it has or is preparing.
    pub runners: u32,
    /// Its quarantined leases.
    pub quarantined: u32,
    /// New VMs of the pool the free capacity can hold, after the pools before it.
    pub carve_out: u32,
    /// Runners it can deliver: `runners + carve_out`.
    pub deliverable: u32,
    /// The `maxCapacity` for its scale set: its deliverable while admission is open, only its
    /// runners while it is closed (OD15).
    pub reported_max: u32,
    /// Why the carve-out is zero, when it is.
    pub hold: Option<HoldReason>,
}

/// The policy's view of one host (plan §9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacityReport {
    /// What the last report admits before any lease ([`admissible`]); zero without a report.
    pub admissible: Footprint,
    /// What every live lease holds, quarantined ones included.
    pub allocated: Footprint,
    /// What quarantined leases hold (part of `allocated`).
    pub quarantined: Footprint,
    /// `admissible − allocated`, floored at zero.
    pub free: Footprint,
    /// One line per pool, in name order.
    pub pools: Vec<PoolCapacity>,
}

/// How many VMs of `vm` fit `free`, at most `cap`.
fn fit_count(free: Footprint, vm: Footprint, cap: u32) -> u32 {
    let mut n = cap.min(free.vms);
    if vm.macos_guests > 0 {
        n = n.min(free.macos_guests);
    }
    if let Some(by_memory) = free
        .resources
        .memory_bytes
        .checked_div(vm.resources.memory_bytes)
    {
        n = n.min(u32::try_from(by_memory).unwrap_or(u32::MAX));
    }
    if let Some(by_disk) = free
        .resources
        .disk_bytes
        .checked_div(vm.resources.disk_bytes)
    {
        n = n.min(u32::try_from(by_disk).unwrap_or(u32::MAX));
    }
    n
}

/// `vm` times `n`, saturating.
fn times(vm: Footprint, n: u32) -> Footprint {
    Footprint {
        resources: Resources::new(
            vm.resources.memory_bytes.saturating_mul(u64::from(n)),
            vm.resources.disk_bytes.saturating_mul(u64::from(n)),
        ),
        vms: vm.vms.saturating_mul(n),
        macos_guests: vm.macos_guests.saturating_mul(n),
    }
}

/// The capacity policy of one host: what is admissible, allocated and free, each pool's
/// carve-out, deliverable and `reported_max`, and the reservations to ask for.
///
/// Pools are served in name order (duplicates after the first are ignored): each is carved as
/// many VMs as fit what the pools before it left, up to its `max_running` headroom, where live
/// and quarantined leases both count. Admission order is a deliberate v0.x simplicity (the same
/// order as `formal/scheduling.qnt`'s `planOf`); an operator balances pools with `max_running`.
/// A pool whose terms or host gate are closed is carved nothing and reports only its runners.
/// A reservation decision is never more than the carve-out, so the `Host` admits every one of
/// them (`formal/scheduling.qnt`'s `carveOutAdmissible`).
#[must_use]
pub fn plan(inputs: &PlanInputs) -> (CapacityReport, Vec<PlanDecision>) {
    let admissible_now = inputs
        .report
        .as_ref()
        .map_or(Footprint::ZERO, |r| admissible(r, inputs.reserves));
    let allocated = inputs
        .leases
        .iter()
        .fold(Footprint::ZERO, |sum, l| sum.saturating_add(l.holds));
    let quarantined = inputs
        .leases
        .iter()
        .filter(|l| l.quarantined)
        .fold(Footprint::ZERO, |sum, l| sum.saturating_add(l.holds));
    let free = admissible_now.saturating_sub(allocated);

    let mut pools: Vec<&PoolInput> = inputs.pools.iter().collect();
    pools.sort_by(|a, b| a.pool.cmp(&b.pool));
    pools.dedup_by(|a, b| a.pool == b.pool);

    let mut remaining = free;
    let mut lines = Vec::with_capacity(pools.len());
    let mut decisions = Vec::new();
    for pool in pools {
        let runners = count(&inputs.leases, &pool.pool, false);
        let held = count(&inputs.leases, &pool.pool, true);
        let headroom = pool
            .max_running
            .saturating_sub(runners.saturating_add(held));
        let vm = Footprint::vm(pool.os, pool.vm);
        let open = inputs.report.is_some() && inputs.admission_open && pool.open;
        let carve_out = if open {
            fit_count(remaining, vm, headroom)
        } else {
            0
        };
        remaining = remaining.saturating_sub(times(vm, carve_out));
        let next = hold_reason(
            inputs,
            pool,
            held,
            headroom.saturating_sub(carve_out),
            remaining,
            quarantined,
            vm,
        );
        let hold = if carve_out > 0 { None } else { Some(next) };
        let deliverable = runners.saturating_add(carve_out);
        lines.push(PoolCapacity {
            pool: pool.pool.clone(),
            runners,
            quarantined: held,
            carve_out,
            deliverable,
            reported_max: if open { deliverable } else { runners },
            hold,
        });
        let reserve = pool.wanted.min(carve_out);
        for _ in 0..reserve {
            decisions.push(PlanDecision::Reserve {
                pool: pool.pool.clone(),
                os: pool.os,
                resources: pool.vm,
            });
        }
        if pool.wanted > reserve {
            decisions.push(PlanDecision::Hold {
                pool: pool.pool.clone(),
                reason: next,
            });
        }
    }
    (
        CapacityReport {
            admissible: admissible_now,
            allocated,
            quarantined,
            free,
            pools: lines,
        },
        decisions,
    )
}

/// Live leases of `pool` that are (or are not) quarantined.
fn count(leases: &[LeaseInput], pool: &PoolName, quarantined: bool) -> u32 {
    let n = leases
        .iter()
        .filter(|l| &l.pool == pool && l.quarantined == quarantined)
        .count();
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Why `pool` is carved nothing: the first closed condition, else the full pool or the first
/// dimension that does not fit; either is put down to quarantine when the quarantined leases
/// alone make the difference.
fn hold_reason(
    inputs: &PlanInputs,
    pool: &PoolInput,
    held: u32,
    headroom: u32,
    remaining: Footprint,
    quarantined: Footprint,
    vm: Footprint,
) -> HoldReason {
    if inputs.report.is_none() {
        return HoldReason::NoReport;
    }
    if !inputs.admission_open {
        return HoldReason::AdmissionClosed;
    }
    if !pool.open {
        return HoldReason::TermsClosed;
    }
    if headroom == 0 {
        return if held > 0 && pool.max_running > 0 {
            HoldReason::QuarantineHeld
        } else {
            HoldReason::PoolFull
        };
    }
    match fit(remaining, Footprint::ZERO, vm) {
        Ok(()) => HoldReason::PoolFull,
        Err(dimension) => {
            if fit(remaining.saturating_add(quarantined), Footprint::ZERO, vm).is_ok() {
                HoldReason::QuarantineHeld
            } else {
                HoldReason::NoCapacity(dimension)
            }
        }
    }
}
