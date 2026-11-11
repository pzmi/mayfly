//! The fake image store, guest transport, host probe and floor: scripted answers, shared state.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use mayfly_data::{
    Diagnostics, FloorFailed, FloorStore, GuestAnswer, GuestTransport, HostProbe, ImageListing,
    ImageStore, ProbeFailed, VmError,
};
use mayfly_domain::{Epoch, ImageDigest, OwnedVmName};
use mayfly_protocol::{BaseName, GuestOs, HostFacts, Nonce, SecretHandle};

fn lock<T>(state: &Mutex<T>) -> MutexGuard<'_, T> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An image store over a listing; materialised bases run Linux guests.
#[derive(Debug, Clone, Default)]
pub struct FakeImageStore {
    state: Arc<Mutex<(ImageListing, Option<VmError>)>>,
}

impl FakeImageStore {
    /// A store holding `listing`.
    #[must_use]
    pub fn new(listing: ImageListing) -> Self {
        Self {
            state: Arc::new(Mutex::new((listing, None))),
        }
    }

    /// Every later call fails with `error`, or none does (`None`).
    pub fn fail(&self, error: Option<VmError>) {
        lock(&self.state).1 = error;
    }

    /// What it holds now.
    #[must_use]
    pub fn listing(&self) -> ImageListing {
        lock(&self.state).0.clone()
    }

    fn apply(&self, change: impl FnOnce(&mut ImageListing)) -> Result<(), VmError> {
        let mut state = lock(&self.state);
        match state.1 {
            Some(error) => Err(error),
            None => {
                change(&mut state.0);
                Ok(())
            }
        }
    }
}

impl ImageStore for FakeImageStore {
    async fn list(&self) -> Result<ImageListing, VmError> {
        let state = lock(&self.state);
        state.1.map_or_else(|| Ok(state.0.clone()), Err)
    }

    async fn pull(&self, digest: &ImageDigest) -> Result<(), VmError> {
        self.apply(|listing| {
            listing.images.insert(digest.clone());
        })
    }

    async fn materialise(
        &self,
        _digest: &ImageDigest,
        base: &BaseName,
        _runner_version: &str,
    ) -> Result<(), VmError> {
        self.apply(|listing| {
            listing.bases.insert(base.clone(), GuestOs::Linux);
        })
    }

    async fn evict(&self, base: &BaseName) -> Result<(), VmError> {
        self.apply(|listing| {
            listing.bases.remove(base);
        })
    }
}

/// A guest transport answering every call as scripted.
#[derive(Debug, Clone)]
pub struct FakeGuest {
    state: Arc<Mutex<GuestScript>>,
}

/// What [`FakeGuest`] answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestScript {
    /// The answer to every challenge.
    pub verify: Result<GuestAnswer, VmError>,
    /// The answer to every launch.
    pub launch: Result<(), VmError>,
    /// The answer to every export.
    pub export: Result<Diagnostics, VmError>,
}

impl FakeGuest {
    /// A guest answering as `script` says.
    #[must_use]
    pub fn new(script: GuestScript) -> Self {
        Self {
            state: Arc::new(Mutex::new(script)),
        }
    }

    /// Answers as `script` says from now on.
    pub fn script(&self, script: GuestScript) {
        *lock(&self.state) = script;
    }
}

impl GuestTransport for FakeGuest {
    async fn verify(&self, _vm: &OwnedVmName, _nonce: Nonce) -> Result<GuestAnswer, VmError> {
        lock(&self.state).verify
    }

    async fn launch(&self, _vm: &OwnedVmName, _launch: SecretHandle) -> Result<(), VmError> {
        lock(&self.state).launch
    }

    async fn export_diagnostics(&self, _vm: &OwnedVmName) -> Result<Diagnostics, VmError> {
        lock(&self.state).export
    }
}

/// A host probe answering a settable reading, or failing; it can be put to sleep and then
/// reports a wake once the time slept since the last reported wake reaches a gap.
#[derive(Debug, Clone)]
pub struct FakeHostProbe {
    state: Arc<Mutex<ProbeState>>,
}

#[derive(Debug)]
struct ProbeState {
    reading: mayfly_data_domain::HostProbe,
    facts: HostFacts,
    failing: bool,
    facts_failing: bool,
    asleep: Duration,
    wake_gap: Option<Duration>,
}

impl FakeHostProbe {
    /// A probe reading `reading` and answering `facts`.
    #[must_use]
    pub fn new(reading: mayfly_data_domain::HostProbe, facts: HostFacts) -> Self {
        Self {
            state: Arc::new(Mutex::new(ProbeState {
                reading,
                facts,
                failing: false,
                facts_failing: false,
                asleep: Duration::ZERO,
                wake_gap: None,
            })),
        }
    }

    /// Reads `reading` from now on.
    pub fn set(&self, reading: mayfly_data_domain::HostProbe) {
        lock(&self.state).reading = reading;
    }

    /// Answers `facts` from now on.
    pub fn set_facts(&self, facts: HostFacts) {
        lock(&self.state).facts = facts;
    }

    /// Fails every call from now on, or none.
    pub fn fail(&self, failing: bool) {
        lock(&self.state).failing = failing;
    }

    /// Fails `facts` alone from now on, or not, as an incomplete provider listing does.
    pub fn fail_facts(&self, failing: bool) {
        lock(&self.state).facts_failing = failing;
    }

    /// Reports a wake from now on once the time slept since the last reported wake reaches
    /// `gap`; until called, sleeping is never a wake.
    pub fn wake_after(&self, gap: Duration) {
        lock(&self.state).wake_gap = Some(gap);
    }

    /// The host sleeps for `asleep`: added up until a successful `facts` reports the wake.
    pub fn sleep(&self, asleep: Duration) {
        let mut state = lock(&self.state);
        state.asleep = state.asleep.saturating_add(asleep);
    }

    fn answer<T>(&self, read: impl FnOnce(&ProbeState) -> T) -> Result<T, ProbeFailed> {
        let state = lock(&self.state);
        if state.failing {
            Err(ProbeFailed)
        } else {
            Ok(read(&state))
        }
    }
}

impl HostProbe for FakeHostProbe {
    async fn resources(&self) -> Result<mayfly_data_domain::HostProbe, ProbeFailed> {
        self.answer(|state| state.reading.clone())
    }

    /// A failed call leaves the time slept for the next one.
    async fn facts(&self) -> Result<HostFacts, ProbeFailed> {
        let mut state = lock(&self.state);
        if state.failing || state.facts_failing {
            return Err(ProbeFailed);
        }
        let slept = state.wake_gap.is_some_and(|gap| state.asleep >= gap);
        if slept {
            state.asleep = Duration::ZERO;
        }
        Ok(HostFacts {
            woke: state.facts.woke || slept,
            ..state.facts.clone()
        })
    }
}

/// The epoch floor in memory; it can refuse to sync.
#[derive(Debug, Clone)]
pub struct MemoryFloor {
    state: Arc<Mutex<(Epoch, bool)>>,
}

impl MemoryFloor {
    /// A floor at `floor`.
    #[must_use]
    pub fn new(floor: Epoch) -> Self {
        Self {
            state: Arc::new(Mutex::new((floor, false))),
        }
    }

    /// The floor now.
    #[must_use]
    pub fn floor(&self) -> Epoch {
        lock(&self.state).0
    }

    /// Fails every raise from now on, or none.
    pub fn fail(&self, failing: bool) {
        lock(&self.state).1 = failing;
    }
}

impl FloorStore for MemoryFloor {
    async fn raise(&self, epoch: Epoch) -> Result<(), FloorFailed> {
        let mut state = lock(&self.state);
        if state.1 {
            return Err(FloorFailed);
        }
        state.0 = epoch;
        Ok(())
    }
}
