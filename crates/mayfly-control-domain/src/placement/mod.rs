//! The `ImagePlacement` aggregate (`doc/domain-model.md` §6.7): one image digest on one host,
//! from pull to eviction, stream `placement-<host_id>-<digest12>` ([`placement_instance`]).
//!
//! Plain data and two pure functions (ADR 0022). Its directives (`PullImage`,
//! `MaterialiseImage`, `EvictImage`) are events on this stream (ADR 0034), executed by the data
//! plane (DP-07) and answered by observations. It protects alone:
//!
//! - **One pull per host and digest**: one directive pending at a time, in flight or unknown,
//!   and a second request while the placement is under way records nothing
//!   (`tests/placement/rules.rs::should_pull_once_when_a_pull_is_pending_or_unknown`,
//!   `tests/placement/rules.rs::should_record_nothing_when_the_digest_is_requested_again`).
//! - **Bases named by digest and runner version** ([`crate::image::base_name`], ADR 0039), the
//!   only names its directives carry, so no VM directive targets a base here (I05)
//!   (`tests/placement/rules.rs::should_name_the_base_by_digest_and_runner_version_when_materialising`).
//! - **Eviction only from `Materialised` with zero references** (P17; the count is the
//!   policy's, re-checked by the data plane against its base set)
//!   (`tests/placement/rules.rs::should_refuse_eviction_when_the_base_is_referenced`,
//!   `tests/placement/rules.rs::should_refuse_eviction_when_the_placement_is_not_materialised`).
//! - **The base pointer and the previous base kept for rollback** (ADR 0030 decision 3, ADR 0039
//!   decision 2): a materialisation moves the pointer and keeps the base it left; a rollback
//!   swaps them; at most two bases, so a refresh waits until the previous is evicted
//!   (`tests/placement/rules.rs::should_keep_the_previous_base_when_a_refresh_is_materialised`,
//!   `tests/placement/rules.rs::should_point_back_at_the_previous_base_when_rolled_back`,
//!   `tests/placement/rules.rs::should_refuse_a_refresh_when_the_previous_base_is_kept`).
//! - **I10**: a directive's outcome becomes unknown only once it was sent and its deadline
//!   passed (a directive held unsent did nothing, ADR 0025), and leaves unknown
//!   only on an observation
//!   (`tests/placement/rules.rs::should_leave_unknown_only_on_an_observation_when_a_pull_times_out`).
//! - **I18**: an older run epoch is refused (`stale_run`); a newer one is recorded as
//!   `PlacementEpochAdvanced` ahead of the command's events
//!   (`tests/placement/rules.rs::should_refuse_an_older_run_epoch_when_a_newer_one_was_accepted`).
//!
//! The digest's own pin and the previous digest are the `Pool`'s (`ImagePinned{digest,
//! previous}`); this stream keeps the base pointer within one digest. Not modelled in
//! `formal/`: one stream whose effects are single-flight by construction; see
//! `doc/formal-models.md`, "Not modelled". The cross-stream rules (P17: a pool admissible on a
//! host only once its pin is materialised there, a base evicted only once nothing references
//! it) are `formal/scheduling.qnt`'s `placedOn` and `leasesKeepTheirBase` (CP-07); the
//! placement manager's pure pass is [`policy::plan`].

mod cases;
mod decide;
mod events;
pub mod policy;
mod state;

pub use decide::{Decision, decide, evolve};
pub use events::{
    BaseRefreshRequested, BaseRolledBack, EvictionRequested, ImageDriftDetected, ImageEvicted,
    ImageMaterialised, ImagePulled, PlacementDirectiveIssued, PlacementDirectiveNotApplied,
    PlacementDirectiveRefused, PlacementDirectiveSent, PlacementEpochAdvanced, PlacementEvent,
    PlacementFailed, PlacementOutcomeUnknown, PlacementRequested,
};
pub use state::{
    EvictionScope, Pending, PendingOutcome, PlacementCommand, PlacementDirective, PlacementFailure,
    PlacementPhase, PlacementRejection, PlacementState, placement_instance,
};

use crate::aggregate::{Aggregate, DeterminismCase, DeterminismCases};
use crate::command::Command;

/// The `ImagePlacement` aggregate: a marker grouping its data and functions (ADR 0022).
#[derive(Debug, Clone, Copy, Default)]
pub struct ImagePlacement;

impl Aggregate for ImagePlacement {
    const NAME: &'static str = "placement";
    type State = PlacementState;
    type Body = PlacementCommand;
    type Event = PlacementEvent;
    type Rejection = PlacementRejection;

    fn initial() -> PlacementState {
        PlacementState::initial()
    }

    fn decide(state: &PlacementState, command: Command<PlacementCommand>) -> Decision {
        decide::decide(state, command)
    }

    fn evolve(state: PlacementState, event: &PlacementEvent) -> PlacementState {
        decide::evolve(state, event)
    }
}

impl DeterminismCases for ImagePlacement {
    fn determinism_cases() -> Vec<DeterminismCase<Self>> {
        cases::cases()
    }
}
