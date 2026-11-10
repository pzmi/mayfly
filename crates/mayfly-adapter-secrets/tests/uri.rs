//! `UriSecretProvider` picks the backend by scheme, and composition's construction keeps an
//! `env:` secret out of every child (ADR 0033, ADR 0058).

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::sync::{Mutex, MutexGuard, PoisonError};

use mayfly_adapter_secrets::{
    DEFAULT_DEADLINE, EnvProvider, ExecProvider, FileProvider, UriSecretProvider,
};
use mayfly_control::secrets::{Refusal, SecretError, SecretProvider};
use mayfly_domain::SecretRef;

/// Serialises the tests that write a script and run it (see `tests/contract.rs`: `ETXTBSY`).
static SPAWN: Mutex<()> = Mutex::new(());

fn spawn_lock() -> MutexGuard<'static, ()> {
    SPAWN.lock().unwrap_or_else(PoisonError::into_inner)
}

fn run<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

fn value(got: Result<mayfly_domain::Secret<String>, SecretError>) -> Result<String, Refusal> {
    got.map(|s| s.expose().clone()).map_err(|e| e.refusal)
}

#[test]
fn should_resolve_each_scheme_with_its_backend_when_references_are_mixed() {
    // given
    let _spawning = spawn_lock();
    let root = tempfile::tempdir().expect("tempdir");
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).expect("chmod");
    let uid = fs::metadata(root.path()).expect("stat").uid();
    let file = root.path().join("key");
    fs::write(&file, "synthetic-file\n").expect("write");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("chmod");
    let script = root.path().join("get");
    fs::write(&script, "#!/bin/sh\nprintf 'synthetic-exec'\n").expect("write");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("chmod");
    let provider = UriSecretProvider::new(
        FileProvider::owned_by(uid),
        EnvProvider::from_values([("APP_KEY".to_owned(), b"synthetic-env".to_vec())]),
        ExecProvider::new(uid, Vec::new(), DEFAULT_DEADLINE),
    );

    // when
    let got = run(async {
        [
            value(provider.resolve(&SecretRef::File { path: file }).await),
            value(
                provider
                    .resolve(&SecretRef::Env {
                        name: "APP_KEY".to_owned(),
                    })
                    .await,
            ),
            value(provider.resolve(&SecretRef::Exec { path: script }).await),
        ]
    });

    // then
    assert_eq!(
        got,
        [
            Ok("synthetic-file".to_owned()),
            Ok("synthetic-env".to_owned()),
            Ok("synthetic-exec".to_owned()),
        ]
    );
}

#[cfg(not(target_os = "macos"))]
#[test]
fn should_refuse_keychain_as_unsupported_when_not_on_macos() {
    // given
    let provider = UriSecretProvider::new(
        FileProvider::owned_by(0),
        EnvProvider::default(),
        ExecProvider::new(0, Vec::new(), DEFAULT_DEADLINE),
    );
    let reference = SecretRef::Keychain {
        service: "mayfly".to_owned(),
        account: "app-key".to_owned(),
    };

    // when
    let got = run(provider.resolve(&reference));

    // then
    assert_eq!(value(got), Err(Refusal::Unsupported));
}

#[test]
fn should_refuse_a_foreign_scheme_when_a_single_backend_is_asked() {
    // given
    let env = SecretRef::Env {
        name: "APP_KEY".to_owned(),
    };
    let file = SecretRef::File {
        path: "/nonexistent/key".into(),
    };

    // when
    let got = run(async {
        [
            value(FileProvider::owned_by(0).resolve(&env).await),
            value(EnvProvider::default().resolve(&file).await),
            value(
                ExecProvider::new(0, Vec::new(), DEFAULT_DEADLINE)
                    .resolve(&env)
                    .await,
            ),
        ]
    });

    // then
    assert_eq!(
        got,
        [
            Err(Refusal::Unsupported),
            Err(Refusal::Unsupported),
            Err(Refusal::Unsupported)
        ]
    );
}

#[test]
fn should_withhold_env_secrets_when_exec_runs_a_command() {
    // given: HOME is one of the variables a command may see, unless an env: reference reads it
    let _spawning = spawn_lock();
    let home: Option<OsString> = std::env::var_os("HOME");
    assert!(home.is_some(), "the test needs HOME in its environment");
    let root = tempfile::tempdir().expect("tempdir");
    let script = root.path().join("home");
    fs::write(&script, "#!/bin/sh\nprintf '%s' \"${HOME-withheld}\"\n").expect("write");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("chmod");
    let reference = SecretRef::Exec { path: script };
    let passing = ExecProvider::for_daemon(&[], DEFAULT_DEADLINE);
    let withholding = UriSecretProvider::from_process(
        &[SecretRef::Env {
            name: "HOME".to_owned(),
        }],
        DEFAULT_DEADLINE,
    );

    // when
    let got = run(async {
        (
            value(passing.resolve(&reference).await),
            value(withholding.resolve(&reference).await),
        )
    });

    // then
    assert_eq!(
        got.0.ok().map(OsString::from),
        home,
        "a basic variable reaches the command"
    );
    assert_eq!(got.1, Ok("withheld".to_owned()));
}

#[test]
fn should_withhold_captured_names_when_exec_was_built_with_them() {
    // given: an exec provider handed a variable an env: reference reads
    let _spawning = spawn_lock();
    let root = tempfile::tempdir().expect("tempdir");
    let uid = fs::metadata(root.path()).expect("stat").uid();
    let script = root.path().join("token");
    fs::write(
        &script,
        "#!/bin/sh\nprintf '%s' \"${APP_TOKEN-withheld}\"\n",
    )
    .expect("write");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("chmod");
    let provider = UriSecretProvider::new(
        FileProvider::owned_by(uid),
        EnvProvider::from_values([("APP_TOKEN".to_owned(), b"synthetic-token".to_vec())]),
        ExecProvider::new(
            uid,
            vec![(
                OsString::from("APP_TOKEN"),
                OsString::from("synthetic-token"),
            )],
            DEFAULT_DEADLINE,
        ),
    );

    // when
    let got = run(provider.resolve(&SecretRef::Exec { path: script }));

    // then
    assert_eq!(value(got), Ok("withheld".to_owned()));
}

#[test]
fn should_print_variable_names_only_when_a_provider_is_debugged() {
    // given
    let provider = UriSecretProvider::new(
        FileProvider::owned_by(0),
        EnvProvider::from_values([("APP_KEY".to_owned(), b"synthetic-env-value".to_vec())]),
        ExecProvider::new(
            0,
            vec![(
                OsString::from("HOME"),
                OsString::from("/synthetic/home/dir"),
            )],
            DEFAULT_DEADLINE,
        ),
    );

    // when
    let shown = format!("{provider:?} {provider:#?}");

    // then
    assert!(shown.contains("HOME"), "{shown}");
    assert!(!shown.contains("/synthetic/home/dir"), "{shown}");
    assert!(!shown.contains("synthetic-env-value"), "{shown}");
}
