//! The JIT config across both planes over `mayfly-fake-tart` (LNK-02, integration level): the
//! control plane's `JitBroker`, the in-process link's two ends wired as `mayflyd` wires them,
//! the data plane's `LaunchStore` (`SharedLaunches`) and the Tart guest transport over the fake
//! binary, which runs the VM under its real shim and emulates the in-image launcher. A
//! composition test, because only the `both` crates may see both planes (D1).
//!
//! It needs the fake's `tart` and the `mayfly` binary (for `mayfly vm-shim`) next to this test
//! executable's directory, where a workspace build puts them (`cargo build -p mayfly-cli -p
//! mayfly-fake-tart` at least). The daemon-level launch, with a config the registration manager
//! minted, is RT-07's.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use mayfly_adapter_link::inproc;
use mayfly_adapter_tart::bases::{
    BaseEntry, BaseState, RECIPE_VERSION, Signature, SignaturePolicy, SignatureResult,
};
use mayfly_adapter_tart::proof::vm_dir;
use mayfly_adapter_tart::{
    BaseRecords, Deadlines, GuestConfig, GuestDeadlines, ShimProgram, TartConfig,
    TartGuestTransport, TartProvider, guest::LAUNCHER, shim, verify,
};
use mayfly_composition::daemon::SharedLaunches;
use mayfly_control::registration::{JitBroker, JitScope};
use mayfly_data::{
    BaseName, GuestTransport as _, LaunchAnswer, LaunchSecrets as _, RunOptions, Shim,
    VmProvider as _,
};
use mayfly_domain::{
    AttemptId, Deadline, DirectiveId, Epoch, Generation, HostId, ImageDigest, Millis, OwnedVmName,
    PoolName, ProviderInstanceId, Resources, Sample, Secret, SecretHandleId, Slot,
};
use mayfly_protocol::{
    AttemptRef, CURRENT, Directive, DirectiveKind, DirectiveSink as _, DirectiveSource as _,
    HandshakeInitiator as _, HandshakeResponder as _, Hello, Mode, SUPPORTED, SecretHandle,
    Welcome,
};
use mayfly_testkit_data::guest_contract::sha256_hex;
use mayfly_testkit_shared::poll::try_wait_until;
use serde_json::Value;
use tempfile::TempDir;

/// The session's run epoch.
const EPOCH: Epoch = Epoch::new(3);

/// How long cleanup waits for a shim to let go of its lock.
const RELEASE: Duration = Duration::from_secs(20);

/// A binary built next to this test executable's directory (`target/<profile>/<name>`).
fn sibling(name: &str) -> PathBuf {
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

/// A `TART_HOME`, a shim directory and a base record in a tempdir, the fake driven directly to
/// seed the base and read what it recorded. When dropped it stops every running VM and waits
/// for every shim to let go.
struct World {
    dir: TempDir,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("tart-home");
        std::fs::create_dir(&home).expect("tart home");
        std::fs::set_permissions(&home, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .expect("a private tart home");
        Self { dir }
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("tart-home")
    }

    fn shims(&self) -> PathBuf {
        self.dir.path().join("shims")
    }

    fn diagnostics(&self) -> PathBuf {
        self.dir.path().join("diagnostics")
    }

    fn config(&self) -> TartConfig {
        let call = Duration::from_secs(5);
        TartConfig {
            tart: sibling("tart"),
            tart_home: self.home(),
            shims: self.shims(),
            shim: ShimProgram {
                program: sibling("mayfly"),
                args: vec!["vm-shim".into()],
            },
            environment: std::env::vars_os().collect::<Vec<(OsString, OsString)>>(),
            deadlines: Deadlines {
                probe: call,
                list: call,
                get: call,
                clone: call,
                set: call,
                start: call,
                stop: call,
                delete: call,
                settle: call,
            },
            output_limit: 1 << 20,
            run_log_limit: 1 << 16,
            poll: Duration::from_millis(20),
            redact: Vec::new(),
        }
    }

    fn guest_config(&self) -> GuestConfig {
        let call = Duration::from_secs(5);
        GuestConfig {
            deadlines: GuestDeadlines {
                verify: call,
                launch: call,
                status: call,
                diagnostics: call,
            },
            claims_limit: 4096,
            diagnostics_limit: 4096,
            diagnostics_dir: self.diagnostics(),
        }
    }

    fn records(&self) -> Arc<BaseRecords> {
        Arc::new(BaseRecords::open(
            self.dir.path().join("state").join("bases.json"),
        ))
    }

    /// Runs the fake `tart` with only `TART_HOME` (and a coverage run's `LLVM_PROFILE_FILE`).
    fn tart(&self, args: &[&str]) -> Output {
        Command::new(sibling("tart"))
            .env_clear()
            .envs(std::env::var_os("LLVM_PROFILE_FILE").map(|v| ("LLVM_PROFILE_FILE", v)))
            .env("TART_HOME", self.home())
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("run the fake tart")
    }

    /// Makes `name` a base this deployment owns, recorded `Ready` as it is now.
    fn seed_base(&self, name: &str) {
        assert!(
            self.tart(&["create", name]).status.success(),
            "create the base"
        );
        let info: Value =
            serde_json::from_slice(&self.tart(&["get", name, "--format", "json"]).stdout)
                .expect("get json");
        let mac = info["MACAddress"]
            .as_str()
            .expect("a MAC")
            .to_ascii_lowercase();
        let dir = vm_dir(&self.home(), name);
        let entry = BaseEntry {
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
            vm_mac: Some(mac),
            vm_file_identity: verify::identity(&dir),
            final_fingerprint: verify::fingerprint(&dir),
            signature: Signature {
                policy: SignaturePolicy::DigestOnly,
                result: SignatureResult::NotChecked,
            },
            clones: BTreeMap::new(),
            clone_disks: BTreeMap::new(),
        };
        self.records()
            .update(|record| record.bases.insert(name.to_owned(), entry))
            .expect("record the base");
    }

    /// Clones `vm` from a seeded base and starts it under its shim; its instance id.
    async fn boot(&self, vm: &OwnedVmName) -> ProviderInstanceId {
        let base = BaseName::new("mfb-link").expect("a valid base name");
        self.seed_base(base.as_str());
        let provider = TartProvider::new(self.config(), self.records());
        let id = provider.clone_vm(&base, vm).await.expect("the clone");
        provider
            .start(
                vm,
                &RunOptions {
                    resources: Resources::new(2 * 1024 * 1024 * 1024, 0),
                },
            )
            .await
            .expect("the start");
        id
    }

    /// The SHA-256 of each config the fake's launcher read on stdin for `vm`.
    fn delivered(&self, vm: &OwnedVmName) -> Vec<String> {
        let path = self.home().join("vms").join(format!("{vm}.json"));
        let record: Value =
            serde_json::from_slice(&std::fs::read(path).expect("vm record")).expect("vm json");
        record["guest_launches"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|l| l["argv"] == serde_json::json!([LAUNCHER]))
            .filter_map(|l| l["stdin_sha256"].as_str().map(str::to_owned))
            .collect()
    }

    /// Every byte the fake, the shims and the transport wrote: the fake's invocation log (argv
    /// and each child's environment included), every file under the home, the shims and the
    /// diagnostics directory.
    fn recorded_text(&self) -> Vec<String> {
        let mut texts = Vec::new();
        let mut stack = vec![self.home(), self.shims(), self.diagnostics()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(bytes) = std::fs::read(&path) {
                    texts.push(String::from_utf8_lossy(&bytes).into_owned());
                }
            }
        }
        texts
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let listed = self.tart(&["list", "--format", "json", "--source", "local"]);
        let entries: Vec<Value> = serde_json::from_slice(&listed.stdout).unwrap_or_default();
        for entry in entries.iter().filter(|e| e["Running"] == true) {
            if let Some(name) = entry["Name"].as_str() {
                let _stopped = self.tart(&["stop", name, "--timeout", "5"]);
            }
        }
        for dir in std::fs::read_dir(self.shims())
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| e.path())
        {
            let released = || shim::read(&dir).shim != Shim::Alive;
            let _ = try_wait_until(RELEASE, "a shim to exit after its VM stopped", released);
        }
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
}

#[test]
fn should_deliver_the_config_on_the_guests_stdin_only_when_a_launch_crosses_the_link() {
    runtime().block_on(async {
        // given
        let world = World::new();
        let attempt = AttemptId::from_u128(0x1a);
        let vm = OwnedVmName::new(
            PoolName::sample(),
            HostId::sample(),
            Slot::new(1),
            Generation::FIRST,
            attempt,
        );
        let instance = world.boot(&vm).await;
        let handle = SecretHandle {
            handle_id: SecretHandleId::from_u128(0x5ec),
        };
        let config = ["link", "launch", "config", &attempt.to_string()].join("-");
        let broker = Arc::new(JitBroker::new());
        broker
            .put(
                handle.handle_id,
                JitScope {
                    attempt,
                    epoch: EPOCH,
                },
                Secret::new(config.clone()),
            )
            .unwrap_or_else(|_| panic!("the first put"));
        let launches = SharedLaunches::default();
        let transport =
            TartGuestTransport::new(world.config(), world.guest_config(), launches.clone());
        let (dp_link, cp_link) = inproc::link(
            inproc::DEFAULT_CAPACITY,
            SUPPORTED.to_vec(),
            SUPPORTED.to_vec(),
        );
        let mut dp_link = dp_link.with_launch_configs(Arc::new(launches.clone()));
        let mut cp_link = cp_link.with_launch_configs(broker.clone());
        let hello = Hello {
            host_id: HostId::sample(),
            boot_id: "boot-1".to_owned(),
            dp_version: "0.1.0".to_owned(),
            supported: SUPPORTED.to_vec(),
            capabilities: BTreeSet::new(),
            epoch_floor: Epoch::new(2),
        };
        let welcome = Welcome {
            epoch: EPOCH,
            mode: Mode::Normal,
            accepted_version: CURRENT,
            cp_clock: None,
        };
        let (entered, opened) = tokio::join!(dp_link.hello(hello), async {
            cp_link.next_hello().await?;
            cp_link.welcome(welcome.clone()).await
        });
        assert_eq!(entered, Ok(welcome));
        assert_eq!(opened, Ok(()));
        let (cp_directives, _cp_observations) = cp_link.split().expect("a session");
        let (mut dp_directives, _dp_observations) = dp_link.split().expect("a session");
        let launch = Directive {
            directive_id: DirectiveId::from_u128(0xd1),
            epoch: EPOCH,
            attempt: Some(AttemptRef {
                attempt_id: attempt,
                generation: Generation::FIRST,
            }),
            deadline: Deadline::at(Millis::new(u64::MAX)),
            kind: DirectiveKind::LaunchRunner {
                vm_name: vm.clone(),
                launch: handle,
                instance_id: Some(instance.clone()),
            },
        };

        // when
        // the relay sends the launch and resends it; the executor launches what it received
        // first and answers the repeat from its table (`executesAtMostOncePerEpoch`)
        cp_directives.send(launch.clone()).await.expect("the send");
        cp_directives
            .send(launch.clone())
            .await
            .expect("the resend");
        let first = dp_directives.recv().await.expect("the launch");
        let second = dp_directives.recv().await.expect("its resend");
        let launched = transport.launch(&vm, &instance, handle).await;

        // then
        assert_eq!((first, second), (launch.clone(), launch));
        assert_eq!(launched, Ok(LaunchAnswer::Delivered));
        assert_eq!(world.delivered(&vm), vec![sha256_hex(config.as_bytes())]);
        assert!(!broker.holds(handle.handle_id), "the link took it once");
        assert!(
            launches.take(handle, &vm, &instance).is_none(),
            "the store holds it no more"
        );
        let leaks: Vec<String> = world
            .recorded_text()
            .into_iter()
            .filter(|text| text.contains(&config))
            .collect();
        assert!(leaks.is_empty(), "the config left stdin: {leaks:?}");
    });
}
