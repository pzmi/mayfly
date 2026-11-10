//! Starting, entering and ending sessions; frames the session does not execute.

use mayfly_data::{EnterError, FloorFailed, ProbeFailed, SessionEnd, StartError, Stats};
use mayfly_data_domain::LinkIgnored;
use mayfly_domain::{DirectiveId, Epoch, Millis};
use mayfly_protocol::{DirectiveKind, LinkError, ObservationKind, RefusalReason, UnknownReason};
use mayfly_testkit_data::{Op, StaticWelcome, directives, observations};

use crate::support::{
    EPOCH, Fakes, START, answers, answers_to, clone, config, directive, due, host, probe, run,
    session, welcome, welcome_at,
};

#[test]
fn should_raise_the_floor_before_entering_when_welcomed() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        let mut exec = fakes.start(2).await;
        let mut link = StaticWelcome::new(Ok(welcome(EPOCH)));

        // when
        let entered = exec.enter(&mut link, &fakes.floor).await;

        // then
        assert_eq!(entered, Ok(EPOCH));
        assert_eq!(fakes.floor.floor(), EPOCH);
        assert_eq!(exec.plane().session_epoch(), Some(EPOCH));
        let hellos = link.hellos();
        assert_eq!(hellos.len(), 1);
        assert_eq!(hellos[0].epoch_floor, Epoch::new(0));
        assert_eq!(hellos[0].host_id, host());
        assert_eq!(hellos[0].boot_id, config(1).boot_id);
    });
}

#[test]
fn should_enter_nothing_when_the_welcome_is_at_or_below_the_floor() {
    run(async {
        // given
        let mut fakes = Fakes::new(probe(4));
        fakes.floor = mayfly_testkit_data::MemoryFloor::new(Epoch::new(7));
        let mut exec = fakes.start(2).await;
        let mut link = StaticWelcome::new(Ok(welcome(EPOCH)));
        let (_feed, mut source) = directives(1);
        let (sink, _tap) = observations(1);

        // when
        let entered = exec.enter(&mut link, &fakes.floor).await;
        let served = exec.serve(&mut source, &sink).await;

        // then
        assert_eq!(
            entered,
            Err(EnterError::Ignored(LinkIgnored::AtOrBelowFloor))
        );
        assert_eq!(served, SessionEnd::NotOpen);
        assert_eq!(fakes.floor.floor(), Epoch::new(7));
    });
}

#[test]
fn should_enter_nothing_when_the_floor_cannot_be_made_durable() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        let mut exec = fakes.start(2).await;
        fakes.floor.fail(true);
        let mut link = StaticWelcome::new(Ok(welcome(EPOCH)));

        // when
        let entered = exec.enter(&mut link, &fakes.floor).await;

        // then
        assert_eq!(entered, Err(EnterError::Floor(FloorFailed)));
        assert_eq!(exec.plane().session_epoch(), None);
    });
}

#[test]
fn should_enter_nothing_when_the_handshake_fails() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        let mut exec = fakes.start(2).await;
        let mut link = StaticWelcome::new(Err(LinkError::Closed));

        // when
        let entered = exec.enter(&mut link, &fakes.floor).await;

        // then
        assert_eq!(entered, Err(EnterError::Link(LinkError::Closed)));
        assert_eq!(exec.plane().session_epoch(), None);
    });
}

#[test]
fn should_refuse_to_start_when_the_host_cannot_be_probed() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        fakes.host.fail(true);

        // when
        let started =
            crate::support::Exec::start(config(1), fakes.ports(), fakes.floor.floor()).await;

        // then
        assert!(matches!(started, Err(StartError::Probe(ProbeFailed))));
    });
}

#[test]
fn should_end_the_session_when_observations_cannot_be_sent() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        let mut exec = fakes.open(2).await;
        let (_feed, mut source) = directives(1);
        let (sink, tap) = observations(1);
        drop(tap);

        // when
        let served = exec.serve(&mut source, &sink).await;

        // then
        assert_eq!(served, SessionEnd::SinkFailed(LinkError::Closed));
    });
}

#[test]
fn should_finish_work_in_flight_and_answer_it_when_the_directive_source_closes() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        let mut exec = fakes.open(2).await;
        let held = fakes.provider.hold_next(Op::Clone);

        // when
        let (end, seen) = session(&mut exec, |feed, tap| async move {
            feed.send(directive(1, clone(1))).await;
            held.started().await;
            held.release();
            tap
        })
        .await;

        // then
        assert_eq!(end, SessionEnd::SourceClosed(LinkError::Closed));
        assert!(matches!(
            answers_to(&seen, 1).as_slice(),
            [ObservationKind::VmCloned { .. }]
        ));
        assert!(exec.ledger().vms.contains_key(&crate::support::vm(1)));
    });
}

#[test]
fn should_answer_unknown_or_discard_when_a_frame_is_not_of_the_session() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        let mut exec = fakes.open(2).await;
        let mut stale = directive(1, clone(1));
        stale.epoch = Epoch::new(4);
        let mut early = directive(2, clone(2));
        early.epoch = Epoch::new(6);

        // when
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            feed.send(stale).await;
            feed.send(early).await;
            feed.send(directive(3, DirectiveKind::ReportCapacity)).await;
            tap.wait_for(answers(3)).await;
            tap
        })
        .await;

        // then
        assert_eq!(
            answers_to(&seen, 1),
            [ObservationKind::OutcomeUnknown {
                reason: UnknownReason::StaleEpoch
            }]
        );
        assert!(answers_to(&seen, 2).is_empty());
        assert_eq!(
            exec.stats(),
            Stats {
                discarded_frames: 1,
                ..Stats::default()
            }
        );
        assert_eq!(fakes.provider.calls(Op::Clone), 0);
        assert!(seen.iter().all(|o| o.epoch == EPOCH));
        assert!(seen.iter().all(|o| {
            o.answers
                .is_none_or(|a| a.directive_id != DirectiveId::from_u128(2))
        }));
    });
}

#[test]
fn should_answer_cloned_without_cloning_again_when_a_clone_is_resent_in_a_new_session() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        let mut exec = fakes.open(2).await;
        let (_, first) = session(&mut exec, |feed, mut tap| async move {
            feed.send(directive(1, clone(1))).await;
            tap.wait_for(answers(1)).await;
            tap
        })
        .await;
        let mut link = StaticWelcome::new(Ok(welcome(Epoch::new(6))));
        let entered = exec.enter(&mut link, &fakes.floor).await;
        let mut resent = directive(1, clone(1));
        resent.epoch = Epoch::new(6);

        // when
        let (_, second) = session(&mut exec, |feed, mut tap| async move {
            feed.send(resent).await;
            tap.wait_for(answers(1)).await;
            tap
        })
        .await;

        // then
        assert_eq!(entered, Ok(Epoch::new(6)));
        let cloned = answers_to(&first, 1);
        assert!(matches!(
            cloned.as_slice(),
            [ObservationKind::VmCloned { .. }]
        ));
        assert_eq!(answers_to(&second, 1), cloned, "the observed instance id");
        assert_eq!(fakes.provider.calls(Op::Clone), 1);
    });
}

#[test]
fn should_answer_booted_without_starting_again_when_a_start_is_resent_in_a_new_session() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        fakes.provider.seed(
            &crate::support::vm(1).to_string(),
            crate::support::described("0a:00:00:00:01:01", false),
        );
        let mut exec = fakes.open(2).await;
        let start = DirectiveKind::StartVm {
            vm_name: crate::support::vm(1),
            run_options: crate::support::run_options(),
        };
        let first_start = directive(1, start.clone());
        let (_, first) = session(&mut exec, |feed, mut tap| async move {
            feed.send(first_start).await;
            tap.wait_for(answers(1)).await;
            tap
        })
        .await;
        let mut link = StaticWelcome::new(Ok(welcome(Epoch::new(6))));
        exec.enter(&mut link, &fakes.floor)
            .await
            .expect("the next session is entered");
        let mut resent = directive(1, start);
        resent.epoch = Epoch::new(6);

        // when
        let (_, second) = session(&mut exec, |feed, mut tap| async move {
            feed.send(resent).await;
            tap.wait_for(answers(1)).await;
            tap
        })
        .await;

        // then
        assert_eq!(answers_to(&first, 1), [ObservationKind::VmBooted]);
        assert_eq!(answers_to(&second, 1), [ObservationKind::VmBooted]);
        assert_eq!(fakes.provider.calls(Op::Start), 1);
    });
}

#[test]
fn should_serve_nothing_more_when_a_served_session_ended() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        let mut exec = fakes.open(2).await;
        let (ended, _) = session(&mut exec, |_feed, tap| async move { tap }).await;
        let (_feed, mut source) = directives(1);
        let (sink, _tap) = observations(4);

        // when
        let again = exec.serve(&mut source, &sink).await;

        // then
        assert_eq!(ended, SessionEnd::SourceClosed(LinkError::Closed));
        assert_eq!(again, SessionEnd::NotOpen);
        assert_eq!(exec.plane().session_epoch(), None);
    });
}

#[test]
fn should_keep_effects_running_when_the_sink_is_full() {
    run(async {
        // given: room for one observation in the sink
        let fakes = Fakes::new(probe(4));
        let mut exec = fakes.open(2).await;
        let held = fakes.provider.hold_next(Op::Clone);
        let (feed, mut source) = directives(1);
        let (sink, mut tap) = observations(1);

        // when: the clone runs, its reservation's report fills the sink, an answer waits for
        // room, and only then is the clone let go; the feed holds one directive, so the third
        // is taken only once the executor has the second
        let script = async move {
            tap.wait_for(|o| matches!(o.kind, ObservationKind::CapacityReported(_)))
                .await;
            feed.send(directive(1, clone(1))).await;
            held.started().await;
            feed.send(directive(2, DirectiveKind::ReportCapacity)).await;
            // room for this one means the executor took the last and waits on the sink
            feed.send(directive(3, DirectiveKind::ReportCapacity)).await;
            held.release();
            held.finished().await;
            let cloned = tap.wait_for(answers(1)).await;
            drop(feed);
            (tap, cloned)
        };
        let (end, (_tap, cloned)) = tokio::join!(exec.serve(&mut source, &sink), script);

        // then
        assert_eq!(end, SessionEnd::SourceClosed(LinkError::Closed));
        assert!(matches!(cloned.kind, ObservationKind::VmCloned { .. }));
    });
}

#[test]
fn should_compare_deadlines_by_the_wall_clock_at_receipt_when_an_older_control_plane_sends_no_clock()
 {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        let mut exec = fakes.open(2).await;
        let clock = fakes.clock.clone();
        let late = crate::support::due(
            1,
            DirectiveKind::ReportHostFacts,
            mayfly_domain::Millis::new(5_000),
        );

        // when
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            tap.wait_for(|o| matches!(o.kind, ObservationKind::CapacityReported(_)))
                .await;
            clock.set_wall(mayfly_domain::Millis::new(0));
            clock.advance(crate::support::OBSERVE);
            tap.wait_for(|o| {
                o.answers.is_none() && matches!(o.kind, ObservationKind::Inventory { .. })
            })
            .await;
            clock.set_wall(mayfly_domain::Millis::new(9_000));
            feed.send(late).await;
            tap.wait_for(answers(1)).await;
            tap
        })
        .await;

        // then
        assert_eq!(
            answers_to(&seen, 1),
            [ObservationKind::DirectiveRefused {
                reason: mayfly_protocol::RefusalReason::DeadlineExpired
            }],
            "without the welcome's clock a deadline is read by the host's wall clock at receipt"
        );
    });
}

#[test]
fn should_decide_no_deadline_differently_when_the_wall_clock_steps_after_the_handshake() {
    run(async {
        // given: the control plane's clock read the data plane's wall clock at the handshake
        let fakes = Fakes::new(probe(4));
        let mut exec = fakes.start(2).await;
        let mut link = StaticWelcome::new(Ok(welcome_at(EPOCH, START)));
        assert_eq!(exec.enter(&mut link, &fakes.floor).await, Ok(EPOCH));
        let clock = fakes.clock.clone();
        let deadline = Millis::new(START.get() + 5_000);
        let in_time = due(1, DirectiveKind::ReportHostFacts, deadline);
        let late = due(2, DirectiveKind::ReportHostFacts, deadline);

        // when: the wall clock steps past the deadline and back to zero, then monotonic time
        // passes the deadline
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            tap.wait_for(|o| matches!(o.kind, ObservationKind::CapacityReported(_)))
                .await;
            clock.set_wall(Millis::new(START.get() + 9_000));
            feed.send(in_time.clone()).await;
            tap.wait_for(answers(1)).await;
            clock.set_wall(Millis::new(0));
            clock.advance(Millis::new(6_000));
            feed.send(late).await;
            tap.wait_for(answers(2)).await;
            tap
        })
        .await;

        // then: only monotonic time since the handshake moved the control plane's clock
        assert!(
            matches!(answers_to(&seen, 1)[..], [ObservationKind::HostFacts(_)]),
            "a wall step forward expires nothing: {seen:?}"
        );
        assert_eq!(
            answers_to(&seen, 2),
            [ObservationKind::DirectiveRefused {
                reason: RefusalReason::DeadlineExpired
            }],
            "a wall step back keeps nothing alive"
        );
    });
}

#[test]
fn should_compare_deadlines_in_the_control_planes_clock_when_the_welcome_carries_it() {
    run(async {
        // given: the control plane's clock is 100 s ahead of this host's wall clock
        let fakes = Fakes::new(probe(4));
        let mut exec = fakes.start(2).await;
        let cp = Millis::new(START.get() + 100_000);
        let mut link = StaticWelcome::new(Ok(welcome_at(EPOCH, cp)));
        assert_eq!(exec.enter(&mut link, &fakes.floor).await, Ok(EPOCH));
        let past = due(
            1,
            DirectiveKind::ReportHostFacts,
            Millis::new(START.get() + 50_000),
        );
        let ahead = due(
            2,
            DirectiveKind::ReportHostFacts,
            Millis::new(cp.get() + 5_000),
        );

        // when
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            tap.wait_for(|o| matches!(o.kind, ObservationKind::CapacityReported(_)))
                .await;
            feed.send(past).await;
            feed.send(ahead).await;
            tap.wait_for(answers(2)).await;
            tap
        })
        .await;

        // then
        assert_eq!(
            answers_to(&seen, 1),
            [ObservationKind::DirectiveRefused {
                reason: RefusalReason::DeadlineExpired
            }],
            "a deadline the control plane's clock has passed is expired, whatever this host reads"
        );
        assert!(
            matches!(answers_to(&seen, 2)[..], [ObservationKind::HostFacts(_)]),
            "{seen:?}"
        );
    });
}
