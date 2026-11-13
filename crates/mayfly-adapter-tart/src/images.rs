//! `TartImageStore`: the `ImageStore` port over the `tart` CLI (plan §8.1, domain-model §9.3,
//! ADR 0039, ADR 0087).
//!
//! - **pull**: by digest only (`<repository>@<digest>`, built from an [`ImageDigest`], which no
//!   tag parses as). One pull per digest at a time (single flight). Before Tart runs, the raw
//!   manifest is fetched independently ([`ManifestSource`]) and its exact bytes are checked
//!   against the digest; its layout must be Tart's disk v2 in Tart's order (disk v1, ASIF and
//!   unknown layers are refused, [`verify::layout`]). After `tart pull`, the cache entry (a
//!   directory, never a symlink: Tart links a requested digest to the entry of whatever manifest
//!   it was served, with an absolute link) must hold `config.json` and `nvram.bin` matching their
//!   descriptors and a `disk.img` whose every chunk hashes to its uncompressed digest. The cached
//!   `manifest.json` is never read: it is whatever Tart was served (pinned by
//!   `tests/tart/images.rs::should_never_read_the_cached_manifest_when_verifying_an_image`). A
//!   failed check deletes the entry and pulls once more, then refuses `Definite`.
//!   Compressed blobs are not re-checked (their content is, through the uncompressed digests).
//!   A digest already verified, whose cache entry is unchanged and whose kept manifest still
//!   hashes to it, is done before anything is fetched, so a pull issued again after an unknown
//!   outcome costs no registry request (DP-08;
//!   `tests/tart/images.rs::should_fetch_nothing_when_the_image_is_already_pulled_and_unchanged`).
//! - **materialise**: the base's name must be the one the digest and runner version give, the
//!   runner must have a pinned checksum, the image must be verified and unchanged, and Tart must
//!   not list the name (a complete listing). Then the registry ([`crate::bases`]) takes each
//!   state before its step: `Reserved`, `Cloning` (`tart clone <reference> <base>`), `Customizing`
//!   (its own MAC by `tart set --random-mac` and its file identity; the fresh clone's files
//!   verified again before the first boot; one boot under the shim, ADR 0037; the runner
//!   installed and checked through `tart exec <base> <launcher> install-runner <version>
//!   <sha256>`, ADR 0038, ADR 0082; `tart stop`), `Verifying` (stopped), `Ready` (with its final
//!   fingerprint). A failure with the identity proven retires and deletes the base and answers
//!   `Definite`; a clone that cannot be proven is quarantined, never deleted.
//! - **evict**: a `Ready` base whose recorded identity (file identity and MAC) holds, that is
//!   stopped and that no recorded clone or unresolved clone intent Tart still lists depends on,
//!   under the admission lock: `Retiring`, then `tart delete <base>`. A quarantined base is never
//!   deleted, whatever its identity; a name the registry does not hold is never deleted; a
//!   recorded base a complete listing no longer shows is done. Absence is always read from a
//!   complete `tart list`, never a per-name `tart get`.
//! - **measure**: the raw manifest fetched by digest ([`ManifestSource`]), checked against the
//!   digest and read as Tart's disk v2 layout, gives the disk the image reconstructs to (the
//!   chunks' uncompressed sizes, [`Layout::disk_bytes`]); nothing is pulled or kept.
//! - **list**: the verified, unchanged images, and only the `Ready` bases that pass the clone
//!   proof ([`crate::proof::ready_for_clone`]), with their guest; each with its disk, read from
//!   the manifest kept at its pull (a manifest that no longer reads leaves its size unknown). Any other `mfb-*` VM is counted
//!   by the ledger from the inventory, in full (DP-06F), so every base is counted once.
//! - **recover**: crash recovery by observation, per base, from what a crash at each state
//!   leaves (ADR 0087's crash table; a base left booted is stopped with `tart stop` before it is
//!   retired); run before each materialisation and eviction of a base, and by composition at
//!   start-up for all of them. Hashing runs on Tokio's blocking pool.
//!
//! Every `tart` child is the runner's (argv only, cleared environment plus the allowlist, own
//! process group, deadline, bounded output). Pinned by the `ImageStore` contract and the fault
//! cases over `mayfly-fake-tart` (`tests/tart/images.rs`); real Tart's pull by digest, cache
//! layout, `tart exec` in a booted base and the launcher's `install-runner` are confirmed in the
//! hardware lane (HW-01).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use mayfly_data::{Ambiguity, BaseName, GuestOs, ImageListing, ImageStore, MeasuredDisks, VmError};
use mayfly_domain::ImageDigest;
use serde::Deserialize;
use tokio::sync::Mutex as AsyncMutex;

use crate::bases::{
    BaseEntry, BaseRecords, BaseState, ImageEntry, RECIPE_VERSION, Record, Signature,
    SignaturePolicy, SignatureResult, random_id,
};
use crate::config::TartConfig;
use crate::guest::LAUNCHER;
use crate::oci::{ManifestSource, RegistryManifests, matches};
use crate::process::blocking::blocking;
use crate::process::vm_shim::ShimTarget;
use crate::proof::{self, NotCloneable, identity_holds, ready_for_clone, vm_dir};
use crate::provider::{TartProvider, after_effect, before_effect, effect_failed};
use crate::tart::{CallError, TartCli};
use crate::verify::{self, FileStamp, Layout, UnsafeLayout};

/// The most bytes of the launcher's `install-runner` answer kept.
const ANSWER_LIMIT: usize = 4096;

/// How many times a pull is tried before it is refused: once, and once more after the entry
/// that failed its check is deleted.
const PULL_TRIES: usize = 2;

/// Each image call's own deadline beyond the Tart adapter's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageDeadlines {
    /// One `tart pull`.
    pub pull: Duration,
    /// The launcher's `install-runner` in a booted base.
    pub install: Duration,
    /// The grace a booted base's guest gets from `tart stop`.
    pub halt: Duration,
}

/// The image store's configuration beside the Tart adapter's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageConfig {
    /// The OCI repository images are pulled from by digest (`ghcr.io/acme/runner-macos`):
    /// no tag, no digest, no scheme.
    pub repository: String,
    /// The pinned runner packages: version (`X.Y.Z`), then the platform each package is built
    /// for, to that package's SHA-256 (64 lowercase hex digits), which the launcher checks before
    /// it installs. The runner ships one package per platform, so a version is pinned per
    /// platform; the base's guest OS (from the image's `config.json`) selects the pin. A version,
    /// or a platform of it, not here is never installed (HW-01X). Pinned by
    /// `tests::should_pick_the_pin_of_the_image_guest_when_a_version_has_one_per_platform`.
    pub runners: BTreeMap<String, BTreeMap<RunnerPlatform, String>>,
    /// This deployment's id, recorded on every base it makes.
    pub deployment_id: String,
    /// The repository's signature policy (the pools' strictest, composed by RT-02).
    pub signature: SignaturePolicy,
    /// Each call's deadline.
    pub deadlines: ImageDeadlines,
}

/// The platform a runner package is built for: the runner's own release names
/// (`actions-runner-<platform>-<version>.tar.gz`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RunnerPlatform {
    /// `linux-arm64`: a Linux guest on Apple silicon.
    LinuxArm64,
    /// `osx-arm64`: a macOS guest on Apple silicon.
    OsxArm64,
}

impl RunnerPlatform {
    /// The release's spelling (`linux-arm64`, `osx-arm64`), the config's key.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LinuxArm64 => "linux-arm64",
            Self::OsxArm64 => "osx-arm64",
        }
    }

    /// The platform spelled `text`, if it is one.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        [Self::LinuxArm64, Self::OsxArm64]
            .into_iter()
            .find(|p| p.as_str() == text)
    }

    /// `version → platform name → checksum` (the configuration's spelling) as the store's
    /// pins; a name that is not a platform is dropped (the configuration refused it already).
    /// Pinned by `tests::should_round_trip_each_platform_when_it_is_spelled_as_the_release_does`.
    #[must_use]
    pub fn pin_table(
        named: &BTreeMap<String, BTreeMap<String, String>>,
    ) -> BTreeMap<String, BTreeMap<Self, String>> {
        named
            .iter()
            .map(|(version, pins)| {
                let pins = pins
                    .iter()
                    .filter_map(|(name, sha)| Self::parse(name).map(|p| (p, sha.clone())))
                    .collect();
                (version.clone(), pins)
            })
            .collect()
    }

    /// The package a guest of `guest` runs (mayfly hosts are Apple silicon).
    #[must_use]
    pub const fn for_guest(guest: GuestOs) -> Self {
        match guest {
            GuestOs::Linux => Self::LinuxArm64,
            GuestOs::Macos => Self::OsxArm64,
        }
    }
}

/// The guest OS Tart's `config.json` names (`"os": "darwin"` or `"linux"`).
fn config_guest(raw: &[u8]) -> Option<GuestOs> {
    let config: serde_json::Value = serde_json::from_slice(raw).ok()?;
    match config.get("os")?.as_str()? {
        "darwin" => Some(GuestOs::Macos),
        "linux" => Some(GuestOs::Linux),
        _ => None,
    }
}

/// A configuration the store refuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// The repository is not a bare `host/path` (a tag, a digest, a scheme, an empty or dot
    /// component, or a character outside `[a-z0-9._-]` and the host's port).
    Repository,
    /// A runner version is not `X.Y.Z`, pins no platform, or a checksum is not 64 lowercase hex
    /// digits.
    Runner(String),
    /// The deployment id is empty.
    Deployment,
    /// `required` signatures: no verifier is built yet (SIG-01), so nothing could pass.
    SignaturesUnsupported,
    /// `TART_HOME` or the registry's directory is not private to the daemon's user.
    UnsafeLayout(UnsafeLayout),
}

fn version_numbers(version: &str) -> Option<[u64; 3]> {
    let mut parts = version.split('.');
    let mut out = [0; 3];
    for slot in &mut out {
        let part = parts.next()?;
        let canonical = !part.is_empty()
            && part.bytes().all(|b| b.is_ascii_digit())
            && (part == "0" || !part.starts_with('0'));
        if !canonical {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    parts.next().is_none().then_some(out)
}

fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl ImageConfig {
    /// Checks the repository, the pinned runners, the deployment id and the signature policy.
    /// Pinned by
    /// `tests::should_refuse_a_repository_when_it_carries_a_tag_digest_or_scheme`.
    ///
    /// # Errors
    /// [`ConfigError`].
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut parts = self.repository.split('/');
        let host = parts.next().unwrap_or("");
        let host_ok = !host.is_empty()
            && host
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-:".contains(&b));
        let path: Vec<&str> = parts.collect();
        let path_ok = !path.is_empty()
            && path.iter().all(|p| {
                !p.is_empty()
                    && *p != "."
                    && *p != ".."
                    && p.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)
                    })
            });
        if !(host_ok && path_ok) || host.starts_with(['.', ':']) || host.contains("..") {
            return Err(ConfigError::Repository);
        }
        for (version, pins) in &self.runners {
            if version_numbers(version).is_none()
                || pins.is_empty()
                || !pins.values().all(|sha| is_sha256(sha))
            {
                return Err(ConfigError::Runner(version.clone()));
            }
        }
        if self.deployment_id.is_empty() {
            return Err(ConfigError::Deployment);
        }
        if self.signature == SignaturePolicy::Required {
            return Err(ConfigError::SignaturesUnsupported);
        }
        Ok(())
    }
}

/// The base `digest` with runner `version` materialises to:
/// `mfb-<digest12>-r<major>-<minor>-<patch>` (domain-model §6.7, the control plane's
/// `image::base_name`). `None` when the version is not canonical `X.Y.Z`. Pinned by
/// `tests::should_name_the_base_by_digest_and_runner_version_when_the_version_is_canonical`.
#[must_use]
pub fn base_name(digest: &ImageDigest, version: &str) -> Option<BaseName> {
    let [major, minor, patch] = version_numbers(version)?;
    let hex = digest.as_str().strip_prefix("sha256:")?;
    BaseName::new(&format!("mfb-{}-r{major}-{minor}-{patch}", hex.get(..12)?)).ok()
}

#[derive(Deserialize)]
struct Installed {
    v: u32,
    runner_version: String,
}

/// The Tart `ImageStore`, reading raw manifests from `M`.
#[derive(Debug)]
pub struct TartImageStore<M = RegistryManifests> {
    tart: TartCli,
    booter: TartProvider,
    images: ImageConfig,
    bases: Arc<BaseRecords>,
    manifests: M,
    flights: Mutex<BTreeMap<ImageDigest, Arc<AsyncMutex<()>>>>,
}

/// The digest of the entry a cache link points to, read from the target's last component,
/// absolute or relative; `None` when the target leaves the link's own directory (the repository's
/// directory in Tart's OCI cache) or names no digest. Pinned by
/// `tests::should_read_the_linked_digest_when_the_link_stays_in_the_cache`.
fn linked_digest(link: &std::path::Path, target: &std::path::Path) -> Option<ImageDigest> {
    let dir = link.parent()?;
    let resolved = if target.is_absolute() {
        target.to_path_buf()
    } else {
        dir.join(target)
    };
    if resolved.parent() != Some(dir) {
        return None;
    }
    ImageDigest::new(resolved.file_name()?.to_str()?).ok()
}

/// Creates the registry's directory `0700` when missing, then checks it and `TART_HOME` are
/// private to the daemon's user (ADR 0087's start-up check).
fn check_layout(tart: &TartConfig, bases: &BaseRecords) -> Result<(), ConfigError> {
    verify::check_path(bases.dir()).map_err(ConfigError::UnsafeLayout)?;
    let created = DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(bases.dir());
    if created.is_err() {
        return Err(ConfigError::UnsafeLayout(UnsafeLayout::NotADirectory(
            bases.dir().to_path_buf(),
        )));
    }
    verify::check_private(&tart.tart_home).map_err(ConfigError::UnsafeLayout)?;
    verify::check_private(bases.dir()).map_err(ConfigError::UnsafeLayout)
}

impl<M: ManifestSource> TartImageStore<M> {
    /// A store over `tart`'s configuration and `images`', recording what it makes in `bases`
    /// (the registry the provider clones from) and reading raw manifests from `manifests`.
    ///
    /// # Errors
    /// [`ConfigError`]: `images` does not validate, or the directories are not private.
    pub fn new(
        tart: TartConfig,
        images: ImageConfig,
        bases: Arc<BaseRecords>,
        manifests: M,
    ) -> Result<Self, ConfigError> {
        images.validate()?;
        check_layout(&tart, &bases)?;
        Ok(Self {
            tart: TartCli::new(tart.clone()),
            booter: TartProvider::new(tart, Arc::clone(&bases)),
            images,
            bases,
            manifests,
            flights: Mutex::new(BTreeMap::new()),
        })
    }

    fn reference(&self, digest: &ImageDigest) -> String {
        format!("{}@{digest}", self.images.repository)
    }

    fn home(&self) -> &std::path::Path {
        &self.tart.config().tart_home
    }

    /// The one lock pulls of `digest` take, so only one runs at a time.
    fn flight(&self, digest: &ImageDigest) -> Arc<AsyncMutex<()>> {
        let mut flights = self.flights.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(flights.entry(digest.clone()).or_default())
    }

    /// Tart's OCI cache entry for `digest`: `cache/OCIs/<repository>/<digest>`.
    fn cache_dir(&self, digest: &ImageDigest) -> PathBuf {
        let mut path = self.home().join("cache").join("OCIs");
        for part in self.images.repository.split('/') {
            path.push(part);
        }
        path.push(digest.as_str());
        path
    }

    /// The stamps of a cache entry's files, never through a symlinked entry.
    fn cache_files(&self, digest: &ImageDigest) -> Option<[FileStamp; 3]> {
        let print = verify::fingerprint(&self.cache_dir(digest))?;
        Some([print.config, print.nvram, print.disk])
    }

    /// The disk of `digest` by the manifest kept at its pull, when it still hashes to the digest
    /// and reads.
    fn kept_disk(&self, digest: &ImageDigest) -> Option<u64> {
        let raw = self.bases.manifest(digest)?;
        verify::layout(&raw).ok().map(|layout| layout.disk_bytes())
    }

    /// Whether `digest` was verified and its cache entry is unchanged since.
    fn image_current(&self, record: &Record, digest: &ImageDigest) -> bool {
        record
            .images
            .get(digest)
            .is_some_and(|entry| self.cache_files(digest).as_ref() == Some(&entry.files))
    }

    /// The names Tart lists locally, or `Definite` when the listing is not complete.
    async fn local_names(&self) -> Result<BTreeSet<String>, VmError> {
        self.tart
            .list()
            .await
            .map(|entries| entries.into_iter().map(|e| e.name).collect())
            .map_err(|_| VmError::Definite)
    }

    /// Checks a freshly pulled cache entry against `layout`; its file stamps when it holds.
    async fn verify_cache(&self, digest: &ImageDigest, layout: &Layout) -> Option<[FileStamp; 3]> {
        let reference = self.reference(digest);
        let listed = self.tart.list_oci().await.ok()?;
        if listed.iter().all(|e| e.name != reference) {
            return None;
        }
        let dir = self.cache_dir(digest);
        let layout = layout.clone();
        blocking(move || verify::check_image(&dir, &layout, None))
            .await
            .ok()?;
        self.cache_files(digest)
    }

    /// Deletes the cache entry of `digest` that failed its check, and, when Tart linked it to
    /// the entry of another manifest (Tart's `VMStorageOCI.link` after a registry served another
    /// one),
    /// that entry too, by its own reference: nothing unverified stays in the cache.
    async fn discard_entry(&self, digest: &ImageDigest) {
        let entry = self.cache_dir(digest);
        let linked = std::fs::read_link(&entry)
            .ok()
            .and_then(|target| linked_digest(&entry, &target))
            .filter(|other| other != digest);
        let _deleted = self.tart.delete(&self.reference(digest)).await;
        if let Some(other) = linked {
            let _deleted = self.tart.delete(&self.reference(&other)).await;
        }
    }

    /// Installs the runner into the booted `base` and checks the version it reports.
    async fn install(&self, base: &str, version: &str, sha256: &str) -> Result<(), VmError> {
        let answer = self
            .tart
            .exec(
                base,
                &[LAUNCHER, "install-runner", version, sha256],
                self.images.deadlines.install,
                ANSWER_LIMIT,
            )
            .await
            .map_err(|err| effect_failed(&err))?;
        match serde_json::from_slice::<Installed>(&answer) {
            Ok(installed) if installed.v == 1 && installed.runner_version == version => Ok(()),
            _ => Err(VmError::Definite),
        }
    }

    fn set_state(&self, name: &str, change: impl FnOnce(&mut BaseEntry)) -> Result<(), VmError> {
        self.bases
            .update(|record| record.bases.get_mut(name).map(change))
            .ok()
            .flatten()
            .ok_or(VmError::Ambiguous(Ambiguity::ProcessLost))
    }

    /// After `tart clone`: the base's own MAC and file identity, recorded as `Customizing`. Never
    /// `--random-serial`: the clone's config must stay the image's in every field but the MAC
    /// ([`verify::check_image`]); the job clones get their own serial
    /// (`tart set --random-serial`, arm64 builds only; `provider`).
    async fn prove(&self, name: &str) -> Result<(), VmError> {
        self.tart
            .set_random_mac(name)
            .await
            .map_err(|err| after_effect(&err))?;
        let mac = match self.tart.get(name).await {
            Ok(Some(info)) => info.instance_text(),
            Ok(None) => return Err(VmError::Ambiguous(Ambiguity::ProcessLost)),
            Err(err) => return Err(after_effect(&err)),
        };
        let identity = verify::identity(&vm_dir(self.home(), name))
            .ok_or(VmError::Ambiguous(Ambiguity::ProcessLost))?;
        self.set_state(name, |entry| {
            entry.state = BaseState::Customizing;
            entry.vm_mac = Some(mac);
            entry.vm_file_identity = Some(identity);
        })
    }

    /// From a proven clone to `Ready`: its content verified before the first boot, one boot, the
    /// install, the stop, `Verifying`, the final fingerprint, `Ready`.
    async fn build(
        &self,
        base: &BaseName,
        layout: &Layout,
        digest: &ImageDigest,
        version: &str,
        sha256: &str,
    ) -> Result<(), VmError> {
        let name = base.as_str();
        let dir = vm_dir(self.home(), name);
        let (clone_dir, cache, layout) = (dir.clone(), self.cache_dir(digest), layout.clone());
        blocking(move || verify::check_image(&clone_dir, &layout, Some(&cache)))
            .await
            .map_err(|_| VmError::Definite)?;
        let run_id = self.booter.boot(ShimTarget::Base(base.clone())).await?;
        let installed = self.install(name, version, sha256).await;
        let halted = self
            .booter
            .halt(name, &run_id, self.images.deadlines.halt)
            .await;
        installed?;
        halted?;
        self.booter.forget_shim(name);
        self.set_state(name, |entry| entry.state = BaseState::Verifying)?;
        let record = self.bases.read().map_err(|_| VmError::Definite)?;
        let entry = record.bases.get(name).ok_or(VmError::Definite)?;
        match self.tart.get(name).await {
            Ok(Some(info)) if !info.running && identity_holds(entry, &info, self.home(), name) => {}
            Ok(_) => return Err(VmError::Ambiguous(Ambiguity::ProcessLost)),
            Err(err) => return Err(after_effect(&err)),
        }
        let print = blocking(move || verify::fingerprint(&dir).ok_or(()))
            .await
            .map_err(|_| VmError::Ambiguous(Ambiguity::ProcessLost))?;
        self.set_state(name, |entry| {
            entry.state = BaseState::Ready;
            entry.final_fingerprint = Some(print);
        })
    }

    /// Deletes `name` when its recorded identity holds (stopping it first) and drops its entry,
    /// or only the entry when Tart no longer lists it. Whether nothing of it is left.
    async fn retire(&self, name: &str) -> bool {
        let Some(entry) = self
            .bases
            .read()
            .ok()
            .and_then(|r| r.bases.get(name).cloned())
        else {
            return false;
        };
        let Ok(names) = self.local_names().await else {
            return false;
        };
        if names.contains(name) {
            // DeleteAllowed needs no dependents: a recorded clone or unresolved intent Tart still
            // lists keeps the base. Only a Ready base can be cloned from, so a base retired
            // during recovery has none by construction; the check holds the rule here too.
            if entry.clones.keys().any(|clone| names.contains(clone)) {
                return false;
            }
            let info = match self.tart.get(name).await {
                Ok(Some(info)) if identity_holds(&entry, &info, self.home(), name) => info,
                _ => return false,
            };
            if self
                .set_state(name, |entry| entry.state = BaseState::Retiring)
                .is_err()
            {
                return false;
            }
            if info.running {
                let _stopped = self.tart.stop(name, self.images.deadlines.halt).await;
            }
            let _deleted = self.tart.delete(name).await;
            let gone = matches!(self.local_names().await, Ok(names) if !names.contains(name));
            if !gone {
                return false;
            }
        }
        self.booter.forget_shim(name);
        self.bases
            .update(|record| record.bases.remove(name))
            .is_ok()
    }

    /// Recovery by observation of the base `name` from whatever state a crash left it in (ADR
    /// 0087's crash table); the caller holds the base.
    async fn settle(&self, name: &str) -> Result<(), VmError> {
        let record = self.bases.read().map_err(|_| VmError::Definite)?;
        let Some(entry) = record.bases.get(name) else {
            return Ok(());
        };
        match entry.state {
            BaseState::Ready | BaseState::Quarantined => Ok(()),
            BaseState::Reserved | BaseState::Cloning => {
                let present = self.local_names().await?.contains(name);
                if present {
                    let reason = if entry.state == BaseState::Reserved {
                        "present_while_reserved"
                    } else {
                        "clone_unproven"
                    };
                    proof::quarantine(&self.bases, name, reason);
                } else if entry.state == BaseState::Cloning {
                    self.set_state(name, |entry| entry.state = BaseState::Reserved)?;
                }
                Ok(())
            }
            BaseState::Customizing | BaseState::Verifying | BaseState::Retiring => {
                if !self.local_names().await?.contains(name) {
                    let _forgotten = self.bases.update(|record| record.bases.remove(name));
                    return Ok(());
                }
                let info = match self.tart.get(name).await {
                    Ok(Some(info)) => info,
                    Ok(None) => return Err(VmError::Definite),
                    Err(err) => return Err(before_effect(&err)),
                };
                if !identity_holds(entry, &info, self.home(), name) {
                    proof::quarantine(&self.bases, name, "identity_changed");
                    return Ok(());
                }
                // A crash while it was booted leaves it running: `retire` stops it with
                // `tart stop` (ADR 0037) before it deletes it.
                if self.retire(name).await {
                    Ok(())
                } else {
                    Err(VmError::Ambiguous(Ambiguity::ProcessLost))
                }
            }
        }
    }

    /// Recovers every base the registry names (composition runs it at start-up, before the
    /// first directive).
    ///
    /// # Errors
    /// [`VmError`]: the first base that could not be settled; the others are still tried.
    pub async fn recover(&self) -> Result<(), VmError> {
        let names: Vec<String> = self
            .bases
            .read()
            .map_err(|_| VmError::Definite)?
            .bases
            .into_keys()
            .collect();
        let mut first = Ok(());
        for name in names {
            let _held = self.bases.hold(&name).await;
            let settled = self.settle(&name).await;
            if first.is_ok() {
                first = settled;
            }
        }
        first
    }
}

impl<M: ManifestSource> ImageStore for TartImageStore<M> {
    async fn list(&self) -> Result<ImageListing, VmError> {
        let record = self.bases.read().map_err(|_| VmError::Definite)?;
        let listed: BTreeSet<String> = self
            .tart
            .list_oci()
            .await
            .map_err(|err| effect_failed(&err))?
            .into_iter()
            .map(|e| e.name)
            .collect();
        let images: BTreeSet<ImageDigest> = record
            .images
            .iter()
            .filter(|(digest, entry)| {
                listed.contains(&entry.source_ref) && self.image_current(&record, digest)
            })
            .map(|(digest, _)| digest.clone())
            .collect();
        let mut disks = MeasuredDisks::default();
        for digest in &images {
            if let Some(bytes) = self.kept_disk(digest) {
                disks.images.insert(digest.clone(), bytes);
            }
        }
        let mut bases = BTreeMap::new();
        for (name, entry) in &record.bases {
            let Ok(base) = BaseName::new(name) else {
                continue;
            };
            if entry.state != BaseState::Ready {
                continue;
            }
            let info = self
                .tart
                .get(name)
                .await
                .map_err(|err| effect_failed(&err))?;
            let guest: Option<GuestOs> = info
                .filter(|info| ready_for_clone(entry, info, self.home(), name).is_ok())
                .and_then(|info| info.guest());
            if let Some(guest) = guest {
                bases.insert(base.clone(), guest);
                disks
                    .base_images
                    .insert(base.clone(), entry.source_digest.clone());
                if let Some(bytes) = self.kept_disk(&entry.source_digest) {
                    disks.bases.insert(base, bytes);
                }
            }
        }
        Ok(ImageListing {
            images,
            bases,
            disks,
        })
    }

    async fn measure(&self, digest: &ImageDigest) -> Result<u64, VmError> {
        let raw = self
            .manifests
            .fetch(&self.images.repository, digest)
            .await
            .map_err(|_| VmError::Definite)?;
        if !matches(&raw, digest) {
            return Err(VmError::Definite);
        }
        verify::layout(&raw)
            .map(|layout| layout.disk_bytes())
            .map_err(|_| VmError::Definite)
    }

    async fn pull(&self, digest: &ImageDigest) -> Result<(), VmError> {
        let flight = self.flight(digest);
        let _flight = flight.lock().await;
        let record = self.bases.read().map_err(|_| VmError::Definite)?;
        let kept = self
            .bases
            .manifest(digest)
            .is_some_and(|raw| matches(&raw, digest));
        if kept && self.image_current(&record, digest) {
            return Ok(());
        }
        let raw = self
            .manifests
            .fetch(&self.images.repository, digest)
            .await
            .map_err(|_| VmError::Definite)?;
        if !matches(&raw, digest) {
            return Err(VmError::Definite);
        }
        let layout = verify::layout(&raw).map_err(|_| VmError::Definite)?;
        let reference = self.reference(digest);
        for _ in 0..PULL_TRIES {
            self.tart
                .pull(&reference, self.images.deadlines.pull)
                .await
                .map_err(|err| effect_failed(&err))?;
            if let Some(files) = self.verify_cache(digest, &layout).await {
                let signature = Signature {
                    policy: self.images.signature,
                    result: SignatureResult::NotChecked,
                };
                let entry = ImageEntry {
                    source_ref: reference.clone(),
                    files,
                    signature,
                };
                return self
                    .bases
                    .keep_manifest(digest, &raw)
                    .and_then(|()| {
                        self.bases
                            .update(|record| record.images.insert(digest.clone(), entry))
                    })
                    .map(drop)
                    .map_err(|_| VmError::Ambiguous(Ambiguity::ProcessLost));
            }
            self.discard_entry(digest).await;
        }
        Err(VmError::Definite)
    }

    async fn materialise(
        &self,
        digest: &ImageDigest,
        base: &BaseName,
        runner_version: &str,
    ) -> Result<(), VmError> {
        if base_name(digest, runner_version).as_ref() != Some(base) {
            return Err(VmError::Definite);
        }
        let pins = self
            .images
            .runners
            .get(runner_version)
            .ok_or(VmError::Definite)?
            .clone();
        let name = base.as_str();
        let _held = self.bases.hold(name).await;
        self.settle(name).await?;
        let record = self.bases.read().map_err(|_| VmError::Definite)?;
        if let Some(entry) = record.bases.get(name) {
            // The twelve digits in the name are only a name: another digest is a collision.
            if entry.source_digest != *digest {
                return Err(VmError::Definite);
            }
            match entry.state {
                BaseState::Ready if entry.runner_version == runner_version => {
                    let info = match self.tart.get(name).await {
                        Ok(Some(info)) => info,
                        Ok(None) => return Err(VmError::Definite),
                        Err(err) => return Err(before_effect(&err)),
                    };
                    return match ready_for_clone(entry, &info, self.home(), name) {
                        Ok(()) => Ok(()),
                        Err(NotCloneable::NotReady) => Err(VmError::Definite),
                        Err(NotCloneable::Changed(reason)) => {
                            proof::quarantine(&self.bases, name, reason);
                            Err(VmError::Definite)
                        }
                    };
                }
                BaseState::Reserved => {}
                _ => return Err(VmError::Definite),
            }
        }
        let raw = self.bases.manifest(digest).ok_or(VmError::Definite)?;
        let layout = verify::layout(&raw).map_err(|_| VmError::Definite)?;
        let flight = self.flight(digest);
        let pulled = flight.lock().await;
        if !self.image_current(&record, digest) {
            return Err(VmError::Definite);
        }
        if self.local_names().await?.contains(name) {
            return Err(VmError::Definite);
        }
        // The image's guest selects the runner package: nothing is created without its pin.
        let config = self.cache_dir(digest).join(verify::CONFIG_FILE);
        let guest = blocking(move || std::fs::read(config).map_err(drop))
            .await
            .ok()
            .and_then(|raw| config_guest(&raw))
            .ok_or(VmError::Definite)?;
        let sha256 = pins
            .get(&RunnerPlatform::for_guest(guest))
            .ok_or(VmError::Definite)?
            .clone();
        let tart_version = self.tart.version().await.map_err(|_| VmError::Definite)?;
        let base_id = random_id().map_err(|_| VmError::Definite)?;
        let signature = record
            .images
            .get(digest)
            .map(|image| image.signature)
            .ok_or(VmError::Definite)?;
        let entry = BaseEntry {
            base_id,
            deployment_id: self.images.deployment_id.clone(),
            source_ref: self.reference(digest),
            source_digest: digest.clone(),
            tart_version,
            runner_version: runner_version.to_owned(),
            runner_sha256: sha256.clone(),
            build_recipe_version: RECIPE_VERSION,
            state: BaseState::Reserved,
            quarantine: None,
            vm_mac: None,
            vm_file_identity: None,
            final_fingerprint: None,
            signature,
            clones: BTreeMap::new(),
            clone_disks: BTreeMap::new(),
        };
        self.bases
            .update(|record| record.bases.insert(name.to_owned(), entry))
            .map_err(|_| VmError::Definite)?;
        self.set_state(name, |entry| entry.state = BaseState::Cloning)
            .map_err(|_| VmError::Definite)?;
        if let Err(err) = self.tart.clone(&self.reference(digest), name).await {
            let failed = effect_failed(&err);
            let absent = matches!(self.local_names().await, Ok(names) if !names.contains(name));
            if failed == VmError::Definite && absent {
                let _forgotten = self.bases.update(|record| record.bases.remove(name));
            }
            return Err(failed);
        }
        drop(pulled);
        if let Err(err) = self.prove(name).await {
            // Cloned, but not proven ours: preserved and quarantined, never deleted.
            proof::quarantine(&self.bases, name, "clone_unproven");
            return Err(err);
        }
        match self
            .build(base, &layout, digest, runner_version, &sha256)
            .await
        {
            Ok(()) => Ok(()),
            Err(err) => {
                if self.retire(name).await {
                    Err(VmError::Definite)
                } else if matches!(err, VmError::Ambiguous(_)) {
                    Err(err)
                } else {
                    Err(VmError::Ambiguous(Ambiguity::ProcessLost))
                }
            }
        }
    }

    async fn evict(&self, base: &BaseName) -> Result<(), VmError> {
        let name = base.as_str();
        let _held = self.bases.hold(name).await;
        let _admitted = self.bases.admit().await;
        self.settle(name).await?;
        let record = self.bases.read().map_err(|_| VmError::Definite)?;
        let names = self.local_names().await?;
        let Some(entry) = record.bases.get(name) else {
            // Never ours to delete; absent, it is done.
            return if names.contains(name) {
                Err(VmError::Definite)
            } else {
                Ok(())
            };
        };
        // A quarantined base is never deleted, nor its entry dropped, automatically, whatever
        // its identity or presence: only an operator resolves it (CLAUDE.md, ADR 0087
        // decision 6). Recovery has already settled every other state but Ready.
        if entry.state != BaseState::Ready {
            return Err(VmError::Definite);
        }
        if !names.contains(name) {
            self.booter.forget_shim(name);
            return self
                .bases
                .update(|record| record.bases.remove(name))
                .map(drop)
                .map_err(|_| VmError::Definite);
        }
        let info = match self.tart.get(name).await {
            Ok(Some(info)) => info,
            Ok(None) => return Err(VmError::Definite),
            Err(err) => return Err(before_effect(&err)),
        };
        if !identity_holds(entry, &info, self.home(), name) {
            proof::quarantine(&self.bases, name, "identity_changed");
            return Err(VmError::Definite);
        }
        if info.running {
            return Err(VmError::Definite);
        }
        let live: BTreeSet<String> = entry
            .clones
            .keys()
            .filter(|clone| names.contains(*clone))
            .cloned()
            .collect();
        let pruned = self.bases.update(|record| {
            if let Some(entry) = record.bases.get_mut(name) {
                entry.clones.retain(|clone, _| live.contains(clone));
                entry.clone_disks.retain(|clone, _| live.contains(clone));
                if live.is_empty() {
                    entry.state = BaseState::Retiring;
                }
            }
        });
        if !live.is_empty() || pruned.is_err() {
            return Err(VmError::Definite);
        }
        if let Err(err) = self.tart.delete(name).await {
            if !matches!(err, CallError::Refused { .. }) {
                return Err(effect_failed(&err));
            }
            if self.local_names().await?.contains(name) {
                return Err(VmError::Definite);
            }
        }
        self.booter.forget_shim(name);
        // The base is gone; an entry left behind is settled by the next observation.
        let _forgotten = self.bases.update(|record| record.bases.remove(name));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(repository: &str, runners: &[(&str, &str)]) -> ImageConfig {
        ImageConfig {
            repository: repository.to_owned(),
            runners: runners
                .iter()
                .map(|(v, s)| {
                    (
                        (*v).to_owned(),
                        BTreeMap::from([(RunnerPlatform::LinuxArm64, (*s).to_owned())]),
                    )
                })
                .collect(),
            deployment_id: "deployment".to_owned(),
            signature: SignaturePolicy::DigestOnly,
            deadlines: ImageDeadlines {
                pull: Duration::from_secs(1),
                install: Duration::from_secs(1),
                halt: Duration::from_secs(1),
            },
        }
    }

    #[test]
    fn should_round_trip_each_platform_when_it_is_spelled_as_the_release_does() {
        // given
        let named = BTreeMap::from([(
            "2.330.0".to_owned(),
            BTreeMap::from([
                ("linux-arm64".to_owned(), "a".repeat(64)),
                ("osx-arm64".to_owned(), "b".repeat(64)),
                ("windows-x64".to_owned(), "c".repeat(64)),
            ]),
        )]);

        // when
        let pins = RunnerPlatform::pin_table(&named);

        // then
        for platform in [RunnerPlatform::LinuxArm64, RunnerPlatform::OsxArm64] {
            assert_eq!(RunnerPlatform::parse(platform.as_str()), Some(platform));
        }
        assert_eq!(RunnerPlatform::parse("windows-x64"), None);
        assert_eq!(
            [GuestOs::Linux, GuestOs::Macos].map(RunnerPlatform::for_guest),
            [RunnerPlatform::LinuxArm64, RunnerPlatform::OsxArm64]
        );
        assert_eq!(
            pins["2.330.0"].keys().copied().collect::<Vec<_>>(),
            [RunnerPlatform::LinuxArm64, RunnerPlatform::OsxArm64]
        );
    }

    #[test]
    fn should_name_the_guest_only_when_the_config_names_a_known_os() {
        // given
        let configs: [&[u8]; 4] = [
            br#"{"os":"darwin"}"#,
            br#"{"os":"linux"}"#,
            br#"{"os":"plan9"}"#,
            b"not json",
        ];

        // when
        let guests = configs.map(config_guest);

        // then
        assert_eq!(
            guests,
            [Some(GuestOs::Macos), Some(GuestOs::Linux), None, None]
        );
    }

    #[test]
    fn should_refuse_a_repository_when_it_carries_a_tag_digest_or_scheme() {
        // given
        let sha = "ab".repeat(32);
        let good = ["ghcr.io/acme/runner", "localhost:5000/a/b-c.d_e"];
        let bad = [
            "ghcr.io/acme/runner:latest",
            "ghcr.io/acme/runner@sha256:00",
            "https://ghcr.io/acme/runner",
            "ghcr.io",
            "ghcr.io//runner",
            "ghcr.io/../runner",
            "GHCR.io/acme/runner",
            "",
        ];

        // when
        let accepted: Vec<bool> = good
            .iter()
            .chain(&bad)
            .map(|r| config(r, &[("2.330.0", &sha)]).validate().is_ok())
            .collect();
        let bad_runner = config(good[0], &[("2.330", &sha)]).validate();
        let bad_sha = config(good[0], &[("2.330.0", "ab")]).validate();

        // then
        assert_eq!(
            accepted,
            [
                true, true, false, false, false, false, false, false, false, false
            ]
        );
        assert_eq!(bad_runner, Err(ConfigError::Runner("2.330".to_owned())));
        assert_eq!(bad_sha, Err(ConfigError::Runner("2.330.0".to_owned())));
    }

    #[test]
    fn should_refuse_required_signatures_and_an_empty_deployment_when_validating() {
        // given
        let mut required = config("ghcr.io/acme/runner", &[]);
        required.signature = SignaturePolicy::Required;
        let mut optional = config("ghcr.io/acme/runner", &[]);
        optional.signature = SignaturePolicy::Optional;
        let mut anonymous = config("ghcr.io/acme/runner", &[]);
        anonymous.deployment_id = String::new();

        // when
        let judged = [
            required.validate(),
            optional.validate(),
            anonymous.validate(),
        ];

        // then
        assert_eq!(
            judged,
            [
                Err(ConfigError::SignaturesUnsupported),
                Ok(()),
                Err(ConfigError::Deployment)
            ]
        );
    }

    #[test]
    fn should_read_the_linked_digest_when_the_link_stays_in_the_cache() {
        // given
        let dir = std::path::Path::new("/home/t/cache/OCIs/ghcr.io/acme/img");
        let link = dir.join(format!("sha256:{}", "a".repeat(64)));
        let other = format!("sha256:{}", "b".repeat(64));

        // when
        let judged = [
            linked_digest(&link, &dir.join(&other)),
            linked_digest(&link, std::path::Path::new(&other)),
            linked_digest(&link, std::path::Path::new(&format!("../img/{other}"))),
            linked_digest(&link, &std::path::Path::new("/elsewhere").join(&other)),
            linked_digest(&link, std::path::Path::new(&format!("../other/{other}"))),
            linked_digest(&link, &dir.join("not-a-digest")),
        ]
        .map(|d| d.map(|d| d.to_string()));

        // then
        assert_eq!(
            judged,
            [
                Some(other.clone()),
                Some(other.clone()),
                None,
                None,
                None,
                None
            ]
        );
    }

    #[test]
    fn should_name_the_base_by_digest_and_runner_version_when_the_version_is_canonical() {
        // given
        let digest =
            ImageDigest::new(&format!("sha256:0123456789ab{}", "f".repeat(52))).expect("a digest");

        // when
        let names = [
            "2.330.0",
            "10.0.12",
            "2.330",
            "2.330.0.1",
            "02.330.0",
            "x.1.2",
        ]
        .map(|v| base_name(&digest, v).map(|b| b.to_string()));

        // then
        assert_eq!(
            names,
            [
                Some("mfb-0123456789ab-r2-330-0".to_owned()),
                Some("mfb-0123456789ab-r10-0-12".to_owned()),
                None,
                None,
                None,
                None
            ]
        );
    }
}
