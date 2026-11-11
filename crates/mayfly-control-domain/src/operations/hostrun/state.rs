//! The host run's folded state, its commands and its refusals.

use mayfly_domain::{Epoch, EventId, Millis, PlanId};
use serde::{Deserialize, Serialize};

use crate::aggregate::{CommandBody, Rejection};

/// Where the current run stands (`doc/domain-model.md` §6.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunPhase {
    /// No run was ever started on the stream.
    NotStarted,
    /// `RunStarted`: the run's epoch is not minted yet.
    Starting,
    /// `EpochMinted`: the startup recovery runs (and, in safe mode, stays here).
    Recovering,
    /// `RecoveryCompleted`: admission may open.
    Operating,
    /// `ShutdownStarted` (P24).
    ShuttingDown,
    /// `RunEnded`: the run ended cleanly.
    Ended,
}

/// Why the host is in safe mode (`doc/domain-model.md` §5.3, I17, I29).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafeModeReason {
    /// The external floor is above every epoch the journal minted: the journal was rolled back
    /// (I17, ADR 0026). Its record of ownership is not trusted until an operator-approved plan
    /// re-establishes it.
    EpochBelowFloor,
    /// The journal holds an event kind this build does not know.
    UnknownEventKind,
    /// The store's structural verification failed.
    VerificationFailed,
    /// The daemon restarted too often before completing recovery (P25, ADR 0031).
    CrashLoop,
}

impl SafeModeReason {
    /// Whether only a completed recovery plan leaves safe mode for this reason: a restart that
    /// verifies the journal's structure says nothing about a rolled-back journal's ownership
    /// record (`formal/recovery.qnt`, `rolledBackJournalIsSafeModeTest`). Pinned by
    /// `tests/hostrun/rules.rs::should_refuse_a_verified_restart_when_the_journal_was_rolled_back`.
    #[must_use]
    pub const fn needs_plan(self) -> bool {
        match self {
            Self::EpochBelowFloor => true,
            Self::UnknownEventKind | Self::VerificationFailed | Self::CrashLoop => false,
        }
    }
}

/// What the start-up verification of this run found (P1: event registry and structure; the
/// floor is the aggregate's own check, [`HostRunCommand::MintEpoch`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verification {
    /// Every stored kind is known and the structure verifies.
    Verified,
    /// A stored event kind is unknown to this build.
    UnknownEventKind,
    /// The structure does not verify.
    StructureFailed,
}

/// The host's safe mode, while it lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafeMode {
    /// The latest reason recorded.
    pub reason: SafeModeReason,
    /// When it was first entered.
    pub since: Millis,
    /// The run that recorded the latest reason: a verified restart is a later run.
    pub entered_run: u64,
    /// A reason that needs a plan was recorded: only a completed plan leaves it.
    pub needs_plan: bool,
}

/// The state of one `hostrun-<host_id>` stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRunState {
    /// The phase of the current (or last) run.
    pub phase: RunPhase,
    /// The current run's number, from 1; 0 before any.
    pub run_no: u64,
    /// The highest epoch ever minted on this stream (I17); every command but `StartRun` and
    /// `MintEpoch` is fenced by it (I18).
    pub epoch: Epoch,
    /// Whether this run's start-up verification trusted the journal's kinds and structure.
    pub verified: bool,
    /// Safe mode, recorded across runs until a completed plan or a verified restart leaves it.
    pub safe_mode: Option<SafeMode>,
}

impl HostRunState {
    /// The state of a stream with no events.
    #[must_use]
    pub const fn initial() -> Self {
        Self {
            phase: RunPhase::NotStarted,
            run_no: 0,
            epoch: Epoch::new(0),
            verified: false,
            safe_mode: None,
        }
    }

    /// Whether the host is recovering (the run started and has not completed recovery): an
    /// input of [`crate::operations::mode`].
    #[must_use]
    pub const fn recovering(&self) -> bool {
        match self.phase {
            RunPhase::Starting | RunPhase::Recovering => true,
            RunPhase::NotStarted
            | RunPhase::Operating
            | RunPhase::ShuttingDown
            | RunPhase::Ended => false,
        }
    }
}

/// What the start-up recovery did, counted for `RecoveryCompleted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RecoveryCounts {
    /// Attempts adopted under the new epoch.
    pub adopted: u32,
    /// Attempts retired.
    pub retired: u32,
    /// Attempts quarantined.
    pub quarantined: u32,
    /// Attempts whose VM was found absent and closed.
    pub lost: u32,
    /// Names reported for operator review.
    pub reported: u32,
}

/// How safe mode is left: only these two ways (`doc/domain-model.md` §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SafeModeExit {
    /// A recovery plan completed: P20 passes the plan and the `RecoveryPlanCompleted` event it
    /// read; `None` is refused.
    PlanCompleted {
        /// The plan.
        plan_id: PlanId,
        /// Its `RecoveryPlanCompleted` event.
        completed_ref: Option<EventId>,
    },
    /// A later run's start-up verification trusted the journal.
    VerifiedRestart,
}

/// The commands of `doc/domain-model.md` §6.8.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum HostRunCommand {
    /// The daemon took the lock and opened the store: a new run. A run that never ended is
    /// recorded as ended unclean first.
    StartRun {
        /// The host's boot session id.
        boot_id: String,
        /// The daemon's build version.
        version: String,
    },
    /// Mint this run's epoch above the journal's and the external floor (I17).
    MintEpoch {
        /// The external floor as read at start-up.
        floor: Epoch,
        /// What the start-up verification found.
        verification: Verification,
    },
    /// The startup recovery finished.
    CompleteRecovery {
        /// What it did.
        counts: RecoveryCounts,
    },
    /// Contain: the journal or the store cannot be trusted.
    EnterSafeMode {
        /// Why.
        reason: SafeModeReason,
    },
    /// Leave safe mode.
    LeaveSafeMode {
        /// How.
        exit: SafeModeExit,
    },
    /// SIGTERM: the shutdown policy P24 runs.
    BeginShutdown,
    /// The shutdown finished its durable writes.
    EndRun,
}

impl CommandBody for HostRunCommand {
    const TYPES: &'static [&'static str] = &[
        "start_run",
        "mint_epoch",
        "complete_recovery",
        "enter_safe_mode",
        "leave_safe_mode",
        "begin_shutdown",
        "end_run",
    ];

    fn command_type(&self) -> &'static str {
        match self {
            Self::StartRun { .. } => "start_run",
            Self::MintEpoch { .. } => "mint_epoch",
            Self::CompleteRecovery { .. } => "complete_recovery",
            Self::EnterSafeMode { .. } => "enter_safe_mode",
            Self::LeaveSafeMode { .. } => "leave_safe_mode",
            Self::BeginShutdown => "begin_shutdown",
            Self::EndRun => "end_run",
        }
    }
}

/// A refusal of the `HostRun` aggregate. Its serde form is its code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostRunRejection {
    /// The command's run epoch is older than the minted one (I18).
    StaleRun,
    /// No run is live (never started, or ended).
    NoRun,
    /// The run's epoch is already minted.
    AlreadyMinted,
    /// The epoch cannot rise any more (I17 forbids reuse).
    EpochExhausted,
    /// The command needs the run's epoch, which is not minted yet.
    NotMinted,
    /// Recovery is not what the run is doing.
    NotRecovering,
    /// Recovery cannot complete in safe mode: only a plan or a verified restart leaves it.
    InSafeMode,
    /// The host is not in safe mode.
    NotInSafeMode,
    /// A plan exit without the plan's completion.
    PlanNotCompleted,
    /// This run's start-up verification did not trust the journal.
    JournalUnverified,
    /// A verified restart in the run that recorded safe mode.
    RestartRequired,
    /// Safe mode for a rolled-back journal is left only by a completed plan.
    PlanRequired,
    /// A shutdown is already under way.
    AlreadyShuttingDown,
    /// The run ends cleanly only after its shutdown began.
    NotShuttingDown,
}

impl Rejection for HostRunRejection {
    const CODES: &'static [&'static str] = &[
        "stale_run",
        "no_run",
        "already_minted",
        "epoch_exhausted",
        "not_minted",
        "not_recovering",
        "in_safe_mode",
        "not_in_safe_mode",
        "plan_not_completed",
        "journal_unverified",
        "restart_required",
        "plan_required",
        "already_shutting_down",
        "not_shutting_down",
    ];

    fn code(&self) -> &'static str {
        match self {
            Self::StaleRun => "stale_run",
            Self::NoRun => "no_run",
            Self::AlreadyMinted => "already_minted",
            Self::EpochExhausted => "epoch_exhausted",
            Self::NotMinted => "not_minted",
            Self::NotRecovering => "not_recovering",
            Self::InSafeMode => "in_safe_mode",
            Self::NotInSafeMode => "not_in_safe_mode",
            Self::PlanNotCompleted => "plan_not_completed",
            Self::JournalUnverified => "journal_unverified",
            Self::RestartRequired => "restart_required",
            Self::PlanRequired => "plan_required",
            Self::AlreadyShuttingDown => "already_shutting_down",
            Self::NotShuttingDown => "not_shutting_down",
        }
    }
}
