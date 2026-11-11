//! The host run's `DETERMINISM_CASES` (ver §3.1): accepted cases along a host's runs and one
//! refused case per [`HostRunRejection`] code. Histories are produced by `decide` itself from
//! command paths (each path is checked to be accepted step by step by
//! `should_accept_every_step_when_a_case_path_is_replayed`).

use mayfly_domain::{CommandId, Epoch, EventId, Millis, PlanId};

use super::events::HostRunEvent;
use super::state::{
    HostRunCommand, HostRunRejection, HostRunState, RecoveryCounts, SafeModeExit, SafeModeReason,
    Verification,
};
use super::{HostRun, decide, evolve};
use crate::aggregate::{DeterminismCase, Expect};
use crate::command::{Actor, Command};

type Step = Command<HostRunCommand>;

fn at(run_epoch: u64, body: HostRunCommand) -> Step {
    Command::new(
        CommandId::from_u128(0xC1),
        Millis::new(1_000),
        Epoch::new(run_epoch),
        Actor::Startup,
        body,
    )
}

fn start() -> HostRunCommand {
    HostRunCommand::StartRun {
        boot_id: "boot0".to_owned(),
        version: "0.1.0".to_owned(),
    }
}

fn mint(floor: u64, verification: Verification) -> HostRunCommand {
    HostRunCommand::MintEpoch {
        floor: Epoch::new(floor),
        verification,
    }
}

fn complete() -> HostRunCommand {
    HostRunCommand::CompleteRecovery {
        counts: RecoveryCounts {
            adopted: 2,
            retired: 1,
            quarantined: 1,
            lost: 0,
            reported: 1,
        },
    }
}

fn by_plan(completed: bool) -> HostRunCommand {
    HostRunCommand::LeaveSafeMode {
        exit: SafeModeExit::PlanCompleted {
            plan_id: PlanId::from_u128(0xB1),
            completed_ref: completed.then(|| EventId::from_u128(0xE1)),
        },
    }
}

fn by_restart() -> HostRunCommand {
    HostRunCommand::LeaveSafeMode {
        exit: SafeModeExit::VerifiedRestart,
    }
}

fn started() -> Vec<Step> {
    vec![at(0, start())]
}

fn recovering() -> Vec<Step> {
    let mut path = started();
    path.push(at(0, mint(0, Verification::Verified)));
    path
}

fn operating() -> Vec<Step> {
    let mut path = recovering();
    path.push(at(1, complete()));
    path
}

fn shutting_down() -> Vec<Step> {
    let mut path = operating();
    path.push(at(1, HostRunCommand::BeginShutdown));
    path
}

fn ended() -> Vec<Step> {
    let mut path = shutting_down();
    path.push(at(1, HostRunCommand::EndRun));
    path
}

/// The floor (5) is above the journal (0): epoch 6, safe mode for a rolled-back journal.
fn rolled_back() -> Vec<Step> {
    let mut path = started();
    path.push(at(0, mint(5, Verification::Verified)));
    path
}

/// The rolled-back journal's next run: epoch 7, verified, still in safe mode.
fn rolled_back_restarted() -> Vec<Step> {
    let mut path = rolled_back();
    path.extend([at(6, start()), at(6, mint(6, Verification::Verified))]);
    path
}

/// An unknown event kind: epoch 1, unverified, safe mode.
fn unknown_kind() -> Vec<Step> {
    let mut path = started();
    path.push(at(0, mint(0, Verification::UnknownEventKind)));
    path
}

/// After an upgrade, the next run verifies: epoch 2, run 2, still in safe mode.
fn unknown_kind_restarted() -> Vec<Step> {
    let mut path = unknown_kind();
    path.extend([at(1, start()), at(1, mint(1, Verification::Verified))]);
    path
}

fn crash_loop() -> Vec<Step> {
    let mut path = operating();
    path.push(at(
        1,
        HostRunCommand::EnterSafeMode {
            reason: SafeModeReason::CrashLoop,
        },
    ));
    path
}

/// The journal's epoch at the numeric limit, and a new run started on it.
fn exhausted() -> Vec<Step> {
    let mut path = started();
    path.extend([
        at(0, mint(u64::MAX - 1, Verification::Verified)),
        at(u64::MAX, start()),
    ]);
    path
}

/// Folds `path` through `decide` and `evolve`; the first refused step, if any.
fn replay_path(path: Vec<Step>) -> (Vec<HostRunEvent>, Option<(usize, HostRunRejection)>) {
    let mut state = HostRunState::initial();
    let mut history = Vec::new();
    for (index, step) in path.into_iter().enumerate() {
        match decide(&state, step) {
            Ok(events) => {
                for event in &events {
                    state = evolve(state, event);
                }
                history.extend(events);
            }
            Err(rejection) => return (history, Some((index, rejection))),
        }
    }
    (history, None)
}

fn case(
    name: &'static str,
    path: Vec<Step>,
    command: Step,
    expect: Expect,
) -> DeterminismCase<HostRun> {
    DeterminismCase {
        name,
        history: replay_path(path).0,
        command,
        expect,
    }
}

/// Every path a case starts from, for the replay check.
#[cfg(test)]
fn paths() -> Vec<Vec<Step>> {
    vec![
        started(),
        recovering(),
        operating(),
        shutting_down(),
        ended(),
        rolled_back(),
        rolled_back_restarted(),
        unknown_kind(),
        unknown_kind_restarted(),
        crash_loop(),
        exhausted(),
    ]
}

pub(super) fn cases() -> Vec<DeterminismCase<HostRun>> {
    use Expect::{Accepted, Refused};
    vec![
        // accepted
        case("start_run", Vec::new(), at(0, start()), Accepted),
        case("start_after_clean_end", ended(), at(1, start()), Accepted),
        case(
            "start_after_unclean_end",
            operating(),
            at(1, start()),
            Accepted,
        ),
        case(
            "mint_epoch",
            started(),
            at(0, mint(0, Verification::Verified)),
            Accepted,
        ),
        case(
            "floor_above_journal_enters_safe_mode",
            started(),
            at(0, mint(5, Verification::Verified)),
            Accepted,
        ),
        case(
            "unverified_journal_enters_safe_mode",
            started(),
            at(0, mint(0, Verification::StructureFailed)),
            Accepted,
        ),
        case(
            "complete_recovery",
            recovering(),
            at(1, complete()),
            Accepted,
        ),
        case(
            "enter_safe_mode",
            operating(),
            at(
                1,
                HostRunCommand::EnterSafeMode {
                    reason: SafeModeReason::CrashLoop,
                },
            ),
            Accepted,
        ),
        case(
            "same_reason_records_nothing",
            unknown_kind(),
            at(
                1,
                HostRunCommand::EnterSafeMode {
                    reason: SafeModeReason::UnknownEventKind,
                },
            ),
            Accepted,
        ),
        case(
            "leave_by_completed_plan",
            rolled_back(),
            at(6, by_plan(true)),
            Accepted,
        ),
        case(
            "leave_by_verified_restart",
            unknown_kind_restarted(),
            at(2, by_restart()),
            Accepted,
        ),
        case(
            "begin_shutdown",
            operating(),
            at(1, HostRunCommand::BeginShutdown),
            Accepted,
        ),
        case(
            "end_run",
            shutting_down(),
            at(1, HostRunCommand::EndRun),
            Accepted,
        ),
        // refused, one per code
        case(
            "stale_run",
            operating(),
            at(0, HostRunCommand::BeginShutdown),
            Refused,
        ),
        case(
            "no_run",
            Vec::new(),
            at(0, HostRunCommand::BeginShutdown),
            Refused,
        ),
        case(
            "already_minted",
            recovering(),
            at(1, mint(1, Verification::Verified)),
            Refused,
        ),
        case(
            "epoch_exhausted",
            exhausted(),
            at(u64::MAX, mint(0, Verification::Verified)),
            Refused,
        ),
        case("not_minted", started(), at(0, complete()), Refused),
        case("not_recovering", operating(), at(1, complete()), Refused),
        case("in_safe_mode", unknown_kind(), at(1, complete()), Refused),
        case(
            "not_in_safe_mode",
            operating(),
            at(1, by_restart()),
            Refused,
        ),
        case(
            "plan_not_completed",
            rolled_back(),
            at(6, by_plan(false)),
            Refused,
        ),
        case(
            "journal_unverified",
            unknown_kind(),
            at(1, by_plan(true)),
            Refused,
        ),
        case(
            "restart_required",
            crash_loop(),
            at(1, by_restart()),
            Refused,
        ),
        case(
            "plan_required",
            rolled_back_restarted(),
            at(7, by_restart()),
            Refused,
        ),
        case(
            "already_shutting_down",
            shutting_down(),
            at(1, HostRunCommand::BeginShutdown),
            Refused,
        ),
        case(
            "not_shutting_down",
            operating(),
            at(1, HostRunCommand::EndRun),
            Refused,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_accept_every_step_when_a_case_path_is_replayed() {
        // given
        let paths = paths();
        // when
        let refusals: Vec<(usize, Option<(usize, HostRunRejection)>)> = paths
            .into_iter()
            .map(|path| replay_path(path).1)
            .enumerate()
            .filter(|(_, refusal)| refusal.is_some())
            .collect();
        // then
        assert!(refusals.is_empty(), "{refusals:?}");
    }
}
