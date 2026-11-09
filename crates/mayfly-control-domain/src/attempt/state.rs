//! The folded state of one attempt stream and the value types its events carry.

use mayfly_domain::{
    AttemptId, Deadline, DirectiveId, Epoch, EventId, Generation, HostId, ImageDigest, Millis,
    OwnedVmName, PoolName, ProviderInstanceId, SecretHandleId, Slot,
};
use serde::{Deserialize, Serialize};

/// A GitHub job's `runnerRequestId`: the job, for mayfly (`doc/domain-model.md` §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunnerRequestId(pub u64);

/// A stage of preparation or activation, the one a `Backoff` returns to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// `CloneVm` from the image base.
    Clone,
    /// `StartVm` under its shim.
    Boot,
    /// `VerifyGuest` before the attempt is prepared.
    Verify,
    /// The fresh `VerifyGuest` of activation.
    Activate,
}

/// Whether the VM was running when the attempt was quarantined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmCondition {
    /// The provider last reported it running (or it was never stopped by us).
    Running,
    /// Stopped, or never started.
    Stopped,
}

/// Why an attempt is quarantined: the closed set of `doc/domain-model.md` §6.10.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineReason {
    /// Ownership of the VM cannot be proven (a VM of our exact name we never recorded cloning).
    OwnershipDoubt,
    /// The guest failed its identity challenge.
    GuestIdentityRefused,
    /// The JIT config may have reached something other than this guest.
    JitPresumedLeaked,
    /// The provider's instance id differs from the recorded one (ADR 0026).
    ProviderIdentityMismatch,
    /// The data plane refused the delete again and again.
    DeleteRefusedRepeatedly,
    /// A secret was found where it must never be.
    RedactionViolation,
    /// Kept for investigation.
    ForensicRetention,
    /// An outcome cannot be proven by any observation the provider can give.
    OutcomeUnprovable,
}

/// Why an attempt was retired (`AttemptRetired`, `RetirementRequested`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetireReason {
    /// Its job completed.
    JobDone,
    /// The reconciler found more runners than demand.
    Surplus,
    /// A host or pool drain.
    Drain,
    /// The pool's image pin moved on.
    ImageReplaced,
    /// A stage failed with its retry budget spent.
    StageFailed,
    /// The VM stopped under a job.
    GuestDied,
    /// The VM stopped before it served a job.
    VmStopped,
    /// The runner registration's JIT was lost before delivery (`RegistrationIssueUnknown`).
    JitLost,
    /// The JIT delivery outcome is unknown or the handle was orphaned.
    JitPresumedLeaked,
    /// The data plane refused to launch the runner.
    LaunchRefused,
    /// An operator resolved the quarantine case with a delete.
    QuarantineResolved,
}

/// The kind of directive pending on the attempt, without its payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectiveTag {
    /// See [`DirectiveKind::CloneVm`].
    CloneVm,
    /// See [`DirectiveKind::StartVm`].
    StartVm,
    /// See [`DirectiveKind::VerifyGuest`].
    VerifyGuest,
    /// See [`DirectiveKind::LaunchRunner`].
    LaunchRunner,
    /// See [`DirectiveKind::ExportDiagnostics`].
    ExportDiagnostics,
    /// See [`DirectiveKind::StopVm`].
    StopVm,
    /// See [`DirectiveKind::DeleteVm`].
    DeleteVm,
}

/// The claim a destructive directive carries (`doc/domain-model.md` §5.3), built from this
/// stream's `AttemptStarted` and `VmCloned` only: the data plane re-observes the VM and refuses
/// on any mismatch (ADR 0026). The run epoch is stamped by the relay at send time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipProof {
    /// The attempt that cloned the VM.
    pub attempt_id: AttemptId,
    /// The host it runs on.
    pub host_id: HostId,
    /// The slot it holds.
    pub slot: Slot,
    /// The slot generation of its name.
    pub generation: Generation,
    /// The exact VM name.
    pub vm_name: OwnedVmName,
    /// The provider instance id recorded by `VmCloned`.
    pub instance_id: ProviderInstanceId,
}

/// What a directive asks the data plane to do. Its intent is persisted as `DirectiveIssued`
/// before it is sent (ADR 0034); the VM name, generation and image come from the stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectiveKind {
    /// Clone the attempt's VM from the image base, by its name.
    CloneVm,
    /// Start the VM under its shim (ADR 0037).
    StartVm,
    /// Challenge the guest; the directive id is the challenge's reference.
    VerifyGuest,
    /// Launch the runner with the JIT config behind `handle_id`, over stdin (ADR 0038).
    LaunchRunner {
        /// The secret handle, never the secret.
        handle_id: SecretHandleId,
    },
    /// Export bounded, redacted diagnostics before the VM is stopped.
    ExportDiagnostics,
    /// Stop the VM with `tart stop`, with proof.
    StopVm {
        /// The ownership proof.
        proof: OwnershipProof,
    },
    /// Delete the VM, with proof.
    DeleteVm {
        /// The ownership proof.
        proof: OwnershipProof,
    },
}

impl DirectiveKind {
    /// The kind without its payload.
    #[must_use]
    pub const fn tag(&self) -> DirectiveTag {
        match self {
            Self::CloneVm => DirectiveTag::CloneVm,
            Self::StartVm => DirectiveTag::StartVm,
            Self::VerifyGuest => DirectiveTag::VerifyGuest,
            Self::LaunchRunner { .. } => DirectiveTag::LaunchRunner,
            Self::ExportDiagnostics => DirectiveTag::ExportDiagnostics,
            Self::StopVm { .. } => DirectiveTag::StopVm,
            Self::DeleteVm { .. } => DirectiveTag::DeleteVm,
        }
    }
}

/// Whether the pending directive's outcome is known to be still on its way, or unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Sent (or about to be); a reply may still come.
    InFlight,
    /// The reply was lost or the deadline passed: whether the effect happened is not known.
    /// Leaves only on an observation (I10, ADR 0025).
    Unknown {
        /// When the outcome became unknown.
        since: Millis,
    },
}

/// The `pending` overlay of `doc/domain-model.md` §6.4: at most one directive per attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pending {
    /// The directive.
    pub directive_id: DirectiveId,
    /// Its kind.
    pub tag: DirectiveTag,
    /// In flight or unknown.
    pub outcome: Outcome,
    /// The deadline it was issued with.
    pub deadline: Deadline,
}

/// The JIT handle's phase on the attempt (`doc/domain-model.md` §6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Jit {
    /// No handle: nothing was minted for this attempt.
    None,
    /// A handle was recorded; the secret waits in the broker.
    Issued(SecretHandleId),
    /// The launcher took it, or may have (a launch whose outcome is unknown): never again.
    Taken(SecretHandleId),
    /// The broker lost it to a restart before it was taken: presumed leaked.
    Orphaned(SecretHandleId),
}

/// Whether diagnostics were exported before the stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Diagnostics {
    /// Not attempted yet.
    NotTaken,
    /// Exported.
    Exported,
    /// Attempted and lost.
    Lost,
}

/// Where the attempt is in its lifecycle: `vm_state` of `doc/domain-model.md` §6.4, plus the
/// stream's two ends (`NotStarted` before `AttemptStarted`, `Closed` after `AttemptClosed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// No `AttemptStarted` yet.
    NotStarted,
    /// Slot reserved, nothing issued.
    Reserved,
    /// `CloneVm` issued.
    Cloning,
    /// Cloned; `StartVm` next or in flight.
    Booting,
    /// Running; `VerifyGuest` next or in flight.
    Verifying,
    /// Booted, guest verified, unregistered, no secret, no workflow code ever ran.
    Prepared,
    /// Granted; fresh verification, JIT handle and launch.
    Activating,
    /// Runner launched with a registration, no job bound yet.
    Listening,
    /// A job is bound to the runner.
    Busy,
    /// The runner exited.
    Finished,
    /// Retired: diagnostics, stop and delete follow.
    Retiring,
    /// `DeleteVm` issued.
    Deleting,
    /// The VM's absence after our delete was observed.
    Deleted,
    /// The VM was observed absent without our delete.
    Lost,
    /// A stage failed definitely; it is retried with the same name and generation.
    Backoff {
        /// The stage to retry.
        stage: Stage,
        /// Directives issued in that stage so far.
        n: u32,
    },
    /// Held for an operator; automation never deletes it (I28, ADR 0025).
    Quarantined {
        /// Why.
        reason: QuarantineReason,
        /// The VM's condition, kept current by observations.
        vm: VmCondition,
    },
    /// `AttemptClosed`: the stream takes nothing more.
    Closed,
}

/// The facts fixed at `AttemptStarted`, all control-plane generated values (I30).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The attempt.
    pub attempt_id: AttemptId,
    /// The host.
    pub host_id: HostId,
    /// The slot.
    pub slot: Slot,
    /// The slot generation.
    pub generation: Generation,
    /// The VM name, for good: retries reuse it (I13).
    pub vm_name: OwnedVmName,
    /// The pool.
    pub pool: PoolName,
    /// The image digest, for good (I14).
    pub image_digest: ImageDigest,
    /// Directives a stage may issue before a stage failure retires the attempt.
    pub retry_budget: u32,
}

/// The state of one attempt stream, folded from its events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptState {
    /// The lifecycle phase.
    pub phase: Phase,
    /// Set by `AttemptStarted`.
    pub identity: Option<Identity>,
    /// The highest run epoch accepted: set at start and raised only by `AttemptAdopted`.
    pub epoch: Epoch,
    /// The one pending directive, if any.
    pub pending: Option<Pending>,
    /// Directives issued in the current stage.
    pub tries: u32,
    /// Recorded from `VmCloned`; the ownership proof's instance id.
    pub instance_id: Option<ProviderInstanceId>,
    /// Whether the VM was last seen running.
    pub vm_running: bool,
    /// The Host's activation grant (`LeaseActivated`), from `ActivationStarted`.
    pub grant_ref: Option<EventId>,
    /// A `GuestVerified` while activating, in the current epoch (cleared by adoption).
    pub fresh_verification: bool,
    /// The JIT handle.
    pub jit: Jit,
    /// The job bound to the runner (I14).
    pub job_binding: Option<RunnerRequestId>,
    /// `RunnerExited` was recorded.
    pub runner_exited: bool,
    /// A retirement waiting for the registration's deregistration (I21).
    pub retirement_requested: Option<RetireReason>,
    /// Why the attempt was retired.
    pub retire_reason: Option<RetireReason>,
    /// Diagnostics before the stop.
    pub diagnostics: Diagnostics,
}

impl AttemptState {
    /// The state of a stream with no events.
    #[must_use]
    pub const fn initial() -> Self {
        Self {
            phase: Phase::NotStarted,
            identity: None,
            epoch: Epoch::new(0),
            pending: None,
            tries: 0,
            instance_id: None,
            vm_running: false,
            grant_ref: None,
            fresh_verification: false,
            jit: Jit::None,
            job_binding: None,
            runner_exited: false,
            retirement_requested: None,
            retire_reason: None,
            diagnostics: Diagnostics::NotTaken,
        }
    }
}
