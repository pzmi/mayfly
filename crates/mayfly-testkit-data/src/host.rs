//! The fake image store, guest transport, host probe and floor: scripted answers, shared state.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use mayfly_data::{
    Diagnostics, FloorFailed, FloorStore, GuestAnswer, GuestTransport, HostProbe, ImageListing,
    ImageStore, LaunchAnswer, ProbeFailed, RunnerStatus, VmError,
};
use mayfly_domain::{Epoch, ImageDigest, OwnedVmName, ProviderInstanceId};
use mayfly_protocol::{BaseName, GuestOs, HostFacts, Nonce, SecretHandle};

fn lock<T>(state: &Mutex<T>) -> MutexGuard<'_, T> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An image store over a listing; materialised bases run Linux guests. A digest given a size
/// ([`FakeImageStore::manifest`]) measures to it and is listed with it once pulled, its bases
/// too; any other digest does not measure.
#[derive(Debug, Clone, Default)]
pub struct FakeImageStore {
    state: Arc<Mutex<(ImageListing, Option<VmError>)>>,
    manifests: Arc<Mutex<BTreeMap<ImageDigest, u64>>>,
    measured: Arc<Mutex<Vec<ImageDigest>>>,
    listing_fails: Arc<Mutex<bool>>,
}

impl FakeImageStore {
    /// Every later listing fails (an unreadable registry), every other call as before; or none
    /// does (`false`).
    pub fn fail_listing(&self, fails: bool) {
        *lock(&self.listing_fails) = fails;
    }

    /// A store holding `listing`.
    #[must_use]
    pub fn new(listing: ImageListing) -> Self {
        Self {
            state: Arc::new(Mutex::new((listing, None))),
            ..Self::default()
        }
    }

    /// The registry's manifest of `digest` states a disk of `disk_bytes`.
    pub fn manifest(&self, digest: &ImageDigest, disk_bytes: u64) {
        lock(&self.manifests).insert(digest.clone(), disk_bytes);
    }

    /// The listing names `digest` with no disk size (its kept manifest no longer reads), and so
    /// does each base made from it.
    pub fn forget_size(&self, digest: &ImageDigest) {
        let mut state = lock(&self.state);
        let listing = &mut state.0;
        listing.disks.images.remove(digest);
        let bases: Vec<BaseName> = listing
            .disks
            .base_images
            .iter()
            .filter(|(_, image)| *image == digest)
            .map(|(base, _)| base.clone())
            .collect();
        for base in bases {
            listing.disks.bases.remove(&base);
        }
    }

    /// Every digest measured, in order.
    #[must_use]
    pub fn measured(&self) -> Vec<ImageDigest> {
        lock(&self.measured).clone()
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
        if *lock(&self.listing_fails) {
            return Err(VmError::Definite);
        }
        let state = lock(&self.state);
        state.1.map_or_else(|| Ok(state.0.clone()), Err)
    }

    async fn measure(&self, digest: &ImageDigest) -> Result<u64, VmError> {
        lock(&self.measured).push(digest.clone());
        if let Some(error) = lock(&self.state).1 {
            return Err(error);
        }
        lock(&self.manifests)
            .get(digest)
            .copied()
            .ok_or(VmError::Definite)
    }

    async fn pull(&self, digest: &ImageDigest) -> Result<(), VmError> {
        let size = lock(&self.manifests).get(digest).copied();
        self.apply(|listing| {
            listing.images.insert(digest.clone());
            if let Some(size) = size {
                listing.disks.images.insert(digest.clone(), size);
            }
        })
    }

    async fn materialise(
        &self,
        digest: &ImageDigest,
        base: &BaseName,
        _runner_version: &str,
    ) -> Result<(), VmError> {
        let size = lock(&self.manifests).get(digest).copied();
        self.apply(|listing| {
            listing.bases.insert(base.clone(), GuestOs::Linux);
            listing
                .disks
                .base_images
                .insert(base.clone(), digest.clone());
            if let Some(size) = size {
                listing.disks.bases.insert(base.clone(), size);
            }
        })
    }

    async fn evict(&self, base: &BaseName) -> Result<(), VmError> {
        self.apply(|listing| {
            listing.bases.remove(base);
            listing.disks.bases.remove(base);
            listing.disks.base_images.remove(base);
        })
    }
}

/// A guest transport answering every call as scripted, for the executor's tests; the
/// `GuestTransport` contract's world is [`crate::guest::MemoryGuests`].
#[derive(Debug, Clone)]
pub struct FakeGuest {
    state: Arc<Mutex<GuestScript>>,
    launches: Arc<Mutex<usize>>,
}

/// What [`FakeGuest`] answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestScript {
    /// The answer to every challenge.
    pub verify: Result<GuestAnswer, VmError>,
    /// The answer to every launch.
    pub launch: Result<LaunchAnswer, VmError>,
    /// The answer to every status read.
    pub status: Result<RunnerStatus, VmError>,
    /// The answer to every export.
    pub export: Result<Diagnostics, VmError>,
}

impl FakeGuest {
    /// A guest answering as `script` says.
    #[must_use]
    pub fn new(script: GuestScript) -> Self {
        Self {
            state: Arc::new(Mutex::new(script)),
            launches: Arc::new(Mutex::new(0)),
        }
    }

    /// Answers as `script` says from now on.
    pub fn script(&self, script: GuestScript) {
        *lock(&self.state) = script;
    }

    /// How many launches reached it.
    #[must_use]
    pub fn launches(&self) -> usize {
        *lock(&self.launches)
    }
}

impl GuestTransport for FakeGuest {
    async fn verify(&self, _vm: &OwnedVmName, _nonce: Nonce) -> Result<GuestAnswer, VmError> {
        lock(&self.state).verify
    }

    async fn launch(
        &self,
        _vm: &OwnedVmName,
        _instance_id: &ProviderInstanceId,
        _launch: SecretHandle,
    ) -> Result<LaunchAnswer, VmError> {
        *lock(&self.launches) += 1;
        lock(&self.state).launch
    }

    async fn runner_status(&self, _vm: &OwnedVmName) -> Result<RunnerStatus, VmError> {
        lock(&self.state).status
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
