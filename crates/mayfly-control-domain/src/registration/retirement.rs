//! Policy P11's decision, pure: what an attempt's requested retirement does next, given its
//! registration (`retirementStep` of `formal/registration.qnt`).
//!
//! The policy (CP-05) reads the attempt's `RetirementRequested` (or, for a quarantined VM, its
//! `StopDeregistrationRequested`: ADR 0076) and the registration stream, and turns the step into
//! one command: `RequestDeregistration{Retirement}` on the registration, `ConfirmRetirement` or
//! `WithdrawRetirement` on the attempt, or nothing yet. Only a `ConfirmRetirement` lets the
//! attempt's cleanup, or its quarantine stop, stop the VM, so this function is where "deregister
//! before stop" (I21) is decided; mutant 21 (verification §9) lives here. The model is
//! `formal/registration.qnt`'s `retirementStep`, with `requestQuarantineStop` for the quarantine.

use mayfly_domain::EventId;

use super::state::{Phase, RegistrationState};

/// The next step of a requested retirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetirementStep {
    /// `RequestDeregistration{Retirement}` first: a runner may live in the VM.
    Deregister,
    /// `ConfirmRetirement`: no runner of this registration can be in the VM, or GitHub no longer
    /// lists it; the VM may be stopped.
    Confirm,
    /// `WithdrawRetirement`: GitHub answered busy for this retirement (I21).
    Withdraw,
    /// Nothing yet: the mint or the removal has not answered.
    Wait,
}

/// The step of `retirement` (the attempt's `RetirementRequested` or
/// `StopDeregistrationRequested` event) given the attempt's registration; an attempt that never
/// asked for one has the initial state.
///
/// - GitHub answered busy for this very retirement: withdraw it, whatever happened since; a busy
///   answer to another (stale) retirement's request does not cancel this one (a job
///   finishing after the answer does not revive it: the counterexample
///   `busyRetirementStaysCancelledAfterTheJobTest` of the model).
/// - Nothing was issued, the issue is unknown (the JIT config never left the control plane, so
///   it cannot be in the VM; the reaper resolves it), the registration is absent or deregistered:
///   confirm.
/// - The mint has not answered: wait.
/// - A runner id is journaled and its config may be in the VM: deregister first, then wait for
///   the answer.
///
/// Pinned by
/// `tests/registration/rules.rs::should_never_confirm_a_retirement_when_a_runner_may_still_be_registered`
/// (which kills mutant 21) and
/// `tests/registration/rules.rs::should_withdraw_the_retirement_when_github_answered_busy_for_it`.
#[must_use]
pub fn retirement_step(state: &RegistrationState, retirement: EventId) -> RetirementStep {
    if state.busy_refused.contains(&retirement) {
        return RetirementStep::Withdraw;
    }
    match state.phase {
        Phase::Unrequested | Phase::IssueUnknown | Phase::Absent | Phase::Deregistered => {
            RetirementStep::Confirm
        }
        Phase::Requested => RetirementStep::Wait,
        Phase::Issued
        | Phase::Delivered
        | Phase::Connected
        | Phase::Consumed
        | Phase::Completed => {
            if state.deregistration.is_some() {
                RetirementStep::Wait
            } else {
                RetirementStep::Deregister
            }
        }
    }
}
