//! `Os` on a real Linux host (DP-06's acceptance): every reading comes from the kernel.
#![cfg(target_os = "linux")]

use std::path::Path;

use mayfly_adapter_host::{HostSource, Os, slept};

fn here() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn should_read_a_volume_with_free_space_within_its_size_when_the_path_exists() {
    // given
    let os = Os::new();

    // when
    let volume = os.volume(here()).expect("statvfs of an existing path");

    // then
    assert!(volume.total_bytes > 0);
    assert!(volume.free_bytes <= volume.total_bytes);
}

#[test]
fn should_fail_rather_than_default_when_the_volume_path_does_not_exist() {
    // given
    let os = Os::new();

    // when
    let volume = os.volume(&here().join("no-such-directory-for-the-probe"));

    // then
    assert!(volume.is_err());
}

#[test]
fn should_read_available_memory_within_the_total_when_the_kernel_reports_both() {
    // given
    let os = Os::new();

    // when
    let memory = os.memory().expect("/proc/meminfo");

    // then
    assert!(memory.total_bytes > 0);
    assert!(memory.available_bytes <= memory.total_bytes);
}

#[test]
fn should_read_the_same_boot_id_twice_when_the_host_has_not_rebooted() {
    // given
    let os = Os::new();

    // when
    let first = os.boot_id().expect("the kernel's boot id");
    let second = os.boot_id().expect("the kernel's boot id");

    // then
    assert!(!first.is_empty());
    assert_eq!(first, second);
}

#[test]
fn should_read_no_sleep_when_two_clock_readings_are_taken_back_to_back() {
    // given
    let os = Os::new();
    let before = os.clocks().expect("the clocks");

    // when
    let now = os.clocks().expect("the clocks");

    // then: no suspend happens within a test; jitter between the two clocks stays far below
    // any wake gap an operator would set
    assert!(now.with_sleep >= before.with_sleep);
    assert!(slept(before, now).as_secs() < 1);
}

#[test]
fn should_read_at_least_one_cpu_when_the_kernel_reports_them() {
    // given
    let os = Os::new();

    // when
    let cpus = os.cpus().expect("the CPU count");

    // then
    assert!(cpus >= 1);
}
