//! The `ImageStore` contract (IC-01..IC-14) against real Tart (DP-07, HW-01): macOS only, with
//! the `real-tart` feature, run by `cargo xtask hw01` after it has built the contract image (the
//! pinned Linux image with the test launcher, `tools/hw01/launcher`) and pushed it to the
//! loopback registry it serves for the run. It names, through the environment:
//!
//! - `MAYFLY_HW01_IC_ROOT`: a `0700` directory where each case's world makes its own `TART_HOME`,
//!   shims and record, removed when the world is dropped;
//! - `MAYFLY_HW01_REGISTRY` and `MAYFLY_HW01_REGISTRY_DIR`: the registry's `127.0.0.1:<port>` and
//!   the directory it serves from (`blobs/sha256/<hex>`, `manifests/<repository>/<reference>`,
//!   `xtask`'s `hw01::registry`), which the world writes to publish;
//! - `MAYFLY_HW01_CONTRACT`: the Linux contract image, `<registry>/mayfly-contract@<digest>`;
//! - `MAYFLY_HW01_CONTRACT_MACOS`: with `--macos`, the macOS one in the same repository (empty
//!   otherwise);
//! - `MAYFLY_HW01_RUNNER`, `MAYFLY_HW01_RUNNER_SHA256_LINUX_ARM64`,
//!   `MAYFLY_HW01_RUNNER_SHA256_OSX_ARM64`: the pinned runner and its packages' checksums, one
//!   per platform (the osx-arm64 one may be empty);
//! - `MAYFLY_HW01_PUBLIC_IMAGE`: the pinned public Linux image, `<repository>@<digest>`.
//!
//! **Publishing.** Each `publish(content)` is the contract image with its Tart config's
//! `memorySize` moved by an amount derived from `content`: a new config blob, a new manifest and
//! so a new digest, every disk chunk shared. Drift serves one content's manifest under the digest
//! of another's. A case that names a macOS guest gets the macOS contract image when the run has
//! one (`--macos`) and the Linux one otherwise; IC-14, which needs a macOS base beside a Linux
//! one, runs only with the macOS image and the osx-arm64 pin, and is reported `not_run` without
//! them. The base's guest selects the runner pin (`ImageConfig::runners`, per platform).
//!
//! **What the store fetched** and **what was installed** are read from a log a wrapper around
//! `tart` keeps (each call's arguments, before it runs Tart); the wrapper also adds `--insecure`
//! to a pull, because the loopback registry speaks plain HTTP. The store's own manifest source
//! reads the registry's directory: `RegistryManifests` speaks HTTPS only, so it is checked
//! against the public registry instead (`public-manifest-fetch`).
//!
//! The test prints one line per case, `hw01-case <id> <pass|fail|not_run> <detail>`, which
//! `hw01` turns into its report, and fails unless every case it ran passed.

#![cfg(all(target_os = "macos", feature = "real-tart"))]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use mayfly_adapter_tart::bases::SignaturePolicy;
use mayfly_adapter_tart::verify::{CONFIG_LAYER, sha256_hex};
use mayfly_adapter_tart::{
    BaseRecords, Deadlines, FetchError, ImageConfig, ImageDeadlines, ManifestSource,
    RegistryManifests, RunnerPlatform, ShimProgram, TartConfig, TartImageStore, TartProvider,
};
use mayfly_data::{BaseName, GuestOs, VmError, VmProvider};
use mayfly_domain::{ImageDigest, OwnedVmName};
use mayfly_testkit_data::image_contract::{ImageCase, ImageWorld, check_case};
use serde_json::Value;

/// The repository path of the contract image on the loopback registry.
const CONTRACT_PATH: &str = "mayfly-contract";

/// The runner's pins, one per platform the run has a checksum for.
fn runner_pins() -> BTreeMap<RunnerPlatform, String> {
    [
        (
            RunnerPlatform::LinuxArm64,
            "MAYFLY_HW01_RUNNER_SHA256_LINUX_ARM64",
        ),
        (
            RunnerPlatform::OsxArm64,
            "MAYFLY_HW01_RUNNER_SHA256_OSX_ARM64",
        ),
    ]
    .into_iter()
    .filter_map(|(platform, name)| {
        std::env::var(name)
            .ok()
            .filter(|sha| !sha.is_empty())
            .map(|sha| (platform, sha))
    })
    .collect()
}

/// The macOS contract image's digest, when the run built one (`--macos`) and pins the osx-arm64
/// package; otherwise every case gets the Linux image.
fn macos_contract() -> Option<String> {
    std::env::var("MAYFLY_HW01_CONTRACT_MACOS")
        .ok()
        .and_then(|r| r.rsplit_once('@').map(|(_, d)| d.to_owned()))
        .filter(|_| runner_pins().contains_key(&RunnerPlatform::OsxArm64))
}

fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name}: run through `cargo xtask hw01`"))
}

fn on_path(name: &str) -> PathBuf {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|d| d.join(name))
                .find(|p| p.is_file())
        })
        .unwrap_or_else(|| panic!("{name} is not on PATH"))
}

fn sibling(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("the test executable");
    let path = exe
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile>/deps")
        .join(name);
    assert!(path.is_file(), "{} is not built", path.display());
    path
}

fn private_dir(path: &Path) {
    std::fs::create_dir_all(path).expect("mkdir");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).expect("chmod");
}

fn digest_of(bytes: &[u8]) -> ImageDigest {
    ImageDigest::new(&format!("sha256:{}", sha256_hex(bytes))).expect("a digest")
}

/// The registry's directory, as `hw01::registry` lays it out.
#[derive(Debug, Clone)]
struct RegistryDir(PathBuf);

impl RegistryDir {
    fn manifest(&self, reference: &str) -> PathBuf {
        self.0
            .join("manifests")
            .join(CONTRACT_PATH.replace('/', "%2F"))
            .join(reference)
    }

    fn blob(&self, digest: &str) -> PathBuf {
        let hex = digest.strip_prefix("sha256:").expect("a sha256 digest");
        self.0.join("blobs").join("sha256").join(hex)
    }
}

/// The store's manifest source: the registry's directory, read as the registry serves it, a
/// slow fetch yielding long enough for a concurrent pull to reach its own.
struct Manifests {
    registry: RegistryDir,
    slow: Arc<AtomicBool>,
}

impl ManifestSource for Manifests {
    async fn fetch(&self, repository: &str, digest: &ImageDigest) -> Result<Vec<u8>, FetchError> {
        if self.slow.swap(false, Ordering::SeqCst) {
            for _ in 0..64 {
                tokio::task::yield_now().await;
            }
        }
        if !repository.ends_with(&format!("/{CONTRACT_PATH}")) {
            return Err(FetchError::NotFound);
        }
        std::fs::read(self.registry.manifest(digest.as_str())).map_err(|_| FetchError::NotFound)
    }
}

static WORLDS: AtomicU32 = AtomicU32::new(0);

/// One case's world: its own home, shims and record under the IC root.
struct RealImageWorld {
    dir: PathBuf,
    tart: PathBuf,
    log: PathBuf,
    registry: RegistryDir,
    contract: Vec<u8>,
    contract_macos: Option<Vec<u8>>,
    slow: Arc<AtomicBool>,
    store: TartImageStore<Manifests>,
    provider: TartProvider,
}

impl RealImageWorld {
    fn new() -> Self {
        let n = WORLDS.fetch_add(1, Ordering::SeqCst);
        let dir = PathBuf::from(var("MAYFLY_HW01_IC_ROOT")).join(format!("world-{n}"));
        private_dir(&dir);
        for sub in ["home", "shims", "state", "bin"] {
            private_dir(&dir.join(sub));
        }
        let log = dir.join("tart-calls.log");
        let tart = dir.join("bin").join("tart");
        let real = on_path("tart");
        std::fs::write(
            &tart,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nif [ \"$1\" = pull ]; then exec '{}' \"$@\" --insecure; fi\nexec '{}' \"$@\"\n",
                log.display(),
                real.display(),
                real.display()
            ),
        )
        .expect("write the tart wrapper");
        std::fs::set_permissions(&tart, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        let registry = RegistryDir(PathBuf::from(var("MAYFLY_HW01_REGISTRY_DIR")));
        let contract_digest = var("MAYFLY_HW01_CONTRACT")
            .rsplit_once('@')
            .map(|(_, d)| d.to_owned())
            .expect("<repository>@<digest>");
        let contract =
            std::fs::read(registry.manifest(&contract_digest)).expect("the contract manifest");
        let contract_macos = macos_contract().map(|digest| {
            std::fs::read(registry.manifest(&digest)).expect("the macOS contract manifest")
        });
        let slow = Arc::new(AtomicBool::new(false));
        let (store, provider) = Self::build(&dir, &tart, &registry, &slow);
        Self {
            dir,
            tart,
            log,
            registry,
            contract,
            contract_macos,
            slow,
            store,
            provider,
        }
    }

    fn config(dir: &Path, tart: &Path) -> TartConfig {
        TartConfig {
            tart: tart.to_path_buf(),
            tart_home: dir.join("home"),
            shims: dir.join("shims"),
            shim: ShimProgram {
                program: sibling("mayfly"),
                args: vec!["vm-shim".into()],
            },
            environment: std::env::vars_os().collect(),
            deadlines: Deadlines::host(),
            output_limit: 1 << 20,
            run_log_limit: 1 << 20,
            poll: Duration::from_millis(250),
            redact: Vec::new(),
        }
    }

    fn build(
        dir: &Path,
        tart: &Path,
        registry: &RegistryDir,
        slow: &Arc<AtomicBool>,
    ) -> (TartImageStore<Manifests>, TartProvider) {
        let records = Arc::new(BaseRecords::open(dir.join("state").join("bases.json")));
        let images = ImageConfig {
            repository: format!("{}/{CONTRACT_PATH}", var("MAYFLY_HW01_REGISTRY")),
            runners: BTreeMap::from([(var("MAYFLY_HW01_RUNNER"), runner_pins())]),
            deployment_id: "hw01-contract".to_owned(),
            signature: SignaturePolicy::DigestOnly,
            deadlines: ImageDeadlines {
                pull: Duration::from_secs(3600),
                install: Duration::from_secs(1800),
                halt: Duration::from_secs(120),
            },
        };
        let source = Manifests {
            registry: registry.clone(),
            slow: Arc::clone(slow),
        };
        let store = TartImageStore::new(
            Self::config(dir, tart),
            images,
            Arc::clone(&records),
            source,
        )
        .expect("a valid image configuration");
        let provider = TartProvider::new(Self::config(dir, tart), records);
        (store, provider)
    }

    /// Real Tart against this world's home, without the wrapper's log.
    fn tart(&self, args: &[&str]) -> std::process::Output {
        Command::new(on_path("tart"))
            .env_clear()
            .env("TART_HOME", self.dir.join("home"))
            .env("TART_NO_AUTO_PRUNE", "1")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("run tart")
    }

    fn calls(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|l| l.split(' ').map(str::to_owned).collect())
            .collect()
    }

    /// The contract manifest with its Tart config's memory moved by `content`'s hash; stored in
    /// the registry under its own digest unless `serve_at` names another. Returns the variant's
    /// own digest and bytes.
    fn variant(&self, content: &[u8], guest: GuestOs) -> (ImageDigest, Vec<u8>) {
        let source = match (guest, &self.contract_macos) {
            (GuestOs::Macos, Some(macos)) => macos,
            _ => &self.contract,
        };
        let mut manifest: Value = serde_json::from_slice(source).expect("manifest JSON");
        let layers = manifest["layers"].as_array_mut().expect("layers");
        let layer = layers
            .iter_mut()
            .find(|l| l["mediaType"] == CONFIG_LAYER)
            .expect("a Tart config layer");
        let old = layer["digest"].as_str().expect("a digest").to_owned();
        let mut config: Value = serde_json::from_slice(
            &std::fs::read(self.registry.blob(&old)).expect("the config blob"),
        )
        .expect("config JSON");
        let shift = u64::from(sha256_hex(content).as_bytes()[0] % 64 + 1) << 20;
        let memory = config["memorySize"].as_u64().expect("memorySize") + shift;
        config["memorySize"] = Value::from(memory);
        let blob = serde_json::to_vec(&config).expect("encode");
        let digest = format!("sha256:{}", sha256_hex(&blob));
        std::fs::write(self.registry.blob(&digest), &blob).expect("write the config blob");
        layer["digest"] = Value::from(digest);
        layer["size"] = Value::from(blob.len());
        let bytes = serde_json::to_vec(&manifest).expect("encode");
        (digest_of(&bytes), bytes)
    }

    fn serve(&self, reference: &ImageDigest, bytes: &[u8]) {
        std::fs::write(self.registry.manifest(reference.as_str()), bytes).expect("serve");
    }
}

impl Drop for RealImageWorld {
    fn drop(&mut self) {
        // Only this world's own home: stop what runs there, then remove the whole directory.
        let listed = self.tart(&["list", "--format", "json", "--source", "local"]);
        let entries: Vec<Value> = serde_json::from_slice(&listed.stdout).unwrap_or_default();
        for entry in entries.iter().filter(|e| e["Running"] == Value::Bool(true)) {
            if let Some(name) = entry["Name"].as_str() {
                let _stopped = self.tart(&["stop", name, "--timeout", "30"]);
            }
        }
        let _removed = std::fs::remove_dir_all(&self.dir);
    }
}

impl ImageWorld for RealImageWorld {
    type Store = TartImageStore<Manifests>;

    fn store(&self) -> &Self::Store {
        &self.store
    }

    fn restart(&mut self) {
        (self.store, self.provider) =
            Self::build(&self.dir, &self.tart, &self.registry, &self.slow);
    }

    fn repository(&self) -> String {
        format!("{}/{CONTRACT_PATH}", var("MAYFLY_HW01_REGISTRY"))
    }

    fn runner(&self) -> String {
        var("MAYFLY_HW01_RUNNER")
    }

    fn publish(&self, content: &[u8], guest: GuestOs) -> ImageDigest {
        let (digest, bytes) = self.variant(content, guest);
        self.serve(&digest, &bytes);
        digest
    }

    fn publish_drifted(&self, content: &[u8], claimed: &[u8]) -> ImageDigest {
        let (claimed, _) = self.variant(claimed, GuestOs::Macos);
        let (_, served) = self.variant(content, GuestOs::Macos);
        self.serve(&claimed, &served);
        claimed
    }

    fn fetched(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|args| args.first().map(String::as_str) == Some("pull"))
            .filter_map(|args| args.get(1).cloned())
            .collect()
    }

    fn slow_next_fetch(&self) {
        self.slow.store(true, Ordering::SeqCst);
    }

    fn installs(&self, base: &BaseName) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|args| {
                args.len() == 6
                    && args[0] == "exec"
                    && args[1] == base.as_str()
                    && args[3] == "install-runner"
            })
            .map(|args| args[4].clone())
            .collect()
    }

    fn exists(&self, name: &str) -> Option<bool> {
        let out = self.tart(&["get", name, "--format", "json"]);
        out.status.success().then(|| {
            serde_json::from_slice::<Value>(&out.stdout)
                .ok()
                .and_then(|v| v["Running"].as_bool())
                .unwrap_or(false)
        })
    }

    fn seed_foreign(&self, name: &str) {
        let made = self.tart(&["create", name, "--linux", "--disk-size", "1"]);
        assert!(made.status.success(), "tart create {name}: {made:?}");
    }

    async fn clone_from(&self, base: &BaseName, vm: &OwnedVmName) -> Result<(), VmError> {
        self.provider.clone_vm(base, vm).await.map(drop)
    }

    fn remove(&self, name: &str) {
        if self.exists(name) == Some(true) {
            let _stopped = self.tart(&["stop", name, "--timeout", "30"]);
        }
        assert!(self.tart(&["delete", name]).status.success());
    }

    fn change_identity(&self, name: &str) {
        assert!(self.tart(&["set", name, "--random-mac"]).status.success());
    }
}

fn verdict(id: &str, status: &str, detail: &str) {
    println!(
        "hw01-case {id} {status} {}",
        detail.replace(['\n', '\r'], " ")
    );
}

#[test]
fn should_pass_the_image_store_contract_when_tart_is_real() {
    // given
    let make = RealImageWorld::new;

    // when
    let mut failed = Vec::new();
    for case in ImageCase::ALL {
        if case == ImageCase::LinuxBaseListedAsLinux && macos_contract().is_none() {
            verdict(
                case.id(),
                "not_run",
                "needs --macos (a macOS contract image beside the Linux one) and the osx-arm64 \
                 runner pin",
            );
            continue;
        }
        match check_case(&make, case) {
            Ok(()) => verdict(case.id(), "pass", &case.to_string()),
            Err(why) => {
                verdict(case.id(), "fail", &why);
                failed.push(format!("{case}: {why}"));
            }
        }
    }

    // then
    assert!(failed.is_empty(), "{failed:#?}");
}

#[test]
fn should_fetch_the_pinned_manifest_when_the_registry_is_public() {
    // given
    let pinned = var("MAYFLY_HW01_PUBLIC_IMAGE");
    let (repository, digest) = pinned.rsplit_once('@').expect("<repository>@<digest>");
    let digest = ImageDigest::new(digest).expect("a digest");
    let source = RegistryManifests::new().expect("TLS");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    // when
    let fetched = runtime.block_on(source.fetch(repository, &digest));

    // then
    let held = fetched.as_ref().is_ok_and(|raw| digest_of(raw) == digest);
    verdict(
        "public-manifest-fetch",
        if held { "pass" } else { "fail" },
        &match &fetched {
            Ok(raw) => format!("{} bytes, hash matches: {held}", raw.len()),
            Err(err) => format!("{err:?}"),
        },
    );
    assert!(held, "{fetched:?}");
}
