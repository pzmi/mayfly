//! The registration's events (`doc/domain-model.md` §6.5), with golden samples under
//! `tests/fixtures/events/<type>/`. No event carries the JIT config: it is a secret that goes to
//! the broker, never to the journal (ADR 0014).

use mayfly_domain::{AttemptId, Epoch, EventId, OwnedVmName};
use serde::{Deserialize, Serialize};

use super::state::{DeregistrationReason, JobResult, LeakReason, Removal, RunnerId, ScaleSetId};
use crate::attempt::RunnerRequestId;
use crate::event::DomainEvent;
use crate::event_set;

/// The registration accepted a command under a newer run epoch than any before (I18); recorded
/// ahead of that command's events so `evolve` knows the epoch to fence with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RegistrationEpochAdvanced {
    /// The new highest run epoch.
    pub epoch: Epoch,
}

impl DomainEvent for RegistrationEpochAdvanced {
    const EVENT_TYPE: &'static str = "registration_epoch_advanced";
    const SCHEMA_VERSION: u32 = 1;
}

/// A JIT registration was asked for, under the attempt's VM name: the name's one issue (I19).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RegistrationRequested {
    /// The attempt.
    pub attempt_id: AttemptId,
    /// The runner name.
    pub runner_name: OwnedVmName,
    /// The scale set.
    pub scale_set: ScaleSetId,
}

impl DomainEvent for RegistrationRequested {
    const EVENT_TYPE: &'static str = "registration_requested";
    const SCHEMA_VERSION: u32 = 1;
}

/// The mint answered: the runner id is journaled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RegistrationIssued {
    /// GitHub's runner id.
    pub runner_id: RunnerId,
}

impl DomainEvent for RegistrationIssued {
    const EVENT_TYPE: &'static str = "registration_issued";
    const SCHEMA_VERSION: u32 = 1;
}

/// The mint's outcome is unknown; the reaper resolves it by name (P10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RegistrationIssueUnknown {}

impl DomainEvent for RegistrationIssueUnknown {
    const EVENT_TYPE: &'static str = "registration_issue_unknown";
    const SCHEMA_VERSION: u32 = 1;
}

/// The reaper found a runner of the registration's name after an unknown issue: its id is
/// journaled and the registration is presumed leaked (`LeakReason::IssueUnknown`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RegistrationFoundByName {
    /// The runner id found.
    pub runner_id: RunnerId,
}

impl DomainEvent for RegistrationFoundByName {
    const EVENT_TYPE: &'static str = "registration_found_by_name";
    const SCHEMA_VERSION: u32 = 1;
}

/// The JIT config reached the guest's launcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RegistrationDelivered {}

impl DomainEvent for RegistrationDelivered {
    const EVENT_TYPE: &'static str = "registration_delivered";
    const SCHEMA_VERSION: u32 = 1;
}

/// GitHub showed the issued runner id online (ADR 0038).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RegistrationConnected {
    /// The runner id seen online.
    pub runner_id: RunnerId,
}

impl DomainEvent for RegistrationConnected {
    const EVENT_TYPE: &'static str = "registration_connected";
    const SCHEMA_VERSION: u32 = 1;
}

/// The runner took its job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RegistrationConsumed {
    /// The job.
    pub request_id: RunnerRequestId,
}

impl DomainEvent for RegistrationConsumed {
    const EVENT_TYPE: &'static str = "registration_consumed";
    const SCHEMA_VERSION: u32 = 1;
}

/// The runner's job completed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RegistrationCompleted {
    /// The job's result.
    pub result: JobResult,
}

impl DomainEvent for RegistrationCompleted {
    const EVENT_TYPE: &'static str = "registration_completed";
    const SCHEMA_VERSION: u32 = 1;
}

/// The JIT config is presumed leaked (P13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RegistrationPresumedLeaked {
    /// Why.
    pub reason: LeakReason,
}

impl DomainEvent for RegistrationPresumedLeaked {
    const EVENT_TYPE: &'static str = "registration_presumed_leaked";
    const SCHEMA_VERSION: u32 = 1;
}

/// The runner is to be removed by its journaled id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DeregistrationRequested {
    /// Why.
    pub reason: DeregistrationReason,
}

impl DomainEvent for DeregistrationRequested {
    const EVENT_TYPE: &'static str = "deregistration_requested";
    const SCHEMA_VERSION: u32 = 1;
}

/// GitHub no longer lists the runner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RegistrationDeregistered {
    /// How the removal ended.
    pub answer: Removal,
}

impl DomainEvent for RegistrationDeregistered {
    const EVENT_TYPE: &'static str = "registration_deregistered";
    const SCHEMA_VERSION: u32 = 1;
}

/// GitHub refused the removal: the runner is busy. The retirement it served, if any, is
/// withdrawn and never retried (I21).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DeregistrationRefusedBusy {
    /// The retirement the refused request served.
    pub retirement: Option<EventId>,
}

impl DomainEvent for DeregistrationRefusedBusy {
    const EVENT_TYPE: &'static str = "deregistration_refused_busy";
    const SCHEMA_VERSION: u32 = 1;
}

/// GitHub holds no runner for the registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RegistrationAbsent {}

impl DomainEvent for RegistrationAbsent {
    const EVENT_TYPE: &'static str = "registration_absent";
    const SCHEMA_VERSION: u32 = 1;
}

event_set! {
    /// Everything that happens to a runner registration.
    pub enum RegistrationEvent {
        /// See [`RegistrationEpochAdvanced`].
        RegistrationEpochAdvanced(RegistrationEpochAdvanced),
        /// See [`RegistrationRequested`].
        RegistrationRequested(RegistrationRequested),
        /// See [`RegistrationIssued`].
        RegistrationIssued(RegistrationIssued),
        /// See [`RegistrationIssueUnknown`].
        RegistrationIssueUnknown(RegistrationIssueUnknown),
        /// See [`RegistrationFoundByName`].
        RegistrationFoundByName(RegistrationFoundByName),
        /// See [`RegistrationDelivered`].
        RegistrationDelivered(RegistrationDelivered),
        /// See [`RegistrationConnected`].
        RegistrationConnected(RegistrationConnected),
        /// See [`RegistrationConsumed`].
        RegistrationConsumed(RegistrationConsumed),
        /// See [`RegistrationCompleted`].
        RegistrationCompleted(RegistrationCompleted),
        /// See [`RegistrationPresumedLeaked`].
        RegistrationPresumedLeaked(RegistrationPresumedLeaked),
        /// See [`DeregistrationRequested`].
        DeregistrationRequested(DeregistrationRequested),
        /// See [`RegistrationDeregistered`].
        RegistrationDeregistered(RegistrationDeregistered),
        /// See [`DeregistrationRefusedBusy`].
        DeregistrationRefusedBusy(DeregistrationRefusedBusy),
        /// See [`RegistrationAbsent`].
        RegistrationAbsent(RegistrationAbsent),
    }
}
