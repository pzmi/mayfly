//! The `ScaleSet` aggregate (`doc/domain-model.md` §6.1): one pool's GitHub scale set and its
//! message-queue session, stream `scaleset-<pool>` (one scale set per pool, ADR 0040).
//!
//! Plain data and two pure functions (ADR 0022); the demand rules it applies are the pure
//! functions of [`crate::demand`]. The model is `formal/scaleset.qnt`; its invariants and the
//! tests that pin them here:
//!
//! - **I12** (record before ack): the listener acks a message only after this stream's
//!   `RecordMessage` receipt commits (GH-04); a redelivery in the same session replays that
//!   receipt (command id `(session, message)`) and writes nothing. Here: every message the
//!   listener may hold is recordable, in every phase with a live session, also degraded or
//!   closing, and in safe mode
//!   (`tests/scaleset/rules.rs::should_record_every_message_when_a_session_is_live_in_any_mode`).
//! - **redeliveryAppliesNothing** (A1): a message at or below the session's floor (the cursor at
//!   its open) was recorded in an earlier session; it is recorded again so it can be acked and
//!   applies nothing: no statistics, no plan, no release, no unknown types
//!   (`tests/scaleset/rules.rs::should_apply_nothing_when_a_message_from_an_earlier_session_is_redelivered`).
//! - **a1RefutationIsFlagged**: any other message at or below the floor (ids restarted or went
//!   back) applies nothing and degrades the session, never silently
//!   (`tests/scaleset/rules.rs::should_degrade_the_session_when_a_new_session_delivers_an_id_below_the_cursor`).
//! - **persistentConflictsAreFlagged** (A3): the `SESSION_CONFLICT_ESCALATION`th create in a row
//!   answered 409 is escalated to the operator
//!   (`tests/scaleset/rules.rs::should_escalate_once_when_creates_keep_answering_conflict`).
//! - **noDoubleReservationPerRequest**: a request id is reserved at most once, whatever a
//!   redelivery in a later session, a repeated offer or a reassignment shows
//!   (`tests/scaleset/rules.rs::should_reserve_a_request_once_when_a_message_is_recorded_again_in_a_new_session`,
//!   `tests/scaleset/properties.rs::should_hold_the_reservation_invariants_when_messages_are_random`).
//! - **desiredFromLatestStatistics**: `desired = min(deliverable, totalAssignedJobs)` of the
//!   latest statistics by message id (a session's initial ones, or a newer message's; an older
//!   message never replaces them). A message emitted before an open but recorded after it has no
//!   id that tells it apart and applies its older statistics until the backlog is consumed (A6:
//!   safe for I23, and the model shows it converges to GitHub's own count)
//!   (`tests/scaleset/rules.rs::should_keep_newer_statistics_when_an_older_message_is_recorded`,
//!   `tests/scaleset/rules.rs::should_recompute_desired_from_initial_statistics_when_a_session_opens`).
//! - **I23**: offers are acquired only while assigned plus open reservations stay below
//!   deliverable (`tests/scaleset/rules.rs::should_acquire_only_while_assigned_is_below_deliverable_when_a_batch_exceeds_it`).
//! - **I32**: only offers the pool's tier admits are reserved
//!   (`tests/scaleset/rules.rs::should_decline_untrusted_offers_when_a_message_is_recorded`).
//! - **ADR 0029**: pause, drain and safe mode keep the session, change the wire max and acquire
//!   nothing (`tests/scaleset/rules.rs::should_acquire_nothing_and_send_the_mode_wire_max_when_held`);
//!   nor does a degraded (P21) or closing session
//!   (`tests/scaleset/rules.rs::should_plan_no_offer_when_the_session_is_degraded_or_closing`).
//! - **I18**: an older run epoch is refused (`stale_run`).
//!
//! Assumptions A1–A6 on facts `doc/github-scaleset.md` marks [unverified: GH-00] are stated in
//! the model's header and in its §12 (A2 and A5 also in [`crate::demand`]); each takes the safe
//! side.

mod cases;
mod decide;
mod events;
mod state;

pub use decide::{Decision, decide, evolve};
pub use events::{
    AcquireObserved, CapacityApplied, ClosingStarted, MessageRecorded, ScaleSetEnsured,
    ScaleSetEpochAdvanced, ScaleSetEvent, SessionClosed, SessionConflictEscalated, SessionDegraded,
    SessionDeleted, SessionLost, SessionOpenRequested, SessionOpened, SessionRecovered,
};
pub use state::{
    DegradedCause, EnsureScaleSet, JobFact, LossCause, MessageId, MessageItem, MessageOrigin,
    PROTOCOL_DRIFT_THRESHOLD, Phase, RecordMessage, SESSION_CONFLICT_ESCALATION, ScaleSetCommand,
    ScaleSetRejection, ScaleSetState, SessionId,
};

use crate::aggregate::{Aggregate, DeterminismCase, DeterminismCases};
use crate::command::Command;

/// The `ScaleSet` aggregate: a marker grouping its data and functions (ADR 0022).
#[derive(Debug, Clone, Copy, Default)]
pub struct ScaleSet;

impl Aggregate for ScaleSet {
    const NAME: &'static str = "scaleset";
    type State = ScaleSetState;
    type Body = ScaleSetCommand;
    type Event = ScaleSetEvent;
    type Rejection = ScaleSetRejection;

    fn initial() -> ScaleSetState {
        ScaleSetState::initial()
    }

    fn decide(state: &ScaleSetState, command: Command<ScaleSetCommand>) -> Decision {
        decide::decide(state, command)
    }

    fn evolve(state: ScaleSetState, event: &ScaleSetEvent) -> ScaleSetState {
        decide::evolve(state, event)
    }
}

impl DeterminismCases for ScaleSet {
    fn determinism_cases() -> Vec<DeterminismCase<Self>> {
        cases::cases()
    }
}
