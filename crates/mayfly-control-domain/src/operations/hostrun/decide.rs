//! `decide` and `evolve` for the host run: every phase against every command, as explicit match
//! arms with no wildcard (`ARCH-exhaustive-transition`); the outcome of every pair is pinned by
//! `tests/hostrun/matrix.rs::should_define_every_pair_when_each_phase_meets_each_command`.

use mayfly_domain::{Epoch, Millis};

use super::events::{
    EpochMinted, HostRunEvent, PreviousRunEndedUnclean, RecoveryCompleted, RunEnded, RunStarted,
    SafeModeEntered, SafeModeLeft, ShutdownStarted,
};
use super::state::{
    HostRunCommand, HostRunRejection, HostRunState, RecoveryCounts, RunPhase, SafeMode,
    SafeModeExit, SafeModeReason, Verification,
};
use crate::command::Command;

/// What `decide` returns.
pub type Decision = Result<Vec<HostRunEvent>, HostRunRejection>;

/// Decides `command` against `state`.
///
/// `StartRun` and `MintEpoch` are the commands of a run that has no epoch yet: they are fenced
/// by I17 instead (the minted epoch is above both the journal's and the floor; a second daemon
/// on the same state is kept out by the state directory's lock). Every other command carries
/// the run's minted epoch: an older one is refused, and once this run minted, so is a newer one
/// it never minted (I18,
/// `tests/hostrun/rules.rs::should_refuse_an_older_run_epoch_when_a_newer_one_was_minted`).
///
/// # Errors
/// The [`HostRunRejection`] that names why the command is not allowed.
pub fn decide(state: &HostRunState, command: Command<HostRunCommand>) -> Decision {
    let Command {
        issued_at,
        run_epoch,
        body,
        ..
    } = command;
    let fenced = match body {
        HostRunCommand::StartRun { .. } | HostRunCommand::MintEpoch { .. } => false,
        HostRunCommand::CompleteRecovery { .. }
        | HostRunCommand::EnterSafeMode { .. }
        | HostRunCommand::LeaveSafeMode { .. }
        | HostRunCommand::BeginShutdown
        | HostRunCommand::EndRun => true,
    };
    if fenced && run_epoch < state.epoch {
        return Err(HostRunRejection::StaleRun);
    }
    let minted = match state.phase {
        RunPhase::Recovering | RunPhase::Operating | RunPhase::ShuttingDown => true,
        RunPhase::NotStarted | RunPhase::Starting | RunPhase::Ended => false,
    };
    if fenced && minted && run_epoch > state.epoch {
        return Err(HostRunRejection::EpochNotMinted);
    }
    match body {
        HostRunCommand::StartRun { boot_id, version } => Ok(decide_start(state, boot_id, version)),
        HostRunCommand::MintEpoch {
            floor,
            verification,
            crash_loop,
        } => decide_mint(state, floor, verification, crash_loop, issued_at),
        HostRunCommand::CompleteRecovery { counts } => decide_complete(state, counts),
        HostRunCommand::EnterSafeMode { reason } => decide_enter(state, reason, issued_at),
        HostRunCommand::LeaveSafeMode { exit } => decide_leave(state, exit),
        HostRunCommand::BeginShutdown => match state.phase {
            RunPhase::Starting | RunPhase::Recovering | RunPhase::Operating => {
                Ok(vec![HostRunEvent::ShutdownStarted(ShutdownStarted {})])
            }
            RunPhase::ShuttingDown => Err(HostRunRejection::AlreadyShuttingDown),
            RunPhase::NotStarted | RunPhase::Ended => Err(HostRunRejection::NoRun),
        },
        HostRunCommand::EndRun => match state.phase {
            RunPhase::ShuttingDown => Ok(vec![HostRunEvent::RunEnded(RunEnded {
                run_no: state.run_no,
            })]),
            RunPhase::Starting | RunPhase::Recovering | RunPhase::Operating => {
                Err(HostRunRejection::NotShuttingDown)
            }
            RunPhase::NotStarted | RunPhase::Ended => Err(HostRunRejection::NoRun),
        },
    }
}

/// One run at a time: a run that never recorded its end is closed as unclean by the next.
fn decide_start(state: &HostRunState, boot_id: String, version: String) -> Vec<HostRunEvent> {
    let mut events = Vec::with_capacity(2);
    match state.phase {
        RunPhase::NotStarted | RunPhase::Ended => {}
        RunPhase::Starting
        | RunPhase::Recovering
        | RunPhase::Operating
        | RunPhase::ShuttingDown => {
            events.push(HostRunEvent::PreviousRunEndedUnclean(
                PreviousRunEndedUnclean {
                    run_no: state.run_no,
                },
            ));
        }
    }
    events.push(HostRunEvent::RunStarted(RunStarted {
        run_no: state.run_no.saturating_add(1),
        boot_id,
        version,
    }));
    events
}

/// I17: the epoch is one above the higher of the journal's and the floor, and a floor above
/// the journal's is a rolled-back journal, recorded as safe mode in the same unit as the epoch,
/// before anything uses it; so is a verification that does not trust the journal, and a crash
/// loop the service manager reports (so a crash-looping run is never a verified restart:
/// `tests/hostrun/rules.rs::should_refuse_a_verified_restart_when_the_run_is_crash_looping`). Pinned by
/// `tests/hostrun/rules.rs::should_mint_above_the_journal_and_the_floor_when_a_run_starts`,
/// `should_enter_safe_mode_when_the_floor_is_above_the_journal` and
/// `tests/hostrun/properties.rs::should_mint_strictly_increasing_epochs_when_runs_restart`.
fn decide_mint(
    state: &HostRunState,
    floor: Epoch,
    verification: Verification,
    crash_loop: bool,
    at: Millis,
) -> Decision {
    match state.phase {
        RunPhase::Starting => {}
        RunPhase::Recovering | RunPhase::Operating | RunPhase::ShuttingDown => {
            return Err(HostRunRejection::AlreadyMinted);
        }
        RunPhase::NotStarted | RunPhase::Ended => return Err(HostRunRejection::NoRun),
    }
    let Some(epoch) = state.epoch.max(floor).next() else {
        return Err(HostRunRejection::EpochExhausted);
    };
    let mut reasons = Vec::with_capacity(3);
    if floor > state.epoch {
        reasons.push(SafeModeReason::EpochBelowFloor);
    }
    let verified = match verification {
        Verification::Verified => true,
        Verification::UnknownEventKind => {
            reasons.push(SafeModeReason::UnknownEventKind);
            false
        }
        Verification::StructureFailed => {
            reasons.push(SafeModeReason::VerificationFailed);
            false
        }
    };
    if crash_loop {
        reasons.push(SafeModeReason::CrashLoop);
    }
    let mut events = vec![HostRunEvent::EpochMinted(EpochMinted {
        epoch,
        floor,
        verified,
    })];
    events.extend(reasons.into_iter().map(|reason| {
        HostRunEvent::SafeModeEntered(SafeModeEntered {
            reason,
            run_no: state.run_no,
            at,
        })
    }));
    Ok(events)
}

fn decide_complete(state: &HostRunState, counts: RecoveryCounts) -> Decision {
    match state.phase {
        RunPhase::Recovering => match state.safe_mode {
            None if state.restart_required => Err(HostRunRejection::RestartRequired),
            None => Ok(vec![HostRunEvent::RecoveryCompleted(RecoveryCompleted {
                counts,
            })]),
            Some(_) => Err(HostRunRejection::InSafeMode),
        },
        RunPhase::Starting => Err(HostRunRejection::NotMinted),
        RunPhase::Operating | RunPhase::ShuttingDown => Err(HostRunRejection::NotRecovering),
        RunPhase::NotStarted | RunPhase::Ended => Err(HostRunRejection::NoRun),
    }
}

/// The same reason in the same run records nothing again; another one is recorded, so a
/// verified restart counts from the run that found the latest reason.
fn decide_enter(state: &HostRunState, reason: SafeModeReason, at: Millis) -> Decision {
    match state.phase {
        RunPhase::Recovering | RunPhase::Operating | RunPhase::ShuttingDown => {}
        RunPhase::Starting => return Err(HostRunRejection::NotMinted),
        RunPhase::NotStarted | RunPhase::Ended => return Err(HostRunRejection::NoRun),
    }
    let recorded = state
        .safe_mode
        .is_some_and(|s| s.reason == reason && s.entered_run == state.run_no);
    if recorded {
        return Ok(vec![]);
    }
    Ok(vec![HostRunEvent::SafeModeEntered(SafeModeEntered {
        reason,
        run_no: state.run_no,
        at,
    })])
}

/// Safe mode is left only by a completed plan of this run, while it recovers, with a
/// verification of the journal taken after it (the run then restarts, as
/// `formal/recovery.qnt`'s `completePlan` takes the daemon down; a plan exit never returns an
/// operating run to operating: safe mode entered while operating is left by a restart that
/// recovers, `tests/hostrun/rules.rs::should_leave_by_a_plan_only_while_recovering_when_safe_mode_was_entered_operating`), or by a later run whose start-up verification trusted the journal and found no
/// crash loop; a rolled-back journal only by the plan. Pinned by `tests/hostrun/rules.rs::should_leave_safe_mode_only_by_a_completed_plan_or_a_verified_restart_when_any_exit_is_tried`
/// and `tests/hostrun/properties.rs::should_leave_safe_mode_only_by_its_two_exits_when_commands_are_random`.
fn decide_leave(state: &HostRunState, exit: SafeModeExit) -> Decision {
    match state.phase {
        RunPhase::Recovering | RunPhase::Operating | RunPhase::ShuttingDown => {}
        RunPhase::Starting => return Err(HostRunRejection::NotMinted),
        RunPhase::NotStarted | RunPhase::Ended => return Err(HostRunRejection::NoRun),
    }
    let Some(safe) = state.safe_mode else {
        return Err(HostRunRejection::NotInSafeMode);
    };
    let plan_id = match exit {
        SafeModeExit::PlanCompleted {
            plan_id,
            completed_ref,
            plan_epoch,
            verification,
        } => {
            match state.phase {
                RunPhase::Recovering => {}
                RunPhase::Operating | RunPhase::ShuttingDown => {
                    return Err(HostRunRejection::NotRecovering);
                }
                RunPhase::NotStarted | RunPhase::Starting | RunPhase::Ended => {
                    return Err(HostRunRejection::NoRun);
                }
            }
            if completed_ref.is_none() {
                return Err(HostRunRejection::PlanNotCompleted);
            }
            if plan_epoch != state.epoch {
                return Err(HostRunRejection::PlanOfAnotherRun);
            }
            match verification {
                Verification::Verified => {}
                Verification::UnknownEventKind | Verification::StructureFailed => {
                    return Err(HostRunRejection::JournalUnverified);
                }
            }
            Some(plan_id)
        }
        SafeModeExit::VerifiedRestart => {
            if !state.verified {
                return Err(HostRunRejection::JournalUnverified);
            }
            if safe.needs_plan {
                return Err(HostRunRejection::PlanRequired);
            }
            if safe.entered_run >= state.run_no {
                return Err(HostRunRejection::RestartRequired);
            }
            None
        }
    };
    Ok(vec![HostRunEvent::SafeModeLeft(SafeModeLeft { plan_id })])
}

/// Applies one recorded event.
#[must_use]
pub fn evolve(mut state: HostRunState, event: &HostRunEvent) -> HostRunState {
    match event {
        HostRunEvent::RunStarted(e) => {
            state.phase = RunPhase::Starting;
            state.run_no = e.run_no;
            state.verified = false;
            state.restart_required = false;
        }
        HostRunEvent::PreviousRunEndedUnclean(_) => {}
        HostRunEvent::EpochMinted(e) => {
            state.phase = RunPhase::Recovering;
            state.epoch = e.epoch;
            state.verified = e.verified;
        }
        HostRunEvent::RecoveryCompleted(_) => state.phase = RunPhase::Operating,
        HostRunEvent::SafeModeEntered(e) => {
            state.safe_mode = Some(match state.safe_mode {
                Some(s) => SafeMode {
                    reason: e.reason,
                    since: s.since,
                    entered_run: e.run_no,
                    needs_plan: s.needs_plan || e.reason.needs_plan(),
                },
                None => SafeMode {
                    reason: e.reason,
                    since: e.at,
                    entered_run: e.run_no,
                    needs_plan: e.reason.needs_plan(),
                },
            });
        }
        HostRunEvent::SafeModeLeft(e) => {
            state.safe_mode = None;
            state.restart_required = e.plan_id.is_some();
        }
        HostRunEvent::ShutdownStarted(_) => state.phase = RunPhase::ShuttingDown,
        HostRunEvent::RunEnded(_) => state.phase = RunPhase::Ended,
    }
    state
}
