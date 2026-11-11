//! The quarantine case's events (`doc/domain-model.md` §6.10), with golden samples under
//! `tests/fixtures/events/<type>/`.

use mayfly_domain::{AttemptId, Epoch, EventId, Millis, ObservationId, ProviderInstanceId};
use serde::{Deserialize, Serialize};

use super::state::{Disposition, EscalationLevel, Holds};
use crate::attempt::QuarantineReason;
use crate::event::DomainEvent;
use crate::event_set;

/// The case accepted a command under a newer run epoch than any before (I18); recorded ahead
/// of that command's events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct QuarantineEpochAdvanced {
    /// The new highest run epoch.
    pub epoch: Epoch,
}

impl DomainEvent for QuarantineEpochAdvanced {
    const EVENT_TYPE: &'static str = "quarantine_epoch_advanced";
    const SCHEMA_VERSION: u32 = 1;
}

/// A case was opened for a quarantined attempt (an ERROR-level event for the operator, P16).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct QuarantineCaseOpened {
    /// The attempt.
    pub attempt_id: AttemptId,
    /// Why it was quarantined.
    pub reason: QuarantineReason,
    /// The VM's recorded instance id, if any.
    pub instance_id: Option<ProviderInstanceId>,
    /// What it holds.
    pub holds: Holds,
    /// The evidence.
    pub evidence: Vec<EventId>,
    /// When.
    pub opened_at: Millis,
}

impl DomainEvent for QuarantineCaseOpened {
    const EVENT_TYPE: &'static str = "quarantine_case_opened";
    const SCHEMA_VERSION: u32 = 1;
}

/// An operator inspected the case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct QuarantineCaseInspected {
    /// The operator's uid.
    pub operator_uid: u32,
}

impl DomainEvent for QuarantineCaseInspected {
    const EVENT_TYPE: &'static str = "quarantine_case_inspected";
    const SCHEMA_VERSION: u32 = 1;
}

/// The case was escalated. Loud, and nothing more: it never acts on the VM (ADR 0025).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct QuarantineCaseEscalated {
    /// The new level.
    pub level: EscalationLevel,
}

impl DomainEvent for QuarantineCaseEscalated {
    const EVENT_TYPE: &'static str = "quarantine_case_escalated";
    const SCHEMA_VERSION: u32 = 1;
}

/// An operator resolved the case. Version 2 records the instance id a `delete_with_proof`
/// relied on: the proof the later `DeleteVm` must carry (KER-10F).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct QuarantineCaseResolved {
    /// How.
    pub disposition: Disposition,
    /// The operator's uid.
    pub operator_uid: u32,
    /// For a release: the complete inventory that did not list the VM.
    pub absence: Option<ObservationId>,
    /// For a delete: the case's recorded instance id, the proof the delete carries.
    pub instance_id: Option<ProviderInstanceId>,
}

impl DomainEvent for QuarantineCaseResolved {
    const EVENT_TYPE: &'static str = "quarantine_case_resolved";
    const SCHEMA_VERSION: u32 = 2;
}

/// Version 1 of [`QuarantineCaseResolved`] (KER-10), without the instance id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct QuarantineCaseResolvedV1 {
    /// How.
    pub disposition: Disposition,
    /// The operator's uid.
    pub operator_uid: u32,
    /// For a release: the complete inventory that did not list the VM.
    pub absence: Option<ObservationId>,
}

impl DomainEvent for QuarantineCaseResolvedV1 {
    const EVENT_TYPE: &'static str = "quarantine_case_resolved";
    const SCHEMA_VERSION: u32 = 1;
}

/// Version 1 recorded no instance id; a delete resolved by it carries none, so the delete's
/// proof is the attempt's own record, as before KER-10F.
fn upcast_resolved_v1(old: QuarantineCaseResolvedV1) -> QuarantineCaseEvent {
    QuarantineCaseEvent::QuarantineCaseResolved(QuarantineCaseResolved {
        disposition: old.disposition,
        operator_uid: old.operator_uid,
        absence: old.absence,
        instance_id: None,
    })
}

event_set! {
    /// Everything that happens to a quarantine case.
    pub enum QuarantineCaseEvent {
        /// See [`QuarantineEpochAdvanced`].
        QuarantineEpochAdvanced(QuarantineEpochAdvanced),
        /// See [`QuarantineCaseOpened`].
        QuarantineCaseOpened(QuarantineCaseOpened),
        /// See [`QuarantineCaseInspected`].
        QuarantineCaseInspected(QuarantineCaseInspected),
        /// See [`QuarantineCaseEscalated`].
        QuarantineCaseEscalated(QuarantineCaseEscalated),
        /// See [`QuarantineCaseResolved`].
        QuarantineCaseResolved(QuarantineCaseResolved),
    }
    legacy {
        QuarantineCaseResolvedV1 => upcast_resolved_v1,
    }
}
