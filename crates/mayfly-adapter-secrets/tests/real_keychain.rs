//! The generic `SecretProvider` contract (SP-01..SP-08) on the real Keychain through
//! `KeychainProvider::system` (SEC-02, HW-01): macOS only, with the `real-keychain` feature, run
//! by `cargo xtask hw01`, which creates a disposable keychain, adds it to the user's search list
//! for the run (restoring the list afterwards) and names it in `MAYFLY_REAL_KEYCHAIN`, with a
//! service prefix of its own in `MAYFLY_REAL_KEYCHAIN_SERVICE`. Items are added to that keychain
//! only, with `security add-generic-password -U`; the provider finds them through the search
//! list, as it finds the operator's. With the feature on and no keychain named, the test fails: it
//! is never skipped.

#![cfg(all(target_os = "macos", feature = "real-keychain"))]

use std::ffi::OsString;
use std::process::{Command, Stdio};

use mayfly_adapter_secrets::{DEFAULT_DEADLINE, KeychainProvider, PASSED_VARIABLES};
use mayfly_domain::SecretRef;
use mayfly_testkit::contracts::secrets::{SecretSourceFactory, check_secret_contract};

fn named(variable: &str) -> String {
    std::env::var(variable).unwrap_or_else(|_| panic!("{variable}: run through `cargo xtask hw01`"))
}

struct RealKeychain {
    keychain: String,
    service: String,
    next: u32,
    provider: KeychainProvider,
}

impl RealKeychain {
    fn new() -> Self {
        let environment: Vec<(OsString, OsString)> = PASSED_VARIABLES
            .iter()
            .filter_map(|name| std::env::var_os(name).map(|v| (OsString::from(name), v)))
            .collect();
        Self {
            keychain: named("MAYFLY_REAL_KEYCHAIN"),
            service: named("MAYFLY_REAL_KEYCHAIN_SERVICE"),
            next: 0,
            provider: KeychainProvider::system(environment, DEFAULT_DEADLINE),
        }
    }

    fn set(&self, reference: &SecretRef, value: &str) {
        let SecretRef::Keychain { service, account } = reference else {
            panic!("not a keychain reference");
        };
        let status = Command::new("/usr/bin/security")
            .args([
                "add-generic-password",
                "-U",
                "-s",
                service,
                "-a",
                account,
                "-w",
                value,
            ])
            .arg(&self.keychain)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .expect("run security");
        assert!(status.success(), "security add-generic-password: {status}");
    }
}

impl SecretSourceFactory for RealKeychain {
    type Provider = KeychainProvider;

    fn provider(&self) -> &KeychainProvider {
        &self.provider
    }

    fn install(&mut self, value: &str) -> SecretRef {
        self.next += 1;
        let reference = SecretRef::Keychain {
            service: format!("{}-{}", self.service, self.next),
            account: "app-key".to_owned(),
        };
        self.set(&reference, value);
        reference
    }

    fn rotate(&mut self, reference: &SecretRef, value: &str) {
        self.set(reference, value);
    }

    fn absent(&mut self) -> SecretRef {
        SecretRef::Keychain {
            service: format!("{}-absent", self.service),
            account: "app-key".to_owned(),
        }
    }
}

#[test]
fn should_pass_the_secret_contract_when_the_keychain_is_real() {
    // given
    let mut factory = RealKeychain::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    // when
    let report = runtime.block_on(check_secret_contract(&mut factory));

    // then
    assert!(report.is_green(), "{report}");
}
