//! The fake image store, guest transport, host probe and floor: scripted answers, shared state.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

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

/// A host probe answering a settable reading, or failing.
#[derive(Debug, Clone)]
pub struct FakeHostProbe {
    state: Arc<Mutex<ProbeState>>,
}

#[derive(Debug)]
struct ProbeState {
    reading: mayfly_data_domain::HostProbe,
    facts: HostFacts,
    failing: bool,
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
            })),
        }
    }

    /// Reads `reading` from now on.
    pub fn set(&self, reading: mayfly_data_domain::HostProbe) {
        lock(&self.state).reading = reading;
    }

    /// Fails every call from now on, or none.
    pub fn fail(&self, failing: bool) {
        lock(&self.state).failing = failing;
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

    async fn facts(&self) -> Result<HostFacts, ProbeFailed> {
        self.answer(|state| state.facts.clone())
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
