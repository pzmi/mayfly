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

/// What the operator set for this host (ADR 0052: configured values, not defaults for facts
/// the probe failed to read). Composition fills it from the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostConfig {
    /// The provider's home (`TART_HOME`): free disk is read for the volume that holds it.
    pub provider_home: PathBuf,
    /// The VM slots this host offers.
    pub vm_slots: u32,
    /// The macOS guests the hypervisor runs at once, foreign ones included.
    pub macos_guests: u32,
    /// What the operator keeps back from VMs.
    pub reserve: Capacity,
    /// Free disk under this is the low-water mark (F17): nothing that needs disk starts.
    pub disk_low_water_bytes: u64,
    /// Available memory under this is memory pressure: nothing that needs memory starts.
    pub memory_low_water_bytes: u64,
    /// Time asleep at or above this between two `HostFacts` reports is a wake (P22).
    pub wake_gap: Duration,
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
    /// The clocks at the last `HostFacts` report (or at construction), the origin of the next
    /// report's wake check; `None` when they could not be read then.
    last: Mutex<Option<Clocks>>,
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
    #[must_use]
    pub fn with_source(config: HostConfig, provider: P, source: S) -> Self {
        let last = source.clocks().ok();
        Self {
            config,
            provider,
            source,
            last: Mutex::new(last),
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

    /// Whether the host slept at least [`HostConfig::wake_gap`] since the last report, which
    /// this reading then becomes the origin of. Clocks that could not be read last time make
    /// the answer unknown: a failed read, never `false`.
    fn woke(&self, now: Clocks) -> Result<bool, ReadFailed> {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        let before = last.replace(now).ok_or(ReadFailed)?;
        Ok(slept(before, now) >= self.config.wake_gap)
    }
}

/// How long the host slept between two readings: how much further the clock that counts sleep
/// moved than the one that does not. Saturates at zero, so reading jitter is never a sleep.
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
/// Pinned by `tests/probe.rs::should_read_totals_foreign_use_and_both_marks_when_the_host_is_readable`.
#[must_use]
pub fn reading(
    config: &HostConfig,
    volume: Volume,
    memory: Memory,
    foreign: ForeignUse,
) -> HostReading {
    HostReading {
        totals: Capacity {
            vm_slots: config.vm_slots,
            macos_guests: config.macos_guests,
            resources: Resources::new(memory.total_bytes, volume.total_bytes),
        },
        reserve: config.reserve,
        foreign: foreign.used,
        foreign_unsized: foreign.unsized_in,
        disk_low_water: volume.free_bytes < config.disk_low_water_bytes,
        memory_pressure: memory.available_bytes < config.memory_low_water_bytes,
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
        let foreign = self.foreign().await;
        Ok(reading(&self.config, volume, memory, foreign))
    }

    /// Every fallible reading is taken before the clocks, so a failure never consumes a wake:
    /// the next report still sees it.
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
