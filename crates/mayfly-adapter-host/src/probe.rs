//! [`OsProbe`]: the `HostProbe` port over a [`HostSource`] and a [`ProviderView`].

use std::future::Future;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use mayfly_data::{
    Capacity, ForeignUse, HostFacts, HostProbe, HostReading, Inventory, ProbeFailed,
    ResourceDimension, foreign_use,
};
use mayfly_domain::Resources;

use crate::source::{Clocks, HostSource, Memory, Os, ReadFailed, Volume};

/// What the operator set for this host, and what the probe measures where nothing is set
/// (ADR 0052 as amended 2026-11-12: a measured fact is not an assumption; a reading that fails
/// is still never replaced by a default). Composition fills it from the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostConfig {
    /// The provider's home (`TART_HOME`): free disk is read for the volume that holds it.
    pub provider_home: PathBuf,
    /// The VM slots this host offers when configured; `None` measures them on every reading
    /// from physical memory and CPUs ([`vm_slots`]).
    pub vm_slots: Option<u32>,
    /// The largest VM a pool runs: what one measured slot must hold.
    pub vm_shape: VmShape,
    /// The macOS guests the hypervisor runs at once, foreign ones included.
    pub macos_guests: u32,
    /// Memory kept back from VMs when configured; `None` measures it on every reading
    /// ([`reserve_memory`]).
    pub reserve_memory_bytes: Option<u64>,
    /// Disk kept back from VMs.
    pub reserve_disk_bytes: u64,
    /// CPUs kept back from VMs when slots are measured.
    pub reserve_cpus: u32,
    /// Free disk under this is the low-water mark (F17): nothing that needs disk starts.
    pub disk_low_water_bytes: u64,
    /// Available memory under this is memory pressure: nothing that needs memory starts.
    /// `None`: the memory reserve in effect, configured or measured.
    pub memory_low_water_bytes: Option<u64>,
    /// Time asleep at or above this since the last reported wake is a wake (P22).
    pub wake_gap: Duration,
}

/// What one VM of the largest pool takes: the divisor of a measured slot count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmShape {
    /// Its memory.
    pub memory_bytes: u64,
    /// Its CPUs.
    pub cpus: u32,
}

/// The least memory a measured reserve keeps for the person using the machine.
pub const RESERVE_MEMORY_FLOOR: u64 = 8 << 30;

/// The memory reserve on a host of `total_bytes` physical memory: the configured one, else
/// a quarter of it but at least [`RESERVE_MEMORY_FLOOR`] (the owner's default, ADR 0052's
/// amendment of 2026-11-12). Pinned by
/// `tests/probe.rs::should_measure_slots_from_memory_and_cpus_when_none_are_configured`.
#[must_use]
pub fn reserve_memory(config: &HostConfig, total_bytes: u64) -> u64 {
    config
        .reserve_memory_bytes
        .unwrap_or_else(|| (total_bytes / 4).max(RESERVE_MEMORY_FLOOR))
}

/// The VM slots a host of `memory_bytes` physical memory and `cpus` CPUs offers after
/// `reserve_memory_bytes` and `reserve_cpus`: as many VMs of `shape` as fit in both, never
/// more than either allows (a reserve larger than the host leaves none). A zero dimension of
/// `shape` does not bound. Pinned by
/// `tests/probe.rs::should_measure_slots_from_memory_and_cpus_when_none_are_configured` and
/// `should_bound_measured_slots_by_cpus_when_cpus_are_scarcer`.
#[must_use]
pub fn vm_slots(
    memory_bytes: u64,
    cpus: u32,
    reserve_memory_bytes: u64,
    reserve_cpus: u32,
    shape: VmShape,
) -> u32 {
    let by_memory = memory_bytes
        .saturating_sub(reserve_memory_bytes)
        .checked_div(shape.memory_bytes)
        .unwrap_or(u64::MAX);
    let by_cpus = u64::from(cpus.saturating_sub(reserve_cpus))
        .checked_div(u64::from(shape.cpus))
        .unwrap_or(u64::MAX);
    u32::try_from(by_memory.min(by_cpus)).unwrap_or(u32::MAX)
}

/// What the probe asks the VM provider; composition implements it over the provider adapter
/// (the Tart adapter's `inventory` and `tart --version`), which this crate never names.
pub trait ProviderView: Send + Sync {
    /// Every VM the provider lists, foreign ones included, or `Incomplete`.
    fn inventory(&self) -> impl Future<Output = Inventory> + Send;

    /// The provider's version string.
    ///
    /// # Errors
    /// [`ReadFailed`] when it cannot be read.
    fn version(&self) -> impl Future<Output = Result<String, ReadFailed>> + Send;
}

/// The host probe over the operating system ([`Os`] by default) and the provider.
#[derive(Debug)]
pub struct OsProbe<P, S = Os> {
    config: HostConfig,
    provider: P,
    source: S,
    /// The origin of the wake check: the clocks at construction, then at the last report that
    /// said `woke`. It moves only when a wake is reported, so sleeps each under the gap add up
    /// until together they reach it. `None` when they could not be read at construction (see
    /// [`OsProbe::with_source`]).
    origin: Mutex<Option<Clocks>>,
}

impl<P: ProviderView> OsProbe<P> {
    /// A probe of the real host.
    #[must_use]
    pub fn new(config: HostConfig, provider: P) -> Self {
        Self::with_source(config, provider, Os::new())
    }
}

impl<P: ProviderView, S: HostSource> OsProbe<P, S> {
    /// A probe that reads the host through `source`: the seam a test scripts.
    ///
    /// The clocks read now are the wake check's origin. If they cannot be read, there is no
    /// origin: the first report that reads them fails (whether the host slept is unknown, never
    /// `false`) and becomes the origin, so a sleep before that report is never seen as a wake.
    /// That window is the same as a sleep before the daemon started: nothing measured it, and
    /// a reboot within it is the boot id's to show. Pinned by
    /// `tests/probe.rs::should_fail_facts_rather_than_say_not_woken_when_the_clocks_were_unreadable`.
    #[must_use]
    pub fn with_source(config: HostConfig, provider: P, source: S) -> Self {
        let origin = source.clocks().ok();
        Self {
            config,
            provider,
            source,
            origin: Mutex::new(origin),
        }
    }

    /// The identity of this boot, for the handshake's `boot_id` (plan §3.3).
    ///
    /// # Errors
    /// [`ReadFailed`] when it cannot be read.
    pub fn boot_id(&self) -> Result<String, ReadFailed> {
        self.source.boot_id()
    }

    /// What the VMs that are not ours use; a listing that is not complete sizes none of them,
    /// so every dimension is unsized (ADR 0052), never zero.
    async fn foreign(&self) -> ForeignUse {
        match self.provider.inventory().await {
            Inventory::Complete { vms } => foreign_use(&vms),
            Inventory::Incomplete { .. } => ForeignUse {
                used: Capacity::default(),
                unsized_in: ResourceDimension::ALL.into_iter().collect(),
                running: 0,
            },
        }
    }

    /// Whether the host slept at least [`HostConfig::wake_gap`] in all since the last reported
    /// wake; a reported wake makes `now` the origin, anything else leaves it, so sub-gap sleeps
    /// accumulate. No origin makes the answer unknown: a failed read, never `false`, and `now`
    /// becomes the origin. Pinned by
    /// `tests/probe.rs::should_report_a_wake_when_sleeps_under_the_gap_add_up_to_it`.
    fn woke(&self, now: Clocks) -> Result<bool, ReadFailed> {
        let mut origin = self.origin.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(before) = *origin else {
            *origin = Some(now);
            return Err(ReadFailed);
        };
        let woke = slept(before, now) >= self.config.wake_gap;
        if woke {
            *origin = Some(now);
        }
        Ok(woke)
    }
}

/// How long the host slept between two readings: how much further the clock that counts sleep
/// moved than the one that does not. Saturates at zero, so reading jitter is never a sleep.
/// Both readings are absolute, so jitter does not add up across reports however long the
/// origin is kept.
/// Pinned by `tests/probe.rs::should_report_a_wake_once_when_the_host_slept_past_the_gap`.
#[must_use]
pub fn slept(before: Clocks, now: Clocks) -> Duration {
    let total = now.with_sleep.saturating_sub(before.with_sleep);
    let awake = now.awake.saturating_sub(before.awake);
    total.saturating_sub(awake)
}

/// The ledger's host reading from the host's readings, the operator's configuration and the
/// foreign VMs. Totals are physical memory and the volume's size; what is actually free is
/// watched by the two low-water marks, which stop what needs that dimension (F17). The
/// hypervisor's own refusal is not something the host shows: the provider answers it as
/// `HypervisorLimitHit`, so the probe never sets [`HostReading::hypervisor_limit`], and the
/// ledger still stops macOS guests at [`HostConfig::macos_guests`], foreign ones included.
///
/// Slots not configured are measured ([`vm_slots`]) from `cpus`; when the CPUs could not be
/// read (`None`) the slot count is not known, so `vm_slots` is reported unsized (no slot is
/// counted, and the ledger refuses `capacity_unknown{vm_slots}`), never guessed (ADR 0052).
/// Pinned by `tests/probe.rs::should_read_totals_foreign_use_and_both_marks_when_the_host_is_readable`
/// and `should_leave_slots_unknown_when_the_cpus_cannot_be_read`.
#[must_use]
pub fn reading(
    config: &HostConfig,
    volume: Volume,
    memory: Memory,
    cpus: Option<u32>,
    foreign: ForeignUse,
) -> HostReading {
    let reserve_memory = reserve_memory(config, memory.total_bytes);
    let mut foreign_unsized = foreign.unsized_in;
    let slots = config.vm_slots.or_else(|| {
        cpus.map(|cpus| {
            vm_slots(
                memory.total_bytes,
                cpus,
                reserve_memory,
                config.reserve_cpus,
                config.vm_shape,
            )
        })
    });
    if slots.is_none() {
        foreign_unsized.insert(ResourceDimension::VmSlots);
    }
    let low_water = config.memory_low_water_bytes.unwrap_or(reserve_memory);
    HostReading {
        totals: Capacity {
            vm_slots: slots.unwrap_or(0),
            macos_guests: config.macos_guests,
            resources: Resources::new(memory.total_bytes, volume.total_bytes),
        },
        reserve: Capacity {
            vm_slots: 0,
            macos_guests: 0,
            resources: Resources::new(reserve_memory, config.reserve_disk_bytes),
        },
        foreign: foreign.used,
        foreign_unsized,
        disk_low_water: volume.free_bytes < config.disk_low_water_bytes,
        memory_pressure: memory.available_bytes < low_water,
        hypervisor_limit: false,
    }
}

impl<P: ProviderView, S: HostSource> HostProbe for OsProbe<P, S> {
    /// Reads the host afresh on every call (the contract's `ReadsTheHostEachCall`): no reading
    /// is cached or carried over a failure.
    async fn resources(&self) -> Result<HostReading, ProbeFailed> {
        let volume = self
            .source
            .volume(&self.config.provider_home)
            .map_err(|ReadFailed| ProbeFailed)?;
        let memory = self.source.memory().map_err(|ReadFailed| ProbeFailed)?;
        let cpus = match self.config.vm_slots {
            Some(_) => None,
            None => self.source.cpus().ok(),
        };
        let foreign = self.foreign().await;
        Ok(reading(&self.config, volume, memory, cpus, foreign))
    }

    /// Every fallible reading is taken before the clocks, so a failure never consumes a wake:
    /// the next report still sees it.
    ///
    /// An incomplete provider listing fails the whole report, free disk and the wake included:
    /// `HostFacts` cannot say that `external_guests` is unknown, and a count of zero would be a
    /// default (ADR 0052). That is the safe side: a failed report is unknown everywhere, the
    /// ledger's own `resources` still read the disk, and the wake waits for the next report.
    /// Pinned by
    /// `tests/probe.rs::should_keep_the_wake_for_the_next_report_when_the_listing_is_incomplete`.
    ///
    /// `external_guests` counts every running VM not of our well-formed shape, malformed
    /// `mf-*` and base `mfb-*` VMs included ([`mayfly_data::ForeignUse::running`]): the
    /// hypervisor's guest limit counts them all. Pinned by
    /// `tests/probe.rs::should_count_every_running_guest_not_ours_when_facts_are_reported`.
    async fn facts(&self) -> Result<HostFacts, ProbeFailed> {
        let volume = self
            .source
            .volume(&self.config.provider_home)
            .map_err(|ReadFailed| ProbeFailed)?;
        let tart_version = self
            .provider
            .version()
            .await
            .map_err(|ReadFailed| ProbeFailed)?;
        let external_guests = match self.provider.inventory().await {
            Inventory::Complete { vms } => foreign_use(&vms).running,
            Inventory::Incomplete { .. } => return Err(ProbeFailed),
        };
        let now = self.source.clocks().map_err(|ReadFailed| ProbeFailed)?;
        let woke = self.woke(now).map_err(|ReadFailed| ProbeFailed)?;
        Ok(HostFacts {
            disk_free_bytes: volume.free_bytes,
            disk_low_water: volume.free_bytes < self.config.disk_low_water_bytes,
            woke,
            tart_version,
            external_guests,
        })
    }
}
