//! Black-box checks of the guard every `cargo xtask` command passes first: the binary knows
//! the checkout it was built from, so it refuses to run from anywhere else (a target directory
//! shared between worktrees would otherwise check the wrong tree), and refuses a
//! `CLIPPY_CONF_DIR` that would switch off the pure crates' `clippy.toml`.

use std::path::Path;
use std::process::Command;

#[test]
fn should_refuse_to_run_when_current_directory_is_outside_its_checkout() {
    // given
    let elsewhere = tempfile::TempDir::new().expect("tempdir");

    // when
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("list-checks")
        .current_dir(elsewhere.path())
        .env_remove("CLIPPY_CONF_DIR")
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
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
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
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("list-checks")
        .current_dir(root)
        .env_remove("CLIPPY_CONF_DIR")
        .output()
        .expect("xtask runs");

    // then
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
