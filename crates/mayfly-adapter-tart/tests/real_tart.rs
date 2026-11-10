//! The `VmProvider` contract against real Tart (HW-01): macOS only, with the `real-tart`
//! feature, in the dedicated `TART_HOME` the hardware lane names through
//! `cargo xtask test --only real-tart --real-tart-home <DIR>` (which sets
//! `MAYFLY_REAL_TART_HOME`). The home must hold a local base `mfb-contract` (the lane prepares
//! it). With the feature on and no home, the test fails: it is never skipped.
//!
//! The cases that need an injected fault (VP-11 hangs, VP-12 failed listings) and the child
//! environments (VP-16) are the fake-tart run's: real Tart cannot be made to hang on cue, and it
//! keeps no record of its environment. Signals to `tart run` are read from the shims' `exit.json`
//! (a run a signal ended says which).

#![cfg(all(target_os = "macos", feature = "real-tart"))]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use mayfly_adapter_tart::shim::{self, ExitRecord};
use mayfly_adapter_tart::{Deadlines, ShimProgram, TartConfig, TartProvider};
use mayfly_data::BaseName;
use mayfly_testkit_data::Op;
use mayfly_testkit_data::provider_contract::{
    ListingFault, ProviderCase, ProviderWorld, check_case,
};

fn home() -> PathBuf {
    let home = std::env::var_os("MAYFLY_REAL_TART_HOME")
        .expect("MAYFLY_REAL_TART_HOME: run through `cargo xtask test --only real-tart`");
    PathBuf::from(home)
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

struct RealTart {
    shims: tempfile::TempDir,
    provider: TartProvider,
}

impl RealTart {
    fn new() -> Self {
        let shims = tempfile::tempdir().expect("tempdir");
        let provider = TartProvider::new(Self::config(shims.path()));
        Self { shims, provider }
    }

    fn config(shims: &Path) -> TartConfig {
        TartConfig {
            tart: on_path("tart"),
            tart_home: home(),
            shims: shims.to_path_buf(),
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

    fn tart(&self, args: &[&str]) -> std::process::Output {
        Command::new(on_path("tart"))
            .env_clear()
            .env("TART_HOME", home())
            .env("TART_NO_AUTO_PRUNE", "1")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("run tart")
    }
}

impl Drop for RealTart {
    fn drop(&mut self) {
        let listed = self.tart(&["list", "--format", "json", "--source", "local"]);
        let entries: Vec<serde_json::Value> =
            serde_json::from_slice(&listed.stdout).unwrap_or_default();
        for name in entries.iter().filter_map(|e| e["Name"].as_str()) {
            if name.starts_with("mf-") || name.ends_with("-copy") || name == "foreign-vm" {
                let _stopped = self.tart(&["stop", name, "--timeout", "30"]);
                let _deleted = self.tart(&["delete", name]);
            }
        }
    }
}

impl ProviderWorld for RealTart {
    type Provider = TartProvider;

    fn provider(&self) -> &TartProvider {
        &self.provider
    }

    fn restart(&mut self) {
        self.provider = TartProvider::new(Self::config(self.shims.path()));
    }

    fn seed_base(&self, base: &BaseName) {
        assert!(
            self.tart(&["get", base.as_str()]).status.success(),
            "the hardware lane prepares the base {base}"
        );
    }

    fn seed_foreign(&self, name: &str) {
        assert!(self.tart(&["clone", "mfb-contract", name]).status.success());
    }

    fn exists(&self, name: &str) -> Option<bool> {
        let out = self.tart(&["get", name, "--format", "json"]);
        out.status.success().then(|| {
            serde_json::from_slice::<serde_json::Value>(&out.stdout)
                .ok()
                .and_then(|v| v["Running"].as_bool())
                .unwrap_or(false)
        })
    }

    fn hang_next(&self, _op: Op) {
        panic!("real Tart cannot be made to hang on cue: VP-11 runs over the fake");
    }

    fn fail_next_listing(&self, _fault: ListingFault) {
        panic!("real Tart cannot be made to fail a listing on cue: VP-12 runs over the fake");
    }

    fn signals_to_runs(&self) -> Vec<String> {
        let Ok(dirs) = std::fs::read_dir(self.shims.path()) else {
            return Vec::new();
        };
        dirs.filter_map(Result::ok)
            .filter_map(|d| shim::read_json::<ExitRecord>(&d.path().join(shim::EXIT)))
            .filter_map(|exit| exit.signal.map(|s| format!("signal {s}")))
            .collect()
    }

    fn child_environments(&self) -> Vec<BTreeSet<String>> {
        Vec::new()
    }
}

const ON_REAL_TART: [ProviderCase; 13] = [
    ProviderCase::CloneGivesFreshIdentity,
    ProviderCase::CloneOntoExistingRefused,
    ProviderCase::CloneNeedsLocalBase,
    ProviderCase::StartRuns,
    ProviderCase::StartOfRunningRefused,
    ProviderCase::StopByProof,
    ProviderCase::StopRefusesMismatch,
    ProviderCase::DeleteEnsuresAbsent,
    ProviderCase::DeleteRefusesMismatch,
    ProviderCase::DeleteTouchesOnlyTarget,
    ProviderCase::InventoryExact,
    ProviderCase::RestartReadopts,
    ProviderCase::NoSignalToRun,
];

#[test]
fn should_pass_the_vm_provider_contract_when_tart_is_real() {
    // given
    let make = RealTart::new;

    // when
    let failures: Vec<String> = ON_REAL_TART
        .iter()
        .filter_map(|case| {
            check_case(&make, *case)
                .err()
                .map(|why| format!("{case}: {why}"))
        })
        .collect();

    // then
    assert!(failures.is_empty(), "{failures:#?}");
}
