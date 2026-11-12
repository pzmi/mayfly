//! The rules of `doc/domain-model.md` §6.1 and plan §7.2, each a test: record before ack (I12),
//! one reservation per request id, desired from the latest statistics, `PlanAcquire` within
//! deliverable (I23, mutant 19) and the tier (I32), the wire behaviour of pause, drain and safe
//! mode (ADR 0029), the session's journaled deletes (A3), pending acquisitions left only on an
//! observation (A4), protocol drift (R7) and the epoch fence (I18).

use mayfly_control_domain::attempt::RunnerRequestId;
use mayfly_control_domain::demand::{DeclineReason, EventName, WireMode};
use mayfly_control_domain::scaleset::{
    DegradedCause, LossCause, MessageId, MessageItem, MessageOrigin, PROTOCOL_DRIFT_THRESHOLD,
    Phase, SESSION_CONFLICT_ESCALATION, ScaleSetCommand, ScaleSetEvent, ScaleSetState,
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
/// its offers are not reserved a second time.
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
    assert_eq!(m.origin, MessageOrigin::Redelivered);
    assert!(m.acquire.is_empty());
    assert_eq!(after.acquiring, ids(&[1, 2]));
}

/// The state after message 1 (`items`) was recorded in the first session, `between` happened,
/// and a restart opened the second session with `opening` assigned.
fn reopened_after(
    items: Vec<MessageItem>,
    between: &[ScaleSetCommand],
    opening: u32,
) -> ScaleSetState {
    let mut path = vec![message(FIRST, 1, Some(assigned(0)), items)];
    path.extend_from_slice(between);
    path.extend([
        loss(LossCause::Restarted),
        deleted(FIRST),
        ScaleSetCommand::BeginOpen,
        opened(SECOND, assigned(opening)),
    ]);
    reach_from(active(), path)
}

/// MED5 of the KER-06 review (A1, A4): offer 1 was planned in the first session and its acquire
/// answer released it (not acquired); the message, its ack lost, is redelivered in the second
/// session. It is recorded and applies nothing: offer 1 is not planned again for a job the
/// stream already handled, and its older statistics stay unapplied.
#[test]
fn should_apply_nothing_when_a_message_from_an_earlier_session_is_redelivered() {
    // given
    let state = reopened_after(vec![push(1)], &[result(&[1], &[])], 1);
    // when
    let (after, events) = apply(
        &state,
        auto(message(SECOND, 1, Some(assigned(0)), vec![push(1)])),
    )
    .expect("recorded");
    // then
    let m = recorded(&events);
    assert_eq!(m.origin, MessageOrigin::Redelivered);
    assert!(m.acquire.is_empty() && m.declined.is_empty() && !m.statistics_applied);
    assert_eq!((after.reserved(), after.desired), (ids(&[]), 1));
    assert_eq!(after.phase, Phase::Active);
}

/// LOW10 of the KER-06 review: a redelivered message's unknown types were counted when it was
/// first recorded; they are not counted into the new session's drift again.
#[test]
fn should_count_no_drift_again_when_a_redelivered_message_carries_unknown_types() {
    // given
    let unknown = vec![MessageItem::Unknown; (PROTOCOL_DRIFT_THRESHOLD - 1) as usize];
    let state = reopened_after(unknown.clone(), &[], 0);
    // when
    let (after, events) =
        apply(&state, auto(message(SECOND, 1, Some(assigned(0)), unknown))).expect("recorded");
    // then
    assert_eq!(recorded(&events).unknown_types, 0);
    assert_eq!((after.drift, after.phase), (0, Phase::Active));
}

/// a1RefutationIsFlagged, the renumbered branch (A1 refuted, recovered: `formal/scaleset.qnt`'s
/// `a1RestartIsRecoveredTest`): the second session's first message has an id below the floor
/// the first session left. Redelivery is oldest first, so it cannot be one: GitHub restarted its
/// numbering. `MessageIdsRestarted` is recorded for the operator, the message applies as new
/// (statistics, offers) and the cursor and floor restart from it; the session goes on, and the
/// next message of the new numbering applies too.
#[test]
fn should_restart_the_numbering_when_a_new_session_delivers_an_id_below_the_floor() {
    // given: messages 1 and 3 recorded in the first session, so the second's floor is 3
    let state = reopened_after(vec![], &[message(FIRST, 3, Some(assigned(0)), vec![])], 0);
    // when
    let (restarted, events) = apply(
        &state,
        auto(message(SECOND, 1, Some(assigned(1)), vec![push(7)])),
    )
    .expect("recorded");
    let (later, more) = apply(
        &restarted,
        auto(message(SECOND, 2, Some(assigned(1)), vec![push(8)])),
    )
    .expect("recorded");
    // then
    assert!(matches!(
        events.first(),
        Some(ScaleSetEvent::MessageIdsRestarted(r)) if r.message_id.0 == 1 && r.floor.0 == 3
    ));
    let m = recorded(&events);
    assert_eq!(m.origin, MessageOrigin::Fresh);
    assert!(m.statistics_applied);
    assert_eq!(m.acquire, vec![RunnerRequestId(7)]);
    assert_eq!(restarted.phase, Phase::Active);
    assert!(restarted.renumbered);
    assert_eq!(
        (restarted.cursor.map(|c| c.0), restarted.session_floor),
        (Some(1), None)
    );
    assert_eq!(recorded(&more).origin, MessageOrigin::Fresh);
    assert_eq!(later.cursor.map(|c| c.0), Some(2));
}

/// a1RefutationIsFlagged, the degrading branch (`a1RefutedMidSessionDegradesTest`,
/// `restartWithinASessionDegradesTest`): once the session recorded a message, an id at or below
/// the cursor cannot start a numbering; it applies nothing and degrades the session, and the
/// next one keeps it so. A reused id is never taken for a duplicate unless it is the cursor's
/// (`ScaleSetState::is_duplicate`).
#[test]
fn should_degrade_the_session_when_an_id_at_or_below_the_cursor_comes_later_in_a_session() {
    // given: the second session (floor 3) recorded message 4
    let state = reopened_after(vec![], &[message(FIRST, 3, Some(assigned(0)), vec![])], 0);
    let state = reach_from(state, [message(SECOND, 4, Some(assigned(0)), vec![])]);
    // when
    let (degraded, events) = apply(
        &state,
        auto(message(SECOND, 1, Some(assigned(1)), vec![push(7)])),
    )
    .expect("recorded");
    let (still, later) = apply(
        &degraded,
        auto(message(SECOND, 2, Some(assigned(1)), vec![push(8)])),
    )
    .expect("recorded");
    // then
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ScaleSetEvent::MessageIdsRestarted(_)))
    );
    let m = recorded(&events);
    assert_eq!(m.origin, MessageOrigin::IdRegressed);
    assert!(m.acquire.is_empty() && !m.statistics_applied);
    assert_eq!(
        degraded.phase,
        Phase::Degraded(DegradedCause::MessageIdsRegressed)
    );
    assert_eq!((degraded.desired, degraded.reserved()), (0, ids(&[])));
    assert_eq!(degraded.cursor.map(|c| c.0), Some(4));
    assert!(!degraded.renumbered);
    assert_eq!(recorded(&later).origin, MessageOrigin::IdRegressed);
    assert_eq!(still.phase, degraded.phase);
    assert!(state.is_duplicate(MessageId(4)) && !state.is_duplicate(MessageId(1)));
}

/// A6 (MED1 of the KER-06 review): message 2 was emitted before the restart, when nothing was
/// assigned, but recorded only after the second session opened with the two acquired jobs
/// assigned. No id tells it from a newer message, so its older statistics apply; the budget
/// still counts both acquisitions as reservations (A2), so the offer it carries is declined and
/// nothing is acquired past deliverable (I23).
#[test]
fn should_stay_within_deliverable_when_a_message_emitted_before_the_open_is_recorded_after_it() {
    // given: 1 and 2 acquired in the first session; deliverable 2
    let state = reopened_after(vec![push(1), push(2)], &[result(&[1, 2], &[1, 2])], 2);
    // when
    let (after, events) = apply(
        &state,
        auto(message(SECOND, 2, Some(assigned(0)), vec![push(3)])),
    )
    .expect("recorded");
    // then
    let m = recorded(&events);
    assert!(m.statistics_applied);
    assert_eq!(after.desired, 0);
    assert!(m.acquire.is_empty());
    assert_eq!(m.declined[0].reason, DeclineReason::NoCapacity);
}

/// desiredFromLatestStatistics (A1): an older message's statistics never replace newer ones;
/// within a session such a message is a reused id and degrades it besides.
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
    assert_eq!(
        after.phase,
        Phase::Degraded(DegradedCause::MessageIdsRegressed)
    );
}

/// A message without statistics (an unknown envelope) moves the cursor; the next message that
/// carries statistics applies them.
#[test]
fn should_apply_statistics_when_only_a_newer_message_without_statistics_was_recorded() {
    // given: message 3 carried no statistics
    let state = reach_from(
        active(),
        [message(FIRST, 3, None, vec![MessageItem::Unknown])],
    );
    // when
    let (after, events) =
        apply(&state, auto(message(FIRST, 4, Some(assigned(1)), vec![]))).expect("recorded");
    // then
    assert!(recorded(&events).statistics_applied);
    assert_eq!((after.desired, after.cursor.map(|c| c.0)), (1, Some(4)));
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
    let m = recorded(&events);
    assert_eq!(m.acquire, vec![RunnerRequestId(3)]);
    assert_eq!(
        m.declined.iter().map(|d| d.reason).collect::<Vec<_>>(),
        vec![DeclineReason::Tier, DeclineReason::Unclassifiable]
    );
    assert_eq!(after.reserved(), ids(&[3]));
}

/// LOW6 of the KER-06 review (P21): a degraded session freezes minting and a closing one ends;
/// both still record, release and apply statistics, but plan no offer (declined as held).
#[test]
fn should_plan_no_offer_when_the_session_is_degraded_or_closing() {
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
    for state in [degraded, closing] {
        // when
        let (after, events) = apply(
            &state,
            auto(message(FIRST, 2, Some(assigned(0)), vec![push(1)])),
        )
        .expect("recorded");
        // then
        let m = recorded(&events);
        assert!(m.acquire.is_empty(), "{:?}", state.phase);
        assert_eq!(m.declined[0].reason, DeclineReason::Held);
        assert_eq!(after.reserved(), ids(&[]));
    }
}

/// MED3 of the KER-06 review (A3): a lost create left a session mayfly has no record of, and
/// every create answers 409. The `SESSION_CONFLICT_ESCALATION`th in a row is escalated, once;
/// the creates go on; a session that opens resets the count.
#[test]
fn should_escalate_once_when_creates_keep_answering_conflict() {
    // given
    let mut state = reach([super::support::ensure(), ScaleSetCommand::BeginOpen]);
    let mut escalations = Vec::new();
    // when
    for _ in 0..=SESSION_CONFLICT_ESCALATION {
        let (lost, events) = apply(&state, auto(loss(LossCause::Conflicted))).expect("lost");
        escalations.push(
            events
                .iter()
                .any(|e| matches!(e, ScaleSetEvent::SessionConflictEscalated(_))),
        );
        state = reach_from(lost, [ScaleSetCommand::BeginOpen]);
    }
    let reopened = reach_from(state.clone(), [opened(FIRST, assigned(0))]);
    // then
    let at = (SESSION_CONFLICT_ESCALATION - 1) as usize;
    assert_eq!(
        escalations.iter().filter(|e| **e).count(),
        1,
        "{escalations:?}"
    );
    assert!(escalations[at]);
    assert!(state.conflict_escalated());
    assert!(!reopened.conflict_escalated());
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
