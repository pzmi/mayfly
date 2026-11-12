//! The start-up sizing report: each value in effect and where it came from (RT-06, ADR 0052's
//! amendment of 2026-11-12).

use std::collections::BTreeSet;

use mayfly_composition::config::HostConfig;
use mayfly_composition::sizing::{Source, Value, report};
use mayfly_data::HostReading;
use mayfly_domain::{ImageDigest, Resources};
use mayfly_protocol::{Capacity, ResourceDimension};

const GIB: u64 = 1 << 30;

/// What the probe measured on a 64 GiB, 10-CPU host with 8 GiB, 2-CPU VMs: 4 slots and a
/// 16 GiB reserve.
fn reading() -> HostReading {
    HostReading {
        totals: Capacity {
            vm_slots: 4,
            macos_guests: 2,
            resources: Resources::new(64 * GIB, 1000 * GIB),
        },
        reserve: Capacity {
            vm_slots: 0,
            macos_guests: 0,
            resources: Resources::new(16 * GIB, 50 * GIB),
        },
        foreign: Capacity::default(),
        foreign_unsized: BTreeSet::new(),
        disk_low_water: false,
        memory_pressure: false,
        hypervisor_limit: false,
    }
}

fn lines(host: &HostConfig, reading: Option<&HostReading>) -> Vec<String> {
    report(host, reading, &BTreeSet::new())
        .to_string()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn should_name_each_source_when_nothing_is_configured() {
    // given
    let host = HostConfig::default();

    // when
    let lines = lines(&host, Some(&reading()));

    // then
    assert_eq!(
        lines,
        [
            "mayflyd sizing vm_slots=4 source=measured",
            "mayflyd sizing macos_guests=2 source=apple_limit",
            "mayflyd sizing reserve_memory_mib=16384 source=measured",
            "mayflyd sizing reserve_cpus=2 source=default",
            "mayflyd sizing image_disk_gib=per_image source=manifest",
            "mayflyd sizing clone_disk_gib=per_image source=manifest",
        ]
    );
}

#[test]
fn should_name_configured_sources_when_everything_is_configured() {
    // given: every key set, macOS guests above Apple's limit
    let host = HostConfig {
        reserve_memory_mib: Some(8192),
        reserve_disk_gib: 50,
        reserve_cpus: Some(4),
        vm_slots: Some(3),
        macos_guests: Some(5),
        image_disk_gib: Some(80),
        clone_disk_gib: Some(90),
    };

    // when
    let lines = lines(&host, Some(&reading()));

    // then: configured values override what was measured
    assert_eq!(
        lines,
        [
            "mayflyd sizing vm_slots=3 source=configured",
            "mayflyd sizing macos_guests=2 source=configured_capped_at_apple_limit",
            "mayflyd sizing reserve_memory_mib=8192 source=configured",
            "mayflyd sizing reserve_cpus=4 source=configured",
            "mayflyd sizing image_disk_gib=80 source=configured",
            "mayflyd sizing clone_disk_gib=90 source=configured",
        ]
    );
}

#[test]
fn should_report_unknown_slots_when_the_probe_could_not_measure_them() {
    // given: the CPUs could not be read, so the probe left slots unsized
    let mut unread = reading();
    unread.totals.vm_slots = 0;
    unread.foreign_unsized = BTreeSet::from([ResourceDimension::VmSlots]);

    // when
    let measured = report(&HostConfig::default(), Some(&unread), &BTreeSet::new());
    let unprobed = report(&HostConfig::default(), None, &BTreeSet::new());

    // then: unknown, never zero or a guess
    assert_eq!(
        (measured.values[0].value, measured.values[0].source),
        (Value::Unknown, Source::Measured)
    );
    assert_eq!(unprobed.values[0].value, Value::Unknown);
    assert_eq!(
        unprobed.values[2].value,
        Value::Unknown,
        "the memory reserve"
    );
    assert!(
        measured
            .to_string()
            .contains("mayflyd sizing vm_slots=unknown source=measured reason=cpus_unreadable\n"),
        "{measured}"
    );
    assert_eq!(unprobed.values[0].reason, Some("probe_unreadable"));
}

#[test]
fn should_name_each_unmeasured_image_when_its_manifest_did_not_read() {
    // given
    let digest = ImageDigest::new(&format!("sha256:{}", "a".repeat(64))).expect("a digest");

    // when
    let text = report(
        &HostConfig::default(),
        Some(&reading()),
        &BTreeSet::from([digest.clone()]),
    )
    .to_string();

    // then
    assert!(
        text.ends_with(&format!(
            "mayflyd sizing unmeasured_image={digest} reason=manifest_unreadable\n"
        )),
        "{text}"
    );
}
