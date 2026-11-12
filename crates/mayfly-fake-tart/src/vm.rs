//! The fake's record of one VM, and how it decides whether the VM is running.
//!
//! One JSON file per VM under `$TART_HOME/vms/`. The record carries what the fault lab observes
//! (`source`, `mac`, `created_at_ms`, `run`, `guest_launches`) beside what Tart's own JSON prints.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::faults::RunMode;

/// Where a VM came from: `local` storage or the `oci` cache of pulled images.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Source {
    Local,
    Oci,
}

/// One `tart run` that holds the VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Run {
    /// Invocation id of the `run` that started it; the foreground process watches for a change.
    pub id: u64,
    /// The foreground process, `None` for a detached run.
    pub pid: Option<u32>,
    /// The process's start time (Linux `/proc` only), so a reused pid is not mistaken for it.
    pub pid_start: Option<u64>,
    pub mode: RunMode,
    pub started_at_ms: u64,
}

/// What a guest `exec` received: the argv and the length and digest of stdin, never stdin itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Launch {
    pub at_ms: u64,
    pub argv: Vec<String>,
    pub stdin_len: Option<u64>,
    pub stdin_sha256: Option<String>,
}

/// The emulated guest launcher's runner (see `guest`): started by a launch, it runs its job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Runner {
    pub started_at_ms: u64,
    /// How long its job runs and its exit code; `None` runs until the VM stops.
    pub job: Option<(u64, i32)>,
}

/// A VM as stored in `vms/<name>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct VmRecord {
    pub v: u32,
    pub name: String,
    pub source: Source,
    pub cloned_from: Option<String>,
    pub os: String,
    pub cpu: u64,
    pub memory_mb: u64,
    pub disk_gb: u64,
    pub mac: String,
    /// Last octet of the VM's address in 192.168.64.0/24.
    pub host_octet: u8,
    pub created_at_ms: u64,
    pub accessed_ms: u64,
    pub run: Option<Run>,
    /// The most recent guest launches, oldest first, at most [`MAX_LAUNCHES`].
    pub guest_launches: Vec<Launch>,
    /// The guest launcher's runner, once launched; it lives in the guest, so a restarted
    /// daemon reads it through `exec` (`guest`).
    #[serde(default)]
    pub runner: Option<Runner>,
    /// The runner version the launcher's `install-runner` put into this image; a clone carries
    /// its source's.
    #[serde(default)]
    pub installed_runner: Option<String>,
    /// An OCI entry pulled by its own digest: Tart's `markExplicitlyPulled`, which keeps it from
    /// the cache's garbage collection when no link names it.
    #[serde(default)]
    pub explicitly_pulled: bool,
    /// `raw` (`None`) or `asif` (`create --disk-format asif`); a clone carries its source's.
    /// Tart cannot read a running VM's ASIF capacity, so `get` and `list` print `Disk` null.
    #[serde(default)]
    pub disk_format: Option<String>,
}

/// How many guest launches a record keeps.
pub(crate) const MAX_LAUNCHES: usize = 32;

impl VmRecord {
    /// A fresh stopped VM.
    pub fn new(name: &str, source: Source, mac: String, host_octet: u8, now_ms: u64) -> Self {
        Self {
            v: 1,
            name: name.to_owned(),
            source,
            cloned_from: None,
            os: "darwin".to_owned(),
            cpu: 4,
            memory_mb: 8192,
            disk_gb: 50,
            mac,
            host_octet,
            created_at_ms: now_ms,
            accessed_ms: now_ms,
            run: None,
            guest_launches: Vec::new(),
            runner: None,
            installed_runner: None,
            explicitly_pulled: false,
            disk_format: None,
        }
    }

    /// Whether the VM is running now: a detached run always is, a foreground run while its process
    /// lives. A killed foreground `tart run` takes its VM with it, as in real Tart.
    pub fn running(&self) -> bool {
        self.run
            .as_ref()
            .is_some_and(|run| run.pid.is_none_or(|pid| pid_alive(pid, run.pid_start)))
    }

    /// The `Disk` Tart prints: `None` (`null`) for a running ASIF disk, whose capacity Tart
    /// cannot read while the VM holds it open (`Commands/Get.swift`, `List.swift`).
    pub fn printed_disk(&self) -> Option<u64> {
        let asif = self.disk_format.as_deref() == Some("asif");
        (!(asif && self.running())).then_some(self.disk_gb)
    }

    /// `running` or `stopped`, Tart's `State` field (`suspended` is not modelled).
    pub fn state(&self) -> &'static str {
        if self.running() { "running" } else { "stopped" }
    }

    /// The address `tart ip` prints.
    pub fn ip(&self) -> String {
        format!("192.168.64.{}", self.host_octet)
    }
}

/// A MAC address for a new VM: locally administered, unicast, derived from the name and a counter
/// so a replayed run produces the same addresses.
pub(crate) fn pseudo_mac(name: &str, counter: u64) -> String {
    let digest = Sha256::digest(format!("{name}/{counter}").as_bytes());
    let bytes: &[u8] = digest.as_ref();
    let mut octets = [0u8; 6];
    octets.copy_from_slice(&bytes[..6]);
    octets[0] = (octets[0] | 0x02) & 0xfe;
    octets.map(|b| format!("{b:02x}")).join(":")
}

/// Lowercase hex of a SHA-256 digest.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The start time of `pid` in clock ticks since boot (Linux), or `None` where `/proc` is absent.
pub(crate) fn pid_start(pid: u32) -> Option<u64> {
    proc_stat(pid).map(|(_, start)| start)
}

/// Whether `pid` is a live, non-zombie process, and (where known) the one that started at `start`.
pub(crate) fn pid_alive(pid: u32, start: Option<u64>) -> bool {
    let Some(raw) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return false;
    };
    match rustix::process::test_kill_process(raw) {
        Ok(()) => {}
        Err(rustix::io::Errno::PERM) => {}
        Err(_) => return false,
    }
    match proc_stat(pid) {
        Some((state, _)) if state == 'Z' || state == 'X' => false,
        Some((_, actual)) => start.is_none_or(|expected| expected == actual),
        None => true,
    }
}

#[cfg(target_os = "linux")]
fn proc_stat(pid: u32) -> Option<(char, u64)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = text.get(text.rfind(')')? + 1..)?;
    let mut fields = after_comm.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let start = fields.nth(18)?.parse().ok()?;
    Some((state, start))
}

#[cfg(not(target_os = "linux"))]
fn proc_stat(_pid: u32) -> Option<(char, u64)> {
    None
}

/// Kills `pid` with SIGKILL; a process that is already gone is not an error.
pub(crate) fn kill_hard(pid: u32) {
    if let Some(raw) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process(raw, rustix::process::Signal::KILL);
    }
}

#[cfg(test)]
mod tests {
    use super::{Run, Source, VmRecord, hex, pid_alive, pid_start, pseudo_mac};
    use crate::faults::RunMode;

    fn record() -> VmRecord {
        VmRecord::new("vm1", Source::Local, pseudo_mac("vm1", 0), 7, 1000)
    }

    #[test]
    fn should_derive_a_locally_administered_unicast_mac_when_given_a_name() {
        // given
        let name = "vm1";

        // when
        let a = pseudo_mac(name, 0);
        let b = pseudo_mac(name, 1);

        // then
        let first = u8::from_str_radix(&a[..2], 16).unwrap();
        assert_eq!(first & 0x03, 0x02, "locally administered, unicast");
        assert_eq!(a.len(), 17);
        assert_ne!(a, b);
        assert_eq!(a, pseudo_mac(name, 0));
    }

    #[test]
    fn should_encode_lowercase_hex_when_given_bytes() {
        // given
        let bytes = [0x00, 0xab, 0xff];

        // when
        let text = hex(&bytes);

        // then
        assert_eq!(text, "00abff");
    }

    #[test]
    fn should_report_running_when_the_run_is_detached() {
        // given
        let mut vm = record();
        vm.run = Some(Run {
            id: 1,
            pid: None,
            pid_start: None,
            mode: RunMode::Detached,
            started_at_ms: 1,
        });

        // when
        let state = vm.state();

        // then
        assert_eq!(state, "running");
    }

    #[test]
    fn should_report_stopped_when_there_is_no_run() {
        // given
        let vm = record();

        // when
        let view = (vm.state(), vm.ip());

        // then
        assert_eq!(view, ("stopped", "192.168.64.7".to_owned()));
    }

    #[test]
    fn should_follow_the_process_when_the_run_is_foreground() {
        // given
        let me = std::process::id();
        let mut vm = record();
        vm.run = Some(Run {
            id: 1,
            pid: Some(me),
            pid_start: pid_start(me),
            mode: RunMode::Foreground,
            started_at_ms: 1,
        });
        let mut stale = vm.clone();
        stale.run.as_mut().unwrap().pid = Some(u32::MAX - 1);
        let mut reused = vm.clone();
        reused.run.as_mut().unwrap().pid_start = pid_start(me).map(|s| s + 1);

        // when
        let (live, dead) = (vm.running(), stale.running());

        // then
        assert!(live);
        assert!(!dead);
        if pid_start(me).is_some() {
            assert!(
                !reused.running(),
                "a pid with another start time is another process"
            );
        }
    }

    #[test]
    fn should_treat_an_out_of_range_pid_as_dead_when_it_cannot_be_a_process_id() {
        // given
        let pid = u32::MAX;

        // when
        let alive = pid_alive(pid, None);

        // then
        assert!(!alive);
    }
}
