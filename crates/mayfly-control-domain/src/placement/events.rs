//! The placement's events (`doc/domain-model.md` §6.7), with golden samples under
//! `tests/fixtures/events/<type>/`. Event types are unique across the registry, so the
//! directive bookkeeping that the attempt also has is named `placement_*` here.

use mayfly_domain::{Deadline, DirectiveId, Epoch, HostId, ImageDigest, Millis};
use serde::{Deserialize, Serialize};

use super::state::{EvictionScope, PlacementDirective};
use crate::attempt::RefusalReason;
use crate::event::DomainEvent;
use crate::event_set;
use crate::image::RunnerVersion;

/// The placement accepted a command under a newer run epoch than any before (I18); recorded
/// ahead of that command's events so `evolve` knows the epoch to fence with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PlacementEpochAdvanced {
    /// The new highest run epoch.
    pub epoch: Epoch,
}

impl DomainEvent for PlacementEpochAdvanced {
    const EVENT_TYPE: &'static str = "placement_epoch_advanced";
    const SCHEMA_VERSION: u32 = 1;
}

/// The image is wanted on the host (P17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PlacementRequested {
    /// The host.
    pub host_id: HostId,
    /// The image.
    pub digest: ImageDigest,
    /// The runner version of the first base.
    pub runner_version: RunnerVersion,
}

impl DomainEvent for PlacementRequested {
    const EVENT_TYPE: &'static str = "placement_requested";
    const SCHEMA_VERSION: u32 = 1;
}

/// A directive's intent, persisted before it is sent (ADR 0034).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PlacementDirectiveIssued {
    /// The directive.
    pub directive_id: DirectiveId,
    /// What it asks.
    pub directive: PlacementDirective,
    /// By when.
    pub deadline: Deadline,
}

impl DomainEvent for PlacementDirectiveIssued {
    const EVENT_TYPE: &'static str = "placement_directive_issued";
    const SCHEMA_VERSION: u32 = 1;
}

/// The directive relay put the pending directive on the link (CP-02): from now on its deadline
/// runs. A directive held unsent never times out (ADR 0025).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PlacementDirectiveSent {
    /// The directive.
    pub directive_id: DirectiveId,
    /// The deadline the frame carried.
    pub deadline: Deadline,
}

impl DomainEvent for PlacementDirectiveSent {
    const EVENT_TYPE: &'static str = "placement_directive_sent";
    const SCHEMA_VERSION: u32 = 1;
}

/// Whether the pending directive took effect is not known; only an observation resolves it
/// (I10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PlacementOutcomeUnknown {
    /// The directive.
    pub directive_id: DirectiveId,
    /// When it became unknown.
    pub since: Millis,
}

impl DomainEvent for PlacementOutcomeUnknown {
    const EVENT_TYPE: &'static str = "placement_outcome_unknown";
    const SCHEMA_VERSION: u32 = 1;
}

/// A complete listing showed that the pending directive, whose outcome was unknown, did not
/// apply: it may be issued again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PlacementDirectiveNotApplied {
    /// The directive.
    pub directive_id: DirectiveId,
}

impl DomainEvent for PlacementDirectiveNotApplied {
    const EVENT_TYPE: &'static str = "placement_directive_not_applied";
    const SCHEMA_VERSION: u32 = 1;
}

/// The data plane definitely did not apply the pending directive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PlacementDirectiveRefused {
    /// The directive.
    pub directive_id: DirectiveId,
    /// Why.
    pub reason: RefusalReason,
}

impl DomainEvent for PlacementDirectiveRefused {
    const EVENT_TYPE: &'static str = "placement_directive_refused";
    const SCHEMA_VERSION: u32 = 1;
}

/// The image was pulled by digest and re-hashed to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ImagePulled {
    /// The directive answered.
    pub directive_id: DirectiveId,
}

impl DomainEvent for ImagePulled {
    const EVENT_TYPE: &'static str = "image_pulled";
    const SCHEMA_VERSION: u32 = 1;
}

/// The pulled content re-hashed to another digest (dm §9.3); P17 rejects the image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ImageDriftDetected {
    /// The directive answered.
    pub directive_id: DirectiveId,
    /// The digest pulled.
    pub expected: ImageDigest,
    /// What it hashed to.
    pub observed: ImageDigest,
}

impl DomainEvent for ImageDriftDetected {
    const EVENT_TYPE: &'static str = "image_drift_detected";
    const SCHEMA_VERSION: u32 = 1;
}

/// The placement failed: its pull or its first materialisation was refused, or it drifted. A
/// new request starts again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PlacementFailed {}

impl DomainEvent for PlacementFailed {
    const EVENT_TYPE: &'static str = "placement_failed";
    const SCHEMA_VERSION: u32 = 1;
}

/// A base named by the digest and this runner version exists; the base pointer moves to it and
/// the base it moved from is kept for rollback. Version 2 records the disk the base
/// reconstructs to, as the data plane measured it from the image's manifest (CP-07, protocol
/// 1.4): what the control plane plans a clone at when `clone_disk_gib` is not configured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ImageMaterialised {
    /// The directive answered.
    pub directive_id: DirectiveId,
    /// The runner installed: with the digest, the base's name.
    pub runner_version: RunnerVersion,
    /// The base's measured disk; `None` when the data plane did not report it (an older data
    /// plane, or a manifest that did not measure): unknown, never a default (ADR 0052).
    pub disk_bytes: Option<u64>,
}

impl DomainEvent for ImageMaterialised {
    const EVENT_TYPE: &'static str = "image_materialised";
    const SCHEMA_VERSION: u32 = 2;
}

/// Version 1 of [`ImageMaterialised`] (KER-11), without the disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ImageMaterialisedV1 {
    /// The directive answered.
    pub directive_id: DirectiveId,
    /// The runner installed: with the digest, the base's name.
    pub runner_version: RunnerVersion,
}

impl DomainEvent for ImageMaterialisedV1 {
    const EVENT_TYPE: &'static str = "image_materialised";
    const SCHEMA_VERSION: u32 = 1;
}

/// Version 1 recorded no disk: it reads as unknown, so a clone of that base is planned with no
/// disk and the data plane's gate (I33) refuses what does not fit, as before CP-07.
fn upcast_materialised_v1(old: ImageMaterialisedV1) -> PlacementEvent {
    PlacementEvent::ImageMaterialised(ImageMaterialised {
        directive_id: old.directive_id,
        runner_version: old.runner_version,
        disk_bytes: None,
    })
}

/// Another base of the same digest is wanted (a runner-only refresh).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BaseRefreshRequested {
    /// The runner version of the new base.
    pub runner_version: RunnerVersion,
}

impl DomainEvent for BaseRefreshRequested {
    const EVENT_TYPE: &'static str = "base_refresh_requested";
    const SCHEMA_VERSION: u32 = 1;
}

/// The base pointer moved back to the previous base; the base it left is kept as the previous.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BaseRolledBack {
    /// The runner version of the base now pointed at.
    pub to: RunnerVersion,
    /// The runner version of the base it left.
    pub from: RunnerVersion,
}

impl DomainEvent for BaseRolledBack {
    const EVENT_TYPE: &'static str = "base_rolled_back";
    const SCHEMA_VERSION: u32 = 1;
}

/// An eviction of bases nothing references started (P17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EvictionRequested {
    /// Which bases.
    pub scope: EvictionScope,
}

impl DomainEvent for EvictionRequested {
    const EVENT_TYPE: &'static str = "eviction_requested";
    const SCHEMA_VERSION: u32 = 1;
}

/// A base was evicted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ImageEvicted {
    /// The directive answered.
    pub directive_id: DirectiveId,
    /// The evicted base's runner version.
    pub runner_version: RunnerVersion,
}

impl DomainEvent for ImageEvicted {
    const EVENT_TYPE: &'static str = "image_evicted";
    const SCHEMA_VERSION: u32 = 1;
}

event_set! {
    /// Everything that happens to a placement.
    pub enum PlacementEvent {
        /// See [`PlacementEpochAdvanced`].
        PlacementEpochAdvanced(PlacementEpochAdvanced),
        /// See [`PlacementRequested`].
        PlacementRequested(PlacementRequested),
        /// See [`PlacementDirectiveIssued`].
        PlacementDirectiveIssued(PlacementDirectiveIssued),
        /// See [`PlacementDirectiveSent`].
        PlacementDirectiveSent(PlacementDirectiveSent),
        /// See [`PlacementOutcomeUnknown`].
        PlacementOutcomeUnknown(PlacementOutcomeUnknown),
        /// See [`PlacementDirectiveNotApplied`].
        PlacementDirectiveNotApplied(PlacementDirectiveNotApplied),
        /// See [`PlacementDirectiveRefused`].
        PlacementDirectiveRefused(PlacementDirectiveRefused),
        /// See [`ImagePulled`].
        ImagePulled(ImagePulled),
        /// See [`ImageDriftDetected`].
        ImageDriftDetected(ImageDriftDetected),
        /// See [`PlacementFailed`].
        PlacementFailed(PlacementFailed),
        /// See [`ImageMaterialised`].
        ImageMaterialised(ImageMaterialised),
        /// See [`BaseRefreshRequested`].
        BaseRefreshRequested(BaseRefreshRequested),
        /// See [`BaseRolledBack`].
        BaseRolledBack(BaseRolledBack),
        /// See [`EvictionRequested`].
        EvictionRequested(EvictionRequested),
        /// See [`ImageEvicted`].
        ImageEvicted(ImageEvicted),
    }
    legacy {
        ImageMaterialisedV1 => upcast_materialised_v1,
    }
}
