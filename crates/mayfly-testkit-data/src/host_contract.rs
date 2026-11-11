//! The `HostProbe` contract suite (ver §5, DP-06): what every host probe, fake or real, must do
//! for the resource ledger to be fed the host as it is (ADR 0024) and nothing it does not know
//! (ADR 0052).
//!
//! A world is a probe plus handles that set what the host and the provider it reads look like,
//! put the host to sleep, or make the host unreadable. Each case runs in a fresh world on its own
//! current-thread runtime, and first sets the host and an empty, complete listing. The world's
//! probe is configured as this module's constants say ([`DISK_LOW_WATER`], [`MEMORY_LOW_WATER`],
//! [`WAKE_GAP`], [`VM_SLOTS`], [`MACOS_GUESTS`], [`RESERVE`]); every expectation is written out
//! here, never derived from what the probe or the fake computes. The suite's own tests
//! (`tests/host_contract.rs`) prove that a probe with each of several bugs fails exactly the
//! cases that exist to catch it.

use std::fmt::{self, Display};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use mayfly_data::{
    Capacity, GuestOs, HostFacts, HostProbe, HostReading, Inventory, InventoryGap, ProbeFailed,
    ResourceDimension, VmObservation, foreign_use,
};
use mayfly_domain::{OwnedVmName, Resources, Sample as _};

/// The free-disk low-water mark every world's probe is configured with.
pub const DISK_LOW_WATER: u64 = 20 * GIB;
/// The available-memory mark every world's probe is configured with.
pub const MEMORY_LOW_WATER: u64 = 4 * GIB;
/// The time asleep every world's probe takes for a wake.
pub const WAKE_GAP: Duration = Duration::from_secs(60);
/// The VM slots every world's probe is configured with.
pub const VM_SLOTS: u32 = 4;
/// The macOS-guest slots every world's probe is configured with.
pub const MACOS_GUESTS: u32 = 2;
/// The operator reserve every world's probe is configured with.
pub const RESERVE: Capacity = Capacity {
    vm_slots: 0,
    macos_guests: 0,
    resources: Resources::new(8 * GIB, 50 * GIB),
};

const GIB: u64 = 1 << 30;

/// What the host looks like to its probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostState {
    /// The size of the volume of the provider's home.
    pub disk_total_bytes: u64,
    /// Free disk on that volume.
    pub disk_free_bytes: u64,
    /// Physical memory.
    pub memory_total_bytes: u64,
    /// Memory the host can still hand out.
    pub memory_available_bytes: u64,
}

/// A host with room: 1000 GiB of disk with 500 free, 64 GiB of memory with 48 available.
pub const ROOMY: HostState = HostState {
    disk_total_bytes: 1000 * GIB,
    disk_free_bytes: 500 * GIB,
    memory_total_bytes: 64 * GIB,
    memory_available_bytes: 48 * GIB,
};

/// A probe and the host and provider it reads.
pub trait HostWorld {
    /// The probe under test.
    type Probe: HostProbe;

    /// The probe.
    fn probe(&self) -> &Self::Probe;

    /// Makes the host look like `state` from now on.
    fn set(&self, state: HostState);

    /// Makes the provider list `inventory` from now on.
    fn list(&self, inventory: Inventory);

    /// Puts the host to sleep for `asleep`: the clock that counts sleep moves by it, the one
    /// that does not stays.
    fn sleep(&self, asleep: Duration);

    /// Makes every read of the host fail from now on, or none.
    fn fail(&self, failing: bool);

    /// Makes only the free-disk read fail from now on, or not: the clocks stay readable.
    fn fail_disk(&self, failing: bool);
}

/// One case of the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HostCase {
    /// Each call reads the host as it is now: free disk, the low-water mark and memory
    /// pressure follow a change at once (a cached reading fails it).
    ReadsTheHostEachCall,
    /// An unreadable host is `ProbeFailed` from both calls, never a default or an old reading.
    UnreadableIsFailed,
    /// The totals are the configured slots and the host's physical memory and volume size, and
    /// the reserve is the configured one.
    ReadsTotals,
    /// Foreign use counts, by what the listing describes, only the VMs whose names are neither
    /// `mf-*` nor a base's; a fact it lacks names its dimension; every running VM not of our
    /// well-formed shape is an external guest.
    CountsForeignUse,
    /// An incomplete listing leaves every foreign dimension unsized (never zero) and fails the
    /// facts, whose external guests are then not known.
    IncompleteListingIsUnknown,
    /// A sleep of the gap is reported as a wake once; no sleep is no wake.
    ReportsAWakeOnce,
    /// Sleeps each under the gap add up until together they reach it (P22).
    SleepsAccumulate,
    /// A report that fails never consumes a wake: the next one that succeeds says it.
    WakeSurvivesFailedReport,
}

impl HostCase {
    /// Every case.
    pub const ALL: [Self; 8] = [
        Self::ReadsTheHostEachCall,
        Self::UnreadableIsFailed,
        Self::ReadsTotals,
        Self::CountsForeignUse,
        Self::IncompleteListingIsUnknown,
        Self::ReportsAWakeOnce,
        Self::SleepsAccumulate,
        Self::WakeSurvivesFailedReport,
    ];
}

impl Display for HostCase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// A case's outcome.
pub type Check = Result<(), String>;

/// The cases that failed, each with why.
#[derive(Debug, Default)]
pub struct HostContractReport {
    /// Each failed case and what went wrong.
    pub failures: Vec<(HostCase, String)>,
}

impl HostContractReport {
    /// Whether every case passed.
    #[must_use]
    pub const fn is_green(&self) -> bool {
        self.failures.is_empty()
    }

    /// The cases that failed.
    #[must_use]
    pub fn failed_cases(&self) -> Vec<HostCase> {
        self.failures.iter().map(|(case, _)| *case).collect()
    }
}

impl Display for HostContractReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "HostProbe contract: {} case(s) failed",
            self.failures.len()
        )?;
        for (case, detail) in &self.failures {
            writeln!(f, "- {case}: {detail}")?;
        }
        Ok(())
    }
}

/// Runs every case, each in a fresh world from `make`.
pub fn check_host_probe_contract<W: HostWorld>(make: &dyn Fn() -> W) -> HostContractReport {
    let mut report = HostContractReport::default();
    for case in HostCase::ALL {
        if let Err(detail) = check_case(make, case) {
            report.failures.push((case, detail));
        }
    }
    report
}

/// Runs every case and panics with the report unless all pass: the form an implementation's
/// own test calls.
///
/// # Panics
/// When a case fails; the message lists each failed case and why.
pub fn run_host_probe_contract<W: HostWorld>(make: &dyn Fn() -> W) {
    let report = check_host_probe_contract(make);
    assert!(report.is_green(), "{report}");
}

/// Runs one case in a fresh world on a fresh current-thread runtime.
///
/// # Errors
/// What was expected and what the probe answered, at the first check that failed.
pub fn check_case<W: HostWorld>(make: &dyn Fn() -> W, case: HostCase) -> Check {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(|e| format!("the case's runtime did not build: {e}"))?;
    let world = make();
    world.set(ROOMY);
    world.list(Inventory::Complete { vms: Vec::new() });
    runtime.block_on(async {
        match case {
            HostCase::ReadsTheHostEachCall => reads_the_host_each_call(&world).await,
            HostCase::UnreadableIsFailed => unreadable_is_failed(&world).await,
            HostCase::ReadsTotals => reads_totals(&world).await,
            HostCase::CountsForeignUse => counts_foreign_use(&world).await,
            HostCase::IncompleteListingIsUnknown => incomplete_listing_is_unknown(&world).await,
            HostCase::ReportsAWakeOnce => reports_a_wake_once(&world).await,
            HostCase::SleepsAccumulate => sleeps_accumulate(&world).await,
            HostCase::WakeSurvivesFailedReport => wake_survives_failed_report(&world).await,
        }
    })
}

async fn resources<W: HostWorld>(world: &W) -> Result<HostReading, String> {
    world
        .probe()
        .resources()
        .await
        .map_err(|ProbeFailed| "resources failed on a readable host".to_owned())
}

async fn facts<W: HostWorld>(world: &W) -> Result<HostFacts, String> {
    world
        .probe()
        .facts()
        .await
        .map_err(|ProbeFailed| "facts failed on a readable host with a complete listing".to_owned())
}

async fn read<W: HostWorld>(world: &W) -> Result<(HostReading, HostFacts), String> {
    Ok((resources(world).await?, facts(world).await?))
}

fn same<T: PartialEq + fmt::Debug>(what: &str, wanted: T, read: T) -> Check {
    if wanted == read {
        Ok(())
    } else {
        Err(format!("{what}: wanted {wanted:?}, read {read:?}"))
    }
}

fn expect(state: HostState, reading: &HostReading, facts: &HostFacts) -> Check {
    let low = state.disk_free_bytes < DISK_LOW_WATER;
    let pressure = state.memory_available_bytes < MEMORY_LOW_WATER;
    same(
        &format!("for {state:?}: (free, facts low water, reading low water, pressure)"),
        (state.disk_free_bytes, low, low, pressure),
        (
            facts.disk_free_bytes,
            facts.disk_low_water,
            reading.disk_low_water,
            reading.memory_pressure,
        ),
    )
}

async fn reads_the_host_each_call<W: HostWorld>(world: &W) -> Check {
    let short = HostState {
        disk_free_bytes: 5 * GIB,
        memory_available_bytes: GIB,
        ..ROOMY
    };
    for state in [ROOMY, short, ROOMY] {
        world.set(state);
        let (reading, facts) = read(world).await?;
        expect(state, &reading, &facts)?;
    }
    Ok(())
}

async fn unreadable_is_failed<W: HostWorld>(world: &W) -> Check {
    read(world).await?;
    world.fail(true);
    let resources = world.probe().resources().await;
    let facts = world.probe().facts().await;
    if resources.is_ok() || facts.is_ok() {
        return Err(format!(
            "an unreadable host answered resources {resources:?} and facts {facts:?}"
        ));
    }
    world.fail(false);
    read(world).await.map(|_| ())
}

async fn reads_totals<W: HostWorld>(world: &W) -> Check {
    let small = HostState {
        disk_total_bytes: 250 * GIB,
        disk_free_bytes: 100 * GIB,
        memory_total_bytes: 16 * GIB,
        memory_available_bytes: 8 * GIB,
    };
    for state in [ROOMY, small] {
        world.set(state);
        let reading = resources(world).await?;
        same(
            &format!("for {state:?}: (totals, reserve)"),
            (
                Capacity {
                    vm_slots: VM_SLOTS,
                    macos_guests: MACOS_GUESTS,
                    resources: Resources::new(state.memory_total_bytes, state.disk_total_bytes),
                },
                RESERVE,
            ),
            (reading.totals, reading.reserve),
        )?;
    }
    Ok(())
}

fn vm(name: &str, running: bool, guest: GuestOs, memory_gib: u64, disk_gib: u64) -> VmObservation {
    VmObservation {
        name: name.to_owned(),
        instance_id: None,
        running,
        guest: Some(guest),
        memory_bytes: Some(memory_gib * GIB),
        disk_bytes: Some(disk_gib * GIB),
    }
}

async fn counts_foreign_use<W: HostWorld>(world: &W) -> Check {
    // one of ours, a malformed `mf-` VM and a base, all running; a running foreign macOS VM
    // and a stopped foreign Linux VM, both described
    let ours = OwnedVmName::sample().to_string();
    world.list(Inventory::Complete {
        vms: vec![
            vm(&ours, true, GuestOs::Macos, 8, 80),
            vm("mf-not-a-name", true, GuestOs::Macos, 8, 80),
            vm("mfb-macos-15", true, GuestOs::Macos, 8, 60),
            vm("someone-elses", true, GuestOs::Macos, 4, 60),
            vm("old-linux", false, GuestOs::Linux, 2, 20),
        ],
    });
    let (reading, facts) = read(world).await?;
    same(
        "a mixed listing's (foreign, unsized, external guests)",
        (
            Capacity {
                vm_slots: 2,
                macos_guests: 1,
                resources: Resources::new(4 * GIB, 80 * GIB),
            },
            Vec::new(),
            3,
        ),
        (
            reading.foreign,
            reading.foreign_unsized.into_iter().collect(),
            facts.external_guests,
        ),
    )?;

    // a running foreign VM the provider does not describe
    world.list(Inventory::Complete {
        vms: vec![VmObservation {
            name: "undescribed".to_owned(),
            instance_id: None,
            running: true,
            guest: None,
            memory_bytes: None,
            disk_bytes: None,
        }],
    });
    let reading = resources(world).await?;
    same(
        "an undescribed running foreign VM's (foreign, unsized)",
        (
            Capacity {
                vm_slots: 1,
                macos_guests: 0,
                resources: Resources::new(0, 0),
            },
            vec![
                ResourceDimension::MacosGuests,
                ResourceDimension::Memory,
                ResourceDimension::Disk,
            ],
        ),
        (
            reading.foreign,
            reading.foreign_unsized.into_iter().collect(),
        ),
    )
}

async fn incomplete_listing_is_unknown<W: HostWorld>(world: &W) -> Check {
    world.list(Inventory::Complete {
        vms: vec![vm("someone-elses", true, GuestOs::Macos, 4, 60)],
    });
    read(world).await?;
    world.list(Inventory::Incomplete {
        reason: InventoryGap::ListFailed,
    });
    let reading = resources(world).await?;
    same(
        "an incomplete listing's (foreign, unsized)",
        (Capacity::default(), ResourceDimension::ALL.to_vec()),
        (
            reading.foreign,
            reading.foreign_unsized.into_iter().collect(),
        ),
    )?;
    let facts = world.probe().facts().await;
    if facts.is_ok() {
        return Err(format!(
            "an incomplete listing answered facts {facts:?}: its external guests are not known"
        ));
    }
    world.list(Inventory::Complete { vms: Vec::new() });
    read(world).await.map(|_| ())
}

async fn woke<W: HostWorld>(world: &W) -> Result<bool, String> {
    facts(world).await.map(|facts| facts.woke)
}

async fn reports_a_wake_once<W: HostWorld>(world: &W) -> Check {
    same("woke with no sleep", false, woke(world).await?)?;
    world.sleep(WAKE_GAP);
    same("woke after a sleep of the gap", true, woke(world).await?)?;
    same("woke again with no sleep since", false, woke(world).await?)
}

async fn sleeps_accumulate<W: HostWorld>(world: &W) -> Check {
    world.sleep(WAKE_GAP / 2);
    same("woke after half the gap", false, woke(world).await?)?;
    world.sleep(WAKE_GAP / 2);
    same("woke after two halves of the gap", true, woke(world).await?)?;
    world.sleep(WAKE_GAP / 2);
    same(
        "woke after half the gap since the reported wake",
        false,
        woke(world).await?,
    )
}

async fn wake_survives_failed_report<W: HostWorld>(world: &W) -> Check {
    same("woke with no sleep", false, woke(world).await?)?;
    world.sleep(WAKE_GAP);
    world.fail_disk(true);
    let failed = world.probe().facts().await;
    world.fail_disk(false);
    if failed.is_ok() {
        return Err(format!(
            "facts answered {failed:?} while free disk was unreadable"
        ));
    }
    same(
        "woke after a sleep of the gap and a failed report",
        true,
        woke(world).await?,
    )?;
    same("woke again with no sleep since", false, woke(world).await?)
}

/// The fake probe's world: it keeps the host and the listing, and sets the fake's answers from
/// them as a real probe configured with this module's constants would read them.
#[derive(Debug)]
pub struct FakeHostWorld {
    probe: crate::FakeHostProbe,
    seen: Mutex<(HostState, Inventory)>,
}

impl FakeHostWorld {
    /// A fake probe over a [`ROOMY`] host whose provider lists nothing.
    #[must_use]
    pub fn new() -> Self {
        let inventory = Inventory::Complete { vms: Vec::new() };
        let (reading, facts) = answers(ROOMY, &inventory);
        let probe = crate::FakeHostProbe::new(reading, facts);
        probe.wake_after(WAKE_GAP);
        Self {
            probe,
            seen: Mutex::new((ROOMY, inventory)),
        }
    }

    fn change(&self, change: impl FnOnce(&mut (HostState, Inventory))) {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        change(&mut seen);
        let (state, inventory) = &*seen;
        let (reading, facts) = answers(*state, inventory);
        self.probe.set(reading);
        self.probe.set_facts(facts);
        self.probe
            .fail_facts(matches!(inventory, Inventory::Incomplete { .. }));
    }
}

impl Default for FakeHostWorld {
    fn default() -> Self {
        Self::new()
    }
}

/// What a probe configured with this module's constants reads of `state` and `inventory`.
fn answers(state: HostState, inventory: &Inventory) -> (HostReading, HostFacts) {
    let foreign = match inventory {
        Inventory::Complete { vms } => foreign_use(vms),
        Inventory::Incomplete { .. } => mayfly_data::ForeignUse {
            used: Capacity::default(),
            unsized_in: ResourceDimension::ALL.into_iter().collect(),
            running: 0,
        },
    };
    let low = state.disk_free_bytes < DISK_LOW_WATER;
    let reading = HostReading {
        totals: Capacity {
            vm_slots: VM_SLOTS,
            macos_guests: MACOS_GUESTS,
            resources: Resources::new(state.memory_total_bytes, state.disk_total_bytes),
        },
        reserve: RESERVE,
        foreign: foreign.used,
        foreign_unsized: foreign.unsized_in,
        disk_low_water: low,
        memory_pressure: state.memory_available_bytes < MEMORY_LOW_WATER,
        hypervisor_limit: false,
    };
    let facts = HostFacts {
        disk_free_bytes: state.disk_free_bytes,
        disk_low_water: low,
        woke: false,
        tart_version: "2.0.0".to_owned(),
        external_guests: foreign.running,
    };
    (reading, facts)
}

impl HostWorld for FakeHostWorld {
    type Probe = crate::FakeHostProbe;

    fn probe(&self) -> &Self::Probe {
        &self.probe
    }

    fn set(&self, state: HostState) {
        self.change(|seen| seen.0 = state);
    }

    fn list(&self, inventory: Inventory) {
        self.change(|seen| seen.1 = inventory);
    }

    fn sleep(&self, asleep: Duration) {
        self.probe.sleep(asleep);
    }

    fn fail(&self, failing: bool) {
        self.probe.fail(failing);
    }

    /// The fake has one read: a failing disk fails it whole, which keeps the wake as well.
    fn fail_disk(&self, failing: bool) {
        self.probe.fail(failing);
    }
}
