//! The `HostRun` aggregate (`doc/domain-model.md` §6.8): one host's runs of the daemon, its run
//! epochs and its safe mode, stream `hostrun-<host_id>`.
//!
//! Plain data and two pure functions (ADR 0022). It protects alone:
//!
//! - **I17**: each run's epoch is minted one above the higher of every epoch the journal minted
//!   and the external floor read at start-up, so epochs strictly increase across restarts and
//!   journal restores; a floor above the journal's epochs is a rolled-back journal and records
//!   safe mode in the same unit as the epoch, before the handshake uses it
//!   (`tests/hostrun/rules.rs::should_mint_above_the_journal_and_the_floor_when_a_run_starts`,
//!   `tests/hostrun/rules.rs::should_enter_safe_mode_when_the_floor_is_above_the_journal`,
//!   `tests/hostrun/properties.rs::should_mint_strictly_increasing_epochs_when_runs_restart`).
//!   Writing the floor before the epoch is used is the handshake's (the data plane's), not
//!   this aggregate's.
//! - **Safe mode is entered by a start-up finding or a command, and left only by a completed
//!   plan or a verified restart**; a rolled-back journal only by the plan
//!   (`tests/hostrun/rules.rs::should_leave_safe_mode_only_by_a_completed_plan_or_a_verified_restart_when_any_exit_is_tried`,
//!   `tests/hostrun/rules.rs::should_refuse_a_verified_restart_when_the_journal_was_rolled_back`,
//!   `tests/hostrun/properties.rs::should_leave_safe_mode_only_by_its_two_exits_when_commands_are_random`).
//!   Recovery does not complete in safe mode.
//! - **One run at a time**: a new run closes one that never ended as `PreviousRunEndedUnclean`.
//! - **I18**: every command after the mint carries the run's epoch; an older one is refused.
//!
//! It does not enforce safe mode (the relay, the data plane's executor and the broker refuse
//! effects: I29) nor admission (the `Host` gate, through P3). The model is `formal/recovery.qnt`
//! (`verify`, `mint`, `welcome`, `completePlan`); what this aggregate adds beyond it is listed in
//! `doc/formal-models.md` (recovery, "Not modelled").

mod cases;
mod decide;
mod events;
mod state;

pub use decide::{Decision, decide, evolve};
pub use events::{
    EpochMinted, HostRunEvent, PreviousRunEndedUnclean, RecoveryCompleted, RunEnded, RunStarted,
    SafeModeEntered, SafeModeLeft, ShutdownStarted,
};
pub use state::{
    HostRunCommand, HostRunRejection, HostRunState, RecoveryCounts, RunPhase, SafeMode,
    SafeModeExit, SafeModeReason, Verification,
};

use crate::aggregate::{Aggregate, DeterminismCase, DeterminismCases};
use crate::command::Command;

/// The `HostRun` aggregate: a marker grouping its data and functions (ADR 0022).
#[derive(Debug, Clone, Copy, Default)]
pub struct HostRun;

impl Aggregate for HostRun {
    const NAME: &'static str = "hostrun";
    type State = HostRunState;
    type Body = HostRunCommand;
    type Event = HostRunEvent;
    type Rejection = HostRunRejection;

    fn initial() -> HostRunState {
        HostRunState::initial()
    }

    fn decide(state: &HostRunState, command: Command<HostRunCommand>) -> Decision {
        decide::decide(state, command)
    }

    fn evolve(state: HostRunState, event: &HostRunEvent) -> HostRunState {
        decide::evolve(state, event)
    }
}

impl DeterminismCases for HostRun {
    fn determinism_cases() -> Vec<DeterminismCase<Self>> {
        cases::cases()
    }
}
