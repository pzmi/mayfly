//! Black-box checks of the guard every `cargo xtask` command passes first: the binary knows
//! the checkout it was built from, so it refuses to run from anywhere else (a target directory
//! shared between worktrees would otherwise check the wrong tree), and refuses a
//! `CLIPPY_CONF_DIR` that would switch off the pure crates' `clippy.toml` or any other variable
//! that steers the build.

use std::path::Path;
use std::process::Command;

/// The build-steering variables `cargo llvm-cov` puts in a test's environment (it wraps rustc):
/// the xtask binary refuses to start with them (foundation.md section 3.6), so a test that runs
/// it removes them, as a developer's shell would not have them.
const STEERING: &[&str] = &[
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTFLAGS",
    "RUSTDOCFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_ENCODED_RUSTDOCFLAGS",
    "CLIPPY_CONF_DIR",
];

/// The xtask binary with [`STEERING`] removed from its environment.
fn xtask_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_xtask"));
    for name in STEERING {
        command.env_remove(name);
    }
    command
}

#[test]
fn should_refuse_to_run_when_current_directory_is_outside_its_checkout() {
    // given
    let elsewhere = tempfile::TempDir::new().expect("tempdir");

    // when
    let output = xtask_command()
        .arg("list-checks")
        .current_dir(elsewhere.path())
        .output()
        .expect("xtask runs");

    // then
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("outside that checkout"), "{stderr}");
    assert!(output.stdout.is_empty());
}

#[test]
fn should_refuse_to_run_when_clippy_conf_dir_is_set() {
    // given
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));

    // when
    let output = xtask_command()
        .arg("list-checks")
        .current_dir(root)
        .env("CLIPPY_CONF_DIR", root)
        .output()
        .expect("xtask runs");

    // then
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("CLIPPY_CONF_DIR is set"), "{stderr}");
}

#[test]
fn should_run_when_current_directory_is_inside_its_checkout() {
    // given
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));

    // when
    let output = xtask_command()
        .arg("list-checks")
        .current_dir(root)
        .output()
        .expect("xtask runs");

    // then
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn should_refuse_to_run_when_a_rustc_wrapper_is_set() {
    // given
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));

    // when
    let output = xtask_command()
        .arg("list-checks")
        .current_dir(root)
        .env("RUSTC_WRAPPER", "true")
        .env("CLIPPY_ARGS", "-Aclippy::all")
        .output()
        .expect("xtask runs");

    // then
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("CLIPPY_ARGS, RUSTC_WRAPPER set"),
        "{stderr}"
    );
    assert!(output.stdout.is_empty());
}
