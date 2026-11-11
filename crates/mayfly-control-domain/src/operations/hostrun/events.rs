//! The host run's events (`doc/domain-model.md` §6.8), with golden samples under
//! `tests/fixtures/events/<type>/`.

use mayfly_domain::{Epoch, Millis, PlanId};
use serde::{Deserialize, Serialize};

use super::state::{RecoveryCounts, SafeModeReason};
use crate::event::DomainEvent;
use crate::event_set;

/// A new run of the daemon began on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RunStarted {
    /// The run's number.
    pub run_no: u64,
    /// The host's boot session id.
    pub boot_id: String,
    /// The daemon's build version.
    pub version: String,
}

impl DomainEvent for RunStarted {
    const EVENT_TYPE: &'static str = "run_started";
    const SCHEMA_VERSION: u32 = 1;
}

/// The run before this one never recorded its end: it crashed, was killed or lost power (P25).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PreviousRunEndedUnclean {
    /// The run that did not end.
    pub run_no: u64,
}

impl DomainEvent for PreviousRunEndedUnclean {
    const EVENT_TYPE: &'static str = "previous_run_ended_unclean";
    const SCHEMA_VERSION: u32 = 1;
}

/// This run's epoch, above every epoch the journal minted and above the external floor (I17).
/// The data plane raises the floor to it before it is used (the handshake).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EpochMinted {
    /// The new epoch.
    pub epoch: Epoch,
    /// The floor read at start-up.
    pub floor: Epoch,
    /// Whether the start-up verification trusted the journal's kinds and structure.
    pub verified: bool,
}

impl DomainEvent for EpochMinted {
    const EVENT_TYPE: &'static str = "epoch_minted";
    const SCHEMA_VERSION: u32 = 1;
}

/// The startup recovery finished; admission may open (P3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RecoveryCompleted {
    /// What it did.
    pub counts: RecoveryCounts,
}

impl DomainEvent for RecoveryCompleted {
    const EVENT_TYPE: &'static str = "recovery_completed";
    const SCHEMA_VERSION: u32 = 1;
}

/// The host entered (or found a further reason to stay in) safe mode: nothing destructive
/// executes and no secret is minted until it is left (I29). The actor is the envelope's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SafeModeEntered {
    /// Why.
    pub reason: SafeModeReason,
    /// The run that recorded it.
    pub run_no: u64,
    /// When.
    pub at: Millis,
}

impl DomainEvent for SafeModeEntered {
    const EVENT_TYPE: &'static str = "safe_mode_entered";
    const SCHEMA_VERSION: u32 = 1;
}

/// The host left safe mode: by a completed plan (`plan_id`) or a verified restart (`None`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SafeModeLeft {
    /// The completed plan, or `None` for a verified restart.
    pub plan_id: Option<PlanId>,
}

impl DomainEvent for SafeModeLeft {
    const EVENT_TYPE: &'static str = "safe_mode_left";
    const SCHEMA_VERSION: u32 = 1;
}

/// SIGTERM: the shutdown policy runs (P24).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ShutdownStarted {}

impl DomainEvent for ShutdownStarted {
    const EVENT_TYPE: &'static str = "shutdown_started";
    const SCHEMA_VERSION: u32 = 1;
}

/// The run ended cleanly; an unclean end is recorded by the next run
/// ([`PreviousRunEndedUnclean`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RunEnded {
    /// The run.
    pub run_no: u64,
}

impl DomainEvent for RunEnded {
    const EVENT_TYPE: &'static str = "run_ended";
    const SCHEMA_VERSION: u32 = 1;
}

event_set! {
    /// Everything that happens to a host's runs.
    pub enum HostRunEvent {
        /// See [`RunStarted`].
        RunStarted(RunStarted),
        /// See [`PreviousRunEndedUnclean`].
        PreviousRunEndedUnclean(PreviousRunEndedUnclean),
        /// See [`EpochMinted`].
        EpochMinted(EpochMinted),
        /// See [`RecoveryCompleted`].
        RecoveryCompleted(RecoveryCompleted),
        /// See [`SafeModeEntered`].
        SafeModeEntered(SafeModeEntered),
        /// See [`SafeModeLeft`].
        SafeModeLeft(SafeModeLeft),
        /// See [`ShutdownStarted`].
        ShutdownStarted(ShutdownStarted),
        /// See [`RunEnded`].
        RunEnded(RunEnded),
    }
}
