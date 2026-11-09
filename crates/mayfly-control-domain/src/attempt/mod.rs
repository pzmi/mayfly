//! The `Attempt` aggregate (`doc/domain-model.md` §6.4, plan §6): one try to serve at most one
//! job on one VM, stream `attempt-<attempt_id>`.
//!
//! Plain data and two pure functions (ADR 0022): [`decide`] and [`evolve`] over
//! [`AttemptState`], every phase against every stimulus written out with no wildcard arm
//! (plan §6 rule 8). The model is `formal/lifecycle.qnt`; its invariants and the tests that pin
//! them here:
//!
//! - **I06, busyNeverWarm**: nothing leads from Listening, Busy or Finished back to Prepared,
//!   and a retired attempt never activates
//!   (`tests/attempt/lifecycle.rs::should_never_return_to_prepared_when_a_runner_has_served`).
//! - **I10**: an `Unknown` outcome leaves only on an observation; no command, timeout or
//!   assertion clears it
//!   (`tests/attempt/lifecycle.rs::should_leave_unknown_only_on_an_observation_when_every_command_is_tried`).
//! - **I13, retryKeepsNameAndGeneration**: one directive in flight per attempt, and a retry
//!   reuses the stream's VM name and generation
//!   (`tests/attempt/lifecycle.rs::should_keep_name_and_generation_when_a_stage_is_retried`).
//! - **I14**: one job binding and one image digest
//!   (`tests/attempt/lifecycle.rs::should_bind_one_job_when_a_second_is_offered`).
//! - **I22 (attempt half)**: a JIT handle only after the grant and a fresh verification in the
//!   current epoch, launched at most once, never after an unknown launch or an orphaned handle
//!   (`tests/attempt/lifecycle.rs::should_refuse_a_jit_handle_when_activation_lacks_a_grant_or_a_fresh_verification`,
//!   `tests/attempt/lifecycle.rs::should_never_launch_again_when_a_launch_outcome_is_unknown`).
//! - **I30**: ownership fields come from the reservation and the provider, never from the guest
//!   (`tests/attempt/lifecycle.rs::should_carry_the_recorded_instance_id_when_a_delete_is_issued`).
//! - **quarantineTerminal, quarantineNeverAutoDeleted (I28)**: only an operator takes an
//!   attempt out of quarantine; no automation issues its delete
//!   (`tests/attempt/lifecycle.rs::should_never_delete_when_quarantined_until_an_operator_resolves`).
//! - **I18**: an older run epoch is refused (`stale_run`); a newer one waits for `Adopt`.

mod cases;
mod command;
mod decide;
mod events;
mod evolve;
mod rejection;
mod state;

pub use command::{
    AttemptCommand, DiagnosticsLoss, Fact, GuestFacts, GuestOs, GuestRejection, Observation,
    RefusalReason, StartAttempt,
};
pub use decide::{Decision, decide};
pub use events::{
    ActivationStarted, AttemptAdopted, AttemptBackedOff, AttemptClosed, AttemptEvent, AttemptLost,
    AttemptPrepared, AttemptQuarantined, AttemptRetired, AttemptStarted, DiagnosticsExported,
    DiagnosticsLost, DirectiveIssued, DirectiveObservedNotApplied, DirectiveOutcomeUnknown,
    DirectiveRefused, GuestRejected, GuestVerified, JitHandleIssued, JitHandleOrphaned,
    JitHandleTaken, JobBound, LostEvidence, RetirementRequested, RetirementWithdrawn, RunnerExited,
    RunnerLaunchUnknown, RunnerLaunched, StaleObservationIgnored, Staleness, VmBooted, VmCloned,
    VmDeleted, VmObservedStopped, VmStopped,
};
pub use evolve::evolve;
pub use rejection::AttemptRejection;
pub use state::{
    AttemptState, Diagnostics, DirectiveKind, DirectiveTag, Identity, Jit, Outcome, OwnershipProof,
    Pending, Phase, QuarantineReason, RetireReason, RunnerRequestId, Stage, VmCondition,
};

use crate::aggregate::{Aggregate, DeterminismCase, DeterminismCases};
use crate::command::Command;

/// The `Attempt` aggregate: a marker grouping its data and functions (ADR 0022).
#[derive(Debug, Clone, Copy, Default)]
pub struct Attempt;

impl Aggregate for Attempt {
    const NAME: &'static str = "attempt";
    type State = AttemptState;
    type Body = AttemptCommand;
    type Event = AttemptEvent;
    type Rejection = AttemptRejection;

    fn initial() -> AttemptState {
        AttemptState::initial()
    }

    fn decide(state: &AttemptState, command: Command<AttemptCommand>) -> Decision {
        decide::decide(state, command)
    }

    fn evolve(state: AttemptState, event: &AttemptEvent) -> AttemptState {
        evolve::evolve(state, event)
    }
}

impl DeterminismCases for Attempt {
    fn determinism_cases() -> Vec<DeterminismCase<Self>> {
        cases::cases()
    }
}
