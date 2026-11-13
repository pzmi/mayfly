//! The `RunnerRegistration` aggregate (`doc/domain-model.md` §6.5): one attempt's JIT runner
//! registration at GitHub, stream `registration-<attempt_id>`, and policy P11's pure decision
//! ([`retirement_step`]).
//!
//! Plain data and two pure functions (ADR 0022). The model is `formal/registration.qnt`; its
//! invariants and the tests that pin them here:
//!
//! - **I19**: a runner name is issued at most once (one `RequestRegistration` per stream, under
//!   the attempt's own VM name), and a deregistration needs a journaled runner id
//!   (`tests/registration/rules.rs::should_never_issue_a_runner_name_twice_when_any_command_follows`,
//!   `tests/registration/rules.rs::should_refuse_a_deregistration_when_no_runner_id_is_journaled`).
//! - **noRemintAfterUnknown**: an issue of unknown outcome leaves `IssueUnknown` only on the
//!   reaper's observation (found by name, or absent), never by a new mint or any other command
//!   (`tests/registration/rules.rs::should_leave_an_unknown_issue_only_on_the_reapers_observation_when_every_command_is_tried`).
//! - **I21, deregisterBeforeStop**: a retirement is confirmed only when no runner of the
//!   registration can still be registered with its config in the VM; a busy answer is final for
//!   the retirement it served
//!   (`tests/registration/rules.rs::should_never_confirm_a_retirement_when_a_runner_may_still_be_registered`,
//!   `tests/registration/rules.rs::should_refuse_the_same_retirement_when_github_answered_busy`).
//! - **I22 (registration half)**: the online confirmation names the issued runner id; a
//!   registration presumed leaked stays leaked and is deregistered
//!   (`tests/registration/rules.rs::should_confirm_only_the_issued_runner_id_when_a_runner_is_seen_online`,
//!   `tests/registration/rules.rs::should_stay_leaked_and_deregister_when_a_delivery_is_presumed_leaked`).
//! - **I18**: an older run epoch is refused (`stale_run`); a newer one is recorded as
//!   `RegistrationEpochAdvanced` ahead of the command's events
//!   (`tests/registration/rules.rs::should_refuse_an_older_run_epoch_when_a_newer_one_was_accepted`).
//!
//! I20 (every issued registration ends terminal or on the reaper's list) is the reaper's (CP-05)
//! with this stream's states: `IssueUnknown`, a leaked runner and a pending deregistration are
//! what it lists. The removal of a busy runner is named busy, never by its status code: 409 on
//! the Actions-service path mayfly uses, 422 on the REST path (`doc/github-scaleset.md` §7).
//! No event carries the JIT config.
//!
//! **Limitations.** `RecordAbsent` after an unknown issue is terminal and trusts the reaper's
//! listing by name: if GitHub's listing lagged behind a lost mint, a runner that appears later
//! would be on no reaper list through this stream. The reaper (CP-05,
//! `mayfly_control::registration`) therefore records it only after a second empty listing at
//! least a grace period after the first (`formal/registration.qnt`, `absentOnlyWhenNoRunner`),
//! and P14's listing still reports a runner of our name shape with no journaled id, never
//! deleting it.
//! A busy answer cancels only the retirement whose request it answered: a stale request of
//! retirement A answered busy after the attempt moved to retirement B cancels A, not B.

mod cases;
mod decide;
mod events;
mod retirement;
mod state;

pub use decide::{Decision, decide, evolve};
pub use events::{
    DeregistrationRefusedBusy, DeregistrationRequested, RegistrationAbsent, RegistrationCompleted,
    RegistrationConnected, RegistrationConsumed, RegistrationDelivered, RegistrationDeregistered,
    RegistrationEpochAdvanced, RegistrationEvent, RegistrationFoundByName,
    RegistrationIssueUnknown, RegistrationIssued, RegistrationPresumedLeaked,
    RegistrationRequested,
};
pub use retirement::{RetirementStep, retirement_step};
pub use state::{
    DeregistrationAnswer, DeregistrationReason, JobResult, LeakReason, Phase, RegistrationCommand,
    RegistrationRejection, RegistrationState, Removal, RequestRegistration, RunnerId, ScaleSetId,
    SessionHealth,
};

use crate::aggregate::{Aggregate, DeterminismCase, DeterminismCases};
use crate::command::Command;

/// The `RunnerRegistration` aggregate: a marker grouping its data and functions (ADR 0022).
#[derive(Debug, Clone, Copy, Default)]
pub struct RunnerRegistration;

impl Aggregate for RunnerRegistration {
    const NAME: &'static str = "registration";
    type State = RegistrationState;
    type Body = RegistrationCommand;
    type Event = RegistrationEvent;
    type Rejection = RegistrationRejection;

    fn initial() -> RegistrationState {
        RegistrationState::initial()
    }

    fn decide(state: &RegistrationState, command: Command<RegistrationCommand>) -> Decision {
        decide::decide(state, command)
    }

    fn evolve(state: RegistrationState, event: &RegistrationEvent) -> RegistrationState {
        decide::evolve(state, event)
    }
}

impl DeterminismCases for RunnerRegistration {
    fn determinism_cases() -> Vec<DeterminismCase<Self>> {
        cases::cases()
    }
}
