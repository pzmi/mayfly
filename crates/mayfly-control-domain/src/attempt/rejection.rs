//! Why the attempt refused a command.

use serde::{Deserialize, Serialize};

use crate::aggregate::Rejection;

/// A refusal of the `Attempt` aggregate: a rejection, never an event (`doc/domain-model.md`
/// §6.0). Each code is returned by a refused determinism case and pinned in
/// `tests/fixtures/decisions/attempt.txt`. Its serde form is its code (`"stale_run"`), which
/// a refusal receipt stores (pinned by `mayfly-control`'s
/// `should_encode_attempt_refusals_as_their_codes_when_recorded`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptRejection {
    /// The stream has no `AttemptStarted`.
    NotStarted,
    /// `StartAttempt` on a started stream.
    AlreadyStarted,
    /// The stream is closed.
    Closed,
    /// The command's run epoch is older than the attempt's (I18).
    StaleRun,
    /// The command's run epoch is newer than the attempt's and it is not an `Adopt` or an
    /// observation: the attempt must be adopted first.
    NotAdopted,
    /// The pending directive's outcome is unknown: nothing but an observation moves the attempt
    /// (I10, ADR 0025).
    OutcomeUnknown,
    /// A directive is in flight: one at a time per target (I13).
    DirectivePending,
    /// The phase has no directive to issue.
    NothingToIssue,
    /// An answer that does not answer the pending directive's kind.
    ObservationMismatch,
    /// Activation begins only from `Prepared`.
    NotPrepared,
    /// A JIT handle outside activation.
    NotActivating,
    /// A JIT handle without the Host's activation grant (I22).
    NoGrant,
    /// A JIT handle without a fresh guest verification in the current epoch (I22).
    NoFreshVerification,
    /// A second JIT handle: a JIT config is minted and delivered at most once (I22).
    JitAlreadyIssued,
    /// A job binds only to a listening runner.
    NotListening,
    /// A second job (I14).
    JobAlreadyBound,
    /// The runner's JIT config is presumed leaked (its handle was orphaned by a restart while
    /// the launch was in flight): it never gets a job (I22).
    JitPresumedLeaked,
    /// A busy attempt is never retired, only finished or forced.
    Busy,
    /// Retirement is already requested or decided.
    AlreadyRetiring,
    /// The VM is deleted or lost.
    Terminal,
    /// A quarantined attempt is resolved only by an operator.
    Quarantined,
    /// Nothing waits for a deregistration.
    NoRetirementRequested,
    /// The attempt is already quarantined.
    AlreadyQuarantined,
    /// `Adopt` under the attempt's own epoch.
    AlreadyCurrent,
    /// `ResolveQuarantine` on an attempt that is not quarantined.
    NotQuarantined,
    /// Only an operator resolves a quarantine case (I28).
    OperatorOnly,
    /// No recorded instance id to prove ownership with: the VM is never deleted (I04).
    NoProof,
    /// `Close` before the VM is deleted or lost.
    NotTerminal,
    /// A timeout before the pending directive's deadline: a deadline is never cut short.
    DeadlineNotReached,
    /// A timeout for a directive that is not the pending one: it was answered, or another
    /// directive is pending now.
    DirectiveNotPending,
    /// A timeout for a directive the relay has not sent: held unsent, nothing happened, and
    /// its deadline does not run (ADR 0025).
    DirectiveNotSent,
}

impl Rejection for AttemptRejection {
    const CODES: &'static [&'static str] = &[
        "not_started",
        "already_started",
        "closed",
        "stale_run",
        "not_adopted",
        "outcome_unknown",
        "directive_pending",
        "nothing_to_issue",
        "observation_mismatch",
        "not_prepared",
        "not_activating",
        "no_grant",
        "no_fresh_verification",
        "jit_already_issued",
        "not_listening",
        "job_already_bound",
        "jit_presumed_leaked",
        "busy",
        "already_retiring",
        "terminal",
        "quarantined",
        "no_retirement_requested",
        "already_quarantined",
        "already_current",
        "not_quarantined",
        "operator_only",
        "no_proof",
        "not_terminal",
        "deadline_not_reached",
        "directive_not_pending",
        "directive_not_sent",
    ];

    fn code(&self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::AlreadyStarted => "already_started",
            Self::Closed => "closed",
            Self::StaleRun => "stale_run",
            Self::NotAdopted => "not_adopted",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::DirectivePending => "directive_pending",
            Self::NothingToIssue => "nothing_to_issue",
            Self::ObservationMismatch => "observation_mismatch",
            Self::NotPrepared => "not_prepared",
            Self::NotActivating => "not_activating",
            Self::NoGrant => "no_grant",
            Self::NoFreshVerification => "no_fresh_verification",
            Self::JitAlreadyIssued => "jit_already_issued",
            Self::NotListening => "not_listening",
            Self::JobAlreadyBound => "job_already_bound",
            Self::JitPresumedLeaked => "jit_presumed_leaked",
            Self::Busy => "busy",
            Self::AlreadyRetiring => "already_retiring",
            Self::Terminal => "terminal",
            Self::Quarantined => "quarantined",
            Self::NoRetirementRequested => "no_retirement_requested",
            Self::AlreadyQuarantined => "already_quarantined",
            Self::AlreadyCurrent => "already_current",
            Self::NotQuarantined => "not_quarantined",
            Self::OperatorOnly => "operator_only",
            Self::NoProof => "no_proof",
            Self::NotTerminal => "not_terminal",
            Self::DeadlineNotReached => "deadline_not_reached",
            Self::DirectiveNotPending => "directive_not_pending",
            Self::DirectiveNotSent => "directive_not_sent",
        }
    }
}
