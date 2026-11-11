//! The image's events (`doc/domain-model.md` §6.6), with golden samples under
//! `tests/fixtures/events/<type>/`.

use mayfly_domain::{Epoch, ImageDigest, Millis};
use serde::{Deserialize, Serialize};

use super::state::{ImageManifest, RejectReason, RunnerVersion};
use crate::event::DomainEvent;
use crate::event_set;

/// The image accepted a command under a newer run epoch than any before (I18); recorded ahead
/// of that command's events so `evolve` knows the epoch to fence with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ImageEpochAdvanced {
    /// The new highest run epoch.
    pub epoch: Epoch,
}

impl DomainEvent for ImageEpochAdvanced {
    const EVENT_TYPE: &'static str = "image_epoch_advanced";
    const SCHEMA_VERSION: u32 = 1;
}

/// An image is known by its digest and manifest, both immutable from here on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ImageRegistered {
    /// The digest.
    pub digest: ImageDigest,
    /// The manifest.
    pub manifest: ImageManifest,
}

impl DomainEvent for ImageRegistered {
    const EVENT_TYPE: &'static str = "image_registered";
    const SCHEMA_VERSION: u32 = 1;
}

/// The image passed verification: a pool may pin it (`PinImage` carries this event's id).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ImageVerified {}

impl DomainEvent for ImageVerified {
    const EVENT_TYPE: &'static str = "image_verified";
    const SCHEMA_VERSION: u32 = 1;
}

/// The image failed verification or drifted: it is never verified again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ImageRejected {
    /// Why.
    pub reason: RejectReason,
}

impl DomainEvent for ImageRejected {
    const EVENT_TYPE: &'static str = "image_rejected";
    const SCHEMA_VERSION: u32 = 1;
}

/// GitHub no longer accepts the image's runner (ADR 0030, P18): the breaker trips for the
/// digest with reason `runner_version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ImageRunnerVersionExpired {
    /// The lowest runner version GitHub accepts.
    pub minimum: RunnerVersion,
    /// When it was recorded.
    pub at: Millis,
}

impl DomainEvent for ImageRunnerVersionExpired {
    const EVENT_TYPE: &'static str = "image_runner_version_expired";
    const SCHEMA_VERSION: u32 = 1;
}

/// An operator withdrew the image: it is never verified again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ImageWithdrawn {
    /// The operator's uid.
    pub operator_uid: u32,
}

impl DomainEvent for ImageWithdrawn {
    const EVENT_TYPE: &'static str = "image_withdrawn";
    const SCHEMA_VERSION: u32 = 1;
}

event_set! {
    /// Everything that happens to an image.
    pub enum ImageEvent {
        /// See [`ImageEpochAdvanced`].
        ImageEpochAdvanced(ImageEpochAdvanced),
        /// See [`ImageRegistered`].
        ImageRegistered(ImageRegistered),
        /// See [`ImageVerified`].
        ImageVerified(ImageVerified),
        /// See [`ImageRejected`].
        ImageRejected(ImageRejected),
        /// See [`ImageRunnerVersionExpired`].
        ImageRunnerVersionExpired(ImageRunnerVersionExpired),
        /// See [`ImageWithdrawn`].
        ImageWithdrawn(ImageWithdrawn),
    }
}
