//! The `GuestTransport` contract over `mayfly-fake-tart` (ADR 0038, ADR 0082): VMs cloned and
//! started by the `TartProvider` under their real shims, so the run-lock check reads the lock a
//! fake `tart run` holds as the shim's child; the fake's emulated launcher answers the guest
//! side. Plus the transport's own promises the contract does not state: the artefact's mode,
//! and the argv of every `tart exec` it ran.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use mayfly_adapter_tart::guest::LAUNCHER;
use mayfly_adapter_tart::shim::{self, RunRecord};
use mayfly_adapter_tart::{GuestConfig, GuestDeadlines, TartGuestTransport, TartProvider};
use mayfly_data::{BaseName, GuestTransport, LaunchAnswer, RunOptions, SecretHandle, VmProvider};
use mayfly_domain::{ProviderInstanceId, Resources};

use mayfly_testkit_data::MemorySecrets;
use mayfly_testkit_data::guest_contract::{
    CANARY, GuestWorld, check_guest_transport_contract, handle, sha256_hex,
};
use mayfly_testkit_data::provider_contract::vm;
use serde_json::Value;

use crate::support::Fake;

/// The longest runner log the world exports.
const DIAGNOSTICS_LIMIT: usize = 4096;

fn guest_config(fake: &Fake) -> GuestConfig {
    let call = Duration::from_secs(5);
    GuestConfig {
        deadlines: GuestDeadlines {
            verify: call,
            launch: call,
            status: call,
            diagnostics: call,
        },
        claims_limit: 4096,
        diagnostics_limit: DIAGNOSTICS_LIMIT,
        diagnostics_dir: fake.dir.path().join("diagnostics"),
    }
}

/// The contract's world over the fake binary.
pub struct FakeTartGuests {
    fake: Fake,
    provider: TartProvider,
    secrets: MemorySecrets,
    transport: TartGuestTransport<MemorySecrets>,
    rules: RefCell<String>,
    ids: RefCell<BTreeMap<u32, ProviderInstanceId>>,
}

impl FakeTartGuests {
    pub fn new() -> Self {
        let fake = Fake::new();
        let provider = fake.provider();
        let secrets = MemorySecrets::default();
        let transport =
            TartGuestTransport::new(fake.config(), guest_config(&fake), secrets.clone());
        Self {
            fake,
            provider,
            secrets,
            transport,
            rules: RefCell::new(String::new()),
            ids: RefCell::new(BTreeMap::new()),
        }
    }

    fn rule(&self, body: &str) {
        let mut rules = self.rules.borrow_mut();
        rules.push_str(&format!("[[rule]]\ncommand = \"exec\"\n{body}\n"));
        self.fake.faults(&rules);
    }

    fn base() -> BaseName {
        BaseName::new("mfb-guests").expect("a valid base name")
    }

    /// The fake's record of VM `n`.
    fn record(&self, n: u32) -> Value {
        let path = self.fake.home().join("vms").join(format!("{}.json", vm(n)));
        serde_json::from_slice(&std::fs::read(path).expect("vm record")).expect("vm json")
    }
}

impl GuestWorld for FakeTartGuests {
    type Transport = TartGuestTransport<MemorySecrets>;

    fn transport(&self) -> &Self::Transport {
        &self.transport
    }

    fn restart(&mut self) {
        self.secrets = MemorySecrets::default();
        self.transport = TartGuestTransport::new(
            self.fake.config(),
            guest_config(&self.fake),
            self.secrets.clone(),
        );
    }

    async fn boot(&self, n: u32) -> Result<ProviderInstanceId, String> {
        if self.fake.exists(Self::base().as_str()).is_none() {
            self.fake.seed_base(Self::base().as_str());
        }
        let id = self
            .provider
            .clone_vm(&Self::base(), &vm(n))
            .await
            .map_err(|e| format!("clone of VM {n}: {e:?}"))?;
        let options = RunOptions {
            resources: Resources::new(2 * 1024 * 1024 * 1024, 0),
        };
        self.provider
            .start(&vm(n), &options)
            .await
            .map_err(|e| format!("start of VM {n}: {e:?}"))?;
        self.ids.borrow_mut().insert(n, id.clone());
        Ok(id)
    }

    fn hold(&self, handle: SecretHandle, n: u32, config: &str) {
        if let Some(id) = self.ids.borrow().get(&n) {
            let _held = self.secrets.put(handle, &vm(n), id, config);
        }
    }

    /// The launch exec (the second launcher exec of VM `n`: the status read comes first)
    /// echoes the config on stderr, then fails.
    fn fails_after_config(&self, n: u32) {
        self.rule(&format!(
            "vm = \"{}\"\narg0 = \"{LAUNCHER}\"\nskip = 1\ntimes = 1\naction = \"fail_after_effect\"\necho_stdin = true",
            vm(n)
        ));
    }

    /// Records another live process as the shim's child: the lock is then held by a process
    /// that is not the recorded child, as when some other `tart run` holds the VM.
    fn foreign_lock(&self, n: u32) {
        let dir = shim::dir(&self.fake.shims(), &vm(n).to_string());
        let path = dir.join(shim::RECORD);
        let mut record: RunRecord = shim::read_json(&path).expect("shim.json");
        record.child_pid = std::process::id();
        std::fs::write(&path, serde_json::to_vec(&record).expect("encode")).expect("rewrite");
    }

    fn change_mac(&self, n: u32) {
        let out = self.fake.tart(&["set", &vm(n).to_string(), "--random-mac"]);
        assert!(out.status.success(), "set --random-mac");
    }

    fn misroute(&self, n: u32, to: u32) {
        self.rule(&format!("vm = \"{}\"\nroute_to = \"{}\"", vm(n), vm(to)));
    }

    fn without_agent(&self, n: u32) {
        self.rule(&format!(
            "vm = \"{}\"\naction = \"fail_before_effect\"\nstderr = \"Failed to connect to the VM using its control socket: is the Tart Guest Agent running?\"",
            vm(n)
        ));
    }

    fn runner_exits(&self, n: u32, code: i32) {
        self.rule(&format!(
            "vm = \"{}\"\narg0 = \"{LAUNCHER}\"\njob_exit_code = {code}",
            vm(n)
        ));
    }

    fn write_log(&self, n: u32, text: &str) {
        let dir = self.fake.home().join("guest-logs");
        std::fs::create_dir_all(&dir).expect("guest-logs");
        std::fs::write(dir.join(format!("{}.log", vm(n))), text).expect("guest log");
    }

    fn diagnostics_limit(&self) -> usize {
        DIAGNOSTICS_LIMIT
    }

    fn delivered(&self, n: u32) -> Vec<String> {
        self.record(n)["guest_launches"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|l| l["argv"] == serde_json::json!([LAUNCHER]))
            .filter_map(|l| l["stdin_sha256"].as_str().map(str::to_owned))
            .collect()
    }

    fn artefact(&self, n: u32) -> Option<Vec<u8>> {
        std::fs::read(
            guest_config(&self.fake)
                .diagnostics_dir
                .join(format!("{}.log", vm(n))),
        )
        .ok()
    }

    fn recorded_text(&self) -> Vec<String> {
        let mut texts: Vec<String> = self.fake.log().iter().map(Value::to_string).collect();
        let mut files = Vec::new();
        for root in [
            self.fake.home(),
            self.fake.shims(),
            guest_config(&self.fake).diagnostics_dir,
        ] {
            let mut stack = vec![root];
            while let Some(dir) = stack.pop() {
                for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else {
                        files.push(path);
                    }
                }
            }
        }
        texts.extend(
            files
                .iter()
                .filter_map(|f| std::fs::read(f).ok())
                .map(|b| String::from_utf8_lossy(&b).into_owned()),
        );
        texts
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

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
}

#[test]
fn should_pass_the_guest_transport_contract_when_the_transport_drives_fake_tart() {
    // given
    let make = FakeTartGuests::new;

    // when
    let report = check_guest_transport_contract(&make);

    // then
    assert!(report.is_green(), "{report}");
}

#[test]
fn should_name_only_the_vm_and_the_launcher_in_argv_when_a_launch_is_delivered() {
    runtime().block_on(async {
        // given
        let world = FakeTartGuests::new();
        let id = world.boot(1).await.expect("boot");
        world.hold(handle(), 1, CANARY);

        // when
        let launched = world.transport().launch(&vm(1), &id, handle()).await;

        // then
        assert_eq!(launched, Ok(LaunchAnswer::Delivered));
        let execs: Vec<Vec<String>> = world
            .fake
            .events("invoke")
            .iter()
            .filter_map(|r| r["argv"].as_array().cloned())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .filter(|argv: &Vec<String>| argv.first().is_some_and(|a| a == "exec"))
            .collect();
        let name = vm(1).to_string();
        assert!(
            execs.contains(&vec![
                "exec".to_owned(),
                "-i".to_owned(),
                name.clone(),
                LAUNCHER.to_owned()
            ]),
            "{execs:?}"
        );
        assert_eq!(world.delivered(1), vec![sha256_hex(CANARY.as_bytes())]);
        assert!(world.secrets.is_empty(), "the config was taken");
    });
}

#[test]
fn should_write_the_artefact_owner_only_when_diagnostics_are_exported_into_a_wider_directory() {
    runtime().block_on(async {
        // given
        let world = FakeTartGuests::new();
        world.boot(1).await.expect("boot");
        world.write_log(1, "runner listening\n");
        let dir = guest_config(&world.fake).diagnostics_dir;
        std::fs::create_dir(&dir).expect("a pre-existing directory");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        // when
        let exported = world.transport().export_diagnostics(&vm(1)).await;

        // then
        assert!(
            matches!(exported, Ok(mayfly_data::Diagnostics::Exported { .. })),
            "{exported:?}"
        );
        let mode = |p: &std::path::Path| {
            std::fs::metadata(p).expect("metadata").permissions().mode() & 0o777
        };
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(format!("{}.log", vm(1)))), 0o600);
    });
}
