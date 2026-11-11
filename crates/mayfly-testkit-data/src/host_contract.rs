//! The `HostProbe` contract suite (ver §5, DP-06): what every host probe, fake or real, must do
//! for the resource ledger to be fed the host as it is (ADR 0024) and nothing it does not know
//! (ADR 0052).
//!
//! A world is a probe plus a handle that sets what the host it reads looks like, or makes the
//! host unreadable. Each case runs in a fresh world on its own current-thread runtime. The
//! world's probe marks low water and memory pressure at [`DISK_LOW_WATER`] and
//! [`MEMORY_LOW_WATER`]. The suite's own tests (`tests/host_contract.rs`) prove that
//! `CachedProbe`, which answers its first reading for ever, fails it.

use std::fmt::{self, Display};

use mayfly_data::{HostFacts, HostProbe, HostReading, ProbeFailed};

/// The free-disk low-water mark every world's probe is configured with.
pub const DISK_LOW_WATER: u64 = 20 * GIB;
/// The available-memory mark every world's probe is configured with.
pub const MEMORY_LOW_WATER: u64 = 4 * GIB;

const GIB: u64 = 1 << 30;

/// What the host looks like to its probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostState {
    /// Free disk on the volume of the provider's home.
    pub disk_free_bytes: u64,
    /// Memory the host can still hand out.
    pub memory_available_bytes: u64,
}

/// A probe and the host it reads.
pub trait HostWorld {
    /// The probe under test.
    type Probe: HostProbe;

    /// The probe.
    fn probe(&self) -> &Self::Probe;

    /// Makes the host look like `state` from now on.
    fn set(&self, state: HostState);

    /// Makes every read of the host fail from now on, or none.
    fn fail(&self, failing: bool);
}

/// One case of the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HostCase {
    /// Each call reads the host as it is now: free disk, the low-water mark and memory
    /// pressure follow a change at once (a cached reading fails it).
    ReadsTheHostEachCall,
    /// An unreadable host is `ProbeFailed` from both calls, never a default or an old reading.
    UnreadableIsFailed,
}

impl HostCase {
    /// Every case.
    pub const ALL: [Self; 2] = [Self::ReadsTheHostEachCall, Self::UnreadableIsFailed];
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
    runtime.block_on(async {
        match case {
            HostCase::ReadsTheHostEachCall => reads_the_host_each_call(&world).await,
            HostCase::UnreadableIsFailed => unreadable_is_failed(&world).await,
        }
    })
}

async fn read<W: HostWorld>(world: &W) -> Result<(HostReading, HostFacts), String> {
    let reading = world
        .probe()
        .resources()
        .await
        .map_err(|ProbeFailed| "resources failed on a readable host".to_owned())?;
    let facts = world
        .probe()
        .facts()
        .await
        .map_err(|ProbeFailed| "facts failed on a readable host".to_owned())?;
    Ok((reading, facts))
}

fn expect(state: HostState, reading: &HostReading, facts: &HostFacts) -> Check {
    let low = state.disk_free_bytes < DISK_LOW_WATER;
    let pressure = state.memory_available_bytes < MEMORY_LOW_WATER;
    let seen = (
        facts.disk_free_bytes,
        facts.disk_low_water,
        reading.disk_low_water,
        reading.memory_pressure,
    );
    let wanted = (state.disk_free_bytes, low, low, pressure);
    if seen == wanted {
        Ok(())
    } else {
        Err(format!(
            "for {state:?}: wanted (free, facts low water, reading low water, pressure) \
             {wanted:?}, read {seen:?}"
        ))
    }
}

async fn reads_the_host_each_call<W: HostWorld>(world: &W) -> Check {
    let roomy = HostState {
        disk_free_bytes: 500 * GIB,
        memory_available_bytes: 48 * GIB,
    };
    let short = HostState {
        disk_free_bytes: 5 * GIB,
        memory_available_bytes: GIB,
    };
    for state in [roomy, short, roomy] {
        world.set(state);
        let (reading, facts) = read(world).await?;
        expect(state, &reading, &facts)?;
    }
    Ok(())
}

async fn unreadable_is_failed<W: HostWorld>(world: &W) -> Check {
    world.set(HostState {
        disk_free_bytes: 500 * GIB,
        memory_available_bytes: 48 * GIB,
    });
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

/// The fake probe's world: the fake applies the marks as a real probe would.
#[derive(Debug)]
pub struct FakeHostWorld {
    probe: crate::FakeHostProbe,
    reading: HostReading,
    facts: HostFacts,
}

impl FakeHostWorld {
    /// A fake probe answering `reading` and `facts` with the free disk and the two flags the
    /// world's state sets.
    #[must_use]
    pub fn new(reading: HostReading, facts: HostFacts) -> Self {
        Self {
            probe: crate::FakeHostProbe::new(reading.clone(), facts.clone()),
            reading,
            facts,
        }
    }
}

impl HostWorld for FakeHostWorld {
    type Probe = crate::FakeHostProbe;

    fn probe(&self) -> &Self::Probe {
        &self.probe
    }

    fn set(&self, state: HostState) {
        let low = state.disk_free_bytes < DISK_LOW_WATER;
        self.probe.set(HostReading {
            disk_low_water: low,
            memory_pressure: state.memory_available_bytes < MEMORY_LOW_WATER,
            ..self.reading.clone()
        });
        self.probe.set_facts(HostFacts {
            disk_free_bytes: state.disk_free_bytes,
            disk_low_water: low,
            ..self.facts.clone()
        });
    }

    fn fail(&self, failing: bool) {
        self.probe.fail(failing);
    }
}
