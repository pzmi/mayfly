//! The `HostProbe` contract's own tests (ver §5): the fake passes every case, and a probe with
//! each of several deliberate bugs fails exactly the cases that exist to catch that bug.

use std::collections::BTreeSet;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use mayfly_data::{Capacity, HostFacts, HostProbe, HostReading, Inventory, ProbeFailed};
use mayfly_domain::Resources;
use mayfly_testkit_data::FakeHostProbe;
use mayfly_testkit_data::host_contract::{
    FakeHostWorld, HostCase, HostState, HostWorld, WAKE_GAP, check_host_probe_contract,
};

/// What a broken probe gets wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bug {
    /// Keeps the free disk and the two flags of its first answers.
    CachesTheHost,
    /// Answers its last good answers when the host cannot be read.
    HidesFailures,
    /// Answers totals from a guess, not from the host.
    GuessesTotals,
    /// Counts no foreign VM and never says one is unsized.
    IgnoresForeignVms,
    /// Measures sleep per report from the last report, failed ones included: sleeps under the
    /// gap never add up, and a failed report consumes the wake.
    WakePerReport,
}

/// The fake probe with one bug.
struct BrokenProbe {
    inner: FakeHostProbe,
    bug: Bug,
    first: Mutex<Option<(HostReading, HostFacts)>>,
    last: Mutex<(Option<HostReading>, Option<HostFacts>)>,
    asleep: Mutex<Duration>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl BrokenProbe {
    /// The first answers of each kind, taken once both are known.
    fn first(&self) -> Option<(HostReading, HostFacts)> {
        let mut first = lock(&self.first);
        if first.is_none()
            && let (Some(reading), Some(facts)) = lock(&self.last).clone()
        {
            *first = Some((reading, facts));
        }
        first.clone()
    }
}

impl HostProbe for BrokenProbe {
    async fn resources(&self) -> Result<HostReading, ProbeFailed> {
        let mut reading = match self.inner.resources().await {
            Ok(reading) => reading,
            Err(ProbeFailed) if self.bug == Bug::HidesFailures => {
                return lock(&self.last).0.clone().ok_or(ProbeFailed);
            }
            Err(ProbeFailed) => return Err(ProbeFailed),
        };
        lock(&self.last).0 = Some(reading.clone());
        match self.bug {
            Bug::CachesTheHost => {
                if let Some((first, _)) = self.first() {
                    reading.disk_low_water = first.disk_low_water;
                    reading.memory_pressure = first.memory_pressure;
                }
            }
            Bug::GuessesTotals => {
                reading.totals.resources = Resources::new(64 << 30, 1000 << 30);
            }
            Bug::IgnoresForeignVms => {
                reading.foreign = Capacity::default();
                reading.foreign_unsized = BTreeSet::new();
            }
            Bug::HidesFailures | Bug::WakePerReport => {}
        }
        Ok(reading)
    }

    async fn facts(&self) -> Result<HostFacts, ProbeFailed> {
        let slept = std::mem::take(&mut *lock(&self.asleep));
        let mut facts = match self.inner.facts().await {
            Ok(facts) => facts,
            Err(ProbeFailed) if self.bug == Bug::HidesFailures => {
                return lock(&self.last).1.clone().ok_or(ProbeFailed);
            }
            Err(ProbeFailed) => return Err(ProbeFailed),
        };
        lock(&self.last).1 = Some(facts.clone());
        match self.bug {
            Bug::CachesTheHost => {
                if let Some((_, first)) = self.first() {
                    facts.disk_free_bytes = first.disk_free_bytes;
                    facts.disk_low_water = first.disk_low_water;
                }
            }
            Bug::IgnoresForeignVms => facts.external_guests = 0,
            Bug::WakePerReport => facts.woke = slept >= WAKE_GAP,
            Bug::HidesFailures | Bug::GuessesTotals => {}
        }
        Ok(facts)
    }
}

/// The fake's world with a broken probe over the fake.
struct BrokenWorld {
    fake: FakeHostWorld,
    probe: BrokenProbe,
}

impl BrokenWorld {
    fn new(bug: Bug) -> Self {
        let fake = FakeHostWorld::new();
        let inner = fake.probe().clone();
        Self {
            fake,
            probe: BrokenProbe {
                inner,
                bug,
                first: Mutex::new(None),
                last: Mutex::new((None, None)),
                asleep: Mutex::new(Duration::ZERO),
            },
        }
    }
}

impl HostWorld for BrokenWorld {
    type Probe = BrokenProbe;

    fn probe(&self) -> &Self::Probe {
        &self.probe
    }

    fn set(&self, state: HostState) {
        self.fake.set(state);
    }

    fn list(&self, inventory: Inventory) {
        self.fake.list(inventory);
    }

    fn sleep(&self, asleep: Duration) {
        self.fake.sleep(asleep);
        let mut slept = lock(&self.probe.asleep);
        *slept = slept.saturating_add(asleep);
    }

    fn fail(&self, failing: bool) {
        self.fake.fail(failing);
    }

    fn fail_disk(&self, failing: bool) {
        self.fake.fail_disk(failing);
    }
}

#[test]
fn should_pass_every_case_when_the_fake_probe_runs_the_contract() {
    // given
    let make = FakeHostWorld::new;

    // when
    let report = check_host_probe_contract(&make);

    // then
    assert!(report.is_green(), "{report}");
}

#[test]
fn should_fail_exactly_the_cases_that_catch_each_bug_when_a_broken_probe_runs_the_contract() {
    let expected = [
        (Bug::CachesTheHost, vec![HostCase::ReadsTheHostEachCall]),
        (
            Bug::HidesFailures,
            vec![
                HostCase::UnreadableIsFailed,
                HostCase::IncompleteListingIsUnknown,
                HostCase::WakeSurvivesFailedReport,
            ],
        ),
        (Bug::GuessesTotals, vec![HostCase::ReadsTotals]),
        (
            Bug::IgnoresForeignVms,
            vec![
                HostCase::CountsForeignUse,
                HostCase::IncompleteListingIsUnknown,
            ],
        ),
        (
            Bug::WakePerReport,
            vec![
                HostCase::SleepsAccumulate,
                HostCase::WakeSurvivesFailedReport,
            ],
        ),
    ];
    for (bug, cases) in expected {
        // given
        let make = || BrokenWorld::new(bug);

        // when
        let report = check_host_probe_contract(&make);

        // then
        assert_eq!(report.failed_cases(), cases, "{bug:?}: {report}");
    }
}
