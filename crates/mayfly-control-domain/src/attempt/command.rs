//! What the attempt is asked (commands) and told (observations).

use mayfly_domain::{
    AttemptId, Deadline, DirectiveId, Epoch, EventId, Generation, HostId, ImageDigest, Millis,
    ObservationId, PoolName, ProviderInstanceId, SecretHandleId, Slot,
};
use serde::{Deserialize, Serialize};

use super::state::{QuarantineReason, RetireReason, RunnerRequestId};
use crate::aggregate::CommandBody;

/// The commands of `doc/domain-model.md` §6.4. There is no `AssertOutcome`: an `Unknown`
/// outcome leaves only on an observation (ADR 0025).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum AttemptCommand {
    /// Start the attempt from the Host's `SlotReserved` (policy P2).
    StartAttempt(StartAttempt),
    /// The saga asks for the stage's next directive (policy P6), with the id and deadline it
    /// minted.
    IssueNext {
        /// The new directive's id.
        directive_id: DirectiveId,
        /// Its deadline.
        deadline: Deadline,
    },
    /// The inbox records an observation from the data plane (policy P8).
    RecordObservation(Observation),
    /// The directive relay put the pending directive on the link with this deadline (P7):
    /// its deadline runs from now on.
    RecordDirectiveSent {
        /// The directive sent.
        directive_id: DirectiveId,
        /// The deadline its frame carried.
        deadline: Deadline,
    },
    /// The saga's timer: the pending directive's deadline passed with no answer (policy P7,
    /// ADR 0034). Its outcome becomes `Unknown`, never a success or a failure (I10, ADR 0025).
    RecordDirectiveTimeout {
        /// The directive whose deadline passed.
        directive_id: DirectiveId,
    },
    /// Activation begins after the Host's grant (policy P9).
    BeginActivation {
        /// The Host's `LeaseActivated` event.
        grant_ref: EventId,
    },
    /// The broker holds the minted JIT config behind a handle.
    RecordJitHandle {
        /// The handle.
        handle_id: SecretHandleId,
        /// The registration's `RegistrationIssued` event.
        registration_ref: EventId,
    },
    /// GitHub bound a job to the attempt's runner (policy P12).
    BindJob {
        /// The job.
        request_id: RunnerRequestId,
    },
    /// Retire the attempt; a registered runner waits for its deregistration (policy P11).
    RequestRetirement {
        /// Why.
        reason: RetireReason,
    },
    /// The registration was deregistered (or there was none): retire.
    ConfirmRetirement,
    /// GitHub answered 422 (busy): the runner stays (I21).
    WithdrawRetirement,
    /// Quarantine the attempt (policies P13, P16).
    Quarantine {
        /// Why.
        reason: QuarantineReason,
    },
    /// Take responsibility under the command's new run epoch after a restart, launching
    /// nothing (`AttemptAdopted`).
    Adopt {
        /// The complete inventory the adoption was decided on.
        inventory_ref: EventId,
    },
    /// An operator resolved the quarantine case with a delete with proof.
    ResolveQuarantine,
    /// Close the stream once the VM is deleted or lost.
    Close,
}

impl CommandBody for AttemptCommand {
    const TYPES: &'static [&'static str] = &[
        "start_attempt",
        "issue_next",
        "record_observation",
        "begin_activation",
        "record_jit_handle",
        "bind_job",
        "request_retirement",
        "confirm_retirement",
        "withdraw_retirement",
        "quarantine",
        "adopt",
        "resolve_quarantine",
        "close",
        "record_directive_timeout",
        "record_directive_sent",
    ];

    fn command_type(&self) -> &'static str {
        match self {
            Self::StartAttempt(_) => "start_attempt",
            Self::IssueNext { .. } => "issue_next",
            Self::RecordObservation(_) => "record_observation",
            Self::BeginActivation { .. } => "begin_activation",
            Self::RecordJitHandle { .. } => "record_jit_handle",
            Self::BindJob { .. } => "bind_job",
            Self::RequestRetirement { .. } => "request_retirement",
            Self::ConfirmRetirement => "confirm_retirement",
            Self::WithdrawRetirement => "withdraw_retirement",
            Self::Quarantine { .. } => "quarantine",
            Self::Adopt { .. } => "adopt",
            Self::ResolveQuarantine => "resolve_quarantine",
            Self::Close => "close",
            Self::RecordDirectiveTimeout { .. } => "record_directive_timeout",
            Self::RecordDirectiveSent { .. } => "record_directive_sent",
        }
    }
}

/// `StartAttempt`: the reservation's values. The VM name is derived from them, never taken
/// from elsewhere (I30).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StartAttempt {
    /// The attempt.
    pub attempt_id: AttemptId,
    /// The host.
    pub host_id: HostId,
    /// The slot.
    pub slot: Slot,
    /// The slot generation.
    pub generation: Generation,
    /// The pool.
    pub pool: PoolName,
    /// The image digest.
    pub image_digest: ImageDigest,
    /// Directives a stage may issue before a stage failure retires the attempt (at least 1).
    pub retry_budget: u32,
}

/// One data-plane observation about the attempt's VM, as the inbox routes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Observation {
    /// The inbox key.
    pub observation_id: ObservationId,
    /// The data-plane session epoch it was observed in.
    pub epoch: Epoch,
    /// The slot generation of the VM it is about.
    pub generation: Generation,
    /// What was observed.
    pub fact: Fact,
}

/// What an observation says. Answers name the directive they answer; inventory facts name
/// none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Fact {
    /// The clone exists, with this instance id.
    VmCloned {
        /// The `CloneVm`.
        directive_id: DirectiveId,
        /// The provider's instance id (ADR 0026).
        instance_id: ProviderInstanceId,
    },
    /// The VM runs under its shim.
    VmBooted {
        /// The `StartVm`.
        directive_id: DirectiveId,
    },
    /// The guest answered the challenge.
    GuestVerified {
        /// The `VerifyGuest`, the challenge's reference.
        directive_id: DirectiveId,
        /// The guest's typed claims.
        facts: GuestFacts,
    },
    /// The guest failed the challenge.
    GuestRejected {
        /// The `VerifyGuest`.
        directive_id: DirectiveId,
        /// Why.
        reason: GuestRejection,
    },
    /// The data plane definitely did not apply the directive.
    DirectiveRefused {
        /// The directive.
        directive_id: DirectiveId,
        /// Why.
        reason: RefusalReason,
    },
    /// The directive's outcome is unknown (lost reply, deadline).
    OutcomeUnknown {
        /// The directive.
        directive_id: DirectiveId,
    },
    /// The launcher took the JIT config and the runner started.
    RunnerLaunched {
        /// The `LaunchRunner`.
        directive_id: DirectiveId,
    },
    /// Whether the launcher took the JIT config is unknown.
    RunnerLaunchUnknown {
        /// The `LaunchRunner`.
        directive_id: DirectiveId,
    },
    /// Diagnostics were exported.
    DiagnosticsExported {
        /// The `ExportDiagnostics`.
        directive_id: DirectiveId,
        /// Their size.
        bytes: u64,
        /// Their SHA-256.
        sha256: [u8; 32],
    },
    /// Diagnostics could not be exported.
    DiagnosticsLost {
        /// The `ExportDiagnostics`.
        directive_id: DirectiveId,
        /// Why.
        reason: DiagnosticsLoss,
    },
    /// The VM stopped on our `StopVm`.
    VmStopped {
        /// The `StopVm`.
        directive_id: DirectiveId,
    },
    /// The VM is gone after our `DeleteVm`.
    VmDeleted {
        /// The `DeleteVm`.
        directive_id: DirectiveId,
    },
    /// The runner process exited.
    RunnerExited {
        /// Its exit code.
        code: i32,
    },
    /// The provider reports the VM stopped, not by our directive.
    VmObservedStopped,
    /// A complete inventory lists the VM.
    VmPresent {
        /// Its instance id.
        instance_id: ProviderInstanceId,
        /// Whether it runs.
        running: bool,
    },
    /// A complete inventory does not list the VM. An incomplete one is never routed as this
    /// (I25).
    VmAbsent,
}

impl Fact {
    /// The directive an answer names; `None` for an inventory fact.
    #[must_use]
    pub const fn directive_id(&self) -> Option<DirectiveId> {
        match self {
            Self::VmCloned { directive_id, .. }
            | Self::VmBooted { directive_id }
            | Self::GuestVerified { directive_id, .. }
            | Self::GuestRejected { directive_id, .. }
            | Self::DirectiveRefused { directive_id, .. }
            | Self::OutcomeUnknown { directive_id }
            | Self::RunnerLaunched { directive_id }
            | Self::RunnerLaunchUnknown { directive_id }
            | Self::DiagnosticsExported { directive_id, .. }
            | Self::DiagnosticsLost { directive_id, .. }
            | Self::VmStopped { directive_id }
            | Self::VmDeleted { directive_id } => Some(*directive_id),
            Self::RunnerExited { .. }
            | Self::VmObservedStopped
            | Self::VmPresent { .. }
            | Self::VmAbsent => None,
        }
    }
}

/// The guest's own claims after a successful challenge. Typed, and read by no ownership field
/// (I30).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestFacts {
    /// The guest's operating system.
    pub os: GuestOs,
    /// How long it has been up, by its own clock.
    pub uptime: Millis,
}

/// A guest operating system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestOs {
    /// macOS.
    Macos,
    /// Linux.
    Linux,
}

/// Why a guest was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestRejection {
    /// The answer to the challenge was wrong.
    ChallengeFailed,
    /// The image has no guest agent (ADR 0038).
    AgentMissing,
    /// The guest's facts contradict the image's manifest.
    FactsMismatch,
}

/// Why the data plane refused a directive (nothing happened).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    /// It does not fit the resource ledger (I33).
    InsufficientResources,
    /// The host is in safe mode (I29).
    SafeMode,
    /// The re-observed VM does not match the proof (ADR 0026).
    ProofMismatch,
    /// A complete listing shows the stop's or start's target, the VM this attempt recorded
    /// cloning, absent: a desync, quarantined `vm_vanished_externally` (ADR 0066).
    TargetAbsent,
    /// The directive's epoch is not the current one (I09).
    StaleEpoch,
    /// The effect failed (the clone failed, the guest was not ready by the deadline).
    EffectFailed,
}

/// Why diagnostics were lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticsLoss {
    /// The data plane refused the export.
    Refused,
    /// The guest did not answer.
    GuestUnreachable,
    /// The output exceeded its bound.
    OverLimit,
}
