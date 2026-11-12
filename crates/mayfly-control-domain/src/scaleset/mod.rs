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
//! - **a1RefutationIsFlagged**: a session's first message strictly below its floor starts a new
//!   numbering, recorded as `MessageIdsRestarted` and applied as new
//!   (`tests/scaleset/rules.rs::should_restart_the_numbering_when_a_new_session_delivers_an_id_below_the_floor`);
//!   any other message at or below the cursor applies nothing and degrades the session, never
//!   silently, and only the cursor's id can be a duplicate ([`ScaleSetState::is_duplicate`];
//!   `tests/scaleset/rules.rs::should_degrade_the_session_when_an_id_at_or_below_the_cursor_comes_later_in_a_session`).
//! - **assignedJobsClassified** and **vmOnlyForAJobClassifiedTrusted** (ADR 0090): a direct
//!   assignment is pending until its run's lookup classifies it trusted or refused
//!   (`AssignedJobRefused`), and a runner starts only for a job classified trusted that waits
//!   ([`ScaleSetState::runner_may_start`];
//!   `tests/scaleset/trust.rs::should_start_a_runner_only_after_the_lookup_when_a_job_is_assigned_directly`,
//!   `tests/scaleset/trust_properties.rs::should_hold_the_trust_invariants_when_assignments_are_random`).
//! - **sinkOnlyForARefusedJob** and **cancelOnlyRefused** (ADR 0090, amended 2026-11-12): a
//!   refusal sink starts once per refusal, one at a time, without the cancel option and only
//!   while no trusted or pending assignment is known, the wire max pinned meanwhile; with
//!   `cancel_refused_runs` a refused run is cancelled instead, once
//!   ([`ScaleSetState::refusal_action`]; `tests/scaleset/trust.rs`).
//! - **Re-ensure** (`recreateScaleSet`): a 404 for the journaled scale set returns the stream to
//!   `Unconfigured`; `RecordScaleSetRecreated` resets the numbering and drops the old set's
//!   reservations, classifications and refusals
//!   (`tests/scaleset/trust.rs::should_reset_the_numbering_and_drop_reservations_when_the_scale_set_is_recreated`).
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
//! Assumptions A1–A6 on facts `doc/github-scaleset.md` leaves client-assumed or [open: HW-02] are
//! stated in the model's header and in its §12 (A2 and A5 also in [`crate::demand`]); each takes
//! the safe side.

mod cases;
mod decide;
mod events;
mod state;

pub use decide::{Decision, decide, evolve};
pub use events::{
    AcquireObserved, AssignedJobRefused, AssignedJobTrusted, CancelAnswered, CapacityApplied,
    ClosingStarted, MessageIdsRestarted, MessageRecorded, ScaleSetEnsured, ScaleSetEpochAdvanced,
    ScaleSetEvent, ScaleSetRecreated, ScaleSetVanished, SessionClosed, SessionConflictEscalated,
    SessionDegraded, SessionDeleted, SessionLost, SessionOpenRequested, SessionOpened,
    SessionRecovered, SinkEnded, SinkStarted, TrustPolicyApplied,
};
pub use state::{
    Assignment, CancelAnswer, DegradedCause, EnsureScaleSet, JobFact, LossCause, MessageId,
    MessageItem, MessageOrigin, PROTOCOL_DRIFT_THRESHOLD, Phase, RecordMessage, RefusalAction,
    SESSION_CONFLICT_ESCALATION, ScaleSetCommand, ScaleSetRejection, ScaleSetState, SessionId,
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
