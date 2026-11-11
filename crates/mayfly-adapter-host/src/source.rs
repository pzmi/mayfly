//! What the operating system says about the host: the seam [`HostSource`], and [`Os`], which
//! reads the real host with per-OS code behind `#[cfg]` items.
//!
//! A test replaces [`Os`] with a scripted source to make any read fail or any clock jump; the
//! probe never learns which it has.

use std::path::Path;
use std::time::{Duration, Instant};

/// A host reading that could not be taken. Never replaced by a default (ADR 0052).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadFailed;

/// The volume that holds a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Volume {
    /// Its size.
    pub total_bytes: u64,
    /// What an unprivileged process may still write (`f_bavail`, not `f_bfree`: the daemon is
    /// not root). On APFS this leaves out purgeable space, so it can under-report, which is the
    /// safe direction.
    pub free_bytes: u64,
}

/// The host's memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Memory {
    /// Physical memory.
    pub total_bytes: u64,
    /// What the kernel says can be handed out without swapping: Linux's `MemAvailable`; on
    /// macOS `hw.memsize` scaled by `kern.memorystatus_level`, the percentage the kernel's
    /// memory-status machinery reports available.
    pub available_bytes: u64,
}

/// Two clock readings taken together: one that stops while the host sleeps and one that does
/// not. Only their deltas mean anything; the difference of the deltas is the time asleep
/// ([`crate::slept`]). A step of the wall clock moves neither, so an NTP correction is never
/// read as a wake (comparing the wall clock with a monotonic one would).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clocks {
    /// Advances only while the host is awake.
    pub awake: Duration,
    /// Advances while it is awake and while it sleeps.
    pub with_sleep: Duration,
}

/// What the probe reads from the operating system.
pub trait HostSource: Send + Sync {
    /// The volume that holds `path`.
    ///
    /// # Errors
    /// [`ReadFailed`] when it cannot be read.
    fn volume(&self, path: &Path) -> Result<Volume, ReadFailed>;

    /// The host's memory.
    ///
    /// # Errors
    /// [`ReadFailed`] when it cannot be read.
    fn memory(&self) -> Result<Memory, ReadFailed>;

    /// The identity of this boot: it changes on every boot and never within one.
    ///
    /// # Errors
    /// [`ReadFailed`] when it cannot be read.
    fn boot_id(&self) -> Result<String, ReadFailed>;

    /// Both clocks, read now.
    ///
    /// # Errors
    /// [`ReadFailed`] when a reading is out of range.
    fn clocks(&self) -> Result<Clocks, ReadFailed>;
}

/// The real host.
#[derive(Debug, Clone, Copy)]
pub struct Os {
    /// The origin of [`Clocks::awake`]. `Instant` is `CLOCK_MONOTONIC` on Linux and
    /// `CLOCK_UPTIME_RAW` on macOS, and neither advances while the host sleeps.
    anchor: Instant,
}

impl Os {
    /// The real host, its awake clock starting now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            anchor: Instant::now(),
        }
    }
}

impl Default for Os {
    fn default() -> Self {
        Self::new()
    }
}

impl HostSource for Os {
    fn volume(&self, path: &Path) -> Result<Volume, ReadFailed> {
        let stat = rustix::fs::statvfs(path).map_err(|_| ReadFailed)?;
        Ok(Volume {
            total_bytes: stat.f_blocks.checked_mul(stat.f_frsize).ok_or(ReadFailed)?,
            free_bytes: stat.f_bavail.checked_mul(stat.f_frsize).ok_or(ReadFailed)?,
        })
    }

    fn memory(&self) -> Result<Memory, ReadFailed> {
        platform::memory()
    }

    fn boot_id(&self) -> Result<String, ReadFailed> {
        platform::boot_id()
    }

    fn clocks(&self) -> Result<Clocks, ReadFailed> {
        Ok(Clocks {
            awake: self.anchor.elapsed(),
            with_sleep: platform::with_sleep()?,
        })
    }
}

/// A clock reading counted from boot; a negative or malformed one is a failed read.
fn duration(t: rustix::time::Timespec) -> Result<Duration, ReadFailed> {
    Ok(Duration::new(
        u64::try_from(t.tv_sec).map_err(|_| ReadFailed)?,
        u32::try_from(t.tv_nsec).map_err(|_| ReadFailed)?,
    ))
}

/// Linux: `/proc` for memory and the boot id, `CLOCK_BOOTTIME` (counts suspend) for the clock
/// that runs while asleep.
#[cfg(target_os = "linux")]
mod platform {
    use super::{Duration, Memory, ReadFailed, duration};

    pub(super) fn memory() -> Result<Memory, ReadFailed> {
        let text = std::fs::read_to_string("/proc/meminfo").map_err(|_| ReadFailed)?;
        parse_meminfo(&text)
    }

    pub(super) fn boot_id() -> Result<String, ReadFailed> {
        let text =
            std::fs::read_to_string("/proc/sys/kernel/random/boot_id").map_err(|_| ReadFailed)?;
        parse_boot_id(&text)
    }

    pub(super) fn with_sleep() -> Result<Duration, ReadFailed> {
        duration(rustix::time::clock_gettime(rustix::time::ClockId::Boottime))
    }

    /// `MemTotal` and `MemAvailable` of `/proc/meminfo`, in KiB there. A missing or unparsable
    /// line is a failed read: `MemFree` is never taken in its place (ADR 0052).
    pub(super) fn parse_meminfo(text: &str) -> Result<Memory, ReadFailed> {
        let field = |key: &str| -> Result<u64, ReadFailed> {
            let line = text
                .lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix(':'))
                .ok_or(ReadFailed)?;
            let kib = line
                .trim()
                .strip_suffix("kB")
                .ok_or(ReadFailed)?
                .trim()
                .parse::<u64>()
                .map_err(|_| ReadFailed)?;
            kib.checked_mul(1024).ok_or(ReadFailed)
        };
        Ok(Memory {
            total_bytes: field("MemTotal")?,
            available_bytes: field("MemAvailable")?,
        })
    }

    pub(super) fn parse_boot_id(text: &str) -> Result<String, ReadFailed> {
        let id = text.trim();
        if id.is_empty() {
            Err(ReadFailed)
        } else {
            Ok(id.to_owned())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const MEMINFO: &str = "MemTotal:       16318412 kB\nMemFree:          512000 kB\nMemAvailable:    8159206 kB\n";

        #[test]
        fn should_read_total_and_available_bytes_when_meminfo_lists_both() {
            // given
            let text = MEMINFO;

            // when
            let memory = parse_meminfo(text);

            // then
            assert_eq!(
                memory,
                Ok(Memory {
                    total_bytes: 16_318_412 * 1024,
                    available_bytes: 8_159_206 * 1024,
                })
            );
        }

        #[test]
        fn should_fail_rather_than_take_mem_free_when_mem_available_is_missing() {
            // given
            let text = "MemTotal:       16318412 kB\nMemFree:          512000 kB\n";

            // when
            let memory = parse_meminfo(text);

            // then
            assert_eq!(memory, Err(ReadFailed));
        }

        #[test]
        fn should_fail_when_a_meminfo_value_does_not_parse() {
            // given
            let text = "MemTotal: lots kB\nMemAvailable: 1 kB\n";

            // when
            let memory = parse_meminfo(text);

            // then
            assert_eq!(memory, Err(ReadFailed));
        }

        #[test]
        fn should_fail_when_the_boot_id_is_empty() {
            // given
            let text = "\n";

            // when
            let id = parse_boot_id(text);

            // then
            assert_eq!(id, Err(ReadFailed));
        }
    }
}

/// macOS: `sysctl` names for memory and the boot id; `CLOCK_MONOTONIC`, which on macOS keeps
/// counting while the host sleeps (unlike Linux's), for the clock that runs while asleep.
#[cfg(target_os = "macos")]
mod platform {
    use sysctl::{Ctl, CtlValue, Sysctl as _};

    use super::{Duration, Memory, ReadFailed, duration};

    fn number(name: &str) -> Result<u64, ReadFailed> {
        let value = Ctl::new(name)
            .and_then(|ctl| ctl.value())
            .map_err(|_| ReadFailed)?;
        match value {
            CtlValue::U64(n) | CtlValue::Ulong(n) => Ok(n),
            CtlValue::Uint(n) | CtlValue::U32(n) => Ok(u64::from(n)),
            CtlValue::S64(n) | CtlValue::Long(n) => u64::try_from(n).map_err(|_| ReadFailed),
            CtlValue::Int(n) | CtlValue::S32(n) => u64::try_from(n).map_err(|_| ReadFailed),
            _ => Err(ReadFailed),
        }
    }

    pub(super) fn memory() -> Result<Memory, ReadFailed> {
        let total_bytes = number("hw.memsize")?;
        let level = number("kern.memorystatus_level")?;
        if level > 100 {
            return Err(ReadFailed);
        }
        let available_bytes = u64::try_from(u128::from(total_bytes) * u128::from(level) / 100)
            .map_err(|_| ReadFailed)?;
        Ok(Memory {
            total_bytes,
            available_bytes,
        })
    }

    pub(super) fn boot_id() -> Result<String, ReadFailed> {
        let id = Ctl::new("kern.bootsessionuuid")
            .and_then(|ctl| ctl.value_string())
            .map_err(|_| ReadFailed)?;
        let id = id.trim();
        if id.is_empty() {
            Err(ReadFailed)
        } else {
            Ok(id.to_owned())
        }
    }

    pub(super) fn with_sleep() -> Result<Duration, ReadFailed> {
        duration(rustix::time::clock_gettime(
            rustix::time::ClockId::Monotonic,
        ))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("mayfly-adapter-host reads the host on Linux and macOS only (ADR 0003)");
