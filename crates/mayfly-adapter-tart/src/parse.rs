//! Tart's output, parsed against the versions it was pinned for (plan §8.2).
//!
//! The shapes are those of Tart's `list --format json` and `get --format json`, pinned by the
//! fixtures in `tests/fixtures/tart-<version>/` (synthetic, in the recorded shape). Fields this
//! adapter does not read are ignored; a field it reads that is missing or of another type makes
//! the whole output unparseable, never a default (ADR 0052). A version outside
//! [`SUPPORTED_VERSIONS`] is refused by the probe (an unsupported Tart closes admission, R12).
//!
//! Units: `Memory` is MiB; `Disk` is counted in GiB (1024³ bytes), the larger reading of Tart's
//! "GB", so a disk is never undercounted. HW-01 confirms both on real Tart.
//!
//! Pinned by `tests::should_parse_every_pinned_fixture_when_the_version_is_supported`.

use mayfly_data::GuestOs;
use serde::Deserialize;

/// The Tart versions whose output this module was checked against: one directory of fixtures
/// each.
pub const SUPPORTED_VERSIONS: &[&str] = &["2.28.0"];

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

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
    /// Disk size in Tart's GB.
    #[serde(rename = "Disk")]
    pub disk_gb: u64,
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

    /// The disk ceiling in bytes.
    #[must_use]
    pub fn disk_bytes(&self) -> Option<u64> {
        self.disk_gb.checked_mul(GIB)
    }

    /// The MAC as an instance id's text: lowercase.
    #[must_use]
    pub fn instance_text(&self) -> String {
        self.mac.to_ascii_lowercase()
    }
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

/// Whether `stderr` is the hypervisor refusing another running VM (Virtualization.framework's
/// "virtual machine limit" error). The wording is matched loosely, in lower case; HW-01
/// confirms it on real Tart.
#[must_use]
pub fn is_hypervisor_limit(stderr: &str) -> bool {
    let text = stderr.to_ascii_lowercase();
    text.contains("number of virtual machines exceeds the limit")
        || text.contains("maximum supported number of active virtual machines")
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

            // when
            let entries = list(&listed).expect("list parses");
            let running = get(&running).expect("get parses");
            let stopped = get(&stopped).expect("get parses");
            let probed = super::version(&fixture(version, "version.txt"));

            // then
            assert_eq!(probed.as_deref(), Ok(*version));
            assert_eq!(entries.len(), 4);
            assert_eq!(entries.iter().filter(|e| e.is_local()).count(), 3);
            assert!(entries[1].running && !entries[0].running);
            assert_eq!(running.guest(), Some(GuestOs::Macos));
            assert_eq!(running.memory_bytes(), Some(8192 * MIB));
            assert_eq!(running.disk_bytes(), Some(50 * GIB));
            assert!(running.running);
            assert_eq!(stopped.guest(), Some(GuestOs::Linux));
            assert_eq!(stopped.instance_text(), "8a:3f:00:11:22:33");
            assert!(!stopped.running);
        }
    }

    #[test]
    fn should_refuse_output_when_a_read_field_is_missing_or_mistyped() {
        // given
        let missing_mac = br#"{"OS":"darwin","Memory":1,"Disk":1,"Running":false}"#;
        let string_memory =
            br#"{"OS":"darwin","Memory":"1","Disk":1,"MACAddress":"a","Running":false}"#;
        let list_object = br#"{"Name":"x"}"#;
        let other_source = br#"[{"Source":"remote","Name":"x","Running":false}]"#;
        let lower_oci = br#"[{"Source":"oci","Name":"x","Running":false}]"#;
        let truncated = b"[{\"Source\": \"local\", \"Na";

        // when
        let refused = [
            get(missing_mac).is_err(),
            get(string_memory).is_err(),
            list(list_object).is_err(),
            list(truncated).is_err(),
            list(b"").is_err(),
            list(other_source).is_err(),
            list(lower_oci).is_err(),
        ];

        // then
        assert_eq!(refused, [true; 7]);
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
        let bad: [&[u8]; 4] = [b"", b"tart 2.28.0", b"2.28", b"2.x.0\n"];

        // when
        let refused: Vec<bool> = bad.iter().map(|text| version(text).is_err()).collect();

        // then
        assert_eq!(refused, [true; 4]);
    }

    #[test]
    fn should_recognise_the_hypervisor_limit_when_tart_run_reports_it() {
        // given
        let stderr =
            String::from_utf8(fixture("2.28.0", "run-hypervisor-limit.stderr")).expect("utf-8");

        // when
        let limit = is_hypervisor_limit(&stderr);

        // then
        assert!(limit);
        assert!(!is_hypervisor_limit("Error: VM \"x\" is already running!"));
    }
}
