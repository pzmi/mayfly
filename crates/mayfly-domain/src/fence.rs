//! Generations, epochs and the fence a directive must match.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::IdError;
use crate::ids::HostId;

/// The generation of one slot's VM name, from [`Generation::FIRST`] (1; zero does not exist).
///
/// Not a retry counter: a retry of an attempt reuses its generation (plan §6 rule 3), and only
/// a new attempt for the slot takes the next one. Deserialises through [`Generation::new`], so
/// a decoded zero is refused like a constructed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct Generation(u32);

impl Generation {
    /// The first generation of a slot.
    pub const FIRST: Self = Self(1);

    /// Wraps a raw value; `None` for 0, since generations start at [`Generation::FIRST`].
    #[must_use]
    pub const fn new(value: u32) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// The raw value, never 0.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The following generation, or `None` at the numeric limit (never wraps).
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }
}

impl TryFrom<u32> for Generation {
    type Error = IdError;
    fn try_from(value: u32) -> Result<Self, IdError> {
        Self::new(value).ok_or(IdError::ZeroGeneration)
    }
}

impl From<Generation> for u32 {
    fn from(value: Generation) -> Self {
        value.0
    }
}

/// A host epoch, minted per data-plane session, reconnects included, so a superseded control
/// plane or session is refused (plan §3.2, §3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Epoch(u64);

impl Epoch {
    /// The first epoch.
    pub const FIRST: Self = Self(1);

    /// Wraps a raw value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The following epoch, or `None` at the numeric limit (never wraps).
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }
}

/// The host and epoch a directive claims to act under.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Fence {
    host: HostId,
    epoch: Epoch,
}

/// Why a presented fence was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceError {
    /// Addressed to another host.
    WrongHost,
    /// Older than the current epoch: a superseded control plane. Never applied.
    Stale,
    /// Newer than the current epoch: not yet adopted through a handshake. Never applied.
    Ahead,
}

impl fmt::Display for FenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::WrongHost => "fence names another host",
            Self::Stale => "fence epoch is older than the current epoch",
            Self::Ahead => "fence epoch is newer than the current epoch",
        })
    }
}

impl std::error::Error for FenceError {}

impl Fence {
    /// A fence for `host` at `epoch`.
    #[must_use]
    pub const fn new(host: HostId, epoch: Epoch) -> Self {
        Self { host, epoch }
    }

    /// The host.
    #[must_use]
    pub const fn host(&self) -> &HostId {
        &self.host
    }

    /// The epoch.
    #[must_use]
    pub const fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// Whether `presented` may act under this, the current fence: host and epoch must be equal.
    ///
    /// # Errors
    /// [`FenceError`] saying which part differs; the epoch check comes after the host check.
    pub fn admit(&self, presented: &Self) -> Result<(), FenceError> {
        if presented.host != self.host {
            return Err(FenceError::WrongHost);
        }
        match presented.epoch.cmp(&self.epoch) {
            std::cmp::Ordering::Less => Err(FenceError::Stale),
            std::cmp::Ordering::Greater => Err(FenceError::Ahead),
            std::cmp::Ordering::Equal => Ok(()),
        }
    }
}
