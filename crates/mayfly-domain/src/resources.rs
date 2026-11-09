//! Memory and disk, in bytes.

use serde::{Deserialize, Serialize};

/// A memory and disk amount; capacity arithmetic never wraps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Resources {
    /// Memory in bytes.
    pub memory_bytes: u64,
    /// Disk in bytes.
    pub disk_bytes: u64,
}

impl Resources {
    /// An amount of memory and disk.
    #[must_use]
    pub const fn new(memory_bytes: u64, disk_bytes: u64) -> Self {
        Self {
            memory_bytes,
            disk_bytes,
        }
    }

    /// The sum, or `None` on overflow.
    #[must_use]
    pub const fn checked_add(self, other: Self) -> Option<Self> {
        match (
            self.memory_bytes.checked_add(other.memory_bytes),
            self.disk_bytes.checked_add(other.disk_bytes),
        ) {
            (Some(memory_bytes), Some(disk_bytes)) => Some(Self {
                memory_bytes,
                disk_bytes,
            }),
            _ => None,
        }
    }

    /// The difference, or `None` when either part would go below zero.
    #[must_use]
    pub const fn checked_sub(self, other: Self) -> Option<Self> {
        match (
            self.memory_bytes.checked_sub(other.memory_bytes),
            self.disk_bytes.checked_sub(other.disk_bytes),
        ) {
            (Some(memory_bytes), Some(disk_bytes)) => Some(Self {
                memory_bytes,
                disk_bytes,
            }),
            _ => None,
        }
    }

    /// Whether both parts fit within `limit`.
    #[must_use]
    pub const fn fits_within(self, limit: Self) -> bool {
        self.memory_bytes <= limit.memory_bytes && self.disk_bytes <= limit.disk_bytes
    }
}
