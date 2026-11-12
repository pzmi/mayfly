//! The `ImageStore` contract suite (ver §5, domain-model §9.3, ADR 0039, ADR 0087): one set of
//! cases every image store passes, written once over an [`ImageWorld`] and run from each
//! implementation's crate: the in-memory [`MemoryImages`] here ([`MemoryWorld`]), the Tart
//! adapter over `mayfly-fake-tart` (`mayfly-adapter-tart/tests/tart/images.rs`), and the Tart
//! adapter over real Tart in the hardware lane (HW-01).
//!
//! Cases are named `IC-01` … ([`ImageCase`]). A world is a store plus what a case needs around
//! it: a registry to publish to (content under its own digest, or drifted content under a digest
//! it does not hash to), the references the store fetched, a slow next fetch, the runners
//! installed into a base, local VMs read independently of the store (the oracle), a foreign VM,
//! a clone through the deployment's provider (the reference an eviction must see), a VM removed
//! or given a new instance id behind the store's back, and a restart over the same host.
//!
//! **No case relies on time.** A slow fetch is the world's to make (a delay fault, a yield); the
//! suite only bounds a whole case ([`CASE_BOUND`]). [`check_image_store_contract`] returns a
//! report of failed cases: the suite's own tests show the broken stores of ver §5 failing exactly
//! the cases that exist to catch them (`TagPullStore`: IC-01, IC-02; `BaseEvictingStore`: IC-10;
//! `NameEvictingStore`: IC-11, IC-12; `GuessingSizeStore` and
//! `TrustingMeasureStore`: IC-15; `tests/image_contract.rs`). A world that cannot build what
//! a case needs fails it.

use std::collections::BTreeSet;
use std::fmt::{self, Debug, Display};
use std::future::Future;
use std::time::Duration;

use mayfly_data::{ImageStore, VmError};
use mayfly_domain::{ImageDigest, OwnedVmName};
use mayfly_protocol::{BaseName, GuestOs};
use mayfly_testkit_shared::poll::within;

use crate::images::{MemoryImages, REPOSITORY, RUNNER, base_for, digest_of, disk_bytes};
use crate::provider_contract::vm;

/// The longest a whole case may take before it is reported as hung.
pub const CASE_BOUND: Duration = Duration::from_secs(120);

/// A store and what a case needs around it.
pub trait ImageWorld {
    /// The store under test.
    type Store: ImageStore;

    /// The store as it is now.
    fn store(&self) -> &Self::Store;

    /// Replaces the store (and the provider beside it) with new ones over the same host and
    /// record, nothing remembered in memory, as a restarted daemon builds them.
    fn restart(&mut self);

    /// The repository the store pulls from.
    fn repository(&self) -> String;

    /// The runner version the store has a pinned checksum for.
    fn runner(&self) -> String;

    /// Publishes an image of `guest` whose content is `content` at `<repository>@<its digest>`
    /// and at `<repository>:latest`; returns the digest.
    fn publish(&self, content: &[u8], guest: GuestOs) -> ImageDigest;

    /// The disk an image published from `content` reconstructs to: what its manifest's disk
    /// layers sum to.
    fn disk_bytes(&self, content: &[u8]) -> u64;

    /// Publishes `content` at `<repository>@<digest>` and `:latest` where `digest` is the digest
    /// of `claimed`, not of `content`: drift (domain-model §9.3); returns that digest.
    fn publish_drifted(&self, content: &[u8], claimed: &[u8]) -> ImageDigest;

    /// Every reference the store fetched from the registry, in order.
    fn fetched(&self) -> Vec<String>;

    /// The next fetch is slow enough for a concurrent pull of the same digest to overlap it.
    fn slow_next_fetch(&self);

    /// The runner versions installed into `base`, one per install.
    fn installs(&self, base: &BaseName) -> Vec<String>;

    /// Whether a local VM `name` exists, and whether it runs: the oracle.
    fn exists(&self, name: &str) -> Option<bool>;

    /// Makes a stopped local VM `name` the store did not make.
    fn seed_foreign(&self, name: &str);

    /// Clones `vm` from `base` through the deployment's provider.
    fn clone_from(
        &self,
        base: &BaseName,
        vm: &OwnedVmName,
    ) -> impl Future<Output = Result<(), VmError>>;

    /// Removes the local VM `name` behind the store's back.
    fn remove(&self, name: &str);

    /// Gives the local VM `name` a new instance id behind the store's back.
    fn change_identity(&self, name: &str);
}

/// One case of the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ImageCase {
    /// IC-01: a pull fetches exactly `<repository>@<digest>`, never a tag, and the listing then
    /// names the digest.
    PullByDigest,
    /// IC-02: content that does not hash to its digest is refused `Definite` and not listed.
    PullRefusesDrift,
    /// IC-03: a digest the registry does not hold is refused `Definite` and not listed.
    PullUnknownRefused,
    /// IC-04: two concurrent pulls of one digest fetch it once (single flight per host), both
    /// succeed, and a later pull of a pulled image fetches nothing.
    PullSingleFlight,
    /// IC-05: a base named by digest and runner version is materialised from a pulled image with
    /// the runner installed once, left stopped and listed; materialising it again is done and
    /// installs nothing.
    MaterialiseBase,
    /// IC-06: a base name other than the digest's and runner's, or a runner with no pinned
    /// checksum, is refused `Definite` and creates nothing.
    MaterialiseRefusesMisnamed,
    /// IC-07: an image that is not pulled is refused `Definite`: nothing is created and nothing
    /// is fetched (never a pull at materialisation).
    MaterialiseNeedsPulledImage,
    /// IC-08: a local VM that already has the base's name and that the store did not make is
    /// refused `Definite`, left as it is, installs nothing and is not listed as a base.
    MaterialiseRefusesForeignName,
    /// IC-09: an unreferenced base is evicted and no longer listed; evicting it again is done
    /// (ensure absent).
    EvictUnreferenced,
    /// IC-10: a base a live clone references is refused `Definite` and stays; once the clone is
    /// gone it is evicted.
    EvictRefusesReferenced,
    /// IC-11: an `mfb-*` VM the store did not make is not listed as a base, and its eviction is
    /// refused `Definite` and leaves it.
    EvictRefusesForeign,
    /// IC-12: a base whose instance id is no longer the recorded one is not listed, and its
    /// eviction is refused `Definite` and leaves it (deletion refused by identity).
    EvictRefusesIdentityMismatch,
    /// IC-13: after a restart the store still lists the base it made, the provider clones from
    /// it, and it evicts it once unreferenced (the record of owned bases is durable).
    RestartKeepsOwnership,
    /// IC-14: a Linux image materialises a base listed as a Linux guest, beside a macOS one, so
    /// the ledger counts it outside the macOS-guest limit (plan §1 G3).
    LinuxBaseListedAsLinux,
    /// IC-15: an image's disk is measured from its manifest fetched by digest, before any pull
    /// and without one; a manifest that does not hash to its digest, or a digest the registry
    /// does not hold, is refused `Definite`; once pulled and materialised, the listing names the
    /// same disk for the image and its base (ADR 0052's amendment of 2026-11-12).
    MeasureFromManifest,
}

impl ImageCase {
    /// Every case, in id order.
    pub const ALL: [Self; 15] = [
        Self::PullByDigest,
        Self::PullRefusesDrift,
        Self::PullUnknownRefused,
        Self::PullSingleFlight,
        Self::MaterialiseBase,
        Self::MaterialiseRefusesMisnamed,
        Self::MaterialiseNeedsPulledImage,
        Self::MaterialiseRefusesForeignName,
        Self::EvictUnreferenced,
        Self::EvictRefusesReferenced,
        Self::EvictRefusesForeign,
        Self::EvictRefusesIdentityMismatch,
        Self::RestartKeepsOwnership,
        Self::LinuxBaseListedAsLinux,
        Self::MeasureFromManifest,
    ];

    /// The case's id, `IC-01` …
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::PullByDigest => "IC-01",
            Self::PullRefusesDrift => "IC-02",
            Self::PullUnknownRefused => "IC-03",
            Self::PullSingleFlight => "IC-04",
            Self::MaterialiseBase => "IC-05",
            Self::MaterialiseRefusesMisnamed => "IC-06",
            Self::MaterialiseNeedsPulledImage => "IC-07",
            Self::MaterialiseRefusesForeignName => "IC-08",
            Self::EvictUnreferenced => "IC-09",
            Self::EvictRefusesReferenced => "IC-10",
            Self::EvictRefusesForeign => "IC-11",
            Self::EvictRefusesIdentityMismatch => "IC-12",
            Self::RestartKeepsOwnership => "IC-13",
            Self::LinuxBaseListedAsLinux => "IC-14",
            Self::MeasureFromManifest => "IC-15",
        }
    }
}

impl Display for ImageCase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {self:?}", self.id())
    }
}

/// A failed case and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseFailure {
    /// The case.
    pub case: ImageCase,
    /// What was expected and what the store did, at the first check that failed.
    pub detail: String,
}

/// The failed cases of one run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageContractReport {
    /// Each failed case, in id order.
    pub failures: Vec<CaseFailure>,
}

impl ImageContractReport {
    /// Whether every case passed.
    #[must_use]
    pub fn is_green(&self) -> bool {
        self.failures.is_empty()
    }

    /// The failed cases.
    #[must_use]
    pub fn failed_cases(&self) -> BTreeSet<ImageCase> {
        self.failures.iter().map(|f| f.case).collect()
    }
}

impl Display for ImageContractReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "ImageStore contract: {} case(s) failed",
            self.failures.len()
        )?;
        for failure in &self.failures {
            writeln!(f, "- {}: {}", failure.case, failure.detail)?;
        }
        Ok(())
    }
}

/// A case's outcome.
pub type Check = Result<(), String>;

/// Runs every case, each in a fresh world from `make` on its own runtime.
pub fn check_image_store_contract<W: ImageWorld>(make: &dyn Fn() -> W) -> ImageContractReport {
    let mut report = ImageContractReport::default();
    for case in ImageCase::ALL {
        if let Err(detail) = check_case(make, case) {
            report.failures.push(CaseFailure { case, detail });
        }
    }
    report
}

/// Runs every case and panics with the report unless all pass.
///
/// # Panics
/// When a case fails; the message lists each failed case and why.
pub fn run_image_store_contract<W: ImageWorld>(make: &dyn Fn() -> W) {
    let report = check_image_store_contract(make);
    assert!(report.is_green(), "{report}");
}

/// Runs one case in a fresh world on a fresh current-thread runtime with a real clock.
///
/// # Errors
/// What was expected and what the store did, at the first check that failed.
pub fn check_case<W: ImageWorld>(make: &dyn Fn() -> W, case: ImageCase) -> Check {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("the case's runtime did not build: {e}"))?;
    let mut world = make();
    runtime.block_on(async {
        within(
            CASE_BOUND,
            "the case: a store future never completed",
            Box::pin(run_case(&mut world, case)),
        )
        .await
        .unwrap_or_else(|timed_out| Err(timed_out.to_string()))
    })
}

async fn run_case<W: ImageWorld>(w: &mut W, case: ImageCase) -> Check {
    match case {
        ImageCase::PullByDigest => pull_by_digest(w).await,
        ImageCase::PullRefusesDrift => pull_refuses_drift(w).await,
        ImageCase::PullUnknownRefused => pull_unknown_refused(w).await,
        ImageCase::PullSingleFlight => pull_single_flight(w).await,
        ImageCase::MaterialiseBase => materialise_base(w).await,
        ImageCase::MaterialiseRefusesMisnamed => materialise_refuses_misnamed(w).await,
        ImageCase::MaterialiseNeedsPulledImage => materialise_needs_pulled_image(w).await,
        ImageCase::MaterialiseRefusesForeignName => materialise_refuses_foreign_name(w).await,
        ImageCase::EvictUnreferenced => evict_unreferenced(w).await,
        ImageCase::EvictRefusesReferenced => evict_refuses_referenced(w).await,
        ImageCase::EvictRefusesForeign => evict_refuses_foreign(w).await,
        ImageCase::EvictRefusesIdentityMismatch => evict_refuses_identity_mismatch(w).await,
        ImageCase::RestartKeepsOwnership => restart_keeps_ownership(w).await,
        ImageCase::LinuxBaseListedAsLinux => linux_base_listed_as_linux(w).await,
        ImageCase::MeasureFromManifest => measure_from_manifest(w).await,
    }
}

// ---------------------------------------------------------------------------------------------
// Checks

fn eq<T: PartialEq + Debug>(what: &str, got: &T, want: &T) -> Check {
    if got == want {
        Ok(())
    } else {
        Err(format!("{what}: got {got:?}, want {want:?}"))
    }
}

fn ensure(what: &str, holds: bool) -> Check {
    if holds { Ok(()) } else { Err(what.to_owned()) }
}

async fn step<T: Debug, F: Future<Output = Result<T, VmError>>>(
    what: &str,
    call: F,
) -> Result<T, String> {
    call.await
        .map_err(|err| format!("{what}: got {err:?}, want success"))
}

/// The image every materialising case uses, published and pulled.
async fn pulled<W: ImageWorld>(w: &W) -> Result<ImageDigest, String> {
    let digest = w.publish(b"{\"image\":\"contract\"}", GuestOs::Macos);
    step("pull", w.store().pull(&digest)).await?;
    Ok(digest)
}

/// The base of `digest` with the world's runner.
fn base_of<W: ImageWorld>(w: &W, digest: &ImageDigest) -> Result<BaseName, String> {
    base_for(digest, &w.runner()).ok_or_else(|| "the runner version is not X.Y.Z".to_owned())
}

/// Pulls and materialises the contract image's base.
async fn materialised<W: ImageWorld>(w: &W) -> Result<BaseName, String> {
    let digest = pulled(w).await?;
    let base = base_of(w, &digest)?;
    step(
        "materialise",
        w.store().materialise(&digest, &base, &w.runner()),
    )
    .await?;
    Ok(base)
}

async fn listed_bases<W: ImageWorld>(w: &W) -> Result<BTreeSet<BaseName>, String> {
    let listing = step("list", w.store().list()).await?;
    Ok(listing.bases.into_keys().collect())
}

// ---------------------------------------------------------------------------------------------
// Cases

async fn pull_by_digest<W: ImageWorld>(w: &mut W) -> Check {
    let digest = w.publish(b"{\"image\":1}", GuestOs::Macos);
    step("pull", w.store().pull(&digest)).await?;
    eq(
        "fetched references",
        &w.fetched(),
        &vec![format!("{}@{digest}", w.repository())],
    )?;
    let listing = step("list", w.store().list()).await?;
    ensure(
        "the pulled digest is not listed",
        listing.images.contains(&digest),
    )
}

async fn pull_refuses_drift<W: ImageWorld>(w: &mut W) -> Check {
    let digest = w.publish_drifted(b"{\"image\":\"served\"}", b"{\"image\":\"claimed\"}");
    let pulled = w.store().pull(&digest).await;
    eq("pull of drifted content", &pulled, &Err(VmError::Definite))?;
    let listing = step("list", w.store().list()).await?;
    ensure(
        "the drifted digest is listed",
        !listing.images.contains(&digest),
    )
}

async fn pull_unknown_refused<W: ImageWorld>(w: &mut W) -> Check {
    let digest = digest_of(b"{\"image\":\"never published\"}");
    let pulled = w.store().pull(&digest).await;
    eq(
        "pull of an unknown digest",
        &pulled,
        &Err(VmError::Definite),
    )?;
    let listing = step("list", w.store().list()).await?;
    ensure(
        "the unknown digest is listed",
        !listing.images.contains(&digest),
    )
}

async fn pull_single_flight<W: ImageWorld>(w: &mut W) -> Check {
    let digest = w.publish(b"{\"image\":4}", GuestOs::Macos);
    w.slow_next_fetch();
    let (a, b) = tokio::join!(w.store().pull(&digest), w.store().pull(&digest));
    eq("concurrent pulls", &(a, b), &(Ok(()), Ok(())))?;
    eq("fetches by the concurrent pulls", &w.fetched().len(), &1)?;
    step("a later pull", w.store().pull(&digest)).await?;
    eq("fetches after a later pull", &w.fetched().len(), &1)
}

async fn materialise_base<W: ImageWorld>(w: &mut W) -> Check {
    let digest = pulled(w).await?;
    let base = base_of(w, &digest)?;
    step(
        "materialise",
        w.store().materialise(&digest, &base, &w.runner()),
    )
    .await?;
    eq("installed runners", &w.installs(&base), &vec![w.runner()])?;
    eq(
        "the base (exists, running)",
        &w.exists(base.as_str()),
        &Some(false),
    )?;
    ensure(
        "the base is not listed",
        listed_bases(w).await?.contains(&base),
    )?;
    step(
        "materialise again",
        w.store().materialise(&digest, &base, &w.runner()),
    )
    .await?;
    eq(
        "installed runners after a repeat",
        &w.installs(&base),
        &vec![w.runner()],
    )
}

async fn materialise_refuses_misnamed<W: ImageWorld>(w: &mut W) -> Check {
    let digest = pulled(w).await?;
    let other = base_for(&digest_of(b"another image"), &w.runner())
        .ok_or_else(|| "the runner version is not X.Y.Z".to_owned())?;
    let unpinned = "9.9.9";
    let unpinned_base = base_for(&digest, unpinned).ok_or("a valid base")?;
    let misnamed = w.store().materialise(&digest, &other, &w.runner()).await;
    eq(
        "a base named for another digest",
        &misnamed,
        &Err(VmError::Definite),
    )?;
    let unpinned_run = w
        .store()
        .materialise(&digest, &unpinned_base, unpinned)
        .await;
    eq(
        "a runner without a pinned checksum",
        &unpinned_run,
        &Err(VmError::Definite),
    )?;
    eq("the misnamed base", &w.exists(other.as_str()), &None)?;
    eq(
        "the unpinned base",
        &w.exists(unpinned_base.as_str()),
        &None,
    )
}

async fn materialise_needs_pulled_image<W: ImageWorld>(w: &mut W) -> Check {
    let digest = w.publish(b"{\"image\":7}", GuestOs::Macos);
    let base = base_of(w, &digest)?;
    let made = w.store().materialise(&digest, &base, &w.runner()).await;
    eq(
        "materialise of an image not pulled",
        &made,
        &Err(VmError::Definite),
    )?;
    eq("the base", &w.exists(base.as_str()), &None)?;
    eq("fetched references", &w.fetched(), &Vec::<String>::new())
}

async fn materialise_refuses_foreign_name<W: ImageWorld>(w: &mut W) -> Check {
    let digest = pulled(w).await?;
    let base = base_of(w, &digest)?;
    w.seed_foreign(base.as_str());
    let made = w.store().materialise(&digest, &base, &w.runner()).await;
    eq(
        "materialise onto a foreign VM's name",
        &made,
        &Err(VmError::Definite),
    )?;
    eq(
        "the foreign VM (exists, running)",
        &w.exists(base.as_str()),
        &Some(false),
    )?;
    eq("installed runners", &w.installs(&base), &Vec::new())?;
    ensure(
        "the foreign VM is listed as a base",
        !listed_bases(w).await?.contains(&base),
    )
}

async fn evict_unreferenced<W: ImageWorld>(w: &mut W) -> Check {
    let base = materialised(w).await?;
    step("evict", w.store().evict(&base)).await?;
    eq("the evicted base", &w.exists(base.as_str()), &None)?;
    ensure(
        "the evicted base is listed",
        !listed_bases(w).await?.contains(&base),
    )?;
    step("evict again", w.store().evict(&base)).await
}

async fn evict_refuses_referenced<W: ImageWorld>(w: &mut W) -> Check {
    let base = materialised(w).await?;
    step("clone", w.clone_from(&base, &vm(1))).await?;
    let evicted = w.store().evict(&base).await;
    eq(
        "evict of a referenced base",
        &evicted,
        &Err(VmError::Definite),
    )?;
    eq(
        "the referenced base (exists, running)",
        &w.exists(base.as_str()),
        &Some(false),
    )?;
    w.remove(&vm(1).to_string());
    step("evict once unreferenced", w.store().evict(&base)).await?;
    eq("the evicted base", &w.exists(base.as_str()), &None)
}

async fn evict_refuses_foreign<W: ImageWorld>(w: &mut W) -> Check {
    let stray = BaseName::new("mfb-stray").map_err(|_| "a valid base name".to_owned())?;
    w.seed_foreign(stray.as_str());
    ensure(
        "a foreign mfb VM is listed as a base",
        !listed_bases(w).await?.contains(&stray),
    )?;
    let evicted = w.store().evict(&stray).await;
    eq("evict of a foreign base", &evicted, &Err(VmError::Definite))?;
    eq(
        "the foreign VM (exists, running)",
        &w.exists(stray.as_str()),
        &Some(false),
    )
}

async fn evict_refuses_identity_mismatch<W: ImageWorld>(w: &mut W) -> Check {
    let base = materialised(w).await?;
    w.change_identity(base.as_str());
    ensure(
        "a base with another instance id is listed",
        !listed_bases(w).await?.contains(&base),
    )?;
    let evicted = w.store().evict(&base).await;
    eq(
        "evict of a base with another instance id",
        &evicted,
        &Err(VmError::Definite),
    )?;
    eq(
        "the base (exists, running)",
        &w.exists(base.as_str()),
        &Some(false),
    )
}

async fn linux_base_listed_as_linux<W: ImageWorld>(w: &mut W) -> Check {
    let mac = materialised(w).await?;
    let digest = w.publish(b"{\"image\":\"linux\"}", GuestOs::Linux);
    step("pull of the Linux image", w.store().pull(&digest)).await?;
    let linux = base_of(w, &digest)?;
    step(
        "materialise the Linux base",
        w.store().materialise(&digest, &linux, &w.runner()),
    )
    .await?;
    let listing = step("list", w.store().list()).await?;
    eq(
        "listed guests",
        &(listing.bases.get(&mac), listing.bases.get(&linux)),
        &(Some(&GuestOs::Macos), Some(&GuestOs::Linux)),
    )
}

async fn measure_from_manifest<W: ImageWorld>(w: &mut W) -> Check {
    let content = b"{\"image\":\"measured\"}";
    let digest = w.publish(content, GuestOs::Macos);
    let want = w.disk_bytes(content);
    let measured = step("measure", w.store().measure(&digest)).await?;
    eq("the measured disk", &measured, &want)?;
    eq("pulls while measuring", &w.fetched(), &Vec::new())?;
    let listing = step("list", w.store().list()).await?;
    ensure(
        "a measured image is listed without a pull",
        !listing.images.contains(&digest),
    )?;
    let drifted = w.publish_drifted(b"{\"image\":\"served\"}", b"{\"image\":\"claimed\"}");
    eq(
        "measuring a drifted manifest",
        &w.store().measure(&drifted).await,
        &Err(VmError::Definite),
    )?;
    let unknown = digest_of(b"never published");
    eq(
        "measuring a digest the registry does not hold",
        &w.store().measure(&unknown).await,
        &Err(VmError::Definite),
    )?;
    step("pull", w.store().pull(&digest)).await?;
    let base = base_of(w, &digest)?;
    step(
        "materialise",
        w.store().materialise(&digest, &base, &w.runner()),
    )
    .await?;
    let listing = step("list", w.store().list()).await?;
    eq(
        "listed disks of the image and its base",
        &(
            listing.disks.images.get(&digest),
            listing.disks.bases.get(&base),
        ),
        &(Some(&want), Some(&want)),
    )
}

async fn restart_keeps_ownership<W: ImageWorld>(w: &mut W) -> Check {
    let base = materialised(w).await?;
    w.restart();
    ensure(
        "the base is not listed after a restart",
        listed_bases(w).await?.contains(&base),
    )?;
    step("clone after a restart", w.clone_from(&base, &vm(2))).await?;
    w.remove(&vm(2).to_string());
    step("evict after a restart", w.store().evict(&base)).await?;
    eq("the evicted base", &w.exists(base.as_str()), &None)
}

// ---------------------------------------------------------------------------------------------
// The in-memory world

/// The contract's world over [`MemoryImages`], or a store wrapping it (the deliberately broken
/// stores).
pub struct MemoryWorld<S = MemoryImages> {
    images: MemoryImages,
    store: S,
    wrap: fn(MemoryImages) -> S,
}

impl MemoryWorld {
    /// The plain in-memory store.
    #[must_use]
    pub fn new() -> Self {
        Self::wrapped(|images| images)
    }
}

impl Default for MemoryWorld {
    fn default() -> Self {
        Self::new()
    }
}

impl<S> MemoryWorld<S> {
    /// The in-memory store behind `wrap`.
    #[must_use]
    pub fn wrapped(wrap: fn(MemoryImages) -> S) -> Self {
        let images = MemoryImages::new();
        Self {
            store: wrap(images.clone()),
            images,
            wrap,
        }
    }
}

impl<S: ImageStore> ImageWorld for MemoryWorld<S> {
    type Store = S;

    fn store(&self) -> &S {
        &self.store
    }

    fn restart(&mut self) {
        self.images = self.images.reopened();
        self.store = (self.wrap)(self.images.clone());
    }

    fn repository(&self) -> String {
        REPOSITORY.to_owned()
    }

    fn runner(&self) -> String {
        RUNNER.to_owned()
    }

    fn publish(&self, content: &[u8], guest: GuestOs) -> ImageDigest {
        let digest = digest_of(content);
        for reference in [
            format!("{REPOSITORY}@{digest}"),
            format!("{REPOSITORY}:latest"),
        ] {
            self.images.serve(&reference, content, guest);
        }
        digest
    }

    fn disk_bytes(&self, content: &[u8]) -> u64 {
        disk_bytes(content)
    }

    fn publish_drifted(&self, content: &[u8], claimed: &[u8]) -> ImageDigest {
        let digest = digest_of(claimed);
        for reference in [
            format!("{REPOSITORY}@{digest}"),
            format!("{REPOSITORY}:latest"),
        ] {
            self.images.serve(&reference, content, GuestOs::Macos);
        }
        digest
    }

    fn fetched(&self) -> Vec<String> {
        self.images.fetched()
    }

    fn slow_next_fetch(&self) {
        self.images.slow_next_fetch();
    }

    fn installs(&self, base: &BaseName) -> Vec<String> {
        self.images.installs(base.as_str())
    }

    fn exists(&self, name: &str) -> Option<bool> {
        self.images.exists(name)
    }

    fn seed_foreign(&self, name: &str) {
        self.images.seed_foreign(name);
    }

    async fn clone_from(&self, base: &BaseName, vm: &OwnedVmName) -> Result<(), VmError> {
        self.images.clone_from(base, vm)
    }

    fn remove(&self, name: &str) {
        self.images.remove(name);
    }

    fn change_identity(&self, name: &str) {
        self.images.change_identity(name);
    }
}
