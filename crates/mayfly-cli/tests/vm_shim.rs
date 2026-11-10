//! `mayfly vm-shim` end to end: the subcommand reaches the shim and keeps its exit codes. (Its
//! behaviour with a VM is `mayfly-adapter-tart`'s `tests/shim.rs`, which runs this binary.)

use std::process::Command;

#[test]
fn should_exit_with_the_usage_code_when_vm_shim_gets_no_arguments() {
    // given
    let mut command = Command::new(env!("CARGO_BIN_EXE_mayfly"));
    command.env_clear().arg("vm-shim");
    command.envs(std::env::var_os("LLVM_PROFILE_FILE").map(|v| ("LLVM_PROFILE_FILE", v)));

    // when
    let output = command.output().expect("run mayfly");

    // then
    assert_eq!(output.status.code(), Some(64));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("mayfly vm-shim: --tart is required"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn should_print_the_version_when_given_no_command() {
    // given
    let mut command = Command::new(env!("CARGO_BIN_EXE_mayfly"));
    command.env_clear();
    command.envs(std::env::var_os("LLVM_PROFILE_FILE").map(|v| ("LLVM_PROFILE_FILE", v)));

    // when
    let output = command.output().expect("run mayfly");

    // then
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("mayfly "));
}
