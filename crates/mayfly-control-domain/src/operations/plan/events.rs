//! The recovery plan's events (`doc/domain-model.md` §6.9), with golden samples under
//! `tests/fixtures/events/<type>/`.

use mayfly_domain::{Epoch, Millis, StateDigest};
use serde::{Deserialize, Serialize};

use super::state::{ActionOutcome, Bindings, PlanAction, StaleBinding};
use crate::event::DomainEvent;
use crate::event_set;

/// The plan accepted a command under a newer run epoch than any before (I18); recorded ahead
/// of that command's events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RecoveryPlanEpochAdvanced {
    /// The new highest run epoch.
    pub epoch: Epoch,
}

impl DomainEvent for RecoveryPlanEpochAdvanced {
    const EVENT_TYPE: &'static str = "recovery_plan_epoch_advanced";
    const SCHEMA_VERSION: u32 = 1;
}

/// A plan was drawn, bound to what it read; immutable from here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RecoveryPlanProposed {
    /// What it is bound to.
    pub bindings: Bindings,
    /// Its actions.
    pub actions: Vec<PlanAction>,
    /// Its expiry.
    pub expires_at: Millis,
}

impl DomainEvent for RecoveryPlanProposed {
    const EVENT_TYPE: &'static str = "recovery_plan_proposed";
    const SCHEMA_VERSION: u32 = 1;
}

/// An operator approved the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RecoveryPlanApproved {
    /// The operator's uid.
    pub operator_uid: u32,
}

impl DomainEvent for RecoveryPlanApproved {
    const EVENT_TYPE: &'static str = "recovery_plan_approved";
    const SCHEMA_VERSION: u32 = 1;
}

/// The intent of one action, recorded before its command (I31).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RecoveryActionStarted {
    /// The action.
    pub index: u32,
}

impl DomainEvent for RecoveryActionStarted {
    const EVENT_TYPE: &'static str = "recovery_action_started";
    const SCHEMA_VERSION: u32 = 1;
}

/// The outcome of one action's command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RecoveryActionApplied {
    /// The action.
    pub index: u32,
    /// How it ended.
    pub outcome: ActionOutcome,
    /// The journal's heads after it: the plan's binding from here.
    pub heads_after: StateDigest,
}

impl DomainEvent for RecoveryActionApplied {
    const EVENT_TYPE: &'static str = "recovery_action_applied";
    const SCHEMA_VERSION: u32 = 1;
}

/// Every action applied while the plan stayed bound: safe mode may be left (P20).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RecoveryPlanCompleted {}

impl DomainEvent for RecoveryPlanCompleted {
    const EVENT_TYPE: &'static str = "recovery_plan_completed";
    const SCHEMA_VERSION: u32 = 1;
}

/// The world moved since the plan was drawn: it is refused, and no further action of it runs
/// (I31, F45). A new plan is drawn against the world as it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RecoveryPlanRefused {
    /// The first binding that moved.
    pub stale: StaleBinding,
}

impl DomainEvent for RecoveryPlanRefused {
    const EVENT_TYPE: &'static str = "recovery_plan_refused";
    const SCHEMA_VERSION: u32 = 1;
}

/// The plan expired before it was approved or started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RecoveryPlanExpired {}

impl DomainEvent for RecoveryPlanExpired {
    const EVENT_TYPE: &'static str = "recovery_plan_expired";
    const SCHEMA_VERSION: u32 = 1;
}

event_set! {
    /// Everything that happens to a recovery plan.
    pub enum RecoveryPlanEvent {
        /// See [`RecoveryPlanEpochAdvanced`].
        RecoveryPlanEpochAdvanced(RecoveryPlanEpochAdvanced),
        /// See [`RecoveryPlanProposed`].
        RecoveryPlanProposed(RecoveryPlanProposed),
        /// See [`RecoveryPlanApproved`].
        RecoveryPlanApproved(RecoveryPlanApproved),
        /// See [`RecoveryActionStarted`].
        RecoveryActionStarted(RecoveryActionStarted),
        /// See [`RecoveryActionApplied`].
        RecoveryActionApplied(RecoveryActionApplied),
        /// See [`RecoveryPlanCompleted`].
        RecoveryPlanCompleted(RecoveryPlanCompleted),
        /// See [`RecoveryPlanRefused`].
        RecoveryPlanRefused(RecoveryPlanRefused),
        /// See [`RecoveryPlanExpired`].
        RecoveryPlanExpired(RecoveryPlanExpired),
    }
}
