//! Paths through a host's runs, driven through the public `decide` and `evolve`.

use mayfly_control_domain::operations::hostrun::{
    HostRun, HostRunCommand, HostRunEvent, HostRunRejection, HostRunState, RecoveryCounts,
    SafeModeExit, SafeModeReason, Verification,
};
use mayfly_control_domain::{Actor, Aggregate, Command, Rejection};
use mayfly_domain::{CommandId, Epoch, EventId, Millis, PlanId};

pub type Step = Command<HostRunCommand>;

pub fn at(run_epoch: u64, body: HostRunCommand) -> Step {
    Command::new(
        CommandId::from_u128(0xC1),
        Millis::new(1_000),
        Epoch::new(run_epoch),
        Actor::Startup,
        body,
    )
}

pub fn start() -> HostRunCommand {
    HostRunCommand::StartRun {
        boot_id: "boot0".to_owned(),
        version: "0.1.0".to_owned(),
    }
}

pub fn mint(floor: u64, verification: Verification) -> HostRunCommand {
    HostRunCommand::MintEpoch {
        floor: Epoch::new(floor),
        verification,
    }
}

pub fn complete() -> HostRunCommand {
    HostRunCommand::CompleteRecovery {
        counts: RecoveryCounts::default(),
    }
}

pub fn enter(reason: SafeModeReason) -> HostRunCommand {
    HostRunCommand::EnterSafeMode { reason }
}

pub fn by_plan(completed: bool) -> HostRunCommand {
    HostRunCommand::LeaveSafeMode {
        exit: SafeModeExit::PlanCompleted {
            plan_id: PlanId::from_u128(0xB1),
            completed_ref: completed.then(|| EventId::from_u128(0xE1)),
        },
    }
}

pub fn by_restart() -> HostRunCommand {
    HostRunCommand::LeaveSafeMode {
        exit: SafeModeExit::VerifiedRestart,
    }
}

/// Decides `step` and folds what it accepted.
pub fn apply(
    state: &HostRunState,
    step: Step,
) -> Result<(HostRunState, Vec<HostRunEvent>), HostRunRejection> {
    let events = HostRun::decide(state, step)?;
    let next = events.iter().fold(state.clone(), HostRun::evolve);
    Ok((next, events))
}

/// The state after `steps`, every one accepted.
///
/// # Panics
/// When a step is refused: the path is wrong, not the aggregate under test.
pub fn reach(steps: impl IntoIterator<Item = Step>) -> HostRunState {
    steps
        .into_iter()
        .enumerate()
        .fold(HostRun::initial(), |state, (index, step)| {
            apply(&state, step)
                .unwrap_or_else(|r| panic!("step {index} of the path refused: {r:?}"))
                .0
        })
}

/// The refusal code of `step` on `state`, or `"accepted"`.
pub fn outcome(state: &HostRunState, step: Step) -> String {
    match HostRun::decide(state, step) {
        Ok(_) => "accepted".to_owned(),
        Err(rejection) => rejection.code().to_owned(),
    }
}

pub fn recovering() -> Vec<Step> {
    vec![at(0, start()), at(0, mint(0, Verification::Verified))]
}

pub fn operating() -> Vec<Step> {
    let mut path = recovering();
    path.push(at(1, complete()));
    path
}

/// An unknown event kind at start-up: run 1, epoch 1, safe mode.
pub fn unknown_kind() -> Vec<Step> {
    vec![
        at(0, start()),
        at(0, mint(0, Verification::UnknownEventKind)),
    ]
}

/// The next run after an upgrade: run 2, epoch 2, verified, still in safe mode.
pub fn unknown_kind_restarted() -> Vec<Step> {
    let mut path = unknown_kind();
    path.extend([at(1, start()), at(1, mint(1, Verification::Verified))]);
    path
}

/// One state per phase, and the safe variants that decide differently.
pub fn matrix_states() -> Vec<(&'static str, HostRunState)> {
    let with = |base: Vec<Step>, more: Vec<Step>| {
        let mut path = base;
        path.extend(more);
        reach(path)
    };
    vec![
        ("not_started", HostRun::initial()),
        ("starting", reach([at(0, start())])),
        ("recovering", reach(recovering())),
        ("recovering/safe", reach(unknown_kind())),
        ("recovering/safe_restarted", reach(unknown_kind_restarted())),
        ("operating", reach(operating())),
        (
            "operating/safe",
            with(operating(), vec![at(1, enter(SafeModeReason::CrashLoop))]),
        ),
        (
            "shutting_down",
            with(operating(), vec![at(1, HostRunCommand::BeginShutdown)]),
        ),
        (
            "ended",
            with(
                operating(),
                vec![
                    at(1, HostRunCommand::BeginShutdown),
                    at(1, HostRunCommand::EndRun),
                ],
            ),
        ),
    ]
}

/// One stimulus of every command type (and the variants that decide differently), under the
/// highest epoch any matrix state minted.
pub fn stimuli() -> Vec<(&'static str, Step)> {
    vec![
        ("start_run", at(2, start())),
        ("mint_verified", at(2, mint(0, Verification::Verified))),
        ("mint_floor_above", at(2, mint(9, Verification::Verified))),
        (
            "mint_unknown_kind",
            at(2, mint(0, Verification::UnknownEventKind)),
        ),
        ("complete_recovery", at(2, complete())),
        ("enter_crash_loop", at(2, enter(SafeModeReason::CrashLoop))),
        ("leave_by_plan", at(2, by_plan(true))),
        ("leave_by_plan_unconfirmed", at(2, by_plan(false))),
        ("leave_by_restart", at(2, by_restart())),
        ("begin_shutdown", at(2, HostRunCommand::BeginShutdown)),
        ("end_run", at(2, HostRunCommand::EndRun)),
        ("stale_run", at(0, HostRunCommand::BeginShutdown)),
    ]
}
