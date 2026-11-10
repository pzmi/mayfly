//! The rules of `doc/domain-model.md` §6.1 and plan §7.2, each a test: record before ack (I12),
//! one reservation per request id, desired from the latest statistics, `PlanAcquire` within
//! deliverable (I23, mutant 19) and the tier (I32), the wire behaviour of pause, drain and safe
//! mode (ADR 0029), the session's journaled deletes (A3), pending acquisitions left only on an
//! observation (A4), protocol drift (R7) and the epoch fence (I18).

use mayfly_control_domain::attempt::RunnerRequestId;
use mayfly_control_domain::demand::{DeclineReason, EventName, WireMode};
use mayfly_control_domain::scaleset::{
    LossCause, MessageItem, PROTOCOL_DRIFT_THRESHOLD, Phase, ScaleSetCommand, ScaleSetEvent,
};

use super::support::{
    FIRST, SECOND, active, active_path, apply, assigned, assigned_fact, at, auto, capacity,
    completed, deleted, ids, loss, message, offer, opened, outcome, push, reach, reach_from,
    result, started,
};

fn recorded(events: &[ScaleSetEvent]) -> &mayfly_control_domain::scaleset::MessageRecorded {
    events
        .iter()
        .find_map(|e| match e {
            ScaleSetEvent::MessageRecorded(m) => Some(m),
            _ => None,
        })
        .expect("a MessageRecorded event")
}

/// I12: the listener acks only after the record's receipt; so every message it may hold must be
/// recordable whenever a session is live, whatever the phase or mode: a degraded or closing
/// session, a paused or draining pool, safe mode with a writable journal (ADR 0029).
#[test]
fn should_record_every_message_when_a_session_is_live_in_any_mode() {
    // given
    let degraded = reach_from(
        active(),
        [message(
            FIRST,
            1,
            None,
            vec![MessageItem::Unknown; PROTOCOL_DRIFT_THRESHOLD as usize],
        )],
    );
    let closing = reach_from(active(), [ScaleSetCommand::StartClosing]);
    let held: Vec<_> = [WireMode::Paused, WireMode::Draining, WireMode::SafeMode]
        .into_iter()
        .map(|mode| reach_from(active(), [capacity(2, 2, 1, mode)]))
        .collect();
    // when
    let outcomes: Vec<String> = [degraded, closing]
        .iter()
        .chain(&held)
        .map(|state| {
            outcome(
                state,
                auto(message(FIRST, 7, Some(assigned(1)), vec![push(1)])),
            )
        })
        .collect();
    // then
    assert_eq!(outcomes, vec!["accepted"; 5]);
}

/// A1: a message redelivered in a later session has a new receipt key and is recorded again;
/// its offers are already reserved and are not reserved a second time.
#[test]
fn should_reserve_a_request_once_when_a_message_is_recorded_again_in_a_new_session() {
    // given: message 1 planned 1 and 2 in the first session; its ack was lost; restart
    let first = message(FIRST, 1, Some(assigned(0)), vec![push(1), push(2)]);
    let state = reach_from(
        active(),
        [
            first,
            loss(LossCause::Restarted),
            deleted(FIRST),
            ScaleSetCommand::BeginOpen,
            opened(SECOND, assigned(0)),
        ],
    );
    // when
    let (after, events) = apply(
        &state,
        auto(message(
            SECOND,
            1,
            Some(assigned(0)),
            vec![push(1), push(2)],
        )),
    )
    .expect("recorded again");
    // then
    let m = recorded(&events);
    assert!(m.acquire.is_empty());
    assert!(
        m.declined
            .iter()
            .all(|d| d.reason == DeclineReason::AlreadyReserved)
    );
    assert_eq!(after.acquiring, ids(&[1, 2]));
}

/// desiredFromLatestStatistics (A1): an older message's statistics never replace newer ones.
#[test]
fn should_keep_newer_statistics_when_an_older_message_is_recorded() {
    // given: message 5 said 2 assigned
    let state = reach_from(active(), [message(FIRST, 5, Some(assigned(2)), vec![])]);
    // when: message 3, older, says 0
    let (after, events) =
        apply(&state, auto(message(FIRST, 3, Some(assigned(0)), vec![]))).expect("recorded");
    // then
    assert!(!recorded(&events).statistics_applied);
    assert_eq!(after.statistics.map(|s| s.total_assigned_jobs), Some(2));
    assert_eq!(after.desired, 2);
}

/// A message without statistics (an unknown envelope) moves the cursor but not the statistics'
/// floor: an older message that carries statistics, still newer than the session's opening,
/// applies them (found by the property test; its seed is kept in `tests/proptest-regressions`).
#[test]
fn should_apply_statistics_when_only_a_newer_message_without_statistics_was_recorded() {
    // given: message 3 carried no statistics
    let state = reach_from(
        active(),
        [message(FIRST, 3, None, vec![MessageItem::Unknown])],
    );
    // when
    let (after, events) =
        apply(&state, auto(message(FIRST, 1, Some(assigned(1)), vec![]))).expect("recorded");
    // then
    assert!(recorded(&events).statistics_applied);
    assert_eq!((after.desired, after.cursor.map(|c| c.0)), (1, Some(3)));
}

/// Plan §7.2 step 5: a session open recomputes desired from its initial statistics, without
/// any message, and keeps the statistics cursor (A1).
#[test]
fn should_recompute_desired_from_initial_statistics_when_a_session_opens() {
    // given: desired 2 from message 5, then the session is lost and deleted
    let state = reach_from(
        active(),
        [
            message(FIRST, 5, Some(assigned(2)), vec![]),
            loss(LossCause::Expired),
            deleted(FIRST),
            ScaleSetCommand::BeginOpen,
        ],
    );
    // when: the new session opens with 1 assigned
    let (after, _) = apply(&state, auto(opened(SECOND, assigned(1)))).expect("opened");
    // then
    assert_eq!(after.desired, 1);
    assert_eq!(after.cursor.map(|c| c.0), Some(5));
}

/// Mutant 5 at the aggregate: the batch shows one `JobAssigned`, the statistics 40.
#[test]
fn should_take_desired_from_statistics_when_a_batch_holds_fewer_job_messages() {
    // given: deliverable 50
    let state = reach_from(active(), [capacity(2, 50, 50, WireMode::Open)]);
    // when
    let (after, _) = apply(
        &state,
        auto(message(
            FIRST,
            1,
            Some(assigned(40)),
            vec![assigned_fact(1)],
        )),
    )
    .expect("recorded");
    // then
    assert_eq!(after.desired, 40);
}

/// Mutant 6 at the aggregate: 3 of the 5 assigned jobs run; desired stays 5.
#[test]
fn should_not_add_running_jobs_when_a_message_is_recorded() {
    // given
    let state = reach_from(active(), [capacity(2, 50, 50, WireMode::Open)]);
    let mut stats = assigned(5);
    stats.total_running_jobs = 3;
    // when
    let (after, _) = apply(&state, auto(message(FIRST, 1, Some(stats), vec![]))).expect("ok");
    // then
    assert_eq!(after.desired, 5);
}

/// I23 and mutant 19 at the aggregate: with 2 deliverable, 1 assigned and 1 offer already
/// reserved, a batch of 4 offers reserves none; once the reservation is released, one.
#[test]
fn should_acquire_only_while_assigned_is_below_deliverable_when_a_batch_exceeds_it() {
    // given
    let state = reach_from(
        active(),
        [message(FIRST, 1, Some(assigned(1)), vec![push(1)])],
    );
    let batch = || vec![push(2), push(3), push(4), push(5)];
    // when
    let (state, full) =
        apply(&state, auto(message(FIRST, 2, Some(assigned(1)), batch()))).expect("ok");
    let (_, freed) = apply(
        &state,
        auto(message(FIRST, 3, Some(assigned(1)), {
            let mut items = vec![started(1)];
            items.extend(batch());
            items
        })),
    )
    .expect("ok");
    // then
    assert!(recorded(&full).acquire.is_empty());
    assert_eq!(recorded(&freed).acquire, vec![RunnerRequestId(2)]);
}

/// I32: only offers the pool's tier admits are reserved.
#[test]
fn should_decline_untrusted_offers_when_a_message_is_recorded() {
    // given
    let state = active();
    // when
    let (after, events) = apply(
        &state,
        auto(message(
            FIRST,
            1,
            Some(assigned(0)),
            vec![
                offer(1, EventName::WorkflowRun),
                offer(2, EventName::Unknown),
                offer(3, EventName::Push),
            ],
        )),
    )
    .expect("ok");
    // then
    assert_eq!(recorded(&events).acquire, vec![RunnerRequestId(3)]);
    assert_eq!(after.reserved(), ids(&[3]));
}

/// ADR 0029: pause sends the live runners, drain and safe mode 0; none acquires; the session
/// stays open.
#[test]
fn should_acquire_nothing_and_send_the_mode_wire_max_when_held() {
    // given
    let modes = [
        (WireMode::Paused, 1),
        (WireMode::Draining, 0),
        (WireMode::SafeMode, 0),
    ];
    for (mode, wire) in modes {
        let state = reach_from(active(), [capacity(2, 2, 1, mode)]);
        // when
        let (after, events) = apply(
            &state,
            auto(message(FIRST, 1, Some(assigned(0)), vec![push(1)])),
        )
        .expect("ok");
        // then
        assert_eq!(state.wire_max(), wire, "{mode:?}");
        assert!(recorded(&events).acquire.is_empty(), "{mode:?}");
        assert_eq!(after.phase, Phase::Active, "{mode:?}");
    }
}

/// Desired follows a new deliverable at once.
#[test]
fn should_recompute_desired_when_the_capacity_input_changes() {
    // given: 3 assigned, 2 deliverable
    let state = reach_from(active(), [message(FIRST, 1, Some(assigned(3)), vec![])]);
    // when
    let (after, _) = apply(&state, auto(capacity(2, 5, 5, WireMode::Open))).expect("ok");
    // then
    assert_eq!((state.desired, after.desired), (2, 3));
}

#[test]
fn should_refuse_a_capacity_input_when_it_is_older_or_reports_over_deliverable() {
    // given: revision 1 applied
    let state = active();
    // when
    let outcomes = [
        outcome(&state, auto(capacity(0, 2, 2, WireMode::Open))),
        outcome(&state, auto(capacity(1, 3, 3, WireMode::Open))),
        outcome(&state, auto(capacity(2, 2, 3, WireMode::Open))),
        outcome(&state, auto(capacity(1, 2, 2, WireMode::Open))),
    ];
    // then
    assert_eq!(
        outcomes,
        [
            "stale_capacity",
            "capacity_conflict",
            "reported_over_deliverable",
            "accepted"
        ]
    );
}

/// §3.2 and A3: a create is sent only after the journaled stale session is deleted.
#[test]
fn should_refuse_a_create_when_a_journaled_session_is_not_deleted() {
    // given
    let lost = reach_from(active(), [loss(LossCause::Restarted)]);
    // when
    let before = outcome(&lost, auto(ScaleSetCommand::BeginOpen));
    let after_delete = reach_from(lost.clone(), [deleted(FIRST)]);
    let after = outcome(&after_delete, auto(ScaleSetCommand::BeginOpen));
    // then
    assert_eq!(lost.stale_session, Some(FIRST));
    assert_eq!(
        (before.as_str(), after.as_str()),
        ("stale_session_pending", "accepted")
    );
}

/// A3: mayfly never deletes a session it has no record of; a 409 on create backs off with no
/// stale session to delete.
#[test]
fn should_refuse_to_delete_a_session_when_mayfly_holds_no_record_of_it() {
    // given: a create answered 409 (another session lives at GitHub)
    let conflicted = reach([
        super::support::ensure(),
        ScaleSetCommand::BeginOpen,
        loss(LossCause::Conflicted),
    ]);
    // when
    let delete = outcome(&conflicted, auto(deleted(SECOND)));
    // then
    assert_eq!(conflicted.phase, Phase::Backoff(LossCause::Conflicted));
    assert_eq!(conflicted.stale_session, None);
    assert_eq!(delete, "unknown_session");
}

/// A4: pending acquisitions leave `acquiring` only on an observation (an acquire answer or a
/// job fact); a session loss, a restart or a capacity change keeps them, counted.
#[test]
fn should_keep_acquisitions_pending_when_no_observation_names_them() {
    // given
    let pending = reach_from(
        active(),
        [message(FIRST, 1, Some(assigned(0)), vec![push(1), push(2)])],
    );
    // when
    let commands = reach_from(
        pending.clone(),
        [
            capacity(2, 0, 0, WireMode::Draining),
            loss(LossCause::Restarted),
            deleted(FIRST),
            ScaleSetCommand::BeginOpen,
            opened(SECOND, assigned(0)),
        ],
    );
    let answered = reach_from(pending.clone(), [result(&[1, 2], &[1])]);
    let fact = reach_from(
        pending,
        [message(FIRST, 2, Some(assigned(1)), vec![started(2)])],
    );
    // then
    assert_eq!(commands.acquiring, ids(&[1, 2]));
    assert_eq!(
        (answered.acquiring, answered.acquired),
        (ids(&[]), ids(&[1]))
    );
    assert_eq!(fact.acquiring, ids(&[1]));
}

/// Reassignment (§4.3): a job completed as canceled and offered again in the same batch is
/// reserved again, once; one offered and then completed in a batch is not acquired.
#[test]
fn should_reserve_a_requeued_job_once_when_its_completion_precedes_the_offer() {
    // given: 1 acquired
    let state = reach_from(
        active(),
        [
            message(FIRST, 1, Some(assigned(0)), vec![push(1)]),
            result(&[1], &[1]),
        ],
    );
    // when
    let (requeued, events) = apply(
        &state,
        auto(message(
            FIRST,
            2,
            Some(assigned(0)),
            vec![completed(1), push(1)],
        )),
    )
    .expect("ok");
    let (_, gone) = apply(
        &active(),
        auto(message(
            FIRST,
            1,
            Some(assigned(0)),
            vec![push(4), completed(4)],
        )),
    )
    .expect("ok");
    // then
    assert_eq!(recorded(&events).released, vec![RunnerRequestId(1)]);
    assert_eq!(
        (requeued.acquiring, requeued.acquired),
        (ids(&[1]), ids(&[]))
    );
    assert!(recorded(&gone).acquire.is_empty());
}

/// R7: unknown message types are counted; at the threshold the session is degraded, keeps
/// recording, and is never closed; the operator recovers it.
#[test]
fn should_degrade_the_session_when_unknown_types_reach_the_threshold() {
    // given
    let below = (PROTOCOL_DRIFT_THRESHOLD - 1) as usize;
    let state = reach_from(
        active(),
        [message(FIRST, 1, None, vec![MessageItem::Unknown; below])],
    );
    // when
    let (degraded, events) = apply(
        &state,
        auto(message(FIRST, 2, None, vec![MessageItem::Unknown])),
    )
    .expect("ok");
    let recovered = reach_from(degraded.clone(), [ScaleSetCommand::RecordSessionRecovered]);
    // then
    assert_eq!(state.phase, Phase::Active);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ScaleSetEvent::SessionDegraded(_)))
    );
    assert_eq!(degraded.session, Some(FIRST));
    assert_eq!((recovered.phase, recovered.drift), (Phase::Active, 0));
}

/// I18: a command under an older run epoch is refused.
#[test]
fn should_refuse_an_older_run_epoch_when_a_newer_one_was_accepted() {
    // given
    let mut path: Vec<_> = active_path().into_iter().map(auto).collect();
    path.push(at(2, capacity(2, 2, 2, WireMode::Open)));
    let state = path.into_iter().fold(
        mayfly_control_domain::scaleset::ScaleSetState::initial(),
        |s, step| apply(&s, step).expect("path").0,
    );
    // when
    let stale = outcome(&state, at(1, ScaleSetCommand::StartClosing));
    // then
    assert_eq!(stale, "stale_run");
}
