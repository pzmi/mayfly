//! github.com's direct assignment (ADR 0090 with its 2026-11-12 amendment), each scenario of
//! `formal/scaleset.qnt` a test: an assignment waits for its run's lookup, a runner starts only
//! for a job classified trusted, a refused job holds nothing and goes to a refusal sink or, with
//! `cancel_refused_runs`, its run is cancelled, a new assignment is classified afresh, and a
//! vanished scale set is re-created with a fresh numbering.

use mayfly_control_domain::attempt::RunnerRequestId;
use mayfly_control_domain::demand::{EventName, RefusalReason, WireMode};
use mayfly_control_domain::registration::ScaleSetId;
use mayfly_control_domain::scaleset::{
    CancelAnswer, JobFact, LossCause, MessageItem, MessageOrigin, Phase, RefusalAction,
    ScaleSetCommand, ScaleSetEvent,
};

use super::support::{
    FIRST, SECOND, apply, assign, assigned, assigned_job, auto, capacity, completed_job, deleted,
    direct, ensure, failed_lookup, ids, key, lookup, loss, message, opened, outcome, policy, push,
    reach_from, result, started_job, trust,
};

fn refused_reason(events: &[ScaleSetEvent]) -> Option<RefusalReason> {
    events.iter().find_map(|e| match e {
        ScaleSetEvent::AssignedJobRefused(r) => Some(r.reason),
        _ => None,
    })
}

fn origin(events: &[ScaleSetEvent]) -> Option<MessageOrigin> {
    events.iter().find_map(|e| match e {
        ScaleSetEvent::MessageRecorded(m) => Some(m.origin),
        _ => None,
    })
}

fn start_sink(n: u64) -> ScaleSetCommand {
    ScaleSetCommand::StartSink { job: key(n) }
}

fn answer(n: u64, answer: CancelAnswer) -> ScaleSetCommand {
    ScaleSetCommand::RecordCancelAnswer {
        job: key(n),
        answer,
    }
}

/// assignedJobsClassified and vmOnlyForAJobClassifiedTrusted (`directAssignmentNeedsAdvertisedRoomTest`;
/// mutant RUN_ASSIGNED, which accepts an assignment at once, fails it): a `JobAssigned` is
/// pending until its run is looked up, and no runner starts meanwhile; once classified trusted
/// one runner may start for it, never two; its `JobStarted` releases it.
#[test]
fn should_start_a_runner_only_after_the_lookup_when_a_job_is_assigned_directly() {
    // given
    let state = direct(false);
    // when
    let pending = reach_from(
        state,
        [message(FIRST, 1, Some(assigned(1)), vec![assign(1)])],
    );
    let trusted = reach_from(pending.clone(), [lookup(1, true)]);
    let running = reach_from(
        trusted.clone(),
        [message(FIRST, 2, Some(assigned(1)), vec![started_job(1)])],
    );
    // then
    assert!(pending.pending.contains_key(&key(1)) && pending.trusted.is_empty());
    assert_eq!(pending.desired, 1);
    assert!(!pending.runner_may_start(0));
    assert!(trusted.pending.is_empty() && trusted.trusted.contains_key(&key(1)));
    assert!(trusted.runner_may_start(0) && !trusted.runner_may_start(1));
    assert!(running.trusted.is_empty() && !running.runner_may_start(0));
}

/// vmOnlyForAJobClassifiedTrusted (`refusedJobHoldsNothingAndTheHookFailsItTest`, GH-00S): a
/// pending or refused job backs no runner, and holds none back: a runner starts for the trusted
/// job beside a refused one (its hook fails the refused job if GitHub hands it that one).
#[test]
fn should_start_no_runner_when_an_assignment_is_pending_or_refused() {
    // given
    let state = reach_from(
        direct(false),
        [message(
            FIRST,
            1,
            Some(assigned(2)),
            vec![assign(3), assign(1)],
        )],
    );
    // when
    let refused = reach_from(state.clone(), [lookup(3, false)]);
    let beside = reach_from(refused.clone(), [lookup(1, true)]);
    // then
    assert!(!state.runner_may_start(0));
    assert!(!refused.runner_may_start(0));
    assert!(refused.refused.contains(&key(3)));
    assert!(beside.runner_may_start(0) && !beside.runner_may_start(1));
}

/// ADR 0090 (`failedLookupRefusesTest`): a lookup that failed refuses the job, trusted or not;
/// no runner starts for it, and a sink may take it.
#[test]
fn should_refuse_an_assignment_when_its_lookup_fails() {
    // given
    let state = reach_from(
        direct(false),
        [message(FIRST, 1, Some(assigned(1)), vec![assign(1)])],
    );
    // when
    let (after, events) = apply(&state, auto(failed_lookup(1))).expect("recorded");
    // then
    assert_eq!(refused_reason(&events), Some(RefusalReason::LookupFailed));
    assert!(after.refused.contains(&key(1)) && after.trusted.is_empty());
    assert!(!after.runner_may_start(0));
    assert_eq!(
        after.refusal_action(),
        Some(RefusalAction::StartSink(key(1)))
    );
    assert_eq!(outcome(&after, auto(lookup(1, true))), "not_pending");
}

/// `reassignedJobIsClassifiedAfreshTest`: a job released by a `JobCompleted` that names no job
/// keeps its refusal; assigned again, it is pending afresh (its earlier refusal does not carry
/// over), so its trusted run is never cancelled.
#[test]
fn should_classify_a_reassigned_job_afresh_when_it_is_assigned_again() {
    // given: job 1's lookup failed, with the cancel option on
    let state = reach_from(
        direct(true),
        [
            message(FIRST, 1, Some(assigned(1)), vec![assign(1)]),
            failed_lookup(1),
        ],
    );
    // when
    let unnamed = reach_from(
        state.clone(),
        [message(
            FIRST,
            2,
            Some(assigned(0)),
            vec![completed_job(1, false)],
        )],
    );
    let again = reach_from(
        unnamed.clone(),
        [message(FIRST, 3, Some(assigned(1)), vec![assign(1)])],
    );
    let trusted = reach_from(again.clone(), [lookup(1, true)]);
    // then
    assert!(unnamed.refused.contains(&key(1)));
    assert!(again.pending.contains_key(&key(1)) && again.refused.is_empty());
    assert_eq!(again.refusal_action(), None);
    assert!(trusted.trusted.contains_key(&key(1)) && trusted.refused.is_empty());
    assert_eq!(
        outcome(&trusted, auto(answer(1, CancelAnswer::Accepted))),
        "cancel_not_due"
    );
}

/// sinkOnlyForARefusedJob (`refusalSinkFailsTheRefusedJobTest`, `noSinkBesideATrustedJobTest`):
/// no sink while a trusted or pending assignment is known; then one sink for the refused job, the
/// wire max pinned at the assigned count while it is up, never a second, and the pin lifted when
/// it ends.
#[test]
fn should_start_a_sink_only_when_no_trusted_or_pending_job_is_known() {
    // given: job 3 refused beside job 1, still pending
    let state = reach_from(
        direct(false),
        [
            message(FIRST, 1, Some(assigned(2)), vec![assign(3), assign(1)]),
            lookup(3, false),
        ],
    );
    let beside_trusted = reach_from(state.clone(), [lookup(1, true)]);
    let alone = reach_from(
        beside_trusted.clone(),
        [message(FIRST, 2, Some(assigned(1)), vec![started_job(1)])],
    );
    // when
    let (up, events) = apply(&alone, auto(start_sink(3))).expect("sink started");
    let taken = reach_from(
        up.clone(),
        [message(
            FIRST,
            3,
            Some(assigned(1)),
            vec![completed_job(3, true)],
        )],
    );
    let ended = reach_from(taken.clone(), [ScaleSetCommand::RecordSinkEnded]);
    // then
    assert_eq!(outcome(&state, auto(start_sink(3))), "sink_not_due");
    assert_eq!(
        outcome(&beside_trusted, auto(start_sink(3))),
        "sink_not_due"
    );
    assert_eq!(state.refusal_action(), None);
    assert_eq!(
        alone.refusal_action(),
        Some(RefusalAction::StartSink(key(3)))
    );
    assert!(matches!(events.as_slice(), [ScaleSetEvent::SinkStarted(s)] if s.job == key(3)));
    assert_eq!((alone.wire_max(), up.wire_max()), (2, 1));
    assert_eq!(outcome(&up, auto(start_sink(3))), "sink_not_due");
    assert_eq!(up.refusal_action(), None);
    assert!(taken.refused.is_empty() && taken.sink_tried.is_empty());
    assert_eq!(ended.wire_max(), 2);
    assert_eq!(
        outcome(&ended, auto(ScaleSetCommand::RecordSinkEnded)),
        "no_sink"
    );
}

/// sinkOnlyForARefusedJob, held: no sink while the pool is not open or the session not active.
#[test]
fn should_start_no_sink_when_the_pool_is_held() {
    // given
    let refused = reach_from(
        direct(false),
        [
            message(FIRST, 1, Some(assigned(1)), vec![assign(3)]),
            lookup(3, false),
        ],
    );
    // when
    let held: Vec<String> = [WireMode::Paused, WireMode::Draining, WireMode::SafeMode]
        .into_iter()
        .map(|mode| reach_from(refused.clone(), [capacity(2, 2, 1, mode)]))
        .map(|state| outcome(&state, auto(start_sink(3))))
        .collect();
    // then
    assert_eq!(held, vec!["sink_not_due"; 3]);
    assert_eq!(outcome(&refused, auto(start_sink(3))), "accepted");
}

/// cancelOnlyRefused (`cancelledRefusedRunTest`, `failedCancelFallsBackToGitHubsReleaseTest`;
/// mutant CANCEL_ANY, which cancels any assigned job's run, option or not, fails it): with
/// `cancel_refused_runs` a refused run is cancelled, once, and no sink starts; a trusted job's
/// run is never cancelled; without the option nothing is cancelled.
#[test]
fn should_cancel_only_a_refused_run_when_the_option_is_on() {
    // given
    let assigned_two = message(FIRST, 1, Some(assigned(2)), vec![assign(3), assign(1)]);
    let with_option = reach_from(
        direct(true),
        [assigned_two.clone(), lookup(3, false), lookup(1, true)],
    );
    let without = reach_from(
        direct(false),
        [assigned_two, lookup(3, false), lookup(1, true)],
    );
    // when
    let (answered, events) =
        apply(&with_option, auto(answer(3, CancelAnswer::Refused))).expect("recorded");
    // then
    assert_eq!(
        with_option.refusal_action(),
        Some(RefusalAction::CancelRun(key(3)))
    );
    assert_eq!(outcome(&with_option, auto(start_sink(3))), "sink_not_due");
    assert!(with_option.runner_may_start(0));
    assert!(matches!(events.as_slice(), [ScaleSetEvent::CancelAnswered(c)] if c.job == key(3)));
    assert_eq!(answered.refusal_action(), None);
    assert_eq!(
        outcome(&answered, auto(answer(3, CancelAnswer::Accepted))),
        "cancel_not_due"
    );
    assert_eq!(
        outcome(&with_option, auto(answer(1, CancelAnswer::Accepted))),
        "cancel_not_due"
    );
    assert_ne!(
        without.refusal_action(),
        Some(RefusalAction::CancelRun(key(3)))
    );
    assert_eq!(
        outcome(&without, auto(answer(3, CancelAnswer::Accepted))),
        "cancel_not_due"
    );
}

/// `dormantScaleSetIsRecreatedTest`: a create answered 404 for the journaled scale set returns
/// the stream to `Unconfigured`; it is re-created under a new id, the numbering starts afresh
/// (cursor and floor reset) and the old set's reservations, classifications and refusals are
/// dropped; the next session's message 1 applies as new.
#[test]
fn should_reset_the_numbering_and_drop_reservations_when_the_scale_set_is_recreated() {
    // given: offers 1 and 2 acquired, job 3 refused, cursor 2; the session lost and deleted
    let state = reach_from(
        direct(false),
        [
            message(FIRST, 1, Some(assigned(0)), vec![push(1), push(2)]),
            result(&[1, 2], &[1, 2]),
            message(FIRST, 2, Some(assigned(3)), vec![assign(3)]),
            lookup(3, false),
            loss(LossCause::Restarted),
            deleted(FIRST),
            ScaleSetCommand::BeginOpen,
        ],
    );
    // when
    let vanished = reach_from(
        state,
        [ScaleSetCommand::RecordScaleSetVanished {
            github_id: ScaleSetId(7),
        }],
    );
    let recreated = reach_from(
        vanished.clone(),
        [ScaleSetCommand::RecordScaleSetRecreated {
            github_id: ScaleSetId(9),
        }],
    );
    let reopened = reach_from(
        recreated.clone(),
        [ScaleSetCommand::BeginOpen, opened(SECOND, assigned(0))],
    );
    let (after, events) = apply(
        &reopened,
        auto(message(
            SECOND,
            1,
            Some(assigned(0)),
            vec![push(1), push(2)],
        )),
    )
    .expect("recorded");
    // then
    assert_eq!(vanished.phase, Phase::Unconfigured);
    assert_eq!(outcome(&vanished, auto(ensure())), "scale_set_vanished");
    assert_eq!(
        outcome(&vanished, auto(ScaleSetCommand::BeginOpen)),
        "not_ensured"
    );
    assert_eq!(recreated.phase, Phase::Ensured);
    assert_eq!(recreated.github_id, Some(ScaleSetId(9)));
    assert_eq!((recreated.cursor, recreated.session_floor), (None, None));
    assert_eq!((recreated.reserved(), recreated.desired), (ids(&[]), 0));
    assert!(recreated.pending.is_empty() && recreated.refused.is_empty());
    assert_eq!(reopened.session_floor, None);
    assert_eq!(origin(&events), Some(MessageOrigin::Fresh));
    assert_eq!(after.acquiring, ids(&[1, 2]));
}

/// A vanished scale set is observed only without a live session, for the journaled id, after
/// the stale session's delete; it is re-created only under a new id.
#[test]
fn should_refuse_a_vanished_observation_when_a_session_is_live_or_the_id_differs() {
    // given
    let live = direct(false);
    let stale = reach_from(direct(false), [loss(LossCause::Expired)]);
    let backoff = reach_from(stale.clone(), [deleted(FIRST)]);
    let gone = |id: u64| {
        auto(ScaleSetCommand::RecordScaleSetVanished {
            github_id: ScaleSetId(id),
        })
    };
    let vanished = reach_from(
        backoff.clone(),
        [ScaleSetCommand::RecordScaleSetVanished {
            github_id: ScaleSetId(7),
        }],
    );
    // when
    let outcomes = [
        outcome(&live, gone(7)),
        outcome(&stale, gone(7)),
        outcome(&backoff, gone(8)),
        outcome(&vanished, gone(7)),
        outcome(
            &vanished,
            auto(ScaleSetCommand::RecordScaleSetRecreated {
                github_id: ScaleSetId(7),
            }),
        ),
        outcome(
            &backoff,
            auto(ScaleSetCommand::RecordScaleSetRecreated {
                github_id: ScaleSetId(9),
            }),
        ),
    ];
    // then
    assert_eq!(
        outcomes,
        [
            "session_live",
            "stale_session_pending",
            "scale_set_mismatch",
            "accepted",
            "scale_set_mismatch",
            "not_vanished",
        ]
    );
}

/// vmOnlyForAJobClassifiedTrusted on GHES (the model's union `acquired ∪ trusted`): an offer
/// planned, its acquire answer lost, then GitHub's `JobAssigned` for it (pending, looked up,
/// trusted), then the acquire's answer names it acquired. It is one job: it backs one runner,
/// never two.
#[test]
fn should_count_a_job_once_when_an_acquire_answer_names_an_assignment_classified_trusted() {
    // given: offer 5 planned (acquiring)
    let planned = reach_from(
        direct(false),
        [message(FIRST, 1, Some(assigned(0)), vec![push(5)])],
    );
    let assignment = MessageItem::Fact(JobFact::Assigned {
        request_id: RunnerRequestId(5),
        job: Some(assigned_job(5, EventName::Push)),
    });
    let trusted = reach_from(
        planned,
        [
            message(FIRST, 2, Some(assigned(1)), vec![assignment]),
            lookup(5, true),
        ],
    );
    // when
    let acquired = reach_from(trusted.clone(), [result(&[5], &[5])]);
    // then
    assert!(trusted.trusted.contains_key(&key(5)) && trusted.acquiring == ids(&[5]));
    assert_eq!(acquired.acquired, ids(&[5]));
    assert!(acquired.trusted.is_empty() && acquired.pending.is_empty());
    assert_eq!(acquired.waiting(), 1);
    assert!(acquired.runner_may_start(0) && !acquired.runner_may_start(1));
}

/// A changed trust policy (KER-06G's own rule, fail closed; the model's `TRUST` is fixed): every
/// trusted job goes back to pending and backs no runner until it is looked up again, so a
/// revoked repository stops backing runners; the same policy again changes nothing.
#[test]
fn should_classify_trusted_jobs_again_when_the_trust_policy_changes() {
    // given: job 1 trusted
    let trusted = reach_from(
        direct(false),
        [
            message(FIRST, 1, Some(assigned(1)), vec![assign(1)]),
            lookup(1, true),
        ],
    );
    let mut revoked = trust(false);
    revoked.allowed_repositories.clear();
    // when
    let same = reach_from(trusted.clone(), [policy(false)]);
    let changed = reach_from(
        trusted.clone(),
        [ScaleSetCommand::ApplyTrustPolicy(revoked)],
    );
    let (_, events) = apply(&changed, auto(lookup(1, true))).expect("looked up again");
    // then
    assert_eq!(same, trusted);
    assert!(changed.trusted.is_empty() && changed.pending.contains_key(&key(1)));
    assert!(!changed.runner_may_start(0));
    assert_eq!(
        refused_reason(&events),
        Some(RefusalReason::RepositoryNotAllowed)
    );
}
