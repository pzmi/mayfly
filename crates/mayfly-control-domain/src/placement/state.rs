//! The placement's folded state, the value types its events carry, its commands and its
//! refusals.

use mayfly_domain::{Deadline, DirectiveId, Epoch, HostId, ImageDigest, Millis};
use serde::{Deserialize, Serialize};

use crate::aggregate::{CommandBody, Rejection};
use crate::attempt::RefusalReason;
use crate::image::{RunnerVersion, base_name, digest12};

/// The stream instance of the placement of `digest` on `host`: `<host_id>-<digest12>`, so the
/// stream is `placement-<host_id>-<digest12>` (`doc/domain-model.md` §6.7).
#[must_use]
pub fn placement_instance(host: &HostId, digest: &ImageDigest) -> String {
    format!("{host}-{}", digest12(digest))
}

/// What a placement directive asks the data plane to do; the digest is the stream's, and a base
/// is named by the digest and its runner version ([`base_name`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementDirective {
    /// Pull the image by digest, never by tag, and re-hash it (dm §9.3).
    PullImage,
    /// Materialise the base for this runner version from the pulled image (ADR 0039).
    MaterialiseImage {
        /// The runner installed into the base.
        runner_version: RunnerVersion,
    },
    /// Evict the base of this runner version (P17).
    EvictImage {
        /// The base's runner version.
        runner_version: RunnerVersion,
    },
}

impl PlacementDirective {
    /// The base this directive creates or removes, for `digest`; `None` for a pull.
    #[must_use]
    pub fn base_name(self, digest: &ImageDigest) -> Option<String> {
        match self {
            Self::PullImage => None,
            Self::MaterialiseImage { runner_version } | Self::EvictImage { runner_version } => {
                Some(base_name(digest, runner_version))
            }
        }
    }
}

/// Which bases an eviction removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvictionScope {
    /// Only the base kept for rollback.
    Previous,
    /// Every base of the placement: the previous one first, then the current one.
    Placement,
}

/// The placement's phase (`doc/domain-model.md` §6.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementPhase {
    /// No `PlacementRequested` yet.
    Unrequested,
    /// Requested: a pull is to be issued.
    Wanted,
    /// A pull is pending: the only one for this host and digest.
    Pulling,
    /// Pulled and re-hashed; the first base is to be materialised.
    Pulled,
    /// A base exists: clones may be made from [`PlacementState::base`].
    Materialised,
    /// An eviction is under way.
    Evicting,
    /// Every base is evicted; a new request starts again.
    Evicted,
    /// The pull or the first materialisation failed, or the image drifted.
    Failed,
}

/// Whether the pending directive's outcome is known to be still on its way, or unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingOutcome {
    /// Issued; an answer may still come.
    InFlight,
    /// The deadline passed without an answer: whether the effect happened is not known. Leaves
    /// only on an observation (I10, ADR 0025).
    Unknown {
        /// When the outcome became unknown.
        since: Millis,
    },
}

/// The `pending` overlay: at most one directive per placement, so at most one pull per host and
/// digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pending {
    /// The directive.
    pub directive_id: DirectiveId,
    /// What it asks.
    pub directive: PlacementDirective,
    /// Its deadline: the one it was issued with until it is sent, then the one its frame
    /// carried (`PlacementDirectiveSent`).
    pub deadline: Deadline,
    /// The relay put it on the link: only then can it time out (a directive held unsent, in safe
    /// mode or without a session, did nothing, ADR 0025).
    pub sent: bool,
    /// The run epoch of the session that carried it, once sent (`PlacementDirectiveSent` v2);
    /// `None` before, or when a version-1 event did not record it. A listing of a later epoch
    /// was taken after that session ended, so after every effect the directive could have had,
    /// finished or lost, and after any removal resolved before the directive was issued (DP-08).
    pub sent_epoch: Option<Epoch>,
    /// In flight or unknown.
    pub outcome: PendingOutcome,
}

/// Why a placement failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementFailure {
    /// The data plane definitely did not apply the pull or the first materialisation.
    Refused(RefusalReason),
    /// The pulled content re-hashed to another digest.
    Drift,
}

/// The state of one placement stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementState {
    /// The phase.
    pub phase: PlacementPhase,
    /// The highest run epoch accepted (I18).
    pub epoch: Epoch,
    /// The host, fixed by the first request.
    pub host_id: Option<HostId>,
    /// The digest, fixed by the first request.
    pub digest: Option<ImageDigest>,
    /// The runner version the next materialisation installs.
    pub wanted: Option<RunnerVersion>,
    /// The base pointer: the runner version of the base clones are made from.
    pub base: Option<RunnerVersion>,
    /// The base the pointer moved from, kept for rollback (ADR 0039 decision 2) until it is
    /// evicted.
    pub previous: Option<RunnerVersion>,
    /// The eviction under way.
    pub eviction: Option<EvictionScope>,
    /// The pending directive.
    pub pending: Option<Pending>,
    /// Why it failed, in `Failed`.
    pub failure: Option<PlacementFailure>,
    /// The disk a base of this image reconstructs to, as the data plane last measured it (every
    /// base of one digest is its image's size); `None` until one is reported.
    pub disk_bytes: Option<u64>,
}

impl PlacementState {
    /// The state of a stream with no events.
    #[must_use]
    pub const fn initial() -> Self {
        Self {
            phase: PlacementPhase::Unrequested,
            epoch: Epoch::new(0),
            host_id: None,
            digest: None,
            wanted: None,
            base: None,
            previous: None,
            eviction: None,
            pending: None,
            failure: None,
            disk_bytes: None,
        }
    }

    /// The name of the base clones are made from, while one is materialised.
    #[must_use]
    pub fn base_name(&self) -> Option<String> {
        Some(base_name(self.digest.as_ref()?, self.base?))
    }
}

/// The commands of `doc/domain-model.md` §6.7, with the observations that answer its
/// directives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum PlacementCommand {
    /// P17: the image is wanted on the host, with this runner version.
    RequestPlacement {
        /// The host.
        host_id: HostId,
        /// The image.
        digest: ImageDigest,
        /// The runner version of the first base.
        runner_version: RunnerVersion,
    },
    /// The placement manager issues the next directive, if one is due.
    IssueDirective {
        /// The directive's id.
        directive_id: DirectiveId,
        /// Its deadline.
        deadline: Deadline,
    },
    /// The directive relay put the pending directive on the link (CP-02).
    RecordSent {
        /// The directive.
        directive_id: DirectiveId,
        /// The deadline the frame carried.
        deadline: Deadline,
    },
    /// The pull completed and re-hashed to the digest.
    RecordPulled {
        /// The directive answered.
        directive_id: DirectiveId,
    },
    /// The pulled content re-hashed to another digest.
    RecordDrift {
        /// The directive answered.
        directive_id: DirectiveId,
        /// What it hashed to.
        observed: ImageDigest,
    },
    /// The base was materialised.
    RecordMaterialised {
        /// The directive answered.
        directive_id: DirectiveId,
        /// The base's disk as the data plane measured it, when it reported one (protocol 1.4);
        /// left out of the intent when absent, so an answer without it digests as before.
        #[serde(skip_serializing_if = "Option::is_none")]
        disk_bytes: Option<u64>,
    },
    /// The base was evicted.
    RecordEvicted {
        /// The directive answered.
        directive_id: DirectiveId,
    },
    /// The data plane definitely did not apply the directive.
    RecordRefused {
        /// The directive answered.
        directive_id: DirectiveId,
        /// Why.
        reason: RefusalReason,
    },
    /// The deadline passed without an answer.
    RecordOutcomeUnknown {
        /// The directive.
        directive_id: DirectiveId,
    },
    /// A complete listing showed that the directive, whose outcome was unknown, did not apply.
    RecordNotApplied {
        /// The directive.
        directive_id: DirectiveId,
    },
    /// Materialise another base of the same digest (a runner-only refresh, ADR 0039 decision 4).
    RequestRefresh {
        /// The runner version of the new base.
        runner_version: RunnerVersion,
    },
    /// Move the base pointer back to the previous base.
    RollBackBase,
    /// Evict bases nothing references (P17).
    RequestEviction {
        /// Which bases.
        scope: EvictionScope,
        /// Pools and live clones referencing them, counted by the policy (the data plane
        /// re-checks against its base set).
        references: u32,
    },
}

impl CommandBody for PlacementCommand {
    const TYPES: &'static [&'static str] = &[
        "request_placement",
        "issue_directive",
        "record_sent",
        "record_pulled",
        "record_drift",
        "record_materialised",
        "record_evicted",
        "record_refused",
        "record_outcome_unknown",
        "record_not_applied",
        "request_refresh",
        "roll_back_base",
        "request_eviction",
    ];

    fn command_type(&self) -> &'static str {
        match self {
            Self::RequestPlacement { .. } => "request_placement",
            Self::IssueDirective { .. } => "issue_directive",
            Self::RecordSent { .. } => "record_sent",
            Self::RecordPulled { .. } => "record_pulled",
            Self::RecordDrift { .. } => "record_drift",
            Self::RecordMaterialised { .. } => "record_materialised",
            Self::RecordEvicted { .. } => "record_evicted",
            Self::RecordRefused { .. } => "record_refused",
            Self::RecordOutcomeUnknown { .. } => "record_outcome_unknown",
            Self::RecordNotApplied { .. } => "record_not_applied",
            Self::RequestRefresh { .. } => "request_refresh",
            Self::RollBackBase => "roll_back_base",
            Self::RequestEviction { .. } => "request_eviction",
        }
    }
}

/// A refusal of the `ImagePlacement` aggregate. Its serde form is its code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementRejection {
    /// The stream has no `PlacementRequested`.
    NotRequested,
    /// The command's run epoch is older than the placement's (I18).
    StaleRun,
    /// A request for another host or digest than the stream's.
    PlacementDiffers,
    /// A directive is pending: one at a time, so one pull per host and digest.
    DirectivePending,
    /// No directive is due in this phase.
    NothingToIssue,
    /// The answer names no pending directive of its kind.
    NotPending,
    /// A drift report whose observed digest is the expected one.
    NoDrift,
    /// A timeout of a directive never sent: held unsent, nothing happened.
    NotSent,
    /// The pending directive's deadline has not passed.
    DeadlineNotPassed,
    /// "Not applied" of a directive whose outcome is not unknown.
    OutcomeNotUnknown,
    /// Refresh, rollback and eviction need a materialised base.
    NotMaterialised,
    /// An eviction while a pool or a live clone references the base.
    BaseReferenced,
    /// A rollback or an eviction of the previous base when none is kept.
    NoPreviousBase,
    /// A refresh while the previous base is still kept: evict it first (at most two bases).
    PreviousBaseHeld,
}

impl Rejection for PlacementRejection {
    const CODES: &'static [&'static str] = &[
        "not_requested",
        "stale_run",
        "placement_differs",
        "directive_pending",
        "nothing_to_issue",
        "not_pending",
        "no_drift",
        "not_sent",
        "deadline_not_passed",
        "outcome_not_unknown",
        "not_materialised",
        "base_referenced",
        "no_previous_base",
        "previous_base_held",
    ];

    fn code(&self) -> &'static str {
        match self {
            Self::NotRequested => "not_requested",
            Self::StaleRun => "stale_run",
            Self::PlacementDiffers => "placement_differs",
            Self::DirectivePending => "directive_pending",
            Self::NothingToIssue => "nothing_to_issue",
            Self::NotPending => "not_pending",
            Self::NoDrift => "no_drift",
            Self::NotSent => "not_sent",
            Self::DeadlineNotPassed => "deadline_not_passed",
            Self::OutcomeNotUnknown => "outcome_not_unknown",
            Self::NotMaterialised => "not_materialised",
            Self::BaseReferenced => "base_referenced",
            Self::NoPreviousBase => "no_previous_base",
            Self::PreviousBaseHeld => "previous_base_held",
        }
    }
}
