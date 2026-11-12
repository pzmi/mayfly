//! The Tart adapter's world over `mayfly-fake-tart` (integration level): a `TART_HOME` and a
//! shim directory in a tempdir, the fake's `tart` binary and the `mayfly` binary (for `mayfly
//! vm-shim`) found next to this test executable's directory, where a workspace build puts them.
//!
//! The world talks to the fake directly (its own `tart` calls with only `TART_HOME`) to seed
//! VMs, read what exists and read the fake's invocation log; faults go into `faults.toml`. When
//! dropped the fake stops every running VM with `tart stop` and waits for every shim to let go of its
//! lock; a `tart run` left hung by a hang fault (it never started a VM, so `tart stop` cannot end
//! it) is killed by its pid from `shim.json`, a process this test started.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use mayfly_adapter_tart::bases::{
    BaseEntry, BaseState, RECIPE_VERSION, Signature, SignaturePolicy, SignatureResult,
};
use mayfly_adapter_tart::proof::vm_dir;
use mayfly_adapter_tart::shim::{self, RunRecord};
use mayfly_adapter_tart::verify;
use mayfly_adapter_tart::{BaseRecords, Deadlines, ShimProgram, TartConfig, TartProvider};
use mayfly_data::{BaseName, Shim};
use mayfly_domain::{ImageDigest, Sample};
use mayfly_testkit_data::Op;
use mayfly_testkit_data::provider_contract::{ListingFault, ProviderWorld};
use mayfly_testkit_shared::poll::try_wait_until;
use serde_json::Value;
use tempfile::TempDir;

/// How long cleanup waits for a shim to let go of its lock.
const RELEASE: Duration = Duration::from_secs(20);

/// A binary built next to this test executable's directory (`target/<profile>/<name>`).
///
/// # Panics
/// When it is not there: build the workspace (`cargo xtask check` does), or at least
/// `cargo build -p mayfly-cli -p mayfly-fake-tart`.
pub fn sibling(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("the test executable");
    let dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile>/deps/<test>");
    let path = dir.join(name);
    assert!(
        path.is_file(),
        "{} is not built: build the workspace, or `cargo build -p mayfly-cli -p mayfly-fake-tart`",
        path.display()
    );
    path
}

/// Deadlines short enough for a hang case to end quickly, long enough for the fake.
pub fn deadlines() -> Deadlines {
    let call = Duration::from_secs(2);
    Deadlines {
        probe: call,
        list: call,
        get: call,
        clone: call,
        set: call,
        start: Duration::from_secs(3),
        stop: call,
        delete: call,
        settle: call,
    }
}

/// The adapter's configuration over `home` and `shims`, given this process's whole environment
/// so the adapter's own allowlist is what keeps it from the children.
pub fn config(home: &Path, shims: &Path) -> TartConfig {
    TartConfig {
        tart: sibling("tart"),
        tart_home: home.to_path_buf(),
        shims: shims.to_path_buf(),
        shim: ShimProgram {
            program: sibling("mayfly"),
            args: vec!["vm-shim".into()],
        },
        environment: std::env::vars_os().collect::<Vec<(OsString, OsString)>>(),
        deadlines: deadlines(),
        output_limit: 1 << 20,
        run_log_limit: 1 << 16,
        poll: Duration::from_millis(20),
        redact: Vec::new(),
    }
}

/// A `Ready` registry entry for the stopped VM `name` in `home` as it is now.
pub fn ready_entry(home: &Path, name: &str, mac: &str) -> BaseEntry {
    let dir = vm_dir(home, name);
    BaseEntry {
        base_id: "5eed".repeat(8),
        deployment_id: "deployment".to_owned(),
        source_ref: format!("registry.example/seed@{}", ImageDigest::sample()),
        source_digest: ImageDigest::sample(),
        tart_version: "2.41.1".to_owned(),
        runner_version: "2.330.0".to_owned(),
        runner_sha256: "5a".repeat(32),
        build_recipe_version: RECIPE_VERSION,
        state: BaseState::Ready,
        quarantine: None,
        vm_mac: Some(mac.to_owned()),
        vm_file_identity: verify::identity(&dir),
        final_fingerprint: verify::fingerprint(&dir),
        signature: Signature {
            policy: SignaturePolicy::DigestOnly,
            result: SignatureResult::NotChecked,
        },
        clones: BTreeMap::new(),
        clone_disks: BTreeMap::new(),
    }
}

/// The record of owned bases kept under `dir` (a world's tempdir).
pub fn records_at(dir: &Path) -> Arc<BaseRecords> {
    Arc::new(BaseRecords::open(dir.join("state").join("bases.json")))
}

/// A provider over the fake home, shims and record under `dir`.
pub fn provider_at(dir: &Path) -> TartProvider {
    TartProvider::new(
        config(&dir.join("tart-home"), &dir.join("shims")),
        records_at(dir),
    )
}

/// A temporary directory reached through a symlink owned by whoever runs the tests
/// (`<tempdir>/l -> r`), as macOS reaches every temporary directory through root's
/// `/var -> private/var`: a start-up check that refused every link on the way would then fail
/// on Linux too, not only on the macOS runners (DP-07F). Removed when dropped.
pub struct LinkedDir {
    _dir: TempDir,
    path: PathBuf,
}

impl LinkedDir {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("r")).expect("the link's target");
        let path = dir.path().join("l");
        std::os::unix::fs::symlink("r", &path).expect("symlink");
        Self { _dir: dir, path }
    }

    /// The directory, through the link.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// A `TART_HOME` and shim directory, and the fake driven directly; cleaned up when dropped,
/// failed test or not.
pub struct Fake {
    pub dir: LinkedDir,
}

impl Fake {
    pub fn new() -> Self {
        let dir = LinkedDir::new();
        let home = dir.path().join("tart-home");
        std::fs::create_dir(&home).expect("tart home");
        std::fs::set_permissions(&home, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .expect("a private tart home");
        Self { dir }
    }

    pub fn home(&self) -> PathBuf {
        self.dir.path().join("tart-home")
    }

    pub fn shims(&self) -> PathBuf {
        self.dir.path().join("shims")
    }

    pub fn config(&self) -> TartConfig {
        config(&self.home(), &self.shims())
    }

    /// The record of the bases this deployment owns.
    pub fn records(&self) -> Arc<BaseRecords> {
        records_at(self.dir.path())
    }

    /// A provider over this fake and its record.
    pub fn provider(&self) -> TartProvider {
        provider_at(self.dir.path())
    }

    /// Makes `name` a base this deployment owns: a local VM recorded `Ready` with the MAC,
    /// file identity and fingerprint it has now (what the image store records once it built
    /// one).
    pub fn seed_base(&self, name: &str) {
        self.seed_base_from(&["create", name]);
    }

    /// [`Fake::seed_base`] with an ASIF disk, whose capacity Tart cannot read while it runs.
    pub fn seed_asif_base(&self, name: &str) {
        self.seed_base_from(&["create", name, "--disk-format", "asif"]);
    }

    /// [`Fake::seed_base`] with a Linux guest.
    pub fn seed_linux_base(&self, name: &str) {
        self.seed_base_from(&["create", name, "--linux"]);
    }

    fn seed_base_from(&self, create: &[&str]) {
        let name = create[1];
        assert!(self.tart(create).status.success(), "create the base");
        let entry = ready_entry(&self.home(), name, &self.mac(name));
        self.records()
            .update(|record| record.bases.insert(name.to_owned(), entry))
            .expect("record the base");
    }

    /// The MAC Tart lists for `name`, lower case.
    pub fn mac(&self, name: &str) -> String {
        let info: Value =
            serde_json::from_slice(&self.tart(&["get", name, "--format", "json"]).stdout)
                .expect("get json");
        info["MACAddress"]
            .as_str()
            .expect("a MAC")
            .to_ascii_lowercase()
    }

    /// Runs the fake `tart` with only `TART_HOME` (and a coverage run's `LLVM_PROFILE_FILE`, so
    /// an instrumented fake writes its profile where the run collects it).
    pub fn tart(&self, args: &[&str]) -> Output {
        Command::new(sibling("tart"))
            .env_clear()
            .envs(std::env::var_os("LLVM_PROFILE_FILE").map(|v| ("LLVM_PROFILE_FILE", v)))
            .env("TART_HOME", self.home())
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("run the fake tart")
    }

    /// Whether `name` exists and runs, as the fake says.
    pub fn exists(&self, name: &str) -> Option<bool> {
        let out = self.tart(&["get", name, "--format", "json"]);
        if !out.status.success() {
            return None;
        }
        let info: Value = serde_json::from_slice(&out.stdout).expect("get json");
        info["Running"].as_bool()
    }

    /// Every record of the fake's invocation log.
    pub fn log(&self) -> Vec<Value> {
        std::fs::read_to_string(self.home().join("invocations.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("log line"))
            .collect()
    }

    /// The log records of one event kind.
    pub fn events(&self, event: &str) -> Vec<Value> {
        self.log()
            .into_iter()
            .filter(|record| record["event"] == event)
            .collect()
    }

    /// Writes `faults.toml`.
    pub fn faults(&self, text: &str) {
        std::fs::write(self.home().join("faults.toml"), text).expect("faults.toml");
    }

    /// Stops every running VM, then waits for every shim to let go; kills a `tart run` that a
    /// hang fault left behind.
    pub fn clean_up(&self) {
        let listed = self.tart(&["list", "--format", "json", "--source", "local"]);
        let entries: Vec<Value> = serde_json::from_slice(&listed.stdout).unwrap_or_default();
        for entry in entries.iter().filter(|e| e["Running"] == true) {
            if let Some(name) = entry["Name"].as_str() {
                let _stopped = self.tart(&["stop", name, "--timeout", "5"]);
            }
        }
        let Ok(dirs) = std::fs::read_dir(self.shims()) else {
            return;
        };
        for dir in dirs.filter_map(Result::ok).map(|e| e.path()) {
            let released = || shim::read(&dir).shim != Shim::Alive;
            if try_wait_until(RELEASE, "a shim to exit after its VM stopped", released).is_ok() {
                continue;
            }
            if let Some(record) = shim::read_json::<RunRecord>(&dir.join(shim::RECORD))
                && let Some(pid) = i32::try_from(record.child_pid)
                    .ok()
                    .and_then(rustix::process::Pid::from_raw)
            {
                let _killed = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
                let _ = try_wait_until(RELEASE, "a shim to exit after its hung run", released);
            }
        }
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.clean_up();
    }
}

/// The contract's world over the fake binary.
pub struct FakeTartWorld {
    fake: Fake,
    provider: TartProvider,
    rules: RefCell<String>,
    next_rule: Cell<u32>,
}

impl FakeTartWorld {
    pub fn new() -> Self {
        let fake = Fake::new();
        let provider = fake.provider();
        Self {
            fake,
            provider,
            rules: RefCell::new(String::new()),
            next_rule: Cell::new(0),
        }
    }

    fn rule(&self, body: &str) {
        let id = self.next_rule.get();
        self.next_rule.set(id + 1);
        let mut rules = self.rules.borrow_mut();
        rules.push_str(&format!(
            "[[rule]]\nid = \"world-{id}\"\ntimes = 1\n{body}\n"
        ));
        self.fake.faults(&rules);
    }
}

impl ProviderWorld for FakeTartWorld {
    type Provider = TartProvider;

    fn provider(&self) -> &TartProvider {
        &self.provider
    }

    fn restart(&mut self) {
        self.provider = self.fake.provider();
    }

    fn seed_base(&self, base: &BaseName) {
        self.fake.seed_base(base.as_str());
    }

    fn seed_foreign(&self, name: &str) {
        let out = self.fake.tart(&["create", name]);
        assert!(out.status.success(), "create a foreign VM");
    }

    fn exists(&self, name: &str) -> Option<bool> {
        self.fake.exists(name)
    }

    fn hang_next(&self, op: Op) {
        let command = match op {
            Op::Inventory => "list",
            Op::Clone => "clone",
            Op::Start => "run",
            Op::Stop => "stop",
            Op::Delete => "delete",
        };
        self.rule(&format!("command = \"{command}\"\naction = \"hang\""));
    }

    fn fail_next_listing(&self, fault: ListingFault) {
        match fault {
            ListingFault::Failed => {
                self.rule("command = \"list\"\naction = \"fail_before_effect\"")
            }
            ListingFault::Garbled => {
                self.rule("command = \"list\"\naction = \"corrupt_output\"\ncorrupt = \"garbage\"");
            }
        }
    }

    fn signals_to_runs(&self) -> Vec<String> {
        self.fake
            .events("signal")
            .iter()
            .filter_map(|record| record["signal"].as_str().map(str::to_owned))
            .collect()
    }

    fn child_environments(&self) -> Vec<BTreeSet<String>> {
        self.fake
            .events("invoke")
            .iter()
            .filter_map(|record| record["env"].as_object())
            .map(|env| {
                env.keys()
                    .filter(|name| !name.starts_with("__LLVM_PROFILE_"))
                    .cloned()
                    .collect()
            })
            .collect()
    }
}
