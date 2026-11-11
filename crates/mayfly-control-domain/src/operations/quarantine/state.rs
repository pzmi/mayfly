//! The quarantine case's folded state, its commands and its refusals.

use mayfly_domain::{AttemptId, Epoch, EventId, Millis, ObservationId, ProviderInstanceId};
use serde::{Deserialize, Serialize};

use crate::aggregate::{CommandBody, Rejection};
use crate::attempt::QuarantineReason;

/// What the quarantined attempt holds, and keeps holding until the case is resolved (I28).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holds {
    /// Disk the VM occupies.
    pub disk_bytes: u64,
    /// Memory, while the VM runs.
    pub memory_bytes: Option<u64>,
    /// Whether it holds a macOS guest slot.
    pub macos_guest: bool,
}

/// How far a case was escalated (P16: at T_q1, then T_q2). Escalation is loud and never acts
/// on the VM (ADR 0025).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationLevel {
    /// A ticket.
    Ticket,
    /// A page.
    Page,
}

/// An operator's resolution of a case (`doc/domain-model.md` §6.10). Only an operator chooses
/// one; no timer or cap does (I28, ADR 0025).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// Delete the VM with its ownership proof, which the data plane re-checks before acting
    /// (ADR 0026): the case needs the recorded instance id.
    DeleteWithProof,
    /// A complete inventory showed the VM absent: release it on that observation.
    ReleasedObservedAbsent,
    /// Keep the VM (and its capacity) as it is.
    Keep,
}

/// The case's phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasePhase {
    /// No case opened on the stream.
    NotOpened,
    /// Opened (P16).
    Open,
    /// An operator inspected it.
    Inspected,
    /// An operator resolved it.
    Resolved(Disposition),
}

/// The case as opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Case {
    /// The quarantined attempt.
    pub attempt_id: AttemptId,
    /// Why.
    pub reason: QuarantineReason,
    /// The VM's recorded instance id: the proof a delete needs.
    pub instance_id: Option<ProviderInstanceId>,
    /// What it holds.
    pub holds: Holds,
    /// The events that are its evidence.
    pub evidence: Vec<EventId>,
    /// When it was opened.
    pub opened_at: Millis,
}

/// The state of one `quarantine-<attempt_id>` stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineCaseState {
    /// The phase.
    pub phase: CasePhase,
    /// The highest run epoch accepted (I18).
    pub epoch: Epoch,
    /// The case, once opened.
    pub case: Option<Case>,
    /// The highest escalation so far.
    pub escalation: Option<EscalationLevel>,
}

impl QuarantineCaseState {
    /// The state of a stream with no events.
    #[must_use]
    pub const fn initial() -> Self {
        Self {
            phase: CasePhase::NotOpened,
            epoch: Epoch::new(0),
            case: None,
            escalation: None,
        }
    }
}

/// The commands of `doc/domain-model.md` §6.10.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum QuarantineCaseCommand {
    /// Open the case of a quarantined attempt (P16).
    OpenCase {
        /// The attempt.
        attempt_id: AttemptId,
        /// Why it was quarantined.
        reason: QuarantineReason,
        /// The VM's recorded instance id, if any.
        instance_id: Option<ProviderInstanceId>,
        /// What it holds.
        holds: Holds,
        /// The evidence.
        evidence: Vec<EventId>,
    },
    /// An operator inspects it (`mayfly quarantine inspect`).
    InspectCase,
    /// Escalate (P16's timers).
    EscalateCase {
        /// To this level.
        level: EscalationLevel,
    },
    /// An operator resolves it.
    ResolveCase {
        /// How.
        disposition: Disposition,
        /// For [`Disposition::ReleasedObservedAbsent`]: the complete inventory that did not
        /// list the VM.
        absence: Option<ObservationId>,
    },
}

impl CommandBody for QuarantineCaseCommand {
    const TYPES: &'static [&'static str] =
        &["open_case", "inspect_case", "escalate_case", "resolve_case"];

    fn command_type(&self) -> &'static str {
        match self {
            Self::OpenCase { .. } => "open_case",
            Self::InspectCase => "inspect_case",
            Self::EscalateCase { .. } => "escalate_case",
            Self::ResolveCase { .. } => "resolve_case",
        }
    }
}

/// A refusal of the `QuarantineCase` aggregate. Its serde form is its code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineCaseRejection {
    /// The command's run epoch is older than the case's (I18).
    StaleRun,
    /// No case is open.
    NotOpen,
    /// The case is resolved.
    Resolved,
    /// A vanished VM is recorded as a desync and its attempt closed, never quarantined (ADR 0066
    /// as amended 2026-11-11).
    VanishedNotQuarantined,
    /// Only an operator inspects or resolves a case (I28).
    OperatorOnly,
    /// A delete without the recorded instance id that proves ownership (ADR 0026).
    NoProof,
    /// A release without a complete inventory that showed the VM absent (I25).
    AbsenceNotObserved,
    /// The stream's case is another attempt's.
    WrongAttempt,
    /// An open case is reopened for another reason.
    AlreadyOpen,
}

impl Rejection for QuarantineCaseRejection {
    const CODES: &'static [&'static str] = &[
        "stale_run",
        "not_open",
        "resolved",
        "vanished_not_quarantined",
        "operator_only",
        "no_proof",
        "absence_not_observed",
        "wrong_attempt",
        "already_open",
    ];

    fn code(&self) -> &'static str {
        match self {
            Self::StaleRun => "stale_run",
            Self::NotOpen => "not_open",
            Self::Resolved => "resolved",
            Self::VanishedNotQuarantined => "vanished_not_quarantined",
            Self::OperatorOnly => "operator_only",
            Self::NoProof => "no_proof",
            Self::AbsenceNotObserved => "absence_not_observed",
            Self::WrongAttempt => "wrong_attempt",
            Self::AlreadyOpen => "already_open",
        }
    }
}
