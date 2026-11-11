//! The host run's rules (`doc/domain-model.md` §6.8): I17's epochs and floor, safe mode's two
//! exits, one run at a time and the epoch fence.

use mayfly_control_domain::operations::hostrun::{
    HostRunCommand, HostRunEvent, RunPhase, SafeModeReason, Verification,
};
use mayfly_domain::Epoch;

use super::support::{
    apply, at, by_plan, by_restart, complete, enter, mint, mint_looping, operating, outcome, reach,
    recovering, start, unknown_kind, unknown_kind_restarted,
};

#[test]
fn should_mint_above_the_journal_and_the_floor_when_a_run_starts() {
    // given: (journal's minted epoch reached by earlier runs, floor read) -> expected epoch
    let rows = [(0, 0, 1), (3, 3, 4), (3, 1, 4), (3, 7, 8), (0, 5, 6)];
    // when
    let minted: Vec<(u64, u64, u64)> = rows
        .iter()
        .map(|&(journal, floor, _)| {
            let mut path = vec![at(0, start())];
            if journal > 0 {
                path.push(at(0, mint(journal - 1, Verification::Verified)));
                path.push(at(journal, start()));
            }
            let state = reach(path);
            let after = apply(&state, at(0, mint(floor, Verification::Verified)))
                .unwrap_or_else(|r| panic!("{r:?}"))
                .0;
            (journal, floor, after.epoch.get())
        })
        .collect();
    // then
    assert_eq!(
        minted,
        rows.to_vec(),
        "each epoch is one above max(journal, floor) (I17)"
    );
}

#[test]
fn should_enter_safe_mode_when_the_floor_is_above_the_journal() {
    // given: the journal minted epoch 1; a later run's epoch 4 is on the floor
    let state = reach([
        at(0, start()),
        at(0, mint(0, Verification::Verified)),
        at(1, start()),
    ]);
    // when
    let (after, events) =
        apply(&state, at(0, mint(4, Verification::Verified))).unwrap_or_else(|r| panic!("{r:?}"));
    // then
    let safe = after
        .safe_mode
        .unwrap_or_else(|| panic!("not in safe mode"));
    assert_eq!(safe.reason, SafeModeReason::EpochBelowFloor);
    assert!(safe.needs_plan);
    assert_eq!(after.epoch, Epoch::new(5));
    assert!(
        matches!(
            events.as_slice(),
            [
                HostRunEvent::EpochMinted(_),
                HostRunEvent::SafeModeEntered(_)
            ]
        ),
        "safe mode is recorded in the same unit as the epoch: {events:?}"
    );
}

#[test]
fn should_trust_the_journal_when_the_floor_lags_it_after_a_crash_before_the_handshake() {
    // given: epoch 2 minted, the daemon died before the data plane raised the floor (still 1)
    let state = reach([
        at(0, start()),
        at(0, mint(0, Verification::Verified)),
        at(1, start()),
        at(1, mint(1, Verification::Verified)),
        at(2, start()),
    ]);
    // when
    let after = apply(&state, at(0, mint(1, Verification::Verified)))
        .unwrap_or_else(|r| panic!("{r:?}"))
        .0;
    // then
    assert_eq!(after.epoch, Epoch::new(3));
    assert!(after.safe_mode.is_none());
}

#[test]
fn should_refuse_an_older_run_epoch_when_a_newer_one_was_minted() {
    // given
    let state = reach(operating());
    // when
    let stale = outcome(&state, at(0, HostRunCommand::BeginShutdown));
    let current = outcome(&state, at(1, HostRunCommand::BeginShutdown));
    // then
    assert_eq!(
        (stale.as_str(), current.as_str()),
        ("stale_run", "accepted")
    );
}

#[test]
fn should_leave_safe_mode_only_by_a_completed_plan_or_a_verified_restart_when_any_exit_is_tried() {
    // given
    let same_run = reach(unknown_kind());
    let mut crash_loop = recovering();
    crash_loop.push(at(1, enter(SafeModeReason::CrashLoop)));
    let crash_loop = reach(crash_loop);
    let restarted = reach(unknown_kind_restarted());
    // when
    let outcomes = [
        outcome(&same_run, at(1, by_restart())),
        outcome(
            &same_run,
            at(1, by_plan(true, 1, Verification::UnknownEventKind)),
        ),
        outcome(&crash_loop, at(1, by_restart())),
        outcome(
            &crash_loop,
            at(1, by_plan(false, 1, Verification::Verified)),
        ),
        outcome(&crash_loop, at(1, by_plan(true, 0, Verification::Verified))),
        outcome(&crash_loop, at(1, by_plan(true, 1, Verification::Verified))),
        outcome(&restarted, at(2, by_restart())),
        outcome(&restarted, at(2, complete())),
    ];
    // then
    assert_eq!(
        outcomes,
        [
            "journal_unverified",
            "journal_unverified",
            "restart_required",
            "plan_not_completed",
            "plan_of_another_run",
            "accepted",
            "accepted",
            "in_safe_mode",
        ]
    );
}

#[test]
fn should_refuse_a_verified_restart_when_the_journal_was_rolled_back() {
    // given: rolled back (floor 5 above an empty journal), then restarted and verified
    let state = reach([
        at(0, start()),
        at(0, mint(5, Verification::Verified)),
        at(6, start()),
        at(6, mint(6, Verification::Verified)),
    ]);
    // when
    let restart = outcome(&state, at(7, by_restart()));
    let plan = apply(&state, at(7, by_plan(true, 7, Verification::Verified)));
    // then
    assert_eq!(restart, "plan_required");
    let after = plan.unwrap_or_else(|r| panic!("{r:?}")).0;
    assert!(after.safe_mode.is_none());
}

#[test]
fn should_keep_needing_a_plan_when_another_reason_is_recorded_after_a_rollback() {
    // given: rolled back, then a crash loop recorded in a later run
    let state = reach([
        at(0, start()),
        at(0, mint(5, Verification::Verified)),
        at(6, start()),
        at(6, mint(6, Verification::Verified)),
        at(7, enter(SafeModeReason::CrashLoop)),
        at(7, start()),
        at(7, mint(7, Verification::Verified)),
    ]);
    // when
    let restart = outcome(&state, at(8, by_restart()));
    // then
    assert_eq!(restart, "plan_required");
}

#[test]
fn should_record_the_previous_run_unclean_when_a_run_starts_over_a_live_one() {
    // given
    let state = reach(recovering());
    // when
    let (after, events) = apply(&state, at(1, start())).unwrap_or_else(|r| panic!("{r:?}"));
    // then
    assert!(
        matches!(
            events.as_slice(),
            [
                HostRunEvent::PreviousRunEndedUnclean(_),
                HostRunEvent::RunStarted(_)
            ]
        ),
        "{events:?}"
    );
    assert_eq!((after.phase, after.run_no), (RunPhase::Starting, 2));
}

#[test]
fn should_require_a_restart_when_a_completed_plan_left_safe_mode() {
    // given: `formal/recovery.qnt`'s `completePlan` takes the daemon down to verify again
    let state = reach([
        at(0, start()),
        at(0, mint(5, Verification::Verified)),
        at(6, by_plan(true, 6, Verification::Verified)),
    ]);
    // when
    let same_run = outcome(&state, at(6, complete()));
    let restarted = reach([
        at(0, start()),
        at(0, mint(5, Verification::Verified)),
        at(6, by_plan(true, 6, Verification::Verified)),
        at(6, start()),
        at(6, mint(6, Verification::Verified)),
    ]);
    let next_run = outcome(&restarted, at(7, complete()));
    // then
    assert!(state.safe_mode.is_none());
    assert_eq!(
        (same_run.as_str(), next_run.as_str()),
        ("restart_required", "accepted")
    );
}

#[test]
fn should_refuse_a_verified_restart_when_the_run_is_crash_looping() {
    // given: safe mode for an unknown kind, then a restart that verifies but crash-loops
    let mut path = unknown_kind();
    path.extend([at(1, start()), at(1, mint_looping(1))]);
    let looping = reach(path);
    // when
    let restart = outcome(&looping, at(2, by_restart()));
    // then
    assert_eq!(restart, "restart_required");
    assert_eq!(
        looping.safe_mode.map(|s| s.reason),
        Some(SafeModeReason::CrashLoop)
    );
}

#[test]
fn should_refuse_an_epoch_it_never_minted_when_the_run_minted_one() {
    // given
    let state = reach(operating());
    // when
    let newer = outcome(&state, at(2, HostRunCommand::BeginShutdown));
    // then
    assert_eq!(newer, "epoch_not_minted");
}

#[test]
fn should_leave_by_a_plan_only_while_recovering_when_safe_mode_was_entered_operating() {
    // given: safe mode entered while operating
    let mut path = operating();
    path.push(at(1, enter(SafeModeReason::CrashLoop)));
    let operating_safe = reach(path.clone());
    // when: a plan exit in that run, then after a restart that recovers
    let same_run = outcome(
        &operating_safe,
        at(1, by_plan(true, 1, Verification::Verified)),
    );
    path.extend([at(1, start()), at(1, mint(1, Verification::Verified))]);
    let restarted = reach(path);
    let next_run = outcome(&restarted, at(2, by_plan(true, 2, Verification::Verified)));
    // then: the plan never returns an operating run to operating
    assert_eq!(
        (same_run.as_str(), next_run.as_str()),
        ("not_recovering", "accepted")
    );
}
