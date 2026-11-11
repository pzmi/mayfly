//! The in-memory image world: [`MemoryImages`], an `ImageStore` over a registry, a cache, local
//! VMs and a record of materialised bases held in memory, keeping the store's contract (ADR 0039,
//! ADR 0087), and the deliberately broken stores of ver §5: [`TagPullStore`] (pulls by tag),
//! [`BaseEvictingStore`] (evicts a referenced base) and [`NameEvictingStore`] (evicts by name,
//! ignoring the recorded identity).
//!
//! It mirrors what the Tart adapter does over `tart`: a pull fetches `<repository>@<digest>` once
//! per digest at a time and keeps the content only when it hashes to the digest; a base is named
//! by digest and runner version, made only from a pulled image, recorded with its own instance
//! id and the runner installed once; an eviction deletes only a recorded base whose instance id
//! matches and that no live clone references. The registry, the cache, the VMs and the record
//! outlive [`MemoryImages::reopened`] (a restarted daemon); the single-flight locks do not.
//! Nothing here spawns, sleeps or reads a clock: a slow fetch yields to the runtime instead.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use mayfly_data::{ImageListing, ImageStore, VmError};
use mayfly_domain::{ImageDigest, OwnedVmName};
use mayfly_protocol::{BaseName, GuestOs};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;

/// The repository every world pulls from (synthetic).
pub const REPOSITORY: &str = "registry.example/mayfly/runner";

/// The runner version every world has a pinned checksum for.
pub const RUNNER: &str = "2.330.0";

/// The pinned checksum of [`RUNNER`]'s package (synthetic).
pub const RUNNER_SHA256: &str = "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";

/// How many times a slow fetch yields: enough for a concurrent pull to reach its own fetch.
const SLOW_YIELDS: u32 = 64;

fn lock<T>(state: &Mutex<T>) -> MutexGuard<'_, T> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The digest of `bytes`.
#[must_use]
pub fn digest_of(bytes: &[u8]) -> ImageDigest {
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    ImageDigest::new(&format!("sha256:{hex}")).expect("a sha256 digest")
}

/// The base `digest` with runner `version` materialises to (`mfb-<digest12>-r<x>-<y>-<z>`,
/// domain-model §6.7); `None` when the version is not canonical `X.Y.Z`.
#[must_use]
pub fn base_for(digest: &ImageDigest, version: &str) -> Option<BaseName> {
    let parts: Vec<&str> = version.split('.').collect();
    let canonical = parts.len() == 3
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.bytes().all(|b| b.is_ascii_digit())
                && (*p == "0" || !p.starts_with('0'))
        });
    if !canonical {
        return None;
    }
    let hex = digest.as_str().strip_prefix("sha256:")?;
    BaseName::new(&format!(
        "mfb-{}-r{}-{}-{}",
        hex.get(..12)?,
        parts[0],
        parts[1],
        parts[2]
    ))
    .ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalVm {
    instance: String,
    running: bool,
    guest: GuestOs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Recorded {
    digest: ImageDigest,
    runner: String,
    instance: String,
    clones: BTreeSet<String>,
}

#[derive(Debug, Default)]
struct World {
    registry: BTreeMap<String, (Vec<u8>, GuestOs)>,
    fetched: Vec<String>,
    slow: bool,
    cache: BTreeMap<ImageDigest, GuestOs>,
    vms: BTreeMap<String, LocalVm>,
    record: BTreeMap<String, Recorded>,
    installs: BTreeMap<String, Vec<String>>,
    minted: u32,
}

impl World {
    fn mint(&mut self) -> String {
        self.minted += 1;
        let [_, a, b, c] = self.minted.to_be_bytes();
        format!("0e:00:00:{a:02x}:{b:02x}:{c:02x}")
    }
}

/// An `ImageStore` over an in-memory world; clones share the world, [`MemoryImages::reopened`]
/// shares it with fresh locks.
#[derive(Debug, Clone, Default)]
pub struct MemoryImages {
    world: Arc<Mutex<World>>,
    flights: Arc<Mutex<BTreeMap<ImageDigest, Arc<AsyncMutex<()>>>>>,
}

fn reference(digest: &ImageDigest) -> String {
    format!("{REPOSITORY}@{digest}")
}

impl MemoryImages {
    /// An empty world.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The same world as a restarted daemon sees it: registry, cache, VMs and record kept.
    #[must_use]
    pub fn reopened(&self) -> Self {
        Self {
            world: Arc::clone(&self.world),
            flights: Arc::default(),
        }
    }

    fn world(&self) -> MutexGuard<'_, World> {
        lock(&self.world)
    }

    fn flight(&self, digest: &ImageDigest) -> Arc<AsyncMutex<()>> {
        Arc::clone(lock(&self.flights).entry(digest.clone()).or_default())
    }

    /// The registry serves `content`, an image of `guest`, at `reference`.
    pub fn serve(&self, reference: &str, content: &[u8], guest: GuestOs) {
        self.world()
            .registry
            .insert(reference.to_owned(), (content.to_vec(), guest));
    }

    /// Every reference fetched from the registry, in order.
    #[must_use]
    pub fn fetched(&self) -> Vec<String> {
        self.world().fetched.clone()
    }

    /// The next fetch yields to the runtime before it answers.
    pub fn slow_next_fetch(&self) {
        self.world().slow = true;
    }

    /// The runner versions installed into `name`.
    #[must_use]
    pub fn installs(&self, name: &str) -> Vec<String> {
        self.world().installs.get(name).cloned().unwrap_or_default()
    }

    /// Whether a local VM `name` exists, and whether it runs.
    #[must_use]
    pub fn exists(&self, name: &str) -> Option<bool> {
        self.world().vms.get(name).map(|vm| vm.running)
    }

    /// A stopped local VM `name` this store did not make.
    pub fn seed_foreign(&self, name: &str) {
        let mut world = self.world();
        let instance = world.mint();
        world.vms.insert(
            name.to_owned(),
            LocalVm {
                instance,
                running: false,
                guest: GuestOs::Macos,
            },
        );
    }

    /// Clones `vm` from `base` as the deployment's provider does: only from a recorded base
    /// with its recorded instance id, the clone recorded under it first.
    ///
    /// # Errors
    /// [`VmError::Definite`] for a base the record does not prove, or a name that exists.
    pub fn clone_from(&self, base: &BaseName, vm: &OwnedVmName) -> Result<(), VmError> {
        let mut world = self.world();
        let name = vm.to_string();
        let proven = world.record.get(base.as_str()).is_some_and(|r| {
            world
                .vms
                .get(base.as_str())
                .is_some_and(|v| v.instance == r.instance)
        });
        if !proven || world.vms.contains_key(&name) {
            return Err(VmError::Definite);
        }
        if let Some(recorded) = world.record.get_mut(base.as_str()) {
            recorded.clones.insert(name.clone());
        }
        let instance = world.mint();
        let guest = world
            .vms
            .get(base.as_str())
            .map_or(GuestOs::Macos, |b| b.guest);
        world.vms.insert(
            name,
            LocalVm {
                instance,
                running: false,
                guest,
            },
        );
        Ok(())
    }

    /// Removes the local VM `name`, as an operator or a `DeleteVm` would.
    pub fn remove(&self, name: &str) {
        self.world().vms.remove(name);
    }

    /// Gives the local VM `name` a new instance id behind the store's back.
    pub fn change_identity(&self, name: &str) {
        let mut world = self.world();
        let instance = world.mint();
        if let Some(vm) = world.vms.get_mut(name) {
            vm.instance = instance;
        }
    }

    /// Fetches `reference` from the registry, after yielding when the fetch is slow.
    async fn fetch(&self, reference: &str) -> Option<(Vec<u8>, GuestOs)> {
        let slow = {
            let mut world = self.world();
            world.fetched.push(reference.to_owned());
            std::mem::take(&mut world.slow)
        };
        if slow {
            for _ in 0..SLOW_YIELDS {
                tokio::task::yield_now().await;
            }
        }
        self.world().registry.get(reference).cloned()
    }

    /// Pulls `digest` through `reference`, one pull per digest at a time; `verify` keeps the
    /// content only when it hashes to `digest`.
    async fn pull_through(
        &self,
        digest: &ImageDigest,
        reference: &str,
        verify: bool,
    ) -> Result<(), VmError> {
        let flight = self.flight(digest);
        let _flight = flight.lock().await;
        if self.world().cache.contains_key(digest) {
            return Ok(());
        }
        let (content, guest) = self.fetch(reference).await.ok_or(VmError::Definite)?;
        if verify && digest_of(&content) != *digest {
            return Err(VmError::Definite);
        }
        self.world().cache.insert(digest.clone(), guest);
        Ok(())
    }

    /// Evicts whatever VM is named `base`, record or not, identity or not (still refusing while
    /// a recorded clone of it exists): the broken store's eviction.
    fn evict_by_name(&self, base: &BaseName) -> Result<(), VmError> {
        let mut world = self.world();
        let name = base.as_str();
        let referenced = world
            .record
            .get(name)
            .is_some_and(|r| r.clones.iter().any(|c| world.vms.contains_key(c)));
        if referenced {
            return Err(VmError::Definite);
        }
        world.vms.remove(name);
        world.record.remove(name);
        Ok(())
    }

    /// Evicts `base`; `check_references` refuses while a recorded clone still exists.
    fn evict_with(&self, base: &BaseName, check_references: bool) -> Result<(), VmError> {
        let mut world = self.world();
        let name = base.as_str();
        let recorded = world.record.get(name).cloned();
        let listed = world.vms.get(name).cloned();
        match (recorded, listed) {
            (None, None) => Ok(()),
            (None, Some(_)) => Err(VmError::Definite),
            (Some(_), None) => {
                world.record.remove(name);
                Ok(())
            }
            (Some(recorded), Some(vm)) => {
                if recorded.instance != vm.instance || vm.running {
                    return Err(VmError::Definite);
                }
                let live: BTreeSet<String> = recorded
                    .clones
                    .iter()
                    .filter(|c| world.vms.contains_key(*c))
                    .cloned()
                    .collect();
                if let Some(entry) = world.record.get_mut(name) {
                    entry.clones.clone_from(&live);
                }
                if check_references && !live.is_empty() {
                    return Err(VmError::Definite);
                }
                world.vms.remove(name);
                world.record.remove(name);
                Ok(())
            }
        }
    }
}

impl ImageStore for MemoryImages {
    async fn list(&self) -> Result<ImageListing, VmError> {
        let world = self.world();
        let bases = world
            .record
            .iter()
            .filter_map(|(name, r)| {
                let vm = world.vms.get(name).filter(|v| v.instance == r.instance)?;
                Some((BaseName::new(name).ok()?, vm.guest))
            })
            .collect();
        Ok(ImageListing {
            images: world.cache.keys().cloned().collect(),
            bases,
        })
    }

    async fn pull(&self, digest: &ImageDigest) -> Result<(), VmError> {
        self.pull_through(digest, &reference(digest), true).await
    }

    async fn materialise(
        &self,
        digest: &ImageDigest,
        base: &BaseName,
        runner_version: &str,
    ) -> Result<(), VmError> {
        if base_for(digest, runner_version).as_ref() != Some(base) || runner_version != RUNNER {
            return Err(VmError::Definite);
        }
        let mut world = self.world();
        let name = base.as_str();
        let recorded = world.record.get(name).cloned();
        match (recorded, world.vms.get(name)) {
            (Some(r), Some(vm)) if r.instance == vm.instance => {
                return if r.digest == *digest && r.runner == runner_version {
                    Ok(())
                } else {
                    Err(VmError::Definite)
                };
            }
            (_, Some(_)) => return Err(VmError::Definite),
            (_, None) => {}
        }
        let Some(guest) = world.cache.get(digest).copied() else {
            return Err(VmError::Definite);
        };
        let instance = world.mint();
        world.vms.insert(
            name.to_owned(),
            LocalVm {
                instance: instance.clone(),
                running: false,
                guest,
            },
        );
        world
            .installs
            .entry(name.to_owned())
            .or_default()
            .push(runner_version.to_owned());
        world.record.insert(
            name.to_owned(),
            Recorded {
                digest: digest.clone(),
                runner: runner_version.to_owned(),
                instance,
                clones: BTreeSet::new(),
            },
        );
        Ok(())
    }

    async fn evict(&self, base: &BaseName) -> Result<(), VmError> {
        self.evict_with(base, true)
    }
}

/// Broken (ver §5): pulls `<repository>:latest` and keeps whatever it served as the asked
/// digest, so it neither fetches by digest nor can verify what it got.
#[derive(Debug, Clone)]
pub struct TagPullStore(pub MemoryImages);

impl ImageStore for TagPullStore {
    async fn list(&self) -> Result<ImageListing, VmError> {
        self.0.list().await
    }

    async fn pull(&self, digest: &ImageDigest) -> Result<(), VmError> {
        self.0
            .pull_through(digest, &format!("{REPOSITORY}:latest"), false)
            .await
    }

    async fn materialise(
        &self,
        digest: &ImageDigest,
        base: &BaseName,
        runner_version: &str,
    ) -> Result<(), VmError> {
        self.0.materialise(digest, base, runner_version).await
    }

    async fn evict(&self, base: &BaseName) -> Result<(), VmError> {
        self.0.evict(base).await
    }
}

/// Broken (ver §5): evicts a base without re-checking that no live clone references it.
#[derive(Debug, Clone)]
pub struct BaseEvictingStore(pub MemoryImages);

impl ImageStore for BaseEvictingStore {
    async fn list(&self) -> Result<ImageListing, VmError> {
        self.0.list().await
    }

    async fn pull(&self, digest: &ImageDigest) -> Result<(), VmError> {
        self.0.pull(digest).await
    }

    async fn materialise(
        &self,
        digest: &ImageDigest,
        base: &BaseName,
        runner_version: &str,
    ) -> Result<(), VmError> {
        self.0.materialise(digest, base, runner_version).await
    }

    async fn evict(&self, base: &BaseName) -> Result<(), VmError> {
        self.0.evict_with(base, false)
    }
}

/// Broken (ver §5): evicts a base by its name alone, a foreign VM of that name or one whose
/// instance id is no longer the recorded one included.
#[derive(Debug, Clone)]
pub struct NameEvictingStore(pub MemoryImages);

impl ImageStore for NameEvictingStore {
    async fn list(&self) -> Result<ImageListing, VmError> {
        self.0.list().await
    }

    async fn pull(&self, digest: &ImageDigest) -> Result<(), VmError> {
        self.0.pull(digest).await
    }

    async fn materialise(
        &self,
        digest: &ImageDigest,
        base: &BaseName,
        runner_version: &str,
    ) -> Result<(), VmError> {
        self.0.materialise(digest, base, runner_version).await
    }

    async fn evict(&self, base: &BaseName) -> Result<(), VmError> {
        self.0.evict_by_name(base)
    }
}
