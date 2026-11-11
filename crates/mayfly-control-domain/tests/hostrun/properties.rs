//! I17 and safe mode's exits, asserted on the Rust `HostRun` over random command sequences: a
//! check of the code, where `formal/recovery.qnt`'s `i17EpochsIncrease` checks the model.

use mayfly_control_domain::operations::hostrun::{
    HostRunCommand, HostRunEvent, HostRunState, SafeModeExit, SafeModeReason, Verification,
};
use mayfly_control_domain::{Actor, Command};
use mayfly_domain::{CommandId, Epoch, Millis};
use proptest::prelude::*;

use super::support::{apply, by_plan, by_restart, complete, enter, mint, start};

/// One random command over a small space, with the run epoch it is issued under: the current
/// one, an older one or a newer one.
fn command() -> impl Strategy<Value = (HostRunCommand, i8)> {
    let verification = prop_oneof![
        3 => Just(Verification::Verified),
        1 => Just(Verification::UnknownEventKind),
        1 => Just(Verification::StructureFailed),
    ];
    let reason = prop_oneof![
        Just(SafeModeReason::EpochBelowFloor),
        Just(SafeModeReason::UnknownEventKind),
        Just(SafeModeReason::VerificationFailed),
        Just(SafeModeReason::CrashLoop),
    ];
    let body = prop_oneof![
        3 => Just(start()),
        3 => (0_u64..12, verification).prop_map(|(floor, v)| mint(floor, v)),
        2 => Just(complete()),
        1 => reason.prop_map(enter),
        2 => any::<bool>().prop_map(by_plan),
        2 => Just(by_restart()),
        1 => Just(HostRunCommand::BeginShutdown),
        1 => Just(HostRunCommand::EndRun),
    ];
    (body, -1_i8..=1)
}

fn issue(state: &HostRunState, body: HostRunCommand, skew: i8) -> Command<HostRunCommand> {
    let epoch = match skew {
        -1 => state.epoch.get().saturating_sub(1),
        0 => state.epoch.get(),
        _ => state.epoch.get().saturating_add(1),
    };
    Command::new(
        CommandId::from_u128(0xC1),
        Millis::new(1_000),
        Epoch::new(epoch),
        Actor::Startup,
        body,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// I17: every minted epoch is above every epoch minted before it and above the floor it
    /// read, and a floor above the journal's epochs is never trusted.
    #[test]
    fn should_mint_strictly_increasing_epochs_when_runs_restart(
        commands in proptest::collection::vec(command(), 1..40)
    ) {
        // given
        let mut state = HostRunState::initial();
        let mut highest = Epoch::new(0);
        // when
        for (body, skew) in commands {
            let floor = match &body {
                HostRunCommand::MintEpoch { floor, .. } => Some(*floor),
                HostRunCommand::StartRun { .. }
                | HostRunCommand::CompleteRecovery { .. }
                | HostRunCommand::EnterSafeMode { .. }
                | HostRunCommand::LeaveSafeMode { .. }
                | HostRunCommand::BeginShutdown
                | HostRunCommand::EndRun => None,
            };
            let before = state.clone();
            if let Ok((after, events)) = apply(&state, issue(&state, body, skew)) {
                // then
                for event in &events {
                    if let HostRunEvent::EpochMinted(e) = event {
                        prop_assert!(e.epoch > highest, "{:?} not above {highest:?}", e.epoch);
                        prop_assert!(Some(e.epoch) > floor, "{:?} not above the floor", e.epoch);
                        highest = e.epoch;
                    }
                }
                if floor.is_some_and(|floor| floor > before.epoch) {
                    prop_assert!(
                        after.safe_mode.is_some_and(|s| s.needs_plan),
                        "a journal below the floor is trusted"
                    );
                }
                state = after;
            }
        }
    }

    /// Safe mode, once entered, is left only by `LeaveSafeMode`: a completed plan in a run
    /// whose journal verifies, or a later run that verifies a journal that was not rolled back.
    #[test]
    fn should_leave_safe_mode_only_by_its_two_exits_when_commands_are_random(
        commands in proptest::collection::vec(command(), 1..40)
    ) {
        // given
        let mut state = HostRunState::initial();
        // when
        for (body, skew) in commands {
            let before = state.clone();
            let exit = match &body {
                HostRunCommand::LeaveSafeMode { exit } => Some(*exit),
                HostRunCommand::StartRun { .. }
                | HostRunCommand::MintEpoch { .. }
                | HostRunCommand::CompleteRecovery { .. }
                | HostRunCommand::EnterSafeMode { .. }
                | HostRunCommand::BeginShutdown
                | HostRunCommand::EndRun => None,
            };
            if let Ok((after, _)) = apply(&state, issue(&state, body, skew)) {
                // then
                if let (Some(safe), None) = (before.safe_mode, after.safe_mode) {
                    prop_assert!(before.verified, "left on an unverified journal");
                    match exit {
                        Some(SafeModeExit::PlanCompleted { completed_ref, .. }) => {
                            prop_assert!(completed_ref.is_some());
                        }
                        Some(SafeModeExit::VerifiedRestart) => {
                            prop_assert!(!safe.needs_plan, "a rolled-back journal left without a plan");
                            prop_assert!(safe.entered_run < before.run_no, "left without a restart");
                        }
                        None => prop_assert!(false, "safe mode left by another command"),
                    }
                }
                state = after;
            }
        }
    }
}
