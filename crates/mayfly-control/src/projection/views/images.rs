//! `placements` and `images`: each image's status and runner version, and each placement of an
//! image on a host with its phase, base and pending directive, kept inline from each image and
//! placement stream (`doc/domain-model.md` §8, `ImageCatalog` and `PendingEffects`; CP-07). The
//! placement manager reads both (P17, P18); the composition root fills the clone details'
//! `Placements` from the materialised rows.

use mayfly_control_domain::image::{Image, ImageEvent, ImageState, ImageStatus, RunnerVersion};
use mayfly_control_domain::placement::{
    ImagePlacement, PendingOutcome, PlacementDirective, PlacementEvent, PlacementPhase,
    PlacementState,
};
use mayfly_control_domain::{EventKind, EventSet};
use mayfly_domain::{Deadline, DirectiveId, HostId, ImageDigest, Millis};
use serde::{Deserialize, Serialize};

use crate::projection::spec::{EventMeta, ProjectionSpec, RowChange, Rows, sync_rows};

/// One image. Key: the image's stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRow {
    /// The digest, once registered.
    pub digest: Option<ImageDigest>,
    /// The status, `snake_case` (`registered`, `verified`, `rejected`, `withdrawn`).
    pub status: String,
    /// The manifest's runner version, once registered.
    pub runner_version: Option<RunnerVersion>,
    /// When its runner version's expiry was recorded (P18).
    pub expired_at: Option<Millis>,
    /// The stream revision this row is the fold of.
    pub revision: u64,
}

/// The `images` projection: one row per image stream.
#[derive(Debug, Clone, Copy, Default)]
pub struct Images;

impl ProjectionSpec for Images {
    const NAME: &'static str = "images";
    const VERSION: u32 = 1;
    const SUBSCRIBES: &'static [EventKind] = <ImageEvent as EventSet>::KINDS;
    type Source = Image;
    type Row = ImageRow;

    fn apply(
        rows: &Rows<ImageRow>,
        _event: &ImageEvent,
        after: &ImageState,
        at: EventMeta,
    ) -> Vec<RowChange<ImageRow>> {
        let row = ImageRow {
            digest: after.digest.clone(),
            status: status_code(after.status).to_owned(),
            runner_version: after.manifest.map(|m| m.runner_version),
            expired_at: after.runner_expiry.map(|e| e.at),
            revision: at.revision.get(),
        };
        sync_rows(rows, [(String::new(), row)].into_iter().collect())
    }
}

/// The status's `snake_case` code.
const fn status_code(status: ImageStatus) -> &'static str {
    match status {
        ImageStatus::Unregistered => "unregistered",
        ImageStatus::Registered => "registered",
        ImageStatus::Verified => "verified",
        ImageStatus::Rejected => "rejected",
        ImageStatus::Withdrawn => "withdrawn",
    }
}

/// The pending directive of a placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementPendingRow {
    /// The directive.
    pub directive_id: DirectiveId,
    /// What it asks.
    pub directive: PlacementDirective,
    /// Its deadline.
    pub deadline: Deadline,
    /// The relay put it on the link.
    pub sent: bool,
    /// Since when its outcome is unknown, if it is.
    pub unknown_since: Option<Millis>,
}

/// One image on one host. Key: the placement's stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementRow {
    /// The host.
    pub host_id: HostId,
    /// The image.
    pub digest: ImageDigest,
    /// The phase, `snake_case` ([`phase_code`]).
    pub phase: String,
    /// The runner version the next materialisation installs.
    pub wanted: Option<RunnerVersion>,
    /// The runner version of the base clones are made from.
    pub base: Option<RunnerVersion>,
    /// That base's name, `mfb-…` (ADR 0039): what a clone is made from.
    pub base_name: Option<String>,
    /// The disk a base of this image reconstructs to, as the data plane measured it; `None`
    /// until reported (CP-07, protocol 1.4).
    pub disk_bytes: Option<u64>,
    /// The pending directive.
    pub pending: Option<PlacementPendingRow>,
    /// The stream revision this row is the fold of.
    pub revision: u64,
}

/// The `placements` projection: one row per requested placement stream.
#[derive(Debug, Clone, Copy, Default)]
pub struct Placements;

impl ProjectionSpec for Placements {
    const NAME: &'static str = "placements";
    const VERSION: u32 = 1;
    const SUBSCRIBES: &'static [EventKind] = <PlacementEvent as EventSet>::KINDS;
    type Source = ImagePlacement;
    type Row = PlacementRow;

    fn apply(
        rows: &Rows<PlacementRow>,
        _event: &PlacementEvent,
        after: &PlacementState,
        at: EventMeta,
    ) -> Vec<RowChange<PlacementRow>> {
        let (Some(host_id), Some(digest)) = (after.host_id.clone(), after.digest.clone()) else {
            return sync_rows(rows, Rows::new());
        };
        let row = PlacementRow {
            host_id,
            digest,
            phase: phase_code(after.phase).to_owned(),
            wanted: after.wanted,
            base: after.base,
            base_name: after.base_name(),
            disk_bytes: after.disk_bytes,
            pending: after.pending.map(|p| PlacementPendingRow {
                directive_id: p.directive_id,
                directive: p.directive,
                deadline: p.deadline,
                sent: p.sent,
                unknown_since: match p.outcome {
                    PendingOutcome::InFlight => None,
                    PendingOutcome::Unknown { since } => Some(since),
                },
            }),
            revision: at.revision.get(),
        };
        sync_rows(rows, [(String::new(), row)].into_iter().collect())
    }
}

/// The phase's `snake_case` code; [`phase_of`] reads it back.
#[must_use]
pub const fn phase_code(phase: PlacementPhase) -> &'static str {
    match phase {
        PlacementPhase::Unrequested => "unrequested",
        PlacementPhase::Wanted => "wanted",
        PlacementPhase::Pulling => "pulling",
        PlacementPhase::Pulled => "pulled",
        PlacementPhase::Materialised => "materialised",
        PlacementPhase::Evicting => "evicting",
        PlacementPhase::Evicted => "evicted",
        PlacementPhase::Failed => "failed",
    }
}

/// The phase a [`phase_code`] names; `None` for a code no build wrote.
#[must_use]
pub fn phase_of(code: &str) -> Option<PlacementPhase> {
    Some(match code {
        "unrequested" => PlacementPhase::Unrequested,
        "wanted" => PlacementPhase::Wanted,
        "pulling" => PlacementPhase::Pulling,
        "pulled" => PlacementPhase::Pulled,
        "materialised" => PlacementPhase::Materialised,
        "evicting" => PlacementPhase::Evicting,
        "evicted" => PlacementPhase::Evicted,
        "failed" => PlacementPhase::Failed,
        _ => return None,
    })
}
