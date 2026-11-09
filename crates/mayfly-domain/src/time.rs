//! Time as values. The domain reads no clock: callers pass `now`.
//!
//! Which clock a reading comes from is the reader's port's business (ADR 0020): the control
//! plane's `Clock` port reads wall-clock time as milliseconds since the Unix epoch, because its
//! readings are persisted (command `issued_at`, event instants, the journal's `recorded_at`,
//! outbox `next_attempt_at`) and must keep their meaning across a restart; the data plane's
//! `ProcessClock` reads a monotonic clock for deadlines that never leave the process.

use serde::{Deserialize, Serialize};

/// Milliseconds, as a duration or as a reading of the caller's clock: Unix-epoch wall time for
/// the control plane's `Clock`, a monotonic reading for the data plane's `ProcessClock`
/// (ADR 0020). Two readings compare meaningfully only when they come from the same clock.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Millis(u64);

impl Millis {
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
}

/// An absolute point, in the caller's clock, by which something must have happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Deadline(Millis);

impl Deadline {
    /// A deadline at an absolute reading.
    #[must_use]
    pub const fn at(instant: Millis) -> Self {
        Self(instant)
    }

    /// `now` plus `timeout`, saturating at the largest reading rather than wrapping.
    #[must_use]
    pub const fn after(now: Millis, timeout: Millis) -> Self {
        Self(Millis(now.0.saturating_add(timeout.0)))
    }

    /// The absolute reading.
    #[must_use]
    pub const fn instant(self) -> Millis {
        self.0
    }

    /// Whether `now` has reached the deadline.
    #[must_use]
    pub const fn is_expired(self, now: Millis) -> bool {
        now.0 >= self.0.0
    }

    /// Time left at `now`, zero once expired.
    #[must_use]
    pub const fn remaining(self, now: Millis) -> Millis {
        Millis(self.0.0.saturating_sub(now.0))
    }
}
