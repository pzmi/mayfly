//! The `ImageStore` contract (`mayfly_testkit_data::image_contract`) run against
//! `TartImageStore` over the fake `tart` binary, one test per case, and the fault cases of ADR
//! 0087: wrong and re-serialized manifests, corrupt layers and chunks, unsupported layouts,
//! foreign and look-alike VMs, a crash at each build state, a base modified after it was
//! verified, concurrent clone and evict, incomplete listings, unrecorded clone lineage, an
//! unsafe directory layout and a lost or corrupt registry.
//!
//! The world publishes synthetic Tart images to the fake's registry (`TART_HOME/registry/`:
//! manifests and blobs) and reads raw manifests from it as the store's `ManifestSource`; it
//! reads what was fetched, installed and booted from the fake's invocation log.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use mayfly_adapter_tart::bases::{BaseEntry, BaseState};
use mayfly_adapter_tart::guest::LAUNCHER;
use mayfly_adapter_tart::images::base_name;
use mayfly_adapter_tart::verify::{
    self, CONFIG_LAYER, DISK_V2_LAYER, MANIFEST_V1, NVRAM_LAYER, UNCOMPRESSED_DIGEST,
    UNCOMPRESSED_SIZE, UnsafeLayout, sha256_hex,
};
use mayfly_adapter_tart::{
    ConfigError, FetchError, ImageConfig, ImageDeadlines, ManifestSource, TartImageStore,
    TartProvider,
};
use mayfly_data::{BaseName, GuestOs, ImageStore, VmError, VmProvider};
use mayfly_domain::{ImageDigest, OwnedVmName};
use mayfly_testkit_data::image_contract::{ImageCase, ImageWorld, check_case};
use mayfly_testkit_data::images::{REPOSITORY, RUNNER, RUNNER_SHA256};
use mayfly_testkit_data::provider_contract::vm;
use serde_json::{Value, json};

use crate::support::Fake;

fn image_config() -> ImageConfig {
    ImageConfig {
        repository: REPOSITORY.to_owned(),
        runners: BTreeMap::from([(RUNNER.to_owned(), RUNNER_SHA256.to_owned())]),
        deployment_id: "deployment-under-test".to_owned(),
        signature: mayfly_adapter_tart::bases::SignaturePolicy::DigestOnly,
        deadlines: ImageDeadlines {
            pull: Duration::from_secs(5),
            install: Duration::from_secs(5),
            halt: Duration::from_secs(2),
        },
    }
}

/// The name the fake keeps a registry entry or blob under (its `home::encode_name`).
fn encoded(reference: &str) -> String {
    reference
        .bytes()
        .enumerate()
        .map(|(i, b)| {
            if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || (b == b'.' && i > 0) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn argv(record: &Value) -> Vec<String> {
    record["argv"]
        .as_array()
        .map(|args| {
            args.iter()
                .filter_map(|a| a.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn digest_of(bytes: &[u8]) -> ImageDigest {
    ImageDigest::new(&format!("sha256:{}", sha256_hex(bytes))).expect("a digest")
}

/// The raw manifests of the fake's registry, as the registry serves them.
pub struct RegistryDir {
    home: PathBuf,
}

impl ManifestSource for RegistryDir {
    async fn fetch(&self, repository: &str, digest: &ImageDigest) -> Result<Vec<u8>, FetchError> {
        // `honest/` holds what the registry served the store when Tart is later served another.
        let name = encoded(&format!("{repository}@{digest}"));
        std::fs::read(self.home.join("honest").join(&name))
            .or_else(|_| std::fs::read(self.home.join("registry").join(&name)))
            .map_err(|_| FetchError::NotFound)
    }
}

/// Which part of a published image the registry serves damaged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Damage {
    None,
    Config,
    Nvram,
    Chunk,
    DiskV1,
}

type Store = TartImageStore<RegistryDir>;

/// The contract's world over the fake binary: a store and a provider sharing one registry.
pub struct ImageTartWorld {
    fake: Fake,
    store: Store,
    provider: TartProvider,
}

impl ImageTartWorld {
    pub fn new() -> Self {
        let fake = Fake::new();
        let (store, provider) = Self::build(&fake);
        Self {
            fake,
            store,
            provider,
        }
    }

    fn build(fake: &Fake) -> (Store, TartProvider) {
        let records = fake.records();
        let source = RegistryDir { home: fake.home() };
        let store = TartImageStore::new(fake.config(), image_config(), records.clone(), source)
            .expect("a valid image configuration");
        let provider = TartProvider::new(fake.config(), records);
        (store, provider)
    }

    fn registry(&self) -> PathBuf {
        self.fake.home().join("registry")
    }

    /// Publishes a synthetic Tart image of `guest` derived from `content`: a config, an NVRAM
    /// and two disk chunks, the registry serving the part `damage` names altered. Returns the
    /// manifest's digest; the manifest is served at it and at `:latest`, or at `serve_at`.
    fn publish_image(
        &self,
        content: &[u8],
        guest: GuestOs,
        damage: Damage,
        serve_at: Option<&ImageDigest>,
    ) -> ImageDigest {
        let os = match guest {
            GuestOs::Macos => "darwin",
            GuestOs::Linux => "linux",
        };
        let config = serde_json::to_vec(&json!({
            "version": 1, "os": os, "arch": "arm64",
            "macAddress": "0e:aa:00:00:00:01", "image": sha256_hex(content),
        }))
        .expect("config");
        let nvram = [b"nvram:".as_slice(), content].concat();
        let chunks = [
            [b"chunk-1:".as_slice(), content].concat(),
            [b"chunk-2:".as_slice(), content].concat(),
        ];
        std::fs::create_dir_all(self.registry().join("blobs")).expect("registry");
        let blob = |digest: &str, bytes: &[u8]| {
            std::fs::write(self.registry().join("blobs").join(encoded(digest)), bytes)
                .expect("blob");
        };
        let served = |part: Damage, bytes: &[u8]| {
            if damage == part {
                [bytes, b"!"].concat()
            } else {
                bytes.to_vec()
            }
        };
        let mut layers = Vec::new();
        for (media, bytes, part) in [
            (CONFIG_LAYER, &config, Damage::Config),
            (NVRAM_LAYER, &nvram, Damage::Nvram),
        ] {
            let digest = format!("sha256:{}", sha256_hex(bytes));
            blob(&digest, &served(part, bytes));
            layers.push(json!({"mediaType": media, "digest": digest, "size": bytes.len()}));
        }
        let disk_media = if damage == Damage::DiskV1 {
            "application/vnd.cirruslabs.tart.disk.v1"
        } else {
            DISK_V2_LAYER
        };
        for chunk in &chunks {
            // The layer digest stands for the compressed blob; the fake stores chunks
            // uncompressed, so only the uncompressed digest can match what it serves.
            let compressed = format!(
                "sha256:{}",
                sha256_hex(&[b"lz4:", chunk.as_slice()].concat())
            );
            blob(&compressed, &served(Damage::Chunk, chunk));
            layers.push(json!({
                "mediaType": disk_media, "digest": compressed, "size": chunk.len() + 4,
                "annotations": {
                    UNCOMPRESSED_SIZE: chunk.len().to_string(),
                    UNCOMPRESSED_DIGEST: format!("sha256:{}", sha256_hex(chunk)),
                },
            }));
        }
        let manifest = serde_json::to_vec(&json!({
            "schemaVersion": 2, "mediaType": MANIFEST_V1,
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                       "digest": format!("sha256:{}", "0".repeat(64)), "size": 0},
            "layers": layers,
        }))
        .expect("manifest");
        let digest = serve_at.cloned().unwrap_or_else(|| digest_of(&manifest));
        for reference in [
            format!("{REPOSITORY}@{digest}"),
            format!("{REPOSITORY}:latest"),
        ] {
            std::fs::write(self.registry().join(encoded(&reference)), &manifest).expect("serve");
        }
        digest
    }

    fn invocations(&self) -> Vec<Vec<String>> {
        self.fake.events("invoke").iter().map(argv).collect()
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
    }

    /// Publishes, pulls and materialises an image; its base.
    fn ready_base(&self, runtime: &tokio::runtime::Runtime, content: &[u8]) -> BaseName {
        let digest = self.publish_image(content, GuestOs::Macos, Damage::None, None);
        let base = base_name(&digest, RUNNER).expect("a base name");
        runtime.block_on(async {
            self.store.pull(&digest).await.expect("pull");
            self.store
                .materialise(&digest, &base, RUNNER)
                .await
                .expect("materialise");
        });
        base
    }

    fn entry(&self, name: &str) -> Option<BaseEntry> {
        self.fake
            .records()
            .read()
            .expect("a readable registry")
            .bases
            .get(name)
            .cloned()
    }

    /// Rewrites the registry entry of `name`, as a crash at another state would have left it.
    fn crashed_at(&self, name: &str, change: impl FnOnce(&mut BaseEntry)) {
        self.fake
            .records()
            .update(|record| record.bases.get_mut(name).map(change))
            .expect("a writable registry")
            .expect("an entry");
    }

    fn pulls(&self) -> usize {
        self.fetched().len()
    }

    fn fail_listings(&self) {
        self.fake.faults(
            "[[rule]]\nid = \"no-list\"\ncommand = \"list\"\naction = \"fail_before_effect\"\n",
        );
    }
}

impl ImageWorld for ImageTartWorld {
    type Store = Store;

    fn store(&self) -> &Store {
        &self.store
    }

    fn restart(&mut self) {
        (self.store, self.provider) = Self::build(&self.fake);
    }

    fn repository(&self) -> String {
        REPOSITORY.to_owned()
    }

    fn runner(&self) -> String {
        RUNNER.to_owned()
    }

    fn publish(&self, content: &[u8], guest: GuestOs) -> ImageDigest {
        self.publish_image(content, guest, Damage::None, None)
    }

    fn publish_drifted(&self, content: &[u8], claimed: &[u8]) -> ImageDigest {
        // The registry serves `content`'s manifest at the digest of `claimed`'s.
        let claimed_digest = self.publish_image(claimed, GuestOs::Macos, Damage::None, None);
        self.publish_image(content, GuestOs::Macos, Damage::None, Some(&claimed_digest))
    }

    fn fetched(&self) -> Vec<String> {
        self.invocations()
            .into_iter()
            .filter(|args| args.first().map(String::as_str) == Some("pull"))
            .filter_map(|args| args.get(1).cloned())
            .collect()
    }

    fn slow_next_fetch(&self) {
        self.fake.faults(
            "[[rule]]\nid = \"slow-pull\"\ncommand = \"pull\"\ntimes = 1\ndelay_ms = 500\n",
        );
    }

    fn installs(&self, base: &BaseName) -> Vec<String> {
        self.invocations()
            .into_iter()
            .filter(|args| {
                args.len() == 6 && args[..4] == ["exec", base.as_str(), LAUNCHER, "install-runner"]
            })
            .map(|args| args[4].clone())
            .collect()
    }

    fn exists(&self, name: &str) -> Option<bool> {
        self.fake.exists(name)
    }

    fn seed_foreign(&self, name: &str) {
        assert!(self.fake.tart(&["create", name]).status.success());
    }

    async fn clone_from(&self, base: &BaseName, vm: &OwnedVmName) -> Result<(), VmError> {
        self.provider.clone_vm(base, vm).await.map(drop)
    }

    fn remove(&self, name: &str) {
        if self.fake.exists(name) == Some(true) {
            let _stopped = self.fake.tart(&["stop", name, "--timeout", "1"]);
        }
        assert!(self.fake.tart(&["delete", name]).status.success());
    }

    fn change_identity(&self, name: &str) {
        assert!(
            self.fake
                .tart(&["set", name, "--random-mac"])
                .status
                .success()
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The contract, one test per case

#[test]
fn should_fetch_only_the_digest_reference_when_pulling() {
    // given
    let world = ImageTartWorld::new;

    // when
    let outcome = check_case(&world, ImageCase::PullByDigest);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_refuse_definite_when_the_registry_serves_another_manifest() {
    // given
    let world = ImageTartWorld::new;

    // when
    let outcome = check_case(&world, ImageCase::PullRefusesDrift);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_refuse_definite_when_the_registry_does_not_hold_the_digest() {
    // given
    let world = ImageTartWorld::new;

    // when
    let outcome = check_case(&world, ImageCase::PullUnknownRefused);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_pull_once_when_two_pulls_of_one_digest_overlap() {
    // given
    let world = ImageTartWorld::new;

    // when
    let outcome = check_case(&world, ImageCase::PullSingleFlight);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_materialise_the_base_once_with_its_runner_when_the_image_is_pulled() {
    // given
    let world = ImageTartWorld::new;

    // when
    let outcome = check_case(&world, ImageCase::MaterialiseBase);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_refuse_definite_when_the_base_is_misnamed_or_the_runner_unpinned() {
    // given
    let world = ImageTartWorld::new;

    // when
    let outcome = check_case(&world, ImageCase::MaterialiseRefusesMisnamed);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_refuse_definite_when_the_image_is_not_pulled() {
    // given
    let world = ImageTartWorld::new;

    // when
    let outcome = check_case(&world, ImageCase::MaterialiseNeedsPulledImage);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_leave_a_foreign_vm_when_it_already_has_the_base_name() {
    // given
    let world = ImageTartWorld::new;

    // when
    let outcome = check_case(&world, ImageCase::MaterialiseRefusesForeignName);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_evict_an_unreferenced_base_and_answer_done_when_it_is_gone() {
    // given
    let world = ImageTartWorld::new;

    // when
    let outcome = check_case(&world, ImageCase::EvictUnreferenced);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_refuse_to_evict_a_base_when_a_live_clone_references_it() {
    // given
    let world = ImageTartWorld::new;

    // when
    let outcome = check_case(&world, ImageCase::EvictRefusesReferenced);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_refuse_to_evict_a_base_when_this_deployment_did_not_make_it() {
    // given
    let world = ImageTartWorld::new;

    // when
    let outcome = check_case(&world, ImageCase::EvictRefusesForeign);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_refuse_to_evict_a_base_when_its_instance_id_differs_from_the_record() {
    // given
    let world = ImageTartWorld::new;

    // when
    let outcome = check_case(&world, ImageCase::EvictRefusesIdentityMismatch);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_keep_ownership_of_its_bases_when_the_daemon_restarts() {
    // given
    let world = ImageTartWorld::new;

    // when
    let outcome = check_case(&world, ImageCase::RestartKeepsOwnership);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_list_a_linux_base_as_linux_when_its_image_is_linux() {
    // given
    let world = ImageTartWorld::new;

    // when
    let outcome = check_case(&world, ImageCase::LinuxBaseListedAsLinux);

    // then
    assert_eq!(outcome, Ok(()));
}

// ---------------------------------------------------------------------------------------------
// Pull verification

#[test]
fn should_refuse_without_running_tart_when_the_registry_serves_another_manifest() {
    // given
    let world = ImageTartWorld::new();
    let digest = world.publish_drifted(b"served", b"claimed");

    // when
    let pulled = ImageTartWorld::runtime().block_on(world.store.pull(&digest));

    // then
    assert_eq!(pulled, Err(VmError::Definite));
    assert_eq!(
        world.pulls(),
        0,
        "no tart pull for a manifest that is not the digest's"
    );
}

#[test]
fn should_verify_against_the_raw_manifest_when_tart_caches_it_reserialized() {
    // given
    let world = ImageTartWorld::new();
    let digest = world.publish_image(b"reserialized", GuestOs::Macos, Damage::None, None);
    let raw = std::fs::read(
        world
            .registry()
            .join(encoded(&format!("{REPOSITORY}@{digest}"))),
    )
    .expect("raw");

    // when
    let pulled = ImageTartWorld::runtime().block_on(world.store.pull(&digest));

    // then
    assert_eq!(pulled, Ok(()));
    let cached = world
        .fake
        .home()
        .join("cache/OCIs")
        .join(REPOSITORY)
        .join(digest.as_str())
        .join("manifest.json");
    assert_ne!(std::fs::read(cached).expect("cached"), raw);
}

#[test]
fn should_delete_pull_once_more_then_refuse_when_a_file_does_not_match_its_descriptor() {
    for damage in [Damage::Config, Damage::Nvram, Damage::Chunk] {
        // given
        let world = ImageTartWorld::new();
        let digest = world.publish_image(b"damaged", GuestOs::Macos, damage, None);
        let runtime = ImageTartWorld::runtime();

        // when
        let pulled = runtime.block_on(world.store.pull(&digest));

        // then
        assert_eq!(pulled, Err(VmError::Definite), "{damage:?}");
        assert_eq!(world.pulls(), 2, "{damage:?}: one pull and one retry");
        let listing = runtime.block_on(world.store.list()).expect("a listing");
        assert!(!listing.images.contains(&digest), "{damage:?}");
        let deletes = world
            .invocations()
            .into_iter()
            .filter(|args| args[0] == "delete")
            .count();
        assert_eq!(deletes, 2, "{damage:?}: each failed entry deleted");
    }
}

#[test]
fn should_accept_the_content_when_only_the_compressed_layer_digest_is_not_checked() {
    // given: every chunk the fake serves differs from its (stand-in) compressed digest, and
    // matches its uncompressed digest; re-checking compressed blobs is a stated follow-up.
    let world = ImageTartWorld::new();
    let digest = world.publish_image(b"compressed", GuestOs::Macos, Damage::None, None);

    // when
    let pulled = ImageTartWorld::runtime().block_on(world.store.pull(&digest));

    // then
    assert_eq!(pulled, Ok(()));
}

#[test]
fn should_refuse_before_pulling_when_the_image_has_a_disk_v1_layer() {
    // given
    let world = ImageTartWorld::new();
    let digest = world.publish_image(b"v1", GuestOs::Macos, Damage::DiskV1, None);

    // when
    let pulled = ImageTartWorld::runtime().block_on(world.store.pull(&digest));

    // then
    assert_eq!(pulled, Err(VmError::Definite));
    assert_eq!(world.pulls(), 0);
}

// ---------------------------------------------------------------------------------------------
// The build and its crash points

#[test]
fn should_clone_set_its_mac_boot_once_install_and_stop_when_materialising() {
    // given
    let world = ImageTartWorld::new();
    let digest = world.publish(b"{\"image\":\"argv\"}", GuestOs::Macos);
    let base = base_name(&digest, RUNNER).expect("a base name");
    let runtime = ImageTartWorld::runtime();
    runtime.block_on(world.store.pull(&digest)).expect("pull");
    let before = world.invocations().len();

    // when
    let made = runtime.block_on(world.store.materialise(&digest, &base, RUNNER));

    // then
    assert_eq!(made, Ok(()));
    let name = base.as_str();
    let reference = format!("{REPOSITORY}@{digest}");
    let effects: Vec<Vec<String>> = world
        .invocations()
        .into_iter()
        .skip(before)
        .filter(|args| !matches!(args[0].as_str(), "get" | "list" | "--version"))
        .collect();
    let expected: Vec<Vec<String>> = [
        vec!["clone", reference.as_str(), name],
        vec!["set", name, "--random-mac"],
        vec!["run", name, "--no-graphics"],
        vec![
            "exec",
            name,
            LAUNCHER,
            "install-runner",
            RUNNER,
            RUNNER_SHA256,
        ],
        vec!["stop", name, "--timeout", "2"],
    ]
    .into_iter()
    .map(|args| args.into_iter().map(str::to_owned).collect())
    .collect();
    assert_eq!(effects, expected);
    assert!(
        world.fake.events("signal").is_empty(),
        "no signal to tart run"
    );
    let entry = world.entry(name).expect("an entry");
    assert_eq!(entry.state, BaseState::Ready);
    assert_eq!(entry.source_digest, digest);
    assert_eq!(entry.runner_sha256, RUNNER_SHA256);
    assert_eq!(entry.tart_version, "2.28.0");
    assert_eq!(entry.deployment_id, "deployment-under-test");
    assert_eq!(entry.base_id.len(), 32);
    assert_ne!(Some(entry.base_id.clone()), entry.vm_mac);
    assert!(entry.final_fingerprint.is_some() && entry.vm_file_identity.is_some());
    assert!(
        !world.fake.shims().join(name).exists(),
        "the base's shim directory is removed"
    );
}

#[test]
fn should_rebuild_when_a_crash_left_the_base_reserved_and_absent() {
    // given
    let world = ImageTartWorld::new();
    let runtime = ImageTartWorld::runtime();
    let digest = world.publish(b"reserved", GuestOs::Macos);
    let base = base_name(&digest, RUNNER).expect("a base name");
    runtime.block_on(world.store.pull(&digest)).expect("pull");
    let mut entry = crate::support::ready_entry(&world.fake.home(), base.as_str(), "");
    (entry.state, entry.source_digest, entry.vm_mac) = (BaseState::Reserved, digest.clone(), None);
    world
        .fake
        .records()
        .update(|record| record.bases.insert(base.to_string(), entry))
        .expect("a writable registry");

    // when
    let made = runtime.block_on(world.store.materialise(&digest, &base, RUNNER));

    // then
    assert_eq!(made, Ok(()));
    assert_eq!(
        world.entry(base.as_str()).map(|e| e.state),
        Some(BaseState::Ready)
    );
}

#[test]
fn should_preserve_and_quarantine_when_a_crash_left_a_clone_without_its_mac() {
    // given
    let world = ImageTartWorld::new();
    let runtime = ImageTartWorld::runtime();
    let base = world.ready_base(&runtime, b"cloning");
    world.crashed_at(base.as_str(), |entry| {
        entry.state = BaseState::Cloning;
        entry.vm_mac = None;
        entry.vm_file_identity = None;
        entry.final_fingerprint = None;
    });
    let digest = world.entry(base.as_str()).expect("an entry").source_digest;

    // when
    let made = runtime.block_on(world.store.materialise(&digest, &base, RUNNER));
    let evicted = runtime.block_on(world.store.evict(&base));
    let cloned = runtime.block_on(world.provider.clone_vm(&base, &vm(1)));

    // then
    assert_eq!(
        (made, evicted),
        (Err(VmError::Definite), Err(VmError::Definite))
    );
    assert_eq!(cloned, Err(VmError::Definite));
    let entry = world.entry(base.as_str()).expect("an entry");
    assert_eq!(
        (entry.state, entry.quarantine.as_deref()),
        (BaseState::Quarantined, Some("clone_unproven"))
    );
    assert_eq!(world.exists(base.as_str()), Some(false), "preserved");
}

#[test]
fn should_retry_when_a_crash_left_the_base_cloning_and_absent() {
    // given
    let world = ImageTartWorld::new();
    let runtime = ImageTartWorld::runtime();
    let base = world.ready_base(&runtime, b"cloning-absent");
    let digest = world.entry(base.as_str()).expect("an entry").source_digest;
    world.remove(base.as_str());
    world.crashed_at(base.as_str(), |entry| entry.state = BaseState::Cloning);

    // when
    let made = runtime.block_on(world.store.materialise(&digest, &base, RUNNER));

    // then
    assert_eq!(made, Ok(()));
    assert_eq!(world.installs(&base).len(), 2);
}

#[test]
fn should_delete_and_rebuild_when_a_crash_left_a_proven_base_customizing_or_verifying() {
    for state in [BaseState::Customizing, BaseState::Verifying] {
        // given
        let world = ImageTartWorld::new();
        let runtime = ImageTartWorld::runtime();
        let base = world.ready_base(&runtime, b"customizing");
        let first = world.entry(base.as_str()).expect("an entry");
        world.crashed_at(base.as_str(), |entry| entry.state = state);
        let listing = runtime.block_on(world.store.list()).expect("a listing");

        // when
        let made = runtime.block_on(world.store.materialise(&first.source_digest, &base, RUNNER));

        // then
        assert!(
            !listing.bases.contains_key(&base),
            "{state:?} is never listed"
        );
        assert_eq!(made, Ok(()), "{state:?}");
        let rebuilt = world.entry(base.as_str()).expect("an entry");
        assert_eq!(rebuilt.state, BaseState::Ready);
        assert_ne!(rebuilt.base_id, first.base_id, "{state:?}: another base");
        assert_eq!(world.installs(&base).len(), 2);
    }
}

#[test]
fn should_settle_a_retiring_base_when_it_is_observed_gone_or_still_proven() {
    // given
    let world = ImageTartWorld::new();
    let runtime = ImageTartWorld::runtime();
    let gone = world.ready_base(&runtime, b"retiring-gone");
    let kept = world.ready_base(&runtime, b"retiring-kept");
    world.remove(gone.as_str());
    for base in [&gone, &kept] {
        world.crashed_at(base.as_str(), |entry| entry.state = BaseState::Retiring);
    }

    // when
    let recovered = runtime.block_on(world.store.recover());

    // then
    assert_eq!(recovered, Ok(()));
    assert_eq!(
        (world.entry(gone.as_str()), world.entry(kept.as_str())),
        (None, None)
    );
    assert_eq!(
        world.exists(kept.as_str()),
        None,
        "a proven retiring base is deleted"
    );
}

#[test]
fn should_refuse_a_base_when_another_digest_already_holds_its_twelve_digit_name() {
    // given
    let world = ImageTartWorld::new();
    let runtime = ImageTartWorld::runtime();
    let base = world.ready_base(&runtime, b"collision");
    let other = world.publish(b"another", GuestOs::Macos);
    runtime.block_on(world.store.pull(&other)).expect("pull");
    world.crashed_at(base.as_str(), |entry| entry.source_digest = other.clone());
    let digest = world.publish(b"collision", GuestOs::Macos);

    // when
    let made = runtime.block_on(world.store.materialise(&digest, &base, RUNNER));

    // then
    assert_eq!(made, Err(VmError::Definite));
}

// ---------------------------------------------------------------------------------------------
// Clone and evict proofs

#[test]
fn should_quarantine_and_refuse_a_clone_when_the_base_disk_changed_after_it_was_verified() {
    // given
    let world = ImageTartWorld::new();
    let runtime = ImageTartWorld::runtime();
    let base = world.ready_base(&runtime, b"tampered");
    let disk = world
        .fake
        .home()
        .join("vms")
        .join(base.as_str())
        .join("disk.img");
    let mut bytes = std::fs::read(&disk).expect("disk");
    bytes.push(0);
    std::fs::write(&disk, bytes).expect("modify the disk in place");

    // when
    let listing = runtime.block_on(world.store.list()).expect("a listing");
    let cloned = runtime.block_on(world.provider.clone_vm(&base, &vm(1)));

    // then
    assert!(!listing.bases.contains_key(&base));
    assert_eq!(cloned, Err(VmError::Definite));
    assert_eq!(world.exists(&vm(1).to_string()), None);
    let entry = world.entry(base.as_str()).expect("an entry");
    assert_eq!(
        (entry.state, entry.quarantine.as_deref()),
        (BaseState::Quarantined, Some("integrity_changed"))
    );
}

#[test]
fn should_refuse_a_look_alike_with_the_same_name_and_mac_when_cloning_or_evicting() {
    // given
    let world = ImageTartWorld::new();
    let runtime = ImageTartWorld::runtime();
    let base = world.ready_base(&runtime, b"look-alike");
    let mac = world.fake.mac(base.as_str());
    world.remove(base.as_str());
    world.seed_foreign(base.as_str());
    let record = world
        .fake
        .home()
        .join("vms")
        .join(format!("{}.json", base.as_str()));
    let mut forged: Value =
        serde_json::from_slice(&std::fs::read(&record).expect("vm")).expect("json");
    forged["mac"] = Value::from(mac.clone());
    std::fs::write(&record, serde_json::to_vec(&forged).expect("json")).expect("forge the MAC");
    assert_eq!(world.fake.mac(base.as_str()), mac);

    // when
    let listing = runtime.block_on(world.store.list()).expect("a listing");
    let cloned = runtime.block_on(world.provider.clone_vm(&base, &vm(1)));
    let evicted = runtime.block_on(world.store.evict(&base));

    // then
    assert!(!listing.bases.contains_key(&base));
    assert_eq!(
        (cloned, evicted),
        (Err(VmError::Definite), Err(VmError::Definite))
    );
    assert_eq!(
        world.exists(base.as_str()),
        Some(false),
        "the look-alike is left"
    );
}

#[test]
fn should_let_exactly_one_win_when_a_clone_and_an_eviction_of_one_base_race() {
    // given
    let world = ImageTartWorld::new();
    let runtime = ImageTartWorld::runtime();
    let base = world.ready_base(&runtime, b"race");

    // when
    let clone = vm(1);
    let (cloned, evicted) = runtime.block_on(async {
        tokio::join!(
            world.provider.clone_vm(&base, &clone),
            world.store.evict(&base)
        )
    });

    // then
    match (cloned, evicted) {
        (Ok(_), Err(VmError::Definite)) => {
            assert_eq!(
                world.exists(base.as_str()),
                Some(false),
                "the referenced base stays"
            );
        }
        (Err(VmError::Definite), Ok(())) => {
            assert_eq!(
                world.exists(&vm(1).to_string()),
                None,
                "no clone of an evicted base"
            );
        }
        other => panic!("both or neither won: {other:?}"),
    }
}

#[test]
fn should_block_eviction_when_a_clone_intent_is_unresolved_and_its_vm_exists() {
    // given
    let world = ImageTartWorld::new();
    let runtime = ImageTartWorld::runtime();
    let base = world.ready_base(&runtime, b"intent");
    let clone = vm(1).to_string();
    assert!(
        world
            .fake
            .tart(&["clone", base.as_str(), &clone])
            .status
            .success()
    );
    world.crashed_at(base.as_str(), |entry| {
        entry.clones.insert(clone.clone(), None);
    });

    // when
    let evicted = runtime.block_on(world.store.evict(&base));

    // then
    assert_eq!(evicted, Err(VmError::Definite));
    assert_eq!(world.exists(base.as_str()), Some(false));
}

#[test]
fn should_refuse_eviction_and_reservation_when_the_listing_is_incomplete() {
    // given
    let world = ImageTartWorld::new();
    let runtime = ImageTartWorld::runtime();
    let base = world.ready_base(&runtime, b"incomplete");
    let digest = world.publish(b"incomplete-2", GuestOs::Macos);
    let other = base_name(&digest, RUNNER).expect("a base name");
    runtime.block_on(world.store.pull(&digest)).expect("pull");
    world.fail_listings();

    // when
    let evicted = runtime.block_on(world.store.evict(&base));
    let made = runtime.block_on(world.store.materialise(&digest, &other, RUNNER));

    // then
    assert_eq!(
        (evicted, made),
        (Err(VmError::Definite), Err(VmError::Definite))
    );
    assert_eq!(world.exists(base.as_str()), Some(false));
    assert_eq!(world.exists(other.as_str()), None);
}

#[test]
fn should_evict_a_base_whose_only_clone_was_made_outside_mayfly_when_lineage_is_unknown() {
    // given: the limit ADR 0087 states: Tart keeps no clone lineage, so a clone nobody
    // recorded is invisible to eviction (and keeps its own copy-on-write data).
    let world = ImageTartWorld::new();
    let runtime = ImageTartWorld::runtime();
    let base = world.ready_base(&runtime, b"lineage");
    assert!(
        world
            .fake
            .tart(&["clone", base.as_str(), "outside-clone"])
            .status
            .success()
    );

    // when
    let evicted = runtime.block_on(world.store.evict(&base));

    // then
    assert_eq!(evicted, Ok(()));
    assert_eq!(
        world.exists("outside-clone"),
        Some(false),
        "the outside clone is untouched"
    );
}

// ---------------------------------------------------------------------------------------------
// The registry and the trust boundary

#[test]
fn should_own_nothing_and_rediscover_nothing_when_the_registry_is_lost() {
    // given
    let mut world = ImageTartWorld::new();
    let runtime = ImageTartWorld::runtime();
    let base = world.ready_base(&runtime, b"lost");
    let digest = world.entry(base.as_str()).expect("an entry").source_digest;
    std::fs::remove_file(world.fake.records().path()).expect("lose the registry");
    world.restart();

    // when
    let listing = runtime.block_on(world.store.list()).expect("a listing");
    let cloned = runtime.block_on(world.provider.clone_vm(&base, &vm(1)));
    let evicted = runtime.block_on(world.store.evict(&base));
    let made = runtime.block_on(world.store.materialise(&digest, &base, RUNNER));

    // then
    assert!(listing.bases.is_empty());
    assert_eq!(
        (cloned, evicted, made),
        (
            Err(VmError::Definite),
            Err(VmError::Definite),
            Err(VmError::Definite)
        )
    );
    assert_eq!(
        world.exists(base.as_str()),
        Some(false),
        "never deleted by name"
    );
}

#[test]
fn should_refuse_everything_when_the_registry_is_corrupt() {
    // given
    let world = ImageTartWorld::new();
    let runtime = ImageTartWorld::runtime();
    let base = world.ready_base(&runtime, b"corrupt");
    std::fs::write(world.fake.records().path(), b"{\"v\":1,").expect("corrupt the registry");

    // when
    let listed = runtime.block_on(world.store.list());
    let cloned = runtime.block_on(world.provider.clone_vm(&base, &vm(1)));
    let evicted = runtime.block_on(world.store.evict(&base));

    // then
    assert_eq!(listed, Err(VmError::Definite));
    assert_eq!(
        (cloned, evicted),
        (Err(VmError::Definite), Err(VmError::Definite))
    );
    assert_eq!(world.exists(base.as_str()), Some(false));
}

fn store_over(home: &Path, state: &Path) -> Result<Store, ConfigError> {
    let config = crate::support::config(home, &state.join("shims"));
    let records = std::sync::Arc::new(mayfly_adapter_tart::BaseRecords::open(
        state.join("bases.json"),
    ));
    TartImageStore::new(
        config,
        image_config(),
        records,
        RegistryDir {
            home: home.to_path_buf(),
        },
    )
}

fn private_dir(path: &Path, mode: u32) {
    std::fs::create_dir_all(path).expect("dir");
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(mode))
        .expect("mode");
}

#[test]
fn should_refuse_to_start_when_tart_home_or_an_ancestor_lets_others_swap_it() {
    // given: the boundary of ADR 0087. Tart names VMs, so a writer to TART_HOME could swap a
    // directory between a check and the effect; the start-up check refuses any layout where
    // anyone but root and the daemon's user could.
    let root = tempfile::tempdir().expect("tempdir");
    let state = root.path().join("state");
    private_dir(&state, 0o700);
    let open_home = root.path().join("open-home");
    private_dir(&open_home, 0o755);
    let shared = root.path().join("shared");
    private_dir(&shared, 0o777);
    let under_shared = shared.join("home");
    private_dir(&under_shared, 0o700);
    let sticky = root.path().join("sticky");
    private_dir(&sticky, 0o1777);
    let under_sticky = sticky.join("home");
    private_dir(&under_sticky, 0o700);
    let target = root.path().join("target");
    private_dir(&target, 0o700);
    let linked = root.path().join("linked");
    std::os::unix::fs::symlink(&target, &linked).expect("symlink");
    let real_parent = root.path().join("real-parent");
    private_dir(&real_parent.join("home"), 0o700);
    let linked_parent = root.path().join("linked-parent");
    std::os::unix::fs::symlink(&real_parent, &linked_parent).expect("symlink");

    // when
    let judged = [
        store_over(&open_home, &state).err(),
        store_over(&under_shared, &state).err(),
        store_over(&under_sticky, &state).err(),
        store_over(&linked, &state).err(),
        store_over(&linked_parent.join("home"), &state).err(),
        store_over(&root.path().join("target/../target"), &state).err(),
        store_over(&target, &state).err(),
    ];

    // then
    assert_eq!(
        judged,
        [
            Some(ConfigError::UnsafeLayout(UnsafeLayout::OpenToOthers(
                open_home
            ))),
            Some(ConfigError::UnsafeLayout(UnsafeLayout::UnsafeAncestor(
                shared
            ))),
            None,
            Some(ConfigError::UnsafeLayout(UnsafeLayout::Symlink(linked))),
            Some(ConfigError::UnsafeLayout(UnsafeLayout::Symlink(
                linked_parent
            ))),
            Some(ConfigError::UnsafeLayout(UnsafeLayout::NotADirectory(
                root.path().join("target/../target")
            ))),
            None,
        ]
    );
    assert!(verify::check_private(&state).is_ok());
}

#[test]
fn should_refuse_a_tart_home_owned_by_another_user_when_starting() {
    // given: as root, a private directory handed to another user; as any other user, the root
    // directory, which root owns
    let root = tempfile::tempdir().expect("tempdir");
    let foreign = root.path().join("foreign-home");
    private_dir(&foreign, 0o700);
    let owned_by_other = if rustix::process::geteuid().is_root() {
        std::os::unix::fs::chown(&foreign, Some(65534), Some(65534)).expect("chown");
        foreign
    } else {
        PathBuf::from("/")
    };

    // when
    let judged = verify::check_private(&owned_by_other);

    // then
    assert_eq!(judged, Err(UnsafeLayout::OwnedByOther(owned_by_other)));
}

#[test]
fn should_never_evict_a_quarantined_base_when_its_identity_still_holds() {
    // given: a disk modified in place keeps the base's file identity; the clone that finds it
    // changed quarantines it
    let world = ImageTartWorld::new();
    let runtime = ImageTartWorld::runtime();
    let base = world.ready_base(&runtime, b"quarantined");
    let disk = world
        .fake
        .home()
        .join("vms")
        .join(base.as_str())
        .join("disk.img");
    let mut bytes = std::fs::read(&disk).expect("disk");
    bytes[0] ^= 1;
    std::fs::write(&disk, bytes).expect("modify the disk in place");
    let cloned = runtime.block_on(world.provider.clone_vm(&base, &vm(1)));
    let entry = world.entry(base.as_str()).expect("an entry");
    assert_eq!(
        (cloned, entry.state),
        (Err(VmError::Definite), BaseState::Quarantined)
    );

    // when
    let evicted = runtime.block_on(world.store.evict(&base));
    let recovered = runtime.block_on(world.store.recover());

    // then
    assert_eq!((evicted, recovered), (Err(VmError::Definite), Ok(())));
    assert_eq!(
        world.exists(base.as_str()),
        Some(false),
        "never deleted automatically"
    );
    assert_eq!(
        world.entry(base.as_str()).map(|e| e.state),
        Some(BaseState::Quarantined)
    );
}

#[test]
fn should_stop_delete_and_rebuild_when_a_crash_left_the_base_booted() {
    // given: the daemon died while the base ran for its install; its `tart run` still runs
    let world = ImageTartWorld::new();
    let runtime = ImageTartWorld::runtime();
    let base = world.ready_base(&runtime, b"booted");
    let first = world.entry(base.as_str()).expect("an entry");
    let mut run = std::process::Command::new(crate::support::sibling("tart"))
        .env_clear()
        .envs(std::env::var_os("LLVM_PROFILE_FILE").map(|v| ("LLVM_PROFILE_FILE", v)))
        .env("TART_HOME", world.fake.home())
        .args(["run", base.as_str(), "--no-graphics"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("tart run");
    let up = world.fake.tart(&["ip", base.as_str(), "--wait", "30"]);
    assert!(up.status.success(), "the base runs");
    world.crashed_at(base.as_str(), |entry| entry.state = BaseState::Customizing);

    // when
    let made = runtime.block_on(world.store.materialise(&first.source_digest, &base, RUNNER));
    let ended = run.wait().expect("the run ends");

    // then
    assert_eq!(made, Ok(()));
    assert!(
        ended.code().is_some(),
        "ended by tart stop, not by a signal"
    );
    assert!(
        world.fake.events("signal").is_empty(),
        "no signal to tart run"
    );
    let rebuilt = world.entry(base.as_str()).expect("an entry");
    assert_eq!(rebuilt.state, BaseState::Ready);
    assert_ne!(rebuilt.base_id, first.base_id);
    let stops = world
        .invocations()
        .into_iter()
        .filter(|args| args[0] == "stop" && args[1] == base.as_str())
        .count();
    assert_eq!(
        stops, 3,
        "the first build's stop, the crashed run's by tart stop, the rebuild's"
    );
}

#[test]
fn should_delete_both_entries_when_tart_links_the_digest_to_another_manifest() {
    // given: the store is served A's manifest for A's digest, Tart is then served B's; Tart
    // 2.28.0 names its entry by B's hash and links A's digest to it
    let world = ImageTartWorld::new();
    let a = world.publish_image(b"honest", GuestOs::Macos, Damage::None, None);
    let b = world.publish_image(b"swapped", GuestOs::Macos, Damage::None, None);
    let at_a = encoded(&format!("{REPOSITORY}@{a}"));
    let honest = world.fake.home().join("honest");
    std::fs::create_dir_all(&honest).expect("honest");
    std::fs::copy(world.registry().join(&at_a), honest.join(&at_a)).expect("keep A");
    std::fs::copy(
        world.registry().join(encoded(&format!("{REPOSITORY}@{b}"))),
        world.registry().join(&at_a),
    )
    .expect("serve B at A");
    let runtime = ImageTartWorld::runtime();

    // when
    let pulled = runtime.block_on(world.store.pull(&a));

    // then
    assert_eq!(pulled, Err(VmError::Definite));
    assert_eq!(world.pulls(), 2);
    let cache = world.fake.home().join("cache/OCIs").join(REPOSITORY);
    for digest in [&a, &b] {
        let entry = cache.join(digest.as_str());
        assert!(
            !entry.exists() && !entry.is_symlink(),
            "{digest} left in the cache"
        );
    }
    let listed = world.fake.tart(&["list", "--source", "oci", "-q"]);
    assert!(
        listed.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&listed.stdout)
    );
}

#[test]
fn should_keep_a_quarantined_entry_when_its_vm_is_gone_and_eviction_is_asked() {
    // given
    let world = ImageTartWorld::new();
    let runtime = ImageTartWorld::runtime();
    let base = world.ready_base(&runtime, b"quarantined-gone");
    world.crashed_at(base.as_str(), |entry| {
        entry.state = BaseState::Quarantined;
        entry.quarantine = Some("integrity_changed".to_owned());
    });
    world.remove(base.as_str());

    // when
    let evicted = runtime.block_on(world.store.evict(&base));

    // then
    assert_eq!(evicted, Err(VmError::Definite));
    assert_eq!(
        world.entry(base.as_str()).map(|e| e.state),
        Some(BaseState::Quarantined),
        "only an operator resolves a quarantine"
    );
}

#[test]
fn should_not_retire_a_base_when_a_recorded_clone_of_it_is_still_listed() {
    // given: a crash left a base that has a live recorded clone in a build state
    let world = ImageTartWorld::new();
    let runtime = ImageTartWorld::runtime();
    let base = world.ready_base(&runtime, b"dependent");
    let digest = world.entry(base.as_str()).expect("an entry").source_digest;
    runtime
        .block_on(world.provider.clone_vm(&base, &vm(1)))
        .expect("clone");
    world.crashed_at(base.as_str(), |entry| entry.state = BaseState::Verifying);

    // when
    let made = runtime.block_on(world.store.materialise(&digest, &base, RUNNER));

    // then
    assert!(made.is_err(), "{made:?}");
    assert_eq!(world.exists(base.as_str()), Some(false), "the base stays");
    assert_eq!(
        world.exists(&vm(1).to_string()),
        Some(false),
        "its clone stays"
    );
}

#[test]
fn should_create_nothing_when_the_registry_directory_would_sit_under_an_unsafe_ancestor() {
    // given
    let root = tempfile::tempdir().expect("tempdir");
    let home = root.path().join("home");
    private_dir(&home, 0o700);
    let shared = root.path().join("shared");
    private_dir(&shared, 0o777);
    let state = shared.join("state");

    // when
    let built = store_over(&home, &state).err();

    // then
    assert_eq!(
        built,
        Some(ConfigError::UnsafeLayout(UnsafeLayout::UnsafeAncestor(
            shared
        )))
    );
    assert!(!state.exists(), "checked before it was created");
}
