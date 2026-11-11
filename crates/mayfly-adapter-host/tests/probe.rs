//! `OsProbe` over a scripted host and provider: the `HostProbe` contract (ver §5), what it
//! makes of the readings, and that a failing read is unknown, never a default (ADR 0052).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use mayfly_adapter_host::{
    Clocks, HostConfig, HostSource, Memory, OsProbe, ProviderView, ReadFailed, Volume,
};
use mayfly_data::{
    Capacity, GuestOs, HostProbe, HostReading, Inventory, InventoryGap, ProbeFailed,
    ResourceDimension, VmObservation,
};
use mayfly_domain::Resources;
use mayfly_testkit_data::host_contract::{
    DISK_LOW_WATER, HostState, HostWorld, MEMORY_LOW_WATER, check_host_probe_contract,
};

const GIB: u64 = 1 << 30;
const WAKE_GAP: Duration = Duration::from_secs(60);

/// Which read the scripted host fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Broken {
    Volume,
    Memory,
    Clocks,
}

#[derive(Debug)]
struct Host {
    volume: Volume,
    memory: Memory,
    clocks: Clocks,
    broken: BTreeSet<Broken>,
    asked: Vec<PathBuf>,
}

/// A host whose readings a test sets, and whose reads it can make fail one by one.
#[derive(Debug, Clone)]
struct ScriptedHost(Arc<Mutex<Host>>);

impl ScriptedHost {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Host {
            volume: Volume {
                total_bytes: 1000 * GIB,
                free_bytes: 500 * GIB,
            },
            memory: Memory {
                total_bytes: 64 * GIB,
                available_bytes: 48 * GIB,
            },
            clocks: Clocks {
                awake: Duration::from_secs(1),
                with_sleep: Duration::from_secs(1),
            },
            broken: BTreeSet::new(),
            asked: Vec::new(),
        })))
    }

    fn with<T>(&self, f: impl FnOnce(&mut Host) -> T) -> T {
        f(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn breaks(&self, read: Broken, broken: bool) {
        self.with(|h| {
            if broken {
                h.broken.insert(read);
            } else {
                h.broken.remove(&read);
            }
        });
    }

    fn answer<T>(&self, read: Broken, value: impl FnOnce(&Host) -> T) -> Result<T, ReadFailed> {
        self.with(|h| {
            if h.broken.contains(&read) {
                Err(ReadFailed)
            } else {
                Ok(value(h))
            }
        })
    }

    /// Moves both clocks: `awake` of running time, and `asleep` more on the clock that counts
    /// sleep.
    fn pass(&self, awake: Duration, asleep: Duration) {
        self.with(|h| {
            h.clocks.awake += awake;
            h.clocks.with_sleep += awake + asleep;
        });
    }
}

impl HostSource for ScriptedHost {
    fn volume(&self, path: &Path) -> Result<Volume, ReadFailed> {
        self.with(|h| h.asked.push(path.to_owned()));
        self.answer(Broken::Volume, |h| h.volume)
    }

    fn memory(&self) -> Result<Memory, ReadFailed> {
        self.answer(Broken::Memory, |h| h.memory)
    }

    fn boot_id(&self) -> Result<String, ReadFailed> {
        Ok("boot-1".to_owned())
    }

    fn clocks(&self) -> Result<Clocks, ReadFailed> {
        self.answer(Broken::Clocks, |h| h.clocks)
    }
}

/// A provider listing a test sets.
#[derive(Debug, Clone)]
struct ScriptedProvider(Arc<Mutex<Inventory>>);

impl ScriptedProvider {
    fn listing(vms: Vec<VmObservation>) -> Self {
        Self(Arc::new(Mutex::new(Inventory::Complete { vms })))
    }

    fn set(&self, inventory: Inventory) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = inventory;
    }
}

impl ProviderView for ScriptedProvider {
    async fn inventory(&self) -> Inventory {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    async fn version(&self) -> Result<String, ReadFailed> {
        Ok("2.0.0".to_owned())
    }
}

fn config() -> HostConfig {
    HostConfig {
        provider_home: PathBuf::from("/var/lib/mayfly/tart"),
        vm_slots: 4,
        macos_guests: 2,
        reserve: Capacity {
            vm_slots: 0,
            macos_guests: 0,
            resources: Resources::new(8 * GIB, 50 * GIB),
        },
        disk_low_water_bytes: DISK_LOW_WATER,
        memory_low_water_bytes: MEMORY_LOW_WATER,
        wake_gap: WAKE_GAP,
    }
}

fn observed(name: &str, running: bool) -> VmObservation {
    VmObservation {
        name: name.to_owned(),
        instance_id: None,
        running,
        guest: Some(GuestOs::Macos),
        memory_bytes: Some(4 * GIB),
        disk_bytes: Some(60 * GIB),
    }
}

fn probe(
    host: &ScriptedHost,
    provider: &ScriptedProvider,
) -> OsProbe<ScriptedProvider, ScriptedHost> {
    OsProbe::with_source(config(), provider.clone(), host.clone())
}

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime")
        .block_on(future)
}

struct World {
    host: ScriptedHost,
    probe: OsProbe<ScriptedProvider, ScriptedHost>,
}

impl HostWorld for World {
    type Probe = OsProbe<ScriptedProvider, ScriptedHost>;

    fn probe(&self) -> &Self::Probe {
        &self.probe
    }

    fn set(&self, state: HostState) {
        self.host.with(|h| {
            h.volume.free_bytes = state.disk_free_bytes;
            h.memory.available_bytes = state.memory_available_bytes;
        });
    }

    fn fail(&self, failing: bool) {
        for read in [Broken::Volume, Broken::Memory, Broken::Clocks] {
            self.host.breaks(read, failing);
        }
    }
}

#[test]
fn should_pass_every_case_when_the_os_probe_runs_the_contract() {
    // given
    let make = || {
        let host = ScriptedHost::new();
        let provider = ScriptedProvider::listing(Vec::new());
        World {
            probe: probe(&host, &provider),
            host,
        }
    };

    // when
    let report = check_host_probe_contract(&make);

    // then
    assert!(report.is_green(), "{report}");
}

#[test]
fn should_read_totals_foreign_use_and_both_marks_when_the_host_is_readable() {
    // given: a foreign macOS VM running, one of ours and a base listed beside it, and a host
    // short of disk and memory
    let host = ScriptedHost::new();
    host.with(|h| {
        h.volume.free_bytes = DISK_LOW_WATER - 1;
        h.memory.available_bytes = MEMORY_LOW_WATER - 1;
    });
    let provider = ScriptedProvider::listing(vec![
        observed("someone-elses", true),
        observed("mfb-macos-15", false),
        observed("mf-not-ours-shape", false),
    ]);
    let probe = probe(&host, &provider);

    // when
    let reading = block_on(probe.resources());

    // then
    assert_eq!(
        reading,
        Ok(HostReading {
            totals: Capacity {
                vm_slots: 4,
                macos_guests: 2,
                resources: Resources::new(64 * GIB, 1000 * GIB),
            },
            reserve: config().reserve,
            foreign: Capacity {
                vm_slots: 1,
                macos_guests: 1,
                resources: Resources::new(4 * GIB, 60 * GIB),
            },
            foreign_unsized: BTreeSet::new(),
            disk_low_water: true,
            memory_pressure: true,
            hypervisor_limit: false,
        })
    );
    assert_eq!(
        host.with(|h| h.asked.clone()),
        vec![config().provider_home],
        "free disk is read for the provider's home"
    );
}

#[test]
fn should_fail_the_probe_rather_than_default_when_a_host_read_fails() {
    for read in [Broken::Volume, Broken::Memory] {
        // given
        let host = ScriptedHost::new();
        let provider = ScriptedProvider::listing(Vec::new());
        let probe = probe(&host, &provider);
        host.breaks(read, true);

        // when
        let reading = block_on(probe.resources());

        // then: the executor makes every dimension unknown on `ProbeFailed`
        assert_eq!(reading, Err(ProbeFailed), "{read:?}");
    }
}

#[test]
fn should_leave_every_foreign_dimension_unsized_when_the_listing_is_incomplete() {
    // given
    let host = ScriptedHost::new();
    let provider = ScriptedProvider::listing(Vec::new());
    provider.set(Inventory::Incomplete {
        reason: InventoryGap::ListFailed,
    });
    let probe = probe(&host, &provider);

    // when
    let reading = block_on(probe.resources()).expect("the host itself is readable");
    let facts = block_on(probe.facts());

    // then
    assert_eq!(
        reading.foreign_unsized,
        ResourceDimension::ALL.into_iter().collect::<BTreeSet<_>>()
    );
    assert_eq!(reading.foreign, Capacity::default());
    assert_eq!(facts, Err(ProbeFailed), "external guests are not known");
}

#[test]
fn should_name_the_unsized_dimension_when_a_foreign_vm_is_not_described() {
    // given: a running foreign VM whose memory the provider does not describe
    let host = ScriptedHost::new();
    let provider = ScriptedProvider::listing(vec![VmObservation {
        memory_bytes: None,
        ..observed("someone-elses", true)
    }]);
    let probe = probe(&host, &provider);

    // when
    let reading = block_on(probe.resources()).expect("a readable host");

    // then
    assert_eq!(
        reading.foreign_unsized,
        BTreeSet::from([ResourceDimension::Memory])
    );
}

#[test]
fn should_report_a_wake_once_when_the_host_slept_past_the_gap() {
    // given
    let host = ScriptedHost::new();
    let provider = ScriptedProvider::listing(vec![observed("someone-elses", true)]);
    let probe = probe(&host, &provider);
    host.pass(Duration::from_secs(5), WAKE_GAP);

    // when
    let first = block_on(probe.facts()).expect("readable");
    host.pass(Duration::from_secs(5), Duration::ZERO);
    let second = block_on(probe.facts()).expect("readable");

    // then
    assert!(first.woke, "a sleep of the gap is a wake");
    assert!(!second.woke, "the wake is reported once");
    assert_eq!(first.external_guests, 1);
    assert_eq!(first.tart_version, "2.0.0");
}

#[test]
fn should_not_report_a_wake_when_the_host_stayed_awake_however_long() {
    // given
    let host = ScriptedHost::new();
    let provider = ScriptedProvider::listing(Vec::new());
    let probe = probe(&host, &provider);
    host.pass(Duration::from_secs(3600), WAKE_GAP - Duration::from_secs(1));

    // when
    let facts = block_on(probe.facts()).expect("readable");

    // then
    assert!(!facts.woke);
}

#[test]
fn should_keep_the_wake_for_the_next_report_when_a_report_fails() {
    // given: the host slept, then the next report cannot read the disk
    let host = ScriptedHost::new();
    let provider = ScriptedProvider::listing(Vec::new());
    let probe = probe(&host, &provider);
    host.pass(Duration::from_secs(1), 2 * WAKE_GAP);
    host.breaks(Broken::Volume, true);
    let failed = block_on(probe.facts());
    host.breaks(Broken::Volume, false);

    // when
    let facts = block_on(probe.facts()).expect("readable again");

    // then
    assert_eq!(failed, Err(ProbeFailed));
    assert!(facts.woke, "a failed report never consumes the wake");
}

#[test]
fn should_fail_facts_rather_than_say_not_woken_when_the_clocks_were_unreadable() {
    // given: the clocks cannot be read when the probe starts
    let host = ScriptedHost::new();
    host.breaks(Broken::Clocks, true);
    let provider = ScriptedProvider::listing(Vec::new());
    let probe = probe(&host, &provider);
    host.breaks(Broken::Clocks, false);

    // when
    let first = block_on(probe.facts());
    let second = block_on(probe.facts());

    // then: no origin, so whether it slept is unknown once; then known again
    assert_eq!(first, Err(ProbeFailed));
    assert!(second.is_ok_and(|facts| !facts.woke));
}
