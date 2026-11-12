//! What sizes the host, and where each value came from (ADR 0052 as amended 2026-11-12).
//!
//! Every `[host]` sizing key is optional. A configured value overrides; otherwise the value is a
//! fact: measured by the host probe (slots and the memory reserve, from physical memory and
//! CPUs: `mayfly_adapter_host::vm_slots`), read from each image's verified manifest (the disk
//! ceilings), or Apple's documented limit (macOS guests). What cannot be measured is reported
//! unknown and the data plane refuses `capacity_unknown`; nothing is guessed.
//!
//! [`SizingReport`] is what `mayflyd` writes to stderr at start-up, one line per value with its
//! source; it is a plain value so the operator socket's status (RT-03) can return it too.

use std::collections::BTreeSet;
use std::fmt;

use mayfly_data::HostReading;
use mayfly_domain::ImageDigest;
use mayfly_protocol::ResourceDimension;

use crate::config::{DEFAULT_RESERVE_CPUS, HostConfig, MACOS_GUESTS_LIMIT};

const MIB: u64 = 1024 * 1024;

/// Where a sizing value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The configuration set it.
    Configured,
    /// The configuration set more than Apple's limit, which applies instead.
    CappedAtAppleLimit,
    /// The host probe measured it.
    Measured,
    /// Each image's verified manifest gives it, per image.
    Manifest,
    /// Apple's documented limit of two macOS guests per host.
    AppleLimit,
    /// The documented default (the owner's, ADR 0052's amendment).
    Default,
}

impl Source {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::CappedAtAppleLimit => "configured_capped_at_apple_limit",
            Self::Measured => "measured",
            Self::Manifest => "manifest",
            Self::AppleLimit => "apple_limit",
            Self::Default => "default",
        }
    }
}

/// One value: a number, per image, or unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value {
    /// A number, in the key's unit.
    Number(u64),
    /// Measured for each image from its manifest.
    PerImage,
    /// Could not be measured: the dimension is unknown (`capacity_unknown`).
    Unknown,
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(n) => write!(f, "{n}"),
            Self::PerImage => f.write_str("per_image"),
            Self::Unknown => f.write_str("unknown"),
        }
    }
}

/// One sized key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sized {
    /// The `[host]` key it is the value of.
    pub key: &'static str,
    /// Its value.
    pub value: Value,
    /// Where it came from.
    pub source: Source,
    /// Why it is unknown, when it is: `cpus_unreadable`, `probe_unreadable`.
    pub reason: Option<&'static str>,
}

/// Every sizing value in effect, with its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizingReport {
    /// In `[host]` key order: `vm_slots`, `macos_guests`, `reserve_memory_mib`,
    /// `reserve_cpus`, `image_disk_gib`, `clone_disk_gib`.
    pub values: Vec<Sized>,
    /// Listed images whose disk could not be measured (their kept manifest did not read and
    /// measuring them again failed): disk is unknown until they are (reported to the control
    /// plane as `images_not_read`).
    pub unmeasured_images: Vec<String>,
}

impl fmt::Display for SizingReport {
    /// One line per value, `mayflyd sizing <key>=<value> source=<source>`, with
    /// ` reason=<reason>` when it is unknown; then one line per unmeasured image,
    /// `mayflyd sizing unmeasured_image=<digest> reason=manifest_unreadable`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for sized in &self.values {
            write!(
                f,
                "mayflyd sizing {}={} source={}",
                sized.key,
                sized.value,
                sized.source.as_str()
            )?;
            match sized.reason {
                Some(reason) => writeln!(f, " reason={reason}")?,
                None => writeln!(f)?,
            }
        }
        for digest in &self.unmeasured_images {
            writeln!(
                f,
                "mayflyd sizing unmeasured_image={digest} reason=manifest_unreadable"
            )?;
        }
        Ok(())
    }
}

/// The sizing of `host`, its measured values read from the probe's `reading` (the data plane's
/// first, or `None` when it could not be taken), with the listed images whose disk did not
/// measure (`unmeasured`). Pinned by
/// `tests/sizing.rs::should_name_each_source_when_nothing_is_configured`,
/// `should_name_configured_sources_when_everything_is_configured` and
/// `should_report_unknown_slots_when_the_probe_could_not_measure_them`.
#[must_use]
pub fn report(
    host: &HostConfig,
    reading: Option<&HostReading>,
    unmeasured: &BTreeSet<ImageDigest>,
) -> SizingReport {
    let unread = if reading.is_none() {
        "probe_unreadable"
    } else {
        "cpus_unreadable"
    };
    let measured = |value: Option<u64>| Sized {
        key: "",
        value: value.map_or(Value::Unknown, Value::Number),
        source: Source::Measured,
        reason: value.is_none().then_some(unread),
    };
    let vm_slots = match host.vm_slots {
        Some(n) => configured(u64::from(n)),
        None => measured(reading.and_then(|r| {
            (!r.foreign_unsized.contains(&ResourceDimension::VmSlots))
                .then_some(u64::from(r.totals.vm_slots))
        })),
    };
    let macos_guests = match host.macos_guests {
        Some(n) if n > MACOS_GUESTS_LIMIT => Sized {
            key: "",
            value: Value::Number(u64::from(MACOS_GUESTS_LIMIT)),
            source: Source::CappedAtAppleLimit,
            reason: None,
        },
        Some(n) => configured(u64::from(n)),
        None => Sized {
            key: "",
            value: Value::Number(u64::from(MACOS_GUESTS_LIMIT)),
            source: Source::AppleLimit,
            reason: None,
        },
    };
    let reserve_memory = match host.reserve_memory_mib {
        Some(mib) => configured(mib),
        None => Sized {
            reason: reading.is_none().then_some("probe_unreadable"),
            ..measured(reading.map(|r| r.reserve.resources.memory_bytes / MIB))
        },
    };
    let reserve_cpus = match host.reserve_cpus {
        Some(n) => configured(u64::from(n)),
        None => Sized {
            key: "",
            value: Value::Number(u64::from(DEFAULT_RESERVE_CPUS)),
            source: Source::Default,
            reason: None,
        },
    };
    let disk = |gib: Option<u64>| match gib {
        Some(gib) => configured(gib),
        None => Sized {
            key: "",
            value: Value::PerImage,
            source: Source::Manifest,
            reason: None,
        },
    };
    let values = [
        ("vm_slots", vm_slots),
        ("macos_guests", macos_guests),
        ("reserve_memory_mib", reserve_memory),
        ("reserve_cpus", reserve_cpus),
        ("image_disk_gib", disk(host.image_disk_gib)),
        ("clone_disk_gib", disk(host.clone_disk_gib)),
    ]
    .into_iter()
    .map(|(key, sized)| Sized { key, ..sized })
    .collect();
    SizingReport {
        values,
        unmeasured_images: unmeasured.iter().map(ToString::to_string).collect(),
    }
}

const fn configured(value: u64) -> Sized {
    Sized {
        key: "",
        value: Value::Number(value),
        source: Source::Configured,
        reason: None,
    }
}
