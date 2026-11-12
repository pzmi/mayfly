//! The reconciler's pass (policy P5 of `doc/domain-model.md` §7, with P23's drain scope; plan
//! §9): one pure function, [`plan_pass`], from what the read side shows now to the commands
//! that move the fleet toward it. The driver (`mayfly_control::reconciler`) reads the
//! projections, calls it, and issues the commands through `AggregateStore`; it decides nothing.
//!
//! **Level-triggered.** A pass reads the whole state and plans from it alone, plus the small
//! [`PassMemory`] the previous pass handed on (when a surplus or an unstarted lease was first
//! seen, which refusals were already read). A pass that changes nothing plans nothing, so the
//! driver may run it on every kick and on its timer; a lost memory only delays a retirement or a
//! release, never causes one (pinned by
//! `tests/reconcile.rs::should_plan_nothing_more_when_a_pass_is_repeated_on_its_own_result`).
//!
//! What it reads, per pool and host:
//!
//! - **Capacity** from each host's latest *applied* report only (ADR 0024), through
//!   [`capacity::plan`]: never more than reported (I03), carve-outs that fit the host together
//!   (I24), nothing while the gate or the pool's terms are closed (I08, I26, I27).
//! - **Demand** from the pool's scale set: `totalAssignedJobs` of its latest statistics, the
//!   shortfall over its activating, listening and busy runners ([`demand::shortfall`]), served
//!   first by prepared and preparing VMs, plus the `min_prepared` deficit: the pool's `waiting`.
//!   It is never capped by a carve-out or the `maxCapacity` sent (DEC-06F, ADR 0069: a capped
//!   count feeds back, carved nothing, told nothing), and a [`CapacityPolicy::Demand`] pool is
//!   planned with it (`tests/reconcile.rs::should_derive_waiting_beyond_live_runners_when_jobs_are_assigned`).
//! - **Each pool's policy** through its [`PolicyOf`] (the composition root maps the operator's
//!   `capacity_policy` with `mayfly_composition::config::plan_policy`).
//! - **Placement** (P17, CP-07): a pool is admissible on a host only once its pinned image's
//!   base is materialised there ([`HostView::placed`]): before, the host carves it nothing,
//!   tells its scale set nothing beyond its live runners and reserves nothing for it, whatever
//!   its terms. The `Host` aggregate does not check placement; the data plane clones only from
//!   a base it holds (ADR 0039), the backstop (`formal/scheduling.qnt`'s `placedOn`,
//!   `leasesKeepTheirBase`; pinned by
//!   `tests/reconcile.rs::should_reserve_nothing_on_a_host_when_the_pin_is_not_placed_there`).
//!
//! What it plans:
//!
//! - `ApplyCapacity` per scale set whose deliverable, reported max or mode changed: the sums of
//!   the pool's lines over every host (I07, I24; [`WireMode`] from the pool's mode, `SafeMode`
//!   in safe mode).
//! - `ReserveSlot` for each new VM a host's plan admits, host by host in id order, each host
//!   seeing what the hosts before it took.
//! - `RequestRetirement`: prepared and listening attempts in a drained scope (`drain`), prepared
//!   ones of an image no longer pinned (`image_replaced`), and prepared ones beyond the shortfall
//!   and `min_prepared` once that surplus has lasted the **hysteresis** ([`Settings::hysteresis`],
//!   `tests/reconcile.rs::should_retire_a_surplus_only_after_the_hysteresis_when_prepared_vms_exceed_demand`). A
//!   reservation is never delayed: an idle VM costs capacity, a waiting job costs time. The
//!   hysteresis reads the surplus at each pass only: a surplus that dips and comes back between
//!   two passes is seen as lasting (memory holds when it was first seen, not every reading), so
//!   it times a surplus by the passes, never by the instants in between.
//!   An attempt whose retirement is already requested (`LeaseView::retiring`) is neither
//!   retired again nor counted as a runner or a prepared VM.
//! - Cleanup: `ReleaseSlot` for a lease whose attempt's VM is gone (`vm_deleted`,
//!   `attempt_lost`) or that no `AttemptStarted` followed within [`Settings::start_grace`]
//!   (`attempt_never_started`, P2's failure path), and `CompleteHostDrain` /
//!   `CompletePoolDrain` once no lease is left in a draining scope (P23). The pass may read a
//!   lease unstarted from a projection behind the journal (or rebuilding), so it only *plans*
//!   the never-started release: the driver saves it with a read of the attempt stream
//!   expecting no stream, and an attempt that started since refuses it at the save (CP-03F;
//!   `formal/scheduling.qnt`'s `releaseUnstarted`, `staleUnstartedReadKeepsTheSlotTest`).
//!
//! **A data-plane `insufficient_resources` refusal is a definite no** (ADR 0024: the data plane
//! accounts, its refusal wins): the host it came from admits nothing more until a report newer
//! (a strictly greater report id) than the one in force when the pass read the refusal is
//! applied; the next pass re-reads it
//! (`tests/reconcile.rs::should_hold_the_host_until_a_newer_report_when_the_data_plane_refuses_insufficient_resources`).
//! The refusal is read from where it stays (the attempts row keeps the revision of the latest
//! one), not from a flag the next directive clears, so a retry before the next pass does not
//! hide it (CP-03F). A lost [`PassMemory`] reads every kept refusal of a live lease as new and
//! holds its host until its next report: a delayed reservation, the safe direction.
//!
//! **Drain scope.** A host's draining leases are retired or finish there; the pool's need does
//! not count them, so they are replaced elsewhere and never also retired as another host's
//! surplus (`tests/reconcile.rs::should_not_retire_below_min_prepared_when_a_draining_host_holds_prepared_vms`).
//!
//! **Safe mode** plans `ApplyCapacity{SafeMode}` only: no reservation, retirement or release
//! (I29).
//!
//! The pass composes actions `formal/scheduling.qnt` already models: a reservation within
//! `planOf`'s carve (`reserve`, `carveOutAdmissible`), a release on evidence (`release`),
//! `completeDrain`; its retirement of a Prepared attempt is `formal/lifecycle.qnt`'s retirement.
//! The hysteresis and the start grace are timing choices: they delay, and never add, a command.

use std::collections::{BTreeMap, BTreeSet};

use mayfly_domain::{AttemptId, HostId, ImageDigest, Millis, PoolName, Resources};

use crate::attempt::{GuestOs, RetireReason};
use crate::capacity::{
    self, CapacityPolicy, CapacityReport, Footprint, LeaseInput, PlanDecision, PlanInputs,
    PoolInput, ReportedCapacity,
};
use crate::demand::{self, Capacity, RunnerCounts, WireMode};
use crate::host::ReleaseEvidence;
use crate::pool::PoolMode;

/// How a pool's capacity policy follows the VMs it waits for: the composition root's mapping of
/// the operator's `capacity_policy` (`mayfly_composition::config::plan_policy`), a plain
/// function so the inputs stay data.
pub type PolicyOf = fn(u32) -> CapacityPolicy;

/// [`CapacityPolicy::Exact`] whatever waits: the default of a pool with no policy given.
#[must_use]
pub const fn exact(_waiting: u32) -> CapacityPolicy {
    CapacityPolicy::Exact
}

/// The pass's timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// How long a pool's surplus of prepared VMs must last before any is retired.
    pub hysteresis: Millis,
    /// How long a reservation may wait for its `AttemptStarted` (P2's T_start) before the slot
    /// is released `attempt_never_started`.
    pub start_grace: Millis,
}

impl Default for Settings {
    /// Five minutes of hysteresis, two of start grace.
    fn default() -> Self {
        Self {
            hysteresis: Millis::new(300_000),
            start_grace: Millis::new(120_000),
        }
    }
}

/// Where a lease's attempt is, as the pass counts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerStage {
    /// No `AttemptStarted` for the reservation yet.
    NotStarted,
    /// Reserved, cloning, booting, verifying or backing off: a VM on its way to Prepared.
    Preparing,
    /// Prepared: booted, verified, unregistered (ADR 0028).
    Prepared,
    /// Activating.
    Activating,
    /// Listening.
    Listening,
    /// Busy.
    Busy,
    /// Finished, retiring or deleting: no runner any more, the VM not gone yet.
    Ending,
    /// The VM is gone, on this evidence: the lease waits for its release.
    Gone(ReleaseEvidence),
    /// Quarantined: held for an operator, never touched (I28).
    Quarantined,
}

/// One live lease of a host, joined with its attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseView {
    /// The attempt holding it.
    pub attempt_id: AttemptId,
    /// Its pool.
    pub pool: PoolName,
    /// What it counts against capacity.
    pub holds: Footprint,
    /// The host's lease is quarantined.
    pub quarantined: bool,
    /// Its attempt's stage.
    pub stage: RunnerStage,
    /// Its attempt's image, once started.
    pub image: Option<ImageDigest>,
    /// The attempt revision of its latest `insufficient_resources` refusal, kept after the
    /// attempt retries (a new revision is a new refusal).
    pub refusal: Option<u64>,
    /// Its attempt's retirement is already requested: not retired again, not counted.
    pub retiring: bool,
}

/// A pool's terms as applied on a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostTerms {
    /// The revision a reservation must carry.
    pub revision: u64,
    /// Open.
    pub open: bool,
    /// Its `max_running`.
    pub max_running: u32,
}

/// A host's latest applied capacity report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostReport {
    /// The data plane's report id.
    pub report_id: u64,
    /// Its content.
    pub capacity: ReportedCapacity,
}

/// One host as the pass reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostView {
    /// The host.
    pub host_id: HostId,
    /// Its latest applied report; none: nothing is allocated blind.
    pub report: Option<HostReport>,
    /// The operator's reserves.
    pub reserves: Resources,
    /// Its gate is open (no drain, no low disk, no closing cause).
    pub admission_open: bool,
    /// It is draining (not yet drained).
    pub draining: bool,
    /// Pool terms as applied here.
    pub terms: BTreeMap<PoolName, HostTerms>,
    /// The images whose base is materialised here (`ImagePlacement` in `Materialised`, P17),
    /// each with the disk its base reconstructs to when the data plane measured it: a pool is
    /// admissible on this host only once its pin is one of them (CP-07).
    pub placed: BTreeMap<ImageDigest, Option<u64>>,
    /// Its live leases.
    pub leases: Vec<LeaseView>,
}

/// A pool's scale set as the pass reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScaleSetView {
    /// `totalAssignedJobs` of its latest statistics; none before any.
    pub assigned: Option<u32>,
    /// The capacity input it holds.
    pub capacity: Capacity,
}

/// One pool as the pass reads it (no `PartialEq`: a function pointer compares by address).
#[derive(Debug, Clone)]
pub struct PoolView {
    /// The pool.
    pub pool: PoolName,
    /// Its guest OS.
    pub os: GuestOs,
    /// What one VM needs.
    pub vm: Resources,
    /// Prepared VMs held ahead of demand.
    pub min_prepared: u32,
    /// Its mode.
    pub mode: PoolMode,
    /// Its pinned image.
    pub image_pin: Option<ImageDigest>,
    /// Its capacity policy.
    pub policy: PolicyOf,
    /// Its scale set, once ensured.
    pub scale_set: Option<ScaleSetView>,
}

/// What one pass hands the next: when things were first seen. Memory only; lost, it delays.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PassMemory {
    /// When each pool's current surplus of prepared VMs was first seen.
    pub surplus_since: BTreeMap<PoolName, Millis>,
    /// When each lease with no `AttemptStarted` was first seen.
    pub unstarted_since: BTreeMap<AttemptId, Millis>,
    /// The refusals already read, by attempt: the revision of each.
    pub refusals: BTreeMap<AttemptId, u64>,
    /// Hosts held after a refusal, with the report id in force when it was read.
    pub held: BTreeMap<HostId, u64>,
}

/// Everything [`plan_pass`] reads.
#[derive(Debug, Clone)]
pub struct PassInputs {
    /// The control plane's clock, read by the driver.
    pub now: Millis,
    /// The timing.
    pub settings: Settings,
    /// The control plane is in safe mode (I29).
    pub safe_mode: bool,
    /// The hosts.
    pub hosts: Vec<HostView>,
    /// The pools.
    pub pools: Vec<PoolView>,
    /// The previous pass's memory.
    pub memory: PassMemory,
}

/// A command the pass plans; the driver gives each its id and envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassCommand {
    /// `ApplyCapacity` on the pool's scale set.
    ApplyCapacity {
        /// The pool.
        pool: PoolName,
        /// The new input, at the next revision.
        capacity: Capacity,
    },
    /// `ReserveSlot` on the host for a new attempt of the pool.
    ReserveSlot {
        /// The host.
        host_id: HostId,
        /// The pool.
        pool: PoolName,
        /// Its guest OS.
        os: GuestOs,
        /// What the VM needs.
        resources: Resources,
        /// The terms revision the plan read.
        terms_revision: u64,
    },
    /// `RequestRetirement` on the attempt.
    Retire {
        /// The attempt.
        attempt_id: AttemptId,
        /// Why.
        reason: RetireReason,
    },
    /// `ReleaseSlot` on the host.
    ReleaseSlot {
        /// The host.
        host_id: HostId,
        /// The lease's attempt.
        attempt_id: AttemptId,
        /// The evidence.
        evidence: ReleaseEvidence,
    },
    /// `CompleteHostDrain`.
    CompleteHostDrain {
        /// The host.
        host_id: HostId,
    },
    /// `CompletePoolDrain`.
    CompletePoolDrain {
        /// The pool.
        pool: PoolName,
    },
}

/// What the pass derived for one pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PoolNeed {
    /// Its runners and prepared VMs of the pinned image, counted over every host.
    pub runners: RunnerCounts,
    /// VMs on their way to Prepared, reservations not started included.
    pub preparing: u32,
    /// `totalAssignedJobs`, 0 before any statistics.
    pub assigned: u32,
    /// [`demand::shortfall`] of `assigned`.
    pub shortfall: u32,
    /// The new VMs it waits for: the shortfall beyond prepared and preparing VMs, plus the
    /// `min_prepared` deficit; 0 while it may not admit.
    pub waiting: u32,
    /// Prepared VMs beyond the shortfall and `min_prepared`.
    pub surplus: u32,
}

/// One pass's plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pass {
    /// The commands, in the order to issue them.
    pub commands: Vec<PassCommand>,
    /// What the next pass reads.
    pub memory: PassMemory,
    /// Each host's capacity report, in host order (`AdmissionExplain`).
    pub reports: Vec<(HostId, CapacityReport)>,
    /// Each pool's need, in pool order.
    pub needs: BTreeMap<PoolName, PoolNeed>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Tally {
    preparing: u32,
    prepared_current: u32,
    activating: u32,
    listening: u32,
    busy: u32,
}

/// Whether a pool admits new VMs in `mode`.
const fn admits(mode: PoolMode) -> bool {
    match mode {
        PoolMode::Active => true,
        PoolMode::Unconfigured
        | PoolMode::Paused
        | PoolMode::Draining
        | PoolMode::Drained
        | PoolMode::Decommissioned => false,
    }
}

/// Whether a pool's attempts are in a drained scope in `mode`.
const fn drains(mode: PoolMode) -> bool {
    match mode {
        PoolMode::Draining | PoolMode::Drained | PoolMode::Decommissioned => true,
        PoolMode::Unconfigured | PoolMode::Active | PoolMode::Paused => false,
    }
}

/// The wire mode of a pool in `mode` (ADR 0029).
const fn wire_mode(mode: PoolMode, safe_mode: bool) -> WireMode {
    if safe_mode {
        return WireMode::SafeMode;
    }
    match mode {
        PoolMode::Active => WireMode::Open,
        PoolMode::Paused => WireMode::Paused,
        PoolMode::Unconfigured
        | PoolMode::Draining
        | PoolMode::Drained
        | PoolMode::Decommissioned => WireMode::Draining,
    }
}

fn report_id(host: &HostView) -> u64 {
    host.report.map_or(0, |r| r.report_id)
}

/// Whether `pool`'s pinned image has its base materialised on `host` (P17): only then is the
/// pool admissible there. A pool with no pin is not (its terms are closed too).
fn placed_on(pool: &PoolView, host: &HostView) -> bool {
    pool.image_pin
        .as_ref()
        .is_some_and(|pin| host.placed.contains_key(pin))
}

/// What one VM of `pool` needs on `host`: the pool's configured shape, with its disk taken from
/// the pinned image's base on that host when the configuration names none (CP-07, ADR 0052's
/// amendment of 2026-11-12). A pool's disk of 0 is "not configured" (`clone_disk_gib` absent:
/// the composition root configures the pool with no disk); then a clone is planned at the disk
/// its base reconstructs to, as the data plane measured it from the image's manifest
/// (`ImageMaterialised::disk_bytes`, protocol 1.4). A base whose disk was not reported (an
/// older data plane, a manifest that did not measure) leaves it 0: unknown is never a default,
/// and the data plane's gate (I33) refuses what does not fit, as before. A configured disk
/// always wins. Pinned by
/// `tests/reconcile.rs::should_cap_reservations_by_the_hosts_disk_when_the_placed_base_is_measured`.
fn vm_on(pool: &PoolView, host: &HostView) -> Resources {
    if pool.vm.disk_bytes != 0 {
        return pool.vm;
    }
    let measured = pool
        .image_pin
        .as_ref()
        .and_then(|pin| host.placed.get(pin).copied().flatten());
    Resources::new(pool.vm.memory_bytes, measured.unwrap_or(0))
}

/// The image is the pool's pinned one (any image while none is pinned).
fn current_image(pool: &PoolView, image: Option<&ImageDigest>) -> bool {
    pool.image_pin
        .as_ref()
        .is_none_or(|pin| image.is_none_or(|i| i == pin))
}

/// The pool's need from its tally and scale set.
fn need_of(pool: &PoolView, tally: Tally, safe_mode: bool) -> PoolNeed {
    let runners = RunnerCounts {
        activating: tally.activating,
        listening: tally.listening,
        busy: tally.busy,
        prepared: tally.prepared_current,
    };
    let assigned = pool.scale_set.and_then(|s| s.assigned).unwrap_or(0);
    let shortfall = demand::shortfall(assigned, &runners);
    let in_flight = tally.preparing.saturating_add(tally.prepared_current);
    let for_jobs = shortfall.saturating_sub(in_flight);
    let spare = in_flight.saturating_sub(shortfall);
    let deficit = pool.min_prepared.saturating_sub(spare);
    let open = admits(pool.mode) && !safe_mode;
    PoolNeed {
        runners,
        preparing: tally.preparing,
        assigned,
        shortfall,
        waiting: if open {
            for_jobs.saturating_add(deficit)
        } else {
            0
        },
        surplus: if open {
            tally
                .prepared_current
                .min(spare.saturating_sub(pool.min_prepared))
        } else {
            0
        },
    }
}

/// One reconciler pass: pure, total and deterministic (the order of the inputs' hosts, pools and
/// leases does not matter: `tests/reconcile.rs::should_plan_the_same_pass_when_inputs_are_permuted`).
#[must_use]
pub fn plan_pass(inputs: &PassInputs) -> Pass {
    let now = inputs.now;
    let safe = inputs.safe_mode;
    let mut hosts: Vec<HostView> = inputs.hosts.clone();
    hosts.sort_by(|a, b| a.host_id.cmp(&b.host_id));
    hosts.dedup_by(|a, b| a.host_id == b.host_id);
    for host in &mut hosts {
        host.leases.sort_by_key(|a| a.attempt_id);
    }
    let mut pools: Vec<&PoolView> = inputs.pools.iter().collect();
    pools.sort_by(|a, b| a.pool.cmp(&b.pool));
    pools.dedup_by(|a, b| a.pool == b.pool);
    let pool_of: BTreeMap<&PoolName, &PoolView> = pools.iter().map(|p| (&p.pool, *p)).collect();

    let mut memory = read_refusals(&hosts, &inputs.memory);
    let needs = needs_of(&hosts, &pools, safe);

    let mut commands = Vec::new();
    let mut reports = Vec::with_capacity(hosts.len());
    let mut remaining: BTreeMap<PoolName, u32> =
        needs.iter().map(|(p, n)| (p.clone(), n.waiting)).collect();
    let mut sums: BTreeMap<PoolName, (u32, u32)> = BTreeMap::new();
    for host in &hosts {
        let held = memory.held.contains_key(&host.host_id);
        let plan_inputs = PlanInputs {
            report: host.report.map(|r| r.capacity),
            reserves: host.reserves,
            admission_open: host.admission_open && !held && !safe,
            leases: host
                .leases
                .iter()
                .map(|l| LeaseInput {
                    pool: l.pool.clone(),
                    holds: l.holds,
                    quarantined: l.quarantined,
                })
                .collect(),
            pools: pools
                .iter()
                .map(|p| {
                    let terms = host.terms.get(&p.pool);
                    let wanted = remaining.get(&p.pool).copied().unwrap_or(0);
                    PoolInput {
                        pool: p.pool.clone(),
                        os: p.os,
                        vm: vm_on(p, host),
                        open: terms.is_some_and(|t| t.open) && placed_on(p, host),
                        max_running: terms.map_or(0, |t| t.max_running),
                        wanted,
                        policy: (p.policy)(wanted),
                    }
                })
                .collect(),
        };
        let (report, decisions) = capacity::plan(&plan_inputs);
        for decision in decisions {
            match decision {
                PlanDecision::Reserve {
                    pool,
                    os,
                    resources,
                } => {
                    let terms_revision = host.terms.get(&pool).map_or(0, |t| t.revision);
                    if let Some(left) = remaining.get_mut(&pool) {
                        *left = left.saturating_sub(1);
                    }
                    commands.push(PassCommand::ReserveSlot {
                        host_id: host.host_id.clone(),
                        pool,
                        os,
                        resources,
                        terms_revision,
                    });
                }
                PlanDecision::Hold { .. } => {}
            }
        }
        for line in &report.pools {
            let sum = sums.entry(line.pool.clone()).or_default();
            sum.0 = sum.0.saturating_add(line.deliverable);
            sum.1 = sum.1.saturating_add(line.reported_max);
        }
        reports.push((host.host_id.clone(), report));
    }

    let mut capacity_commands = Vec::new();
    for pool in &pools {
        let Some(scale_set) = pool.scale_set else {
            continue;
        };
        let (deliverable, reported_max) = sums.get(&pool.pool).copied().unwrap_or((0, 0));
        let mode = wire_mode(pool.mode, safe);
        let current = scale_set.capacity;
        if (current.deliverable, current.reported_max, current.mode)
            != (deliverable, reported_max, mode)
        {
            capacity_commands.push(PassCommand::ApplyCapacity {
                pool: pool.pool.clone(),
                capacity: Capacity {
                    revision: current.revision.saturating_add(1),
                    deliverable,
                    reported_max,
                    mode,
                },
            });
        }
    }
    // the scale sets learn the new input before any reservation is made under it, and the
    // releases go before the reservations, so a slot is given back before one is asked for
    let reservations = commands;
    let mut commands = capacity_commands;
    if safe {
        commands.extend(reservations);
    } else {
        let (releases, completions) = cleanup(&hosts, &pools, now, inputs.settings, &mut memory);
        commands.extend(releases);
        commands.extend(reservations);
        commands.extend(retirements(
            &hosts,
            &pool_of,
            &needs,
            now,
            inputs.settings,
            &mut memory,
        ));
        commands.extend(completions);
    }
    Pass {
        commands,
        memory,
        reports,
        needs,
    }
}

/// The memory after reading every refusal: a refusal not read before holds its host at the
/// report in force now; a hold ends once a newer report is applied.
fn read_refusals(hosts: &[HostView], before: &PassMemory) -> PassMemory {
    let mut memory = before.clone();
    let mut live = BTreeSet::new();
    for host in hosts {
        for lease in &host.leases {
            live.insert(lease.attempt_id);
            if let Some(revision) = lease.refusal
                && memory.refusals.get(&lease.attempt_id) != Some(&revision)
            {
                memory.refusals.insert(lease.attempt_id, revision);
                memory.held.insert(host.host_id.clone(), report_id(host));
            }
        }
    }
    memory.refusals.retain(|attempt, _| live.contains(attempt));
    memory.held.retain(|host_id, at| {
        hosts
            .iter()
            .find(|h| &h.host_id == host_id)
            .is_some_and(|h| report_id(h) <= *at)
    });
    memory
}

/// The stage a lease counts as in its pool's need: a VM on its way to Prepared, a prepared VM
/// or a listening runner on a draining host is retired there (P23), and an attempt whose
/// retirement is requested is on its way out; neither serves the pool any more.
const fn counted_stage(host: &HostView, lease: &LeaseView) -> RunnerStage {
    match lease.stage {
        RunnerStage::NotStarted
        | RunnerStage::Preparing
        | RunnerStage::Prepared
        | RunnerStage::Listening
            if host.draining =>
        {
            RunnerStage::Ending
        }
        RunnerStage::Prepared | RunnerStage::Listening | RunnerStage::Activating
            if lease.retiring =>
        {
            RunnerStage::Ending
        }
        RunnerStage::NotStarted => RunnerStage::NotStarted,
        RunnerStage::Preparing => RunnerStage::Preparing,
        RunnerStage::Prepared => RunnerStage::Prepared,
        RunnerStage::Activating => RunnerStage::Activating,
        RunnerStage::Listening => RunnerStage::Listening,
        RunnerStage::Busy => RunnerStage::Busy,
        RunnerStage::Ending => RunnerStage::Ending,
        RunnerStage::Gone(evidence) => RunnerStage::Gone(evidence),
        RunnerStage::Quarantined => RunnerStage::Quarantined,
    }
}

/// Each pool's need, counted over every host's unquarantined leases.
fn needs_of(hosts: &[HostView], pools: &[&PoolView], safe: bool) -> BTreeMap<PoolName, PoolNeed> {
    let mut tallies: BTreeMap<&PoolName, Tally> =
        pools.iter().map(|p| (&p.pool, Tally::default())).collect();
    for (host, lease) in hosts
        .iter()
        .flat_map(|h| h.leases.iter().map(move |l| (h, l)))
    {
        let Some(pool) = pools.iter().find(|p| p.pool == lease.pool) else {
            continue;
        };
        let Some(tally) = tallies.get_mut(&pool.pool) else {
            continue;
        };
        if lease.quarantined {
            continue;
        }
        match counted_stage(host, lease) {
            RunnerStage::NotStarted | RunnerStage::Preparing => {
                tally.preparing = tally.preparing.saturating_add(1);
            }
            RunnerStage::Prepared => {
                if current_image(pool, lease.image.as_ref()) {
                    tally.prepared_current = tally.prepared_current.saturating_add(1);
                }
            }
            RunnerStage::Activating => tally.activating = tally.activating.saturating_add(1),
            RunnerStage::Listening => tally.listening = tally.listening.saturating_add(1),
            RunnerStage::Busy => tally.busy = tally.busy.saturating_add(1),
            RunnerStage::Ending | RunnerStage::Gone(..) | RunnerStage::Quarantined => {}
        }
    }
    pools
        .iter()
        .map(|p| {
            let tally = tallies.get(&p.pool).copied().unwrap_or_default();
            (p.pool.clone(), need_of(p, tally, safe))
        })
        .collect()
}

/// Retirements: the drained scope, images no longer pinned, and a surplus that lasted the
/// hysteresis. Never a quarantined lease, never a busy runner.
fn retirements(
    hosts: &[HostView],
    pool_of: &BTreeMap<&PoolName, &PoolView>,
    needs: &BTreeMap<PoolName, PoolNeed>,
    now: Millis,
    settings: Settings,
    memory: &mut PassMemory,
) -> Vec<PassCommand> {
    let mut retired: BTreeMap<AttemptId, RetireReason> = BTreeMap::new();
    let mut surplus_candidates: BTreeMap<&PoolName, Vec<(&HostId, AttemptId)>> = BTreeMap::new();
    for host in hosts {
        for lease in &host.leases {
            let Some(pool) = pool_of.get(&lease.pool) else {
                continue;
            };
            if lease.quarantined || lease.retiring {
                continue;
            }
            let in_drain = host.draining || drains(pool.mode);
            let current = current_image(pool, lease.image.as_ref());
            let reason = match lease.stage {
                RunnerStage::Prepared if in_drain => Some(RetireReason::Drain),
                RunnerStage::Prepared if !current => Some(RetireReason::ImageReplaced),
                RunnerStage::Listening if in_drain => Some(RetireReason::Drain),
                RunnerStage::Prepared => {
                    surplus_candidates
                        .entry(&pool.pool)
                        .or_default()
                        .push((&host.host_id, lease.attempt_id));
                    None
                }
                RunnerStage::NotStarted
                | RunnerStage::Preparing
                | RunnerStage::Activating
                | RunnerStage::Listening
                | RunnerStage::Busy
                | RunnerStage::Ending
                | RunnerStage::Gone(..)
                | RunnerStage::Quarantined => None,
            };
            if let Some(reason) = reason {
                retired.insert(lease.attempt_id, reason);
            }
        }
    }
    let mut out: Vec<PassCommand> = retired
        .into_iter()
        .map(|(attempt_id, reason)| PassCommand::Retire { attempt_id, reason })
        .collect();
    for (pool, need) in needs {
        if need.surplus == 0 {
            memory.surplus_since.remove(pool);
            continue;
        }
        let since = *memory.surplus_since.entry(pool.clone()).or_insert(now);
        if now.get().saturating_sub(since.get()) < settings.hysteresis.get() {
            continue;
        }
        let mut candidates = surplus_candidates.remove(pool).unwrap_or_default();
        candidates.sort();
        let take = usize::try_from(need.surplus).unwrap_or(usize::MAX);
        for (_, attempt_id) in candidates.into_iter().rev().take(take) {
            out.push(PassCommand::Retire {
                attempt_id,
                reason: RetireReason::Surplus,
            });
        }
    }
    out
}

/// Cleanup: releases on evidence and of reservations never started, then drain completions.
fn cleanup(
    hosts: &[HostView],
    pools: &[&PoolView],
    now: Millis,
    settings: Settings,
    memory: &mut PassMemory,
) -> (Vec<PassCommand>, Vec<PassCommand>) {
    let mut out = Vec::new();
    let mut completions = Vec::new();
    let mut unstarted = BTreeSet::new();
    for host in hosts {
        for lease in &host.leases {
            let evidence = match lease.stage {
                RunnerStage::Gone(evidence) => Some(evidence),
                RunnerStage::NotStarted => {
                    unstarted.insert(lease.attempt_id);
                    let since = *memory
                        .unstarted_since
                        .entry(lease.attempt_id)
                        .or_insert(now);
                    let late = now.get().saturating_sub(since.get()) >= settings.start_grace.get();
                    (late && !lease.quarantined).then_some(ReleaseEvidence::AttemptNeverStarted)
                }
                RunnerStage::Preparing
                | RunnerStage::Prepared
                | RunnerStage::Activating
                | RunnerStage::Listening
                | RunnerStage::Busy
                | RunnerStage::Ending
                | RunnerStage::Quarantined => None,
            };
            if let Some(evidence) = evidence {
                out.push(PassCommand::ReleaseSlot {
                    host_id: host.host_id.clone(),
                    attempt_id: lease.attempt_id,
                    evidence,
                });
            }
        }
        if host.draining && host.leases.is_empty() {
            completions.push(PassCommand::CompleteHostDrain {
                host_id: host.host_id.clone(),
            });
        }
    }
    memory
        .unstarted_since
        .retain(|attempt, _| unstarted.contains(attempt));
    for pool in pools {
        let draining = match pool.mode {
            PoolMode::Draining => true,
            PoolMode::Unconfigured
            | PoolMode::Active
            | PoolMode::Paused
            | PoolMode::Drained
            | PoolMode::Decommissioned => false,
        };
        let leased = hosts
            .iter()
            .flat_map(|h| &h.leases)
            .any(|l| l.pool == pool.pool);
        if draining && !leased {
            completions.push(PassCommand::CompletePoolDrain {
                pool: pool.pool.clone(),
            });
        }
    }
    (out, completions)
}
