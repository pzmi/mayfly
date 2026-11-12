//! Tart's output, parsed against the versions it was pinned for (plan §8.2).
//!
//! The shapes are those of Tart's `list --format json` and `get --format json`, pinned by the
//! fixtures in `tests/fixtures/tart-<version>/` (synthetic values in the shape the release's
//! source prints; its `README.md` names the source of each). Fields this adapter does not read
//! are ignored; a field it reads that is missing or of another type makes the whole output
//! unparseable, never a default (ADR 0052). A version outside [`SUPPORTED_VERSIONS`] is refused
//! by the probe (an unsupported Tart closes admission, R12).
//!
//! Units (Tart 2.41.1, `Commands/Get.swift`): `Memory` is MiB (`memorySize / 1024 / 1024`);
//! `Disk` is whole decimal gigabytes rounded down (`bytes / 1000 / 1000 / 1000`), and `null`
//! whenever Tart's read of the disk's size fails (`try?`: for one, an ASIF disk held open by its
//! running VM), which leaves the disk unknown here (ADR 0052; the provider then uses the ceiling
//! it recorded at clone time, `provider`). HW-01 confirms both on real Tart.
//!
//! Pinned by `tests::should_parse_every_pinned_fixture_when_the_version_is_supported`.

use mayfly_data::GuestOs;
use serde::Deserialize;

/// The Tart versions whose output this module was checked against: one directory of fixtures
/// each. Only 2.41.1 (TART-01): 2.28.0's `get` prints no MAC, its `exec` aborts when its
/// standard output is not a terminal, and its `clone` replaces an existing VM.
pub const SUPPORTED_VERSIONS: &[&str] = &["2.41.1"];

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
const GB: u64 = 1_000_000_000;

/// One entry of `tart list --format json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ListEntry {
    /// `local` or `OCI`.
    pub source: String,
    /// The VM's or image's name.
    pub name: String,
    /// Whether it runs.
    pub running: bool,
}

impl ListEntry {
    /// Whether it is a local VM rather than an entry of the OCI cache.
    #[must_use]
    pub fn is_local(&self) -> bool {
        self.source == "local"
    }
}

/// What `tart get --format json` says of one VM.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct VmInfo {
    /// `darwin` or `linux`.
    #[serde(rename = "OS")]
    pub os: String,
    /// Memory in MiB.
    #[serde(rename = "Memory")]
    pub memory_mib: u64,
    /// Disk size in Tart's GB; present but `null` when Tart's read of it failed.
    #[serde(rename = "Disk", deserialize_with = "present")]
    pub disk_gb: Option<u64>,
    /// The MAC address, the provider instance id (ADR 0026).
    #[serde(rename = "MACAddress")]
    pub mac: String,
    /// Whether it runs.
    #[serde(rename = "Running")]
    pub running: bool,
}

impl VmInfo {
    /// The guest, when Tart names one this adapter knows; otherwise unknown (ADR 0052).
    #[must_use]
    pub fn guest(&self) -> Option<GuestOs> {
        match self.os.as_str() {
            "darwin" => Some(GuestOs::Macos),
            "linux" => Some(GuestOs::Linux),
            _ => None,
        }
    }

    /// Memory in bytes.
    #[must_use]
    pub fn memory_bytes(&self) -> Option<u64> {
        self.memory_mib.checked_mul(MIB)
    }

    /// The disk ceiling in bytes, `None` when Tart printed none. Tart rounds the size down to
    /// whole decimal gigabytes, so the size is below `(Disk + 1) × 10⁹`; the ceiling is the
    /// larger of that and `Disk` GiB, never an undercount and never below the earlier reading.
    /// Pinned by `tests::should_count_the_disk_ceiling_above_tarts_rounded_size_when_reading_its_disk`.
    #[must_use]
    pub fn disk_bytes(&self) -> Option<u64> {
        let gb = self.disk_gb?;
        let decimal = gb.checked_add(1)?.checked_mul(GB)?;
        let binary = gb.checked_mul(GIB)?;
        Some(decimal.max(binary))
    }

    /// The MAC as an instance id's text: lowercase.
    #[must_use]
    pub fn instance_text(&self) -> String {
        self.mac.to_ascii_lowercase()
    }
}

/// A field that must be present, though it may be `null`: serde would read a missing `Option`
/// as `None`, and a missing field is a shape this adapter was not pinned for.
fn present<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<u64>::deserialize(deserializer)
}

/// The `Source` values of the pinned versions: a local VM and an entry of the OCI cache.
pub const SOURCES: &[&str] = &["local", "OCI"];

/// Parses `tart list --format json`. An entry whose `Source` is not one of [`SOURCES`] makes the
/// whole output unparseable: dropping it would make a listing complete but shorter (I25).
///
/// # Errors
/// [`Unparseable`] for anything else.
pub fn list(stdout: &[u8]) -> Result<Vec<ListEntry>, Unparseable> {
    let entries: Vec<ListEntry> = serde_json::from_slice(stdout).map_err(|_| Unparseable)?;
    if entries.iter().all(|e| SOURCES.contains(&e.source.as_str())) {
        Ok(entries)
    } else {
        Err(Unparseable)
    }
}

/// Parses `tart get --format json`.
///
/// # Errors
/// [`Unparseable`] for anything else.
pub fn get(stdout: &[u8]) -> Result<VmInfo, Unparseable> {
    serde_json::from_slice(stdout).map_err(|_| Unparseable)
}

/// Parses `tart --version`: its first line, trimmed.
///
/// # Errors
/// [`Unparseable`] when it is empty or not `<digits>.<digits>.<digits>`.
pub fn version(stdout: &[u8]) -> Result<String, Unparseable> {
    let text = std::str::from_utf8(stdout).map_err(|_| Unparseable)?;
    let line = text.lines().next().unwrap_or("").trim();
    let parts: Vec<&str> = line.split('.').collect();
    let numeric = parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    if numeric {
        Ok(line.to_owned())
    } else {
        Err(Unparseable)
    }
}

/// Whether `stderr` is the hypervisor refusing another running VM. Tart turns
/// Virtualization.framework's `virtualMachineLimitExceeded` into its own
/// `RuntimeError.VirtualMachineLimitExceeded` and `tart run` prints its description, "The number
/// of VMs exceeds the system limit", followed by the running VMs' names (`Commands/Run.swift`,
/// `VMStorageHelper.swift`). Matched in lower case; HW-01 confirms it on real Tart.
#[must_use]
pub fn is_hypervisor_limit(stderr: &str) -> bool {
    stderr
        .to_ascii_lowercase()
        .contains("the number of vms exceeds the system limit")
}

/// Output that does not have the pinned shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unparseable;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn fixture(version: &str, name: &str) -> Vec<u8> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(format!("tart-{version}"))
            .join(name);
        std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    #[test]
    fn should_parse_every_pinned_fixture_when_the_version_is_supported() {
        for version in SUPPORTED_VERSIONS {
            // given
            let listed = fixture(version, "list.json");
            let running = fixture(version, "get-running.json");
            let stopped = fixture(version, "get-stopped-linux.json");
            let unread = fixture(version, "get-running-disk-unknown.json");

            // when
            let entries = list(&listed).expect("list parses");
            let running = get(&running).expect("get parses");
            let stopped = get(&stopped).expect("get parses");
            let unread = get(&unread).expect("get parses with a null disk");
            let probed = super::version(&fixture(version, "version.txt"));

            // then
            assert_eq!(probed.as_deref(), Ok(*version));
            assert_eq!(entries.len(), 5);
            assert_eq!(entries.iter().filter(|e| e.is_local()).count(), 3);
            assert!(entries[0].running && !entries[1].running);
            assert_eq!(running.guest(), Some(GuestOs::Macos));
            assert_eq!(running.memory_bytes(), Some(8192 * MIB));
            assert_eq!(running.disk_bytes(), Some(50 * GIB));
            assert!(running.running);
            assert_eq!(stopped.guest(), Some(GuestOs::Linux));
            assert_eq!(stopped.instance_text(), "8a:3f:00:11:22:33");
            assert_eq!(stopped.disk_bytes(), Some(20 * GIB));
            assert!(!stopped.running);
            assert_eq!(unread.disk_bytes(), None);
            assert!(unread.running);
        }
    }

    #[test]
    fn should_count_the_disk_ceiling_above_tarts_rounded_size_when_reading_its_disk() {
        // given: Tart prints `bytes / 10⁹` rounded down, so 5 means up to 5 999 999 999 bytes
        let small = get(br#"{"OS":"linux","Memory":1,"Disk":5,"MACAddress":"a","Running":false}"#)
            .expect("parses");
        let large = get(br#"{"OS":"linux","Memory":1,"Disk":50,"MACAddress":"a","Running":false}"#)
            .expect("parses");

        // when
        let ceilings = (small.disk_bytes(), large.disk_bytes());

        // then
        assert_eq!(ceilings, (Some(6_000_000_000), Some(50 * GIB)));
    }

    #[test]
    fn should_refuse_output_when_a_read_field_is_missing_or_mistyped() {
        // given
        let missing_mac = br#"{"OS":"darwin","Memory":1,"Disk":1,"Running":false}"#;
        let missing_disk = br#"{"OS":"darwin","Memory":1,"MACAddress":"a","Running":false}"#;
        let string_memory =
            br#"{"OS":"darwin","Memory":"1","Disk":1,"MACAddress":"a","Running":false}"#;
        let list_object = br#"{"Name":"x"}"#;
        let other_source = br#"[{"Source":"remote","Name":"x","Running":false}]"#;
        let lower_oci = br#"[{"Source":"oci","Name":"x","Running":false}]"#;
        let truncated = b"[{\"Source\": \"local\", \"Na";

        // when
        let refused = [
            get(missing_mac).is_err(),
            get(missing_disk).is_err(),
            get(string_memory).is_err(),
            list(list_object).is_err(),
            list(truncated).is_err(),
            list(b"").is_err(),
            list(other_source).is_err(),
            list(lower_oci).is_err(),
        ];

        // then
        assert_eq!(refused, [true; 8]);
    }

    #[test]
    fn should_refuse_tart_2_28_0_when_its_get_prints_no_mac() {
        // given: 2.28.0's `Commands/Get.swift` prints these fields and no `MACAddress`
        let old_get = br#"{"OS":"darwin","CPU":4,"Memory":8192,"Disk":50,"DiskFormat":"raw","Size":"23.104","Display":"1024x768","Running":false,"State":"stopped"}"#;

        // when
        let parsed = get(old_get);

        // then
        assert_eq!(parsed, Err(Unparseable));
        assert!(!SUPPORTED_VERSIONS.contains(&"2.28.0"));
    }

    #[test]
    fn should_leave_the_guest_unknown_when_tart_names_another_os() {
        // given
        let info = get(br#"{"OS":"plan9","Memory":1,"Disk":1,"MACAddress":"a","Running":false}"#)
            .expect("parses");

        // when
        let guest = info.guest();

        // then
        assert_eq!(guest, None);
    }

    #[test]
    fn should_refuse_a_version_when_it_is_not_three_numbers() {
        // given
        let bad: [&[u8]; 4] = [b"", b"tart 2.41.1", b"2.41", b"2.x.1\n"];

        // when
        let refused: Vec<bool> = bad.iter().map(|text| version(text).is_err()).collect();

        // then
        assert_eq!(refused, [true; 4]);
    }

    #[test]
    fn should_recognise_the_hypervisor_limit_when_tart_run_reports_it() {
        // given
        let stderr =
            String::from_utf8(fixture("2.41.1", "run-hypervisor-limit.stderr")).expect("utf-8");

        // when
        let limit = is_hypervisor_limit(&stderr);

        // then
        assert!(limit);
        assert!(!is_hypervisor_limit("VM \"x\" is already running!"));
    }
}
