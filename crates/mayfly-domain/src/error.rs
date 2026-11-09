//! Error types of the kernel's strict parsers.

use std::fmt;

/// Why an id or a pool/host name was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdError {
    /// A pool or host name that is not `[a-z0-9]{1,16}`.
    InvalidName,
    /// A 128-bit id that is not exactly 32 lowercase hexadecimal digits.
    InvalidHexId,
    /// A generation of 0; generations start at 1.
    ZeroGeneration,
    /// A provider instance id that is not `[0-9a-z:._-]{1,64}`.
    InvalidInstanceId,
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName => f.write_str("a name must match [a-z0-9]{1,16}"),
            Self::InvalidHexId => f.write_str("an id must be 32 lowercase hexadecimal digits"),
            Self::ZeroGeneration => f.write_str("a generation starts at 1"),
            Self::InvalidInstanceId => f.write_str("an instance id must match [0-9a-z:._-]{1,64}"),
        }
    }
}

impl std::error::Error for IdError {}

/// Why a string is not an owned VM name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmNameError {
    /// Does not start with `mf-`: not one of ours.
    NotManaged,
    /// Starts with `mf-` but has the wrong number of `-` separated parts.
    WrongShape,
    /// The pool part is not a valid [`crate::PoolName`].
    Pool,
    /// The host part is not a valid [`crate::HostId`].
    Host,
    /// The slot part is not `s` and a canonical decimal number.
    Slot,
    /// The generation part is not `g` and a canonical decimal number of at least 1.
    Generation,
    /// The attempt part is not eight lowercase hexadecimal digits.
    AttemptShort,
}

impl fmt::Display for VmNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotManaged => "not a managed VM name (no mf- prefix)",
            Self::WrongShape => "managed VM name has the wrong number of parts",
            Self::Pool => "invalid pool part in VM name",
            Self::Host => "invalid host part in VM name",
            Self::Slot => "invalid slot part in VM name",
            Self::Generation => "invalid generation part in VM name",
            Self::AttemptShort => "invalid attempt part in VM name",
        })
    }
}

impl std::error::Error for VmNameError {}

/// Why a string is not an image digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestError;

impl fmt::Display for DigestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an image digest must be sha256: and 64 lowercase hexadecimal digits")
    }
}

impl std::error::Error for DigestError {}
