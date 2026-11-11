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
use mayfly_domain::{OwnedVmName, Resources, Sample as _};
use mayfly_testkit_data::host_contract::{
    DISK_LOW_WATER, HostState, HostWorld, MACOS_GUESTS, MEMORY_LOW_WATER, RESERVE, VM_SLOTS,
    WAKE_GAP, check_host_probe_contract,
};

const GIB: u64 = 1 << 30;

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
        vm_slots: VM_SLOTS,
        macos_guests: MACOS_GUESTS,
        reserve: RESERVE,
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
    provider: ScriptedProvider,
    probe: OsProbe<ScriptedProvider, ScriptedHost>,
}

impl HostWorld for World {
    type Probe = OsProbe<ScriptedProvider, ScriptedHost>;

    fn probe(&self) -> &Self::Probe {
        &self.probe
    }

    fn set(&self, state: HostState) {
        self.host.with(|h| {
            h.volume = Volume {
                total_bytes: state.disk_total_bytes,
                free_bytes: state.disk_free_bytes,
            };
            h.memory = Memory {
                total_bytes: state.memory_total_bytes,
                available_bytes: state.memory_available_bytes,
            };
        });
    }

    fn list(&self, inventory: Inventory) {
        self.provider.set(inventory);
    }

    fn sleep(&self, asleep: Duration) {
        self.host.pass(Duration::ZERO, asleep);
    }

    fn fail(&self, failing: bool) {
        for read in [Broken::Volume, Broken::Memory, Broken::Clocks] {
            self.host.breaks(read, failing);
        }
    }

    fn fail_disk(&self, failing: bool) {
        self.host.breaks(Broken::Volume, failing);
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
            provider,
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
    // given: the clocks cannot be read when the probe starts, and the host sleeps before they
    // can be
    let host = ScriptedHost::new();
    host.breaks(Broken::Clocks, true);
    let provider = ScriptedProvider::listing(Vec::new());
    let probe = probe(&host, &provider);
    host.breaks(Broken::Clocks, false);
    host.pass(Duration::from_secs(1), 2 * WAKE_GAP);

    // when
    let first = block_on(probe.facts());
    let second = block_on(probe.facts());
    host.pass(Duration::from_secs(1), WAKE_GAP);
    let third = block_on(probe.facts());

    // then: no origin, so whether it slept is unknown once, and that sleep is never seen (the
    // documented window: nothing measured it); from the first readable report on, a sleep is
    assert_eq!(first, Err(ProbeFailed));
    assert!(second.is_ok_and(|facts| !facts.woke));
    assert!(third.is_ok_and(|facts| facts.woke));
}

#[test]
fn should_report_a_wake_when_sleeps_under_the_gap_add_up_to_it() {
    // given: three sleeps, each a third of the gap, with reports between them
    let host = ScriptedHost::new();
    let provider = ScriptedProvider::listing(Vec::new());
    let probe = probe(&host, &provider);
    let mut woke = Vec::new();

    // when
    for _ in 0..3 {
        host.pass(Duration::from_secs(10), WAKE_GAP / 3);
        woke.push(block_on(probe.facts()).expect("readable").woke);
    }
    host.pass(Duration::from_secs(10), WAKE_GAP / 3);
    woke.push(block_on(probe.facts()).expect("readable").woke);

    // then: the third report sees the gap reached in all; the wake resets the count
    assert_eq!(woke, vec![false, false, true, false]);
}

#[test]
fn should_keep_the_wake_for_the_next_report_when_the_listing_is_incomplete() {
    // given: the host slept, then the provider's listing is incomplete
    let host = ScriptedHost::new();
    let provider = ScriptedProvider::listing(Vec::new());
    let probe = probe(&host, &provider);
    host.pass(Duration::from_secs(1), WAKE_GAP);
    provider.set(Inventory::Incomplete {
        reason: InventoryGap::ListFailed,
    });
    let failed = block_on(probe.facts());
    let reading = block_on(probe.resources());
    provider.set(Inventory::Complete { vms: Vec::new() });

    // when
    let facts = block_on(probe.facts()).expect("complete again");

    // then: the whole report failed, disk included, but the disk is still read for the ledger
    // and the wake is reported by the next report
    assert_eq!(failed, Err(ProbeFailed));
    assert!(reading.is_ok_and(|reading| !reading.disk_low_water));
    assert!(facts.woke);
}

#[test]
fn should_count_every_running_guest_not_ours_when_facts_are_reported() {
    // given: one of ours, a malformed `mf-` VM, a base and a foreign VM, all running, and a
    // stopped foreign VM
    let host = ScriptedHost::new();
    let provider = ScriptedProvider::listing(vec![
        observed(&OwnedVmName::sample().to_string(), true),
        observed("mf-not-ours-shape", true),
        observed("mfb-macos-15", true),
        observed("someone-elses", true),
        observed("stopped", false),
    ]);
    let probe = probe(&host, &provider);

    // when
    let facts = block_on(probe.facts()).expect("readable");

    // then: the hypervisor counts every running guest but ours
    assert_eq!(facts.external_guests, 3);
}
