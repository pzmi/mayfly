//! The attempt's events (`doc/domain-model.md` §6.4): immutable, past-tense facts, each with
//! its type and schema version, and golden samples that together show every field and variant
//! (`tests/fixtures/events/<type>/`).

use mayfly_domain::{
    AttemptId, Deadline, DirectiveId, Epoch, EventId, Generation, HostId, ImageDigest, Millis,
    ObservationId, OwnedVmName, PoolName, ProviderInstanceId, SecretHandleId, Slot,
};
use serde::{Deserialize, Serialize};

use super::command::{DiagnosticsLoss, GuestFacts, GuestRejection, RefusalReason};
use super::state::{
    DirectiveKind, QuarantineReason, RetireReason, RunnerRequestId, Stage, VmCondition,
};
use crate::event::DomainEvent;
use crate::event_set;

/// How a VM was found lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LostEvidence {
    /// A complete inventory did not list it.
    InventoryAbsent,
}

/// What made an observation stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Staleness {
    /// It is about another slot generation.
    Generation {
        /// The generation it named.
        observed: Generation,
    },
    /// It was observed in an older data-plane epoch than the attempt's.
    Epoch {
        /// The epoch it named.
        observed: Epoch,
    },
    /// It answers a directive that is not the pending one.
    Directive {
        /// The directive it named.
        directive_id: DirectiveId,
    },
}

/// The attempt began from its reservation; its name and generation are fixed for good.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AttemptStarted {
    /// The attempt.
    pub attempt_id: AttemptId,
    /// The host.
    pub host_id: HostId,
    /// The slot.
    pub slot: Slot,
    /// The slot generation.
    pub generation: Generation,
    /// The VM name, derived from the values above.
    pub vm_name: OwnedVmName,
    /// The pool.
    pub pool: PoolName,
    /// The image digest.
    pub image_digest: ImageDigest,
    /// Directives a stage may issue before a stage failure retires the attempt.
    pub retry_budget: u32,
    /// The run epoch it started under.
    pub epoch: Epoch,
}

impl DomainEvent for AttemptStarted {
    const EVENT_TYPE: &'static str = "attempt_started";
    const SCHEMA_VERSION: u32 = 1;
}

/// A directive's intent, persisted before it is sent (ADR 0034).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DirectiveIssued {
    /// The directive.
    pub directive_id: DirectiveId,
    /// What it asks.
    pub kind: DirectiveKind,
    /// By when.
    pub deadline: Deadline,
}

impl DomainEvent for DirectiveIssued {
    const EVENT_TYPE: &'static str = "directive_issued";
    const SCHEMA_VERSION: u32 = 1;
}

/// The directive relay put the pending directive on the link (CP-02): from now on its deadline
/// runs. Recorded after the link took the frame, with the deadline the frame carried, so a
/// directive held unsent (safe mode, no session, not adopted) never times out: its outcome is
/// known, nothing happened (ADR 0025). An answer may be recorded before it; then it is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DirectiveSent {
    /// The directive.
    pub directive_id: DirectiveId,
    /// The deadline the frame carried: the relay's clock at its first send plus the kind's
    /// timeout.
    pub deadline: Deadline,
}

impl DomainEvent for DirectiveSent {
    const EVENT_TYPE: &'static str = "directive_sent";
    const SCHEMA_VERSION: u32 = 1;
}

/// The data plane definitely did not apply the pending directive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DirectiveRefused {
    /// The directive.
    pub directive_id: DirectiveId,
    /// Why.
    pub reason: RefusalReason,
}

impl DomainEvent for DirectiveRefused {
    const EVENT_TYPE: &'static str = "directive_refused";
    const SCHEMA_VERSION: u32 = 1;
}

/// Whether the pending directive took effect is not known; only an observation resolves it
/// (I10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DirectiveOutcomeUnknown {
    /// The directive.
    pub directive_id: DirectiveId,
    /// When it became unknown.
    pub since: Millis,
}

impl DomainEvent for DirectiveOutcomeUnknown {
    const EVENT_TYPE: &'static str = "directive_outcome_unknown";
    const SCHEMA_VERSION: u32 = 1;
}

/// A complete inventory showed that the pending directive, whose outcome was unknown, did not
/// take effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DirectiveObservedNotApplied {
    /// The directive.
    pub directive_id: DirectiveId,
}

impl DomainEvent for DirectiveObservedNotApplied {
    const EVENT_TYPE: &'static str = "directive_observed_not_applied";
    const SCHEMA_VERSION: u32 = 1;
}

/// The clone exists; its instance id is recorded for the ownership proof (ADR 0026).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct VmCloned {
    /// The `CloneVm`.
    pub directive_id: DirectiveId,
    /// The provider's instance id.
    pub instance_id: ProviderInstanceId,
}

impl DomainEvent for VmCloned {
    const EVENT_TYPE: &'static str = "vm_cloned";
    const SCHEMA_VERSION: u32 = 1;
}

/// The VM runs under its shim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct VmBooted {
    /// The `StartVm`.
    pub directive_id: DirectiveId,
}

impl DomainEvent for VmBooted {
    const EVENT_TYPE: &'static str = "vm_booted";
    const SCHEMA_VERSION: u32 = 1;
}

/// The guest answered the challenge of this attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GuestVerified {
    /// The `VerifyGuest` that carried the challenge.
    pub nonce_ref: DirectiveId,
    /// The guest's typed claims.
    pub facts: GuestFacts,
}

impl DomainEvent for GuestVerified {
    const EVENT_TYPE: &'static str = "guest_verified";
    const SCHEMA_VERSION: u32 = 1;
}

/// The guest failed its challenge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GuestRejected {
    /// The `VerifyGuest`.
    pub directive_id: DirectiveId,
    /// Why.
    pub reason: GuestRejection,
}

impl DomainEvent for GuestRejected {
    const EVENT_TYPE: &'static str = "guest_rejected";
    const SCHEMA_VERSION: u32 = 1;
}

/// Booted, guest verified, unregistered, no secret: ready for activation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AttemptPrepared {}

impl DomainEvent for AttemptPrepared {
    const EVENT_TYPE: &'static str = "attempt_prepared";
    const SCHEMA_VERSION: u32 = 1;
}

/// The Host granted activation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ActivationStarted {
    /// The Host's `LeaseActivated`.
    pub grant_ref: EventId,
}

impl DomainEvent for ActivationStarted {
    const EVENT_TYPE: &'static str = "activation_started";
    const SCHEMA_VERSION: u32 = 1;
}

/// The broker holds this attempt's JIT config behind a handle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct JitHandleIssued {
    /// The handle.
    pub handle_id: SecretHandleId,
    /// The registration's `RegistrationIssued`.
    pub registration_ref: EventId,
}

impl DomainEvent for JitHandleIssued {
    const EVENT_TYPE: &'static str = "jit_handle_issued";
    const SCHEMA_VERSION: u32 = 1;
}

/// The launcher took the JIT config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct JitHandleTaken {
    /// The handle.
    pub handle_id: SecretHandleId,
}

impl DomainEvent for JitHandleTaken {
    const EVENT_TYPE: &'static str = "jit_handle_taken";
    const SCHEMA_VERSION: u32 = 1;
}

/// A restart emptied the broker before the handle was taken: presumed leaked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct JitHandleOrphaned {
    /// The handle.
    pub handle_id: SecretHandleId,
}

impl DomainEvent for JitHandleOrphaned {
    const EVENT_TYPE: &'static str = "jit_handle_orphaned";
    const SCHEMA_VERSION: u32 = 1;
}

/// The runner started with its registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RunnerLaunched {
    /// The `LaunchRunner`.
    pub directive_id: DirectiveId,
}

impl DomainEvent for RunnerLaunched {
    const EVENT_TYPE: &'static str = "runner_launched";
    const SCHEMA_VERSION: u32 = 1;
}

/// Whether the launcher took the JIT config is unknown: presumed leaked, never launched again
/// (I22).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RunnerLaunchUnknown {
    /// The `LaunchRunner`.
    pub directive_id: DirectiveId,
}

impl DomainEvent for RunnerLaunchUnknown {
    const EVENT_TYPE: &'static str = "runner_launch_unknown";
    const SCHEMA_VERSION: u32 = 1;
}

/// GitHub bound a job to the runner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct JobBound {
    /// The job.
    pub request_id: RunnerRequestId,
}

impl DomainEvent for JobBound {
    const EVENT_TYPE: &'static str = "job_bound";
    const SCHEMA_VERSION: u32 = 1;
}

/// The runner process exited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RunnerExited {
    /// Its exit code.
    pub code: i32,
}

impl DomainEvent for RunnerExited {
    const EVENT_TYPE: &'static str = "runner_exited";
    const SCHEMA_VERSION: u32 = 1;
}

/// The provider reported the VM stopped, not by our directive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct VmObservedStopped {}

impl DomainEvent for VmObservedStopped {
    const EVENT_TYPE: &'static str = "vm_observed_stopped";
    const SCHEMA_VERSION: u32 = 1;
}

/// A stage failed definitely; it is retried with the same name and generation (I13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AttemptBackedOff {
    /// The stage.
    pub stage: Stage,
    /// Directives issued in it so far.
    pub n: u32,
}

impl DomainEvent for AttemptBackedOff {
    const EVENT_TYPE: &'static str = "attempt_backed_off";
    const SCHEMA_VERSION: u32 = 1;
}

/// Retirement waits for the registration's deregistration (I21).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RetirementRequested {
    /// Why.
    pub reason: RetireReason,
}

impl DomainEvent for RetirementRequested {
    const EVENT_TYPE: &'static str = "retirement_requested";
    const SCHEMA_VERSION: u32 = 1;
}

/// GitHub answered 422 or bound a job: the runner stays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RetirementWithdrawn {}

impl DomainEvent for RetirementWithdrawn {
    const EVENT_TYPE: &'static str = "retirement_withdrawn";
    const SCHEMA_VERSION: u32 = 1;
}

/// The attempt was quarantined with its VM running and a runner of its registration that may
/// be registered and idle in it: the automatic stop waits for P11 to deregister that runner
/// first, as a retirement does (I21, ADR 0076). This event is P11's key for the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StopDeregistrationRequested {}

impl DomainEvent for StopDeregistrationRequested {
    const EVENT_TYPE: &'static str = "stop_deregistration_requested";
    const SCHEMA_VERSION: u32 = 1;
}

/// P11 had the deregistration's answer (removed, or not found): the quarantine stop may go
/// (ADR 0076). The attempt stays quarantined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StopDeregistrationConfirmed {}

impl DomainEvent for StopDeregistrationConfirmed {
    const EVENT_TYPE: &'static str = "stop_deregistration_confirmed";
    const SCHEMA_VERSION: u32 = 1;
}

/// GitHub answered busy for the quarantine stop's deregistration: a job runs on the runner, so
/// the quarantined VM is not stopped until the runner is seen exiting (ADR 0076).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StopDeregistrationRefusedBusy {}

impl DomainEvent for StopDeregistrationRefusedBusy {
    const EVENT_TYPE: &'static str = "stop_deregistration_refused_busy";
    const SCHEMA_VERSION: u32 = 1;
}

/// The attempt will serve no (further) job; cleanup begins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AttemptRetired {
    /// Why.
    pub reason: RetireReason,
}

impl DomainEvent for AttemptRetired {
    const EVENT_TYPE: &'static str = "attempt_retired";
    const SCHEMA_VERSION: u32 = 1;
}

/// Diagnostics were exported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DiagnosticsExported {
    /// The `ExportDiagnostics`.
    pub directive_id: DirectiveId,
    /// Their size.
    pub bytes: u64,
    /// Their SHA-256.
    pub sha256: [u8; 32],
}

impl DomainEvent for DiagnosticsExported {
    const EVENT_TYPE: &'static str = "diagnostics_exported";
    const SCHEMA_VERSION: u32 = 1;
}

/// Diagnostics could not be exported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DiagnosticsLost {
    /// The `ExportDiagnostics`.
    pub directive_id: DirectiveId,
    /// Why.
    pub reason: DiagnosticsLoss,
}

impl DomainEvent for DiagnosticsLost {
    const EVENT_TYPE: &'static str = "diagnostics_lost";
    const SCHEMA_VERSION: u32 = 1;
}

/// The VM stopped on our `StopVm`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct VmStopped {
    /// The `StopVm`.
    pub directive_id: DirectiveId,
}

impl DomainEvent for VmStopped {
    const EVENT_TYPE: &'static str = "vm_stopped";
    const SCHEMA_VERSION: u32 = 1;
}

/// The VM's absence after our `DeleteVm` was observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct VmDeleted {
    /// The `DeleteVm`.
    pub directive_id: DirectiveId,
}

impl DomainEvent for VmDeleted {
    const EVENT_TYPE: &'static str = "vm_deleted";
    const SCHEMA_VERSION: u32 = 1;
}

/// The VM was observed absent without our delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AttemptLost {
    /// How.
    pub evidence: LostEvidence,
}

impl DomainEvent for AttemptLost {
    const EVENT_TYPE: &'static str = "attempt_lost";
    const SCHEMA_VERSION: u32 = 1;
}

/// Held for an operator; never deleted automatically (I28, ADR 0025).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AttemptQuarantined {
    /// Why.
    pub reason: QuarantineReason,
    /// The VM's condition then.
    pub vm: VmCondition,
}

impl DomainEvent for AttemptQuarantined {
    const EVENT_TYPE: &'static str = "attempt_quarantined";
    const SCHEMA_VERSION: u32 = 1;
}

/// Responsibility taken under a new run epoch after a restart; nothing launched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AttemptAdopted {
    /// The new run epoch.
    pub epoch: Epoch,
    /// The complete inventory it was decided on.
    pub inventory_ref: EventId,
}

impl DomainEvent for AttemptAdopted {
    const EVENT_TYPE: &'static str = "attempt_adopted";
    const SCHEMA_VERSION: u32 = 1;
}

/// An observation for another generation, an older epoch or another directive was ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StaleObservationIgnored {
    /// The observation.
    pub observation_id: ObservationId,
    /// What was stale.
    pub staleness: Staleness,
}

impl DomainEvent for StaleObservationIgnored {
    const EVENT_TYPE: &'static str = "stale_observation_ignored";
    const SCHEMA_VERSION: u32 = 1;
}

/// The stream is closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AttemptClosed {}

impl DomainEvent for AttemptClosed {
    const EVENT_TYPE: &'static str = "attempt_closed";
    const SCHEMA_VERSION: u32 = 1;
}

event_set! {
    /// Everything that happens to an attempt.
    pub enum AttemptEvent {
        /// See [`AttemptStarted`].
        AttemptStarted(AttemptStarted),
        /// See [`DirectiveIssued`].
        DirectiveIssued(DirectiveIssued),
        /// See [`DirectiveSent`].
        DirectiveSent(DirectiveSent),
        /// See [`DirectiveRefused`].
        DirectiveRefused(DirectiveRefused),
        /// See [`DirectiveOutcomeUnknown`].
        DirectiveOutcomeUnknown(DirectiveOutcomeUnknown),
        /// See [`DirectiveObservedNotApplied`].
        DirectiveObservedNotApplied(DirectiveObservedNotApplied),
        /// See [`VmCloned`].
        VmCloned(VmCloned),
        /// See [`VmBooted`].
        VmBooted(VmBooted),
        /// See [`GuestVerified`].
        GuestVerified(GuestVerified),
        /// See [`GuestRejected`].
        GuestRejected(GuestRejected),
        /// See [`AttemptPrepared`].
        AttemptPrepared(AttemptPrepared),
        /// See [`ActivationStarted`].
        ActivationStarted(ActivationStarted),
        /// See [`JitHandleIssued`].
        JitHandleIssued(JitHandleIssued),
        /// See [`JitHandleTaken`].
        JitHandleTaken(JitHandleTaken),
        /// See [`JitHandleOrphaned`].
        JitHandleOrphaned(JitHandleOrphaned),
        /// See [`RunnerLaunched`].
        RunnerLaunched(RunnerLaunched),
        /// See [`RunnerLaunchUnknown`].
        RunnerLaunchUnknown(RunnerLaunchUnknown),
        /// See [`JobBound`].
        JobBound(JobBound),
        /// See [`RunnerExited`].
        RunnerExited(RunnerExited),
        /// See [`VmObservedStopped`].
        VmObservedStopped(VmObservedStopped),
        /// See [`AttemptBackedOff`].
        AttemptBackedOff(AttemptBackedOff),
        /// See [`RetirementRequested`].
        RetirementRequested(RetirementRequested),
        /// See [`RetirementWithdrawn`].
        RetirementWithdrawn(RetirementWithdrawn),
        /// See [`AttemptRetired`].
        AttemptRetired(AttemptRetired),
        /// See [`DiagnosticsExported`].
        DiagnosticsExported(DiagnosticsExported),
        /// See [`DiagnosticsLost`].
        DiagnosticsLost(DiagnosticsLost),
        /// See [`VmStopped`].
        VmStopped(VmStopped),
        /// See [`VmDeleted`].
        VmDeleted(VmDeleted),
        /// See [`AttemptLost`].
        AttemptLost(AttemptLost),
        /// See [`AttemptQuarantined`].
        AttemptQuarantined(AttemptQuarantined),
        /// See [`AttemptAdopted`].
        AttemptAdopted(AttemptAdopted),
        /// See [`StopDeregistrationRequested`].
        StopDeregistrationRequested(StopDeregistrationRequested),
        /// See [`StopDeregistrationConfirmed`].
        StopDeregistrationConfirmed(StopDeregistrationConfirmed),
        /// See [`StopDeregistrationRefusedBusy`].
        StopDeregistrationRefusedBusy(StopDeregistrationRefusedBusy),
        /// See [`StaleObservationIgnored`].
        StaleObservationIgnored(StaleObservationIgnored),
        /// See [`AttemptClosed`].
        AttemptClosed(AttemptClosed),
    }
}
