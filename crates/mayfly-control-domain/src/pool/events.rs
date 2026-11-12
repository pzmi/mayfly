//! The pool's events (`doc/domain-model.md` §6.2), with golden samples under
//! `tests/fixtures/events/<type>/`.

use mayfly_domain::{AttemptId, Epoch, EventId, ImageDigest, Millis, Resources};
use serde::{Deserialize, Serialize};

use super::state::{TripReason, TrustTier};
use crate::attempt::{GuestOs, Stage};
use crate::event::DomainEvent;
use crate::event_set;

/// The pool accepted a command under a newer run epoch than any before (I18); recorded ahead
/// of that command's events so `evolve` knows the epoch to fence with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PoolEpochAdvanced {
    /// The new highest run epoch.
    pub epoch: Epoch,
}

impl DomainEvent for PoolEpochAdvanced {
    const EVENT_TYPE: &'static str = "pool_epoch_advanced";
    const SCHEMA_VERSION: u32 = 1;
}

/// The pool's configuration at a new revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PoolConfigured {
    /// The revision.
    pub revision: u64,
    /// The trust tier.
    pub tier: TrustTier,
    /// The guest OS.
    pub os: GuestOs,
    /// What one VM needs.
    pub vm_resources: Resources,
    /// At most this many attempts at once.
    pub max_running: u32,
    /// Prepared VMs ahead of demand (0 unless configured, ADR 0028).
    pub min_prepared: u32,
    /// Failures per window that trip the breaker.
    pub breaker_threshold: u32,
    /// The breaker's window.
    pub breaker_window: Millis,
}

impl DomainEvent for PoolConfigured {
    const EVENT_TYPE: &'static str = "pool_configured";
    const SCHEMA_VERSION: u32 = 1;
}

/// A verified image is the pool's pin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ImagePinned {
    /// The digest.
    pub digest: ImageDigest,
    /// The pin it replaced.
    pub previous: Option<ImageDigest>,
    /// The image's `ImageVerified` event.
    pub verified_ref: EventId,
}

impl DomainEvent for ImagePinned {
    const EVENT_TYPE: &'static str = "image_pinned";
    const SCHEMA_VERSION: u32 = 1;
}

/// Closed for new admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PoolPaused {}

impl DomainEvent for PoolPaused {
    const EVENT_TYPE: &'static str = "pool_paused";
    const SCHEMA_VERSION: u32 = 1;
}

/// Open again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PoolResumed {}

impl DomainEvent for PoolResumed {
    const EVENT_TYPE: &'static str = "pool_resumed";
    const SCHEMA_VERSION: u32 = 1;
}

/// The drain started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PoolDrainStarted {}

impl DomainEvent for PoolDrainStarted {
    const EVENT_TYPE: &'static str = "pool_drain_started";
    const SCHEMA_VERSION: u32 = 1;
}

/// The drain completed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PoolDrainCompleted {}

impl DomainEvent for PoolDrainCompleted {
    const EVENT_TYPE: &'static str = "pool_drain_completed";
    const SCHEMA_VERSION: u32 = 1;
}

/// A stage failure counted against a digest's breaker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AttemptFailureCounted {
    /// The attempt.
    pub attempt_id: AttemptId,
    /// The digest.
    pub digest: ImageDigest,
    /// The stage that failed.
    pub stage: Stage,
    /// When.
    pub at: Millis,
}

impl DomainEvent for AttemptFailureCounted {
    const EVENT_TYPE: &'static str = "attempt_failure_counted";
    const SCHEMA_VERSION: u32 = 1;
}

/// The breaker of a digest opened: nothing is admitted with it until an operator resets it
/// (I27). Version 2 names why (CP-07, P18): counted failures, or the runner version's expiry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PoolBreakerTripped {
    /// The digest.
    pub digest: ImageDigest,
    /// Failures in the window when it tripped.
    pub failures: u32,
    /// When.
    pub at: Millis,
    /// Why; `None` for a trip recorded by version 1, which tripped on failures only.
    pub reason: Option<TripReason>,
}

impl DomainEvent for PoolBreakerTripped {
    const EVENT_TYPE: &'static str = "pool_breaker_tripped";
    const SCHEMA_VERSION: u32 = 2;
}

/// Version 1 of [`PoolBreakerTripped`] (KER-05), without the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PoolBreakerTrippedV1 {
    /// The digest.
    pub digest: ImageDigest,
    /// Failures in the window when it tripped.
    pub failures: u32,
    /// When.
    pub at: Millis,
}

impl DomainEvent for PoolBreakerTrippedV1 {
    const EVENT_TYPE: &'static str = "pool_breaker_tripped";
    const SCHEMA_VERSION: u32 = 1;
}

/// Version 1 named no reason: it read the same as a failures trip, so the reason stays unknown
/// (`None`) rather than assumed.
fn upcast_tripped_v1(old: PoolBreakerTrippedV1) -> PoolEvent {
    PoolEvent::PoolBreakerTripped(PoolBreakerTripped {
        digest: old.digest,
        failures: old.failures,
        at: old.at,
        reason: None,
    })
}

/// An operator reset a digest's breaker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PoolBreakerReset {
    /// The digest.
    pub digest: ImageDigest,
    /// The operator's uid.
    pub operator_uid: u32,
}

impl DomainEvent for PoolBreakerReset {
    const EVENT_TYPE: &'static str = "pool_breaker_reset";
    const SCHEMA_VERSION: u32 = 1;
}

/// The pool left the configuration; its stream is kept (attempts reference it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PoolDecommissioned {}

impl DomainEvent for PoolDecommissioned {
    const EVENT_TYPE: &'static str = "pool_decommissioned";
    const SCHEMA_VERSION: u32 = 1;
}

event_set! {
    /// Everything that happens to a pool.
    pub enum PoolEvent {
        /// See [`PoolEpochAdvanced`].
        PoolEpochAdvanced(PoolEpochAdvanced),
        /// See [`PoolConfigured`].
        PoolConfigured(PoolConfigured),
        /// See [`ImagePinned`].
        ImagePinned(ImagePinned),
        /// See [`PoolPaused`].
        PoolPaused(PoolPaused),
        /// See [`PoolResumed`].
        PoolResumed(PoolResumed),
        /// See [`PoolDrainStarted`].
        PoolDrainStarted(PoolDrainStarted),
        /// See [`PoolDrainCompleted`].
        PoolDrainCompleted(PoolDrainCompleted),
        /// See [`AttemptFailureCounted`].
        AttemptFailureCounted(AttemptFailureCounted),
        /// See [`PoolBreakerTripped`].
        PoolBreakerTripped(PoolBreakerTripped),
        /// See [`PoolBreakerReset`].
        PoolBreakerReset(PoolBreakerReset),
        /// See [`PoolDecommissioned`].
        PoolDecommissioned(PoolDecommissioned),
    }
    legacy {
        PoolBreakerTrippedV1 => upcast_tripped_v1,
    }
}
