//! The `HostProbe` contract's own tests (ver §5): the fake passes every case, and
//! `CachedProbe`, which answers its first reading for ever (ver §5's broken probe), fails the
//! case that exists to catch it.

use std::collections::BTreeSet;
use std::sync::{Mutex, PoisonError};

use mayfly_data::{Capacity, HostFacts, HostProbe, HostReading, ProbeFailed};
use mayfly_domain::Resources;
use mayfly_testkit_data::FakeHostProbe;
use mayfly_testkit_data::host_contract::{
    FakeHostWorld, HostCase, HostState, HostWorld, check_host_probe_contract,
};

const GIB: u64 = 1 << 30;

fn reading() -> HostReading {
    HostReading {
        totals: Capacity {
            vm_slots: 4,
            macos_guests: 2,
            resources: Resources::new(64 * GIB, 1000 * GIB),
        },
        reserve: Capacity::default(),
        foreign: Capacity::default(),
        foreign_unsized: BTreeSet::new(),
        disk_low_water: false,
        memory_pressure: false,
        hypervisor_limit: false,
    }
}

fn facts() -> HostFacts {
    HostFacts {
        disk_free_bytes: 500 * GIB,
        disk_low_water: false,
        woke: false,
        tart_version: "2.0.0".to_owned(),
        external_guests: 0,
    }
}

/// Answers the first reading it took, for ever.
struct CachedProbe {
    inner: FakeHostProbe,
    first: Mutex<Option<(HostReading, HostFacts)>>,
}

impl CachedProbe {
    async fn cached(&self) -> Result<(HostReading, HostFacts), ProbeFailed> {
        if let Some(first) = self
            .first
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            return Ok(first);
        }
        let taken = (self.inner.resources().await?, self.inner.facts().await?);
        *self.first.lock().unwrap_or_else(PoisonError::into_inner) = Some(taken.clone());
        Ok(taken)
    }
}

impl HostProbe for CachedProbe {
    async fn resources(&self) -> Result<HostReading, ProbeFailed> {
        self.cached().await.map(|(reading, _)| reading)
    }

    async fn facts(&self) -> Result<HostFacts, ProbeFailed> {
        self.cached().await.map(|(_, facts)| facts)
    }
}

struct CachedWorld {
    fake: FakeHostWorld,
    probe: CachedProbe,
}

impl HostWorld for CachedWorld {
    type Probe = CachedProbe;

    fn probe(&self) -> &Self::Probe {
        &self.probe
    }

    fn set(&self, state: HostState) {
        self.fake.set(state);
    }

    fn fail(&self, failing: bool) {
        self.fake.fail(failing);
    }
}

#[test]
fn should_pass_every_case_when_the_fake_probe_runs_the_contract() {
    // given
    let make = || FakeHostWorld::new(reading(), facts());

    // when
    let report = check_host_probe_contract(&make);

    // then
    assert!(report.is_green(), "{report}");
}

#[test]
fn should_fail_reads_the_host_each_call_when_the_probe_caches_its_first_reading() {
    // given
    let make = || {
        let fake = FakeHostWorld::new(reading(), facts());
        let inner = fake.probe().clone();
        CachedWorld {
            fake,
            probe: CachedProbe {
                inner,
                first: Mutex::new(None),
            },
        }
    };

    // when
    let report = check_host_probe_contract(&make);

    // then
    assert!(
        report
            .failed_cases()
            .contains(&HostCase::ReadsTheHostEachCall),
        "{report}"
    );
}
