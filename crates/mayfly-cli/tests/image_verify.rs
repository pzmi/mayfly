//! `mayfly image verify <manifest>` end to end (OPS-02): the built binary over the image
//! manifest fixtures of `mayfly-composition` (`tests/fixtures/manifest/`), its output and its
//! exit codes: 0 valid, 78 invalid or unreadable, 64 for a command line that does not parse.

use std::path::PathBuf;
use std::process::{Command, Output};

fn fixture(family: &str, name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../mayfly-composition/tests/fixtures/manifest")
        .join(family)
        .join(name)
}

fn mayfly(args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mayfly"));
    command.env_clear().args(args);
    command.envs(std::env::var_os("LLVM_PROFILE_FILE").map(|v| ("LLVM_PROFILE_FILE", v)));
    command.output().expect("run mayfly")
}

#[test]
fn should_print_the_image_and_exit_zero_when_the_manifest_is_valid() {
    // given
    let path = fixture("valid", "macos.toml");

    // when
    let output = mayfly(&["image", "verify", &path.display().to_string()]);

    // then
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!(
            "image manifest valid: ghcr.io/acme/runner-macos@sha256:{} os=macos arch=arm64 runner=2.330.0 revision=1\n",
            "a".repeat(64)
        )
    );
}

#[test]
fn should_report_the_tag_and_exit_78_when_the_manifest_names_only_a_tag() {
    // given
    let path = fixture("invalid", "tag_only.toml");
    let golden = std::fs::read_to_string(fixture("invalid", "tag_only.errors.txt"))
        .expect("the golden report");

    // when
    let output = mayfly(&["image", "verify", &path.display().to_string()]);

    // then
    assert_eq!(output.status.code(), Some(78));
    assert_eq!(String::from_utf8_lossy(&output.stderr), golden);
    assert!(output.stdout.is_empty());
}

#[test]
fn should_exit_78_when_the_manifest_cannot_be_read() {
    // given
    let path = fixture("valid", "no-such-manifest.toml");

    // when
    let output = mayfly(&["image", "verify", &path.display().to_string()]);

    // then
    assert_eq!(output.status.code(), Some(78));
    assert!(
        String::from_utf8_lossy(&output.stderr).starts_with("cannot read image manifest "),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn should_print_the_usage_and_exit_64_when_verify_has_no_manifest() {
    // given
    let args = ["image", "verify"];

    // when
    let output = mayfly(&args);

    // then
    assert_eq!(output.status.code(), Some(64));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "usage: mayfly image verify <manifest>\n"
    );
}
