//! The directive loop across both planes (CP-02, component level): the attempt saga and the
//! directive relay as durable consumers on the control plane's store, the in-process link, and
//! the data plane's executor over the fake provider. Each test is one `Accept:` clause of
//! CP-02: a lost reply is `Unknown`, never a retry under a new name; a resend carries the same
//! directive id; a relay killed between its send and its advance resends that id; a late
//! directive 5 after 6 neither undoes 6 nor runs twice; safe mode holds create and destroy
//! kinds. Composition tests, because only the `both` crates may see both planes (D1).

mod support;

use mayfly_control::delivery::DrainReport;
use mayfly_control::delivery::{Consumer as _, ConsumerName, Delivery, DeliveryKey, LeaseFence};
use mayfly_control::journal::{Journal as _, StreamId};
use mayfly_control::saga::SagaConfig;
use mayfly_control::saga::TimeoutReport;
use mayfly_control::store::Clock as _;
use mayfly_control_domain::attempt::{
    AttemptEvent, AttemptState, DirectiveIssued, Outcome, Phase, Staleness,
};
use mayfly_data::SessionEnd;
use mayfly_domain::{AttemptId, Deadline, Generation, HostId, Millis, Sample};
use mayfly_protocol::{DirectiveTag, LinkError, Mode, ObservationKind};
use mayfly_testkit::attempts::instance;
use mayfly_testkit_data::Op;

use support::{Backend, Cp, Dp, NeverFires, Tapped, connect, memory, run, sqlite};

fn closed() -> SessionEnd {
    SessionEnd::SourceClosed(LinkError::Closed)
}

fn stream(n: u128) -> StreamId {
    StreamId::new(&format!("attempt-{}", instance(n))).unwrap_or_else(|e| panic!("{e}"))
}

/// Past the deadline of a clone the saga issued at the start, and its reply grace.
fn past_clone_deadline() -> Millis {
    let config = SagaConfig::default();
    Millis::new(config.timeouts.clone.get() + config.reply_grace.get())
}

/// A session and attempt 1 started on `backend`; the clone is issued, sent and executed, its
/// reply is lost (never recorded), and the deadline passes.
async fn lose_the_clone_reply<B: Backend>(backend: B) -> LostReply {
    let cp = Cp::new(backend);
    let mut dp = Dp::start().await;
    let provider = dp.provider.clone();
    let (mut cp_link, mut dp_link) = connect(&cp, &mut dp, Mode::Normal).await;
    let saga = cp.saga();
    let saga_worker = cp.worker(NeverFires);
    let relay = cp.relay(cp_link.directives.clone(), NeverFires);
    let relay_worker = cp.worker(NeverFires);
    cp.start(1).await;

    let script = async {
        saga_worker
            .catch_up(&saga)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        relay_worker
            .catch_up(&relay)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let lost = cp_link.next_answer().await;
        // the relay recorded `DirectiveSent`: the saga arms the deadline
        saga_worker
            .catch_up(&saga)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        cp.clock.advance(past_clone_deadline());
        let timeouts = saga.expire_due().await;
        let after = saga_worker
            .catch_up(&saga)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        relay_worker
            .catch_up(&relay)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        drop((relay, cp_link));
        (lost, timeouts, after)
    };
    let (end, (lost, timeouts, after)) = tokio::join!(
        dp.exec
            .serve(&mut dp_link.directives, &dp_link.observations),
        script
    );

    let state = cp.state(1).await;
    LostReply {
        end,
        lost: lost.kind,
        directives: cp.directives(1).await,
        timeouts,
        after,
        state,
        clones: provider.calls(Op::Clone),
        names: provider.vms().into_keys().collect(),
    }
}

/// What [`lose_the_clone_reply`] left.
struct LostReply {
    end: SessionEnd,
    lost: ObservationKind,
    directives: Vec<DirectiveIssued>,
    timeouts: TimeoutReport,
    after: DrainReport,
    state: AttemptState,
    clones: usize,
    names: Vec<String>,
}

/// The effect happened, but the attempt records `Unknown`, not a clone, and issues nothing
/// more: no second directive, no second name.
fn assert_unknown_and_not_retried(seen: &LostReply) {
    assert_eq!(seen.end, closed());
    assert!(matches!(seen.lost, ObservationKind::VmCloned { .. }));
    assert_eq!(seen.directives.len(), 1);
    assert_eq!(seen.timeouts.unknown, vec![seen.directives[0].directive_id]);
    assert!(seen.after.failed.is_empty(), "{:?}", seen.after);
    assert!(matches!(
        seen.state.pending.map(|p| p.outcome),
        Some(Outcome::Unknown { .. })
    ));
    assert_eq!(seen.state.phase, Phase::Cloning);
    assert_eq!(seen.state.instance_id, None);
    assert_eq!(seen.clones, 1);
    let identity = seen
        .state
        .identity
        .clone()
        .unwrap_or_else(|| panic!("started"));
    assert_eq!(seen.names, [identity.vm_name.to_string()]);
}

#[test]
fn should_record_unknown_not_retry_with_a_new_name_when_the_reply_is_lost() {
    // given
    let backend = memory();

    // when
    let seen = run(lose_the_clone_reply(backend));

    // then
    assert_unknown_and_not_retried(&seen);
}

#[test]
fn should_record_unknown_not_retry_with_a_new_name_when_the_reply_is_lost_on_sqlite() {
    // given
    let backend = sqlite();

    // when
    let seen = run(lose_the_clone_reply(backend));

    // then
    assert_unknown_and_not_retried(&seen);
}

#[test]
fn should_send_the_same_directive_id_when_the_relay_delivers_it_again() {
    run(async {
        // given: the clone issued
        let cp = Cp::new(memory());
        let mut dp = Dp::start().await;
        let provider = dp.provider.clone();
        let (mut cp_link, mut dp_link) = connect(&cp, &mut dp, Mode::Normal).await;
        let saga = cp.saga();
        let tap = Tapped::new(cp_link.directives.clone());
        let log = tap.log();
        let relay = cp.relay(tap, NeverFires);

        // when: the relay is handed the `DirectiveIssued` twice, as a redelivery would
        let script = async {
            cp.start(1).await;
            cp.worker(NeverFires)
                .catch_up(&saga)
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            let recorded = cp
                .store
                .journal()
                .read_stream(&stream(1), None, None)
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            let issued = recorded
                .iter()
                .find(|e| e.kind.event_type == "directive_issued")
                .unwrap_or_else(|| panic!("a directive"));
            let key = DeliveryKey {
                consumer: ConsumerName::new("directive_relay").unwrap_or_else(|e| panic!("{e}")),
                event_id: issued.event_id,
            };
            let mut answers = Vec::new();
            for fence in [1, 2] {
                relay
                    .handle(Delivery {
                        event: issued,
                        fence: LeaseFence::new(fence),
                        key: &key,
                    })
                    .await
                    .unwrap_or_else(|e| panic!("{e}"));
                answers.push(cp_link.next_answer().await);
            }
            drop((relay, cp_link));
            answers
        };
        let (end, answers) = tokio::join!(
            dp.exec
                .serve(&mut dp_link.directives, &dp_link.observations),
            script
        );

        // then: two frames, one directive id, one clone, the same answer twice
        assert_eq!(end, closed());
        let frames = log.sent();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], frames[1]);
        assert_eq!(
            frames[0].directive_id,
            cp.directives(1).await[0].directive_id
        );
        assert_eq!(provider.calls(Op::Clone), 1);
        assert_eq!(answers[0].kind, answers[1].kind);
        assert_eq!(answers[0].answers, answers[1].answers);
    });
}

#[test]
fn should_resend_the_same_directive_id_when_the_relay_is_killed_between_send_and_advance() {
    run(async {
        // given: the clone issued; relay A puts it on the link and is killed before it
        // advances
        let cp = Cp::new(memory());
        let mut dp = Dp::start().await;
        let provider = dp.provider.clone();
        let (mut cp_link, mut dp_link) = connect(&cp, &mut dp, Mode::Normal).await;
        let saga = cp.saga();
        cp.start(1).await;
        let tap_a = Tapped {
            hold_after_send: true,
            ..Tapped::new(cp_link.directives.clone())
        };
        let tap_b = Tapped::new(cp_link.directives.clone());
        let (log_a, log_b) = (tap_a.log(), tap_b.log());

        // when
        let script = async {
            cp.worker(NeverFires)
                .catch_up(&saga)
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            let relay_a = cp.relay(tap_a, NeverFires);
            let worker_a = cp.worker(NeverFires);
            let first = tokio::select! {
                _ = worker_a.catch_up(&relay_a) => panic!("relay A was not held after its send"),
                answer = cp_link.next_answer() => answer,
            };
            // relay A is gone with its future; its lease runs out by the store clock
            cp.store_clock
                .advance(Millis::new(cp.config.lease_ttl.get() + 1));
            let relay_b = cp.relay(tap_b, NeverFires);
            let report = cp
                .worker(NeverFires)
                .catch_up(&relay_b)
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            let second = cp_link.next_answer().await;
            cp.record(1, &second).await;
            drop((relay_a, relay_b, cp_link));
            (first, second, report)
        };
        let (end, (first, second, report)) = tokio::join!(
            dp.exec
                .serve(&mut dp_link.directives, &dp_link.observations),
            script
        );

        // then: B resent A's frame under the one directive id; the data plane cloned once and
        // answered the repeat with the recorded outcome
        assert_eq!(end, closed());
        let (a, b) = (log_a.sent(), log_b.sent());
        assert_eq!((a.len(), b.len()), (1, 1));
        assert_eq!(
            (a[0].directive_id, &a[0].kind),
            (b[0].directive_id, &b[0].kind)
        );
        assert_eq!(a[0].directive_id, cp.directives(1).await[0].directive_id);
        assert!(
            report.delivered >= 1 && report.failed.is_empty(),
            "{report:?}"
        );
        assert_eq!(provider.calls(Op::Clone), 1);
        assert_eq!(first.kind, second.kind);
        assert_eq!(cp.state(1).await.phase, Phase::Booting);
    });
}

#[test]
fn should_neither_undo_6_nor_run_5_twice_when_a_stale_relay_sends_5_after_6() {
    run(async {
        // given: the clone (5) is issued; relay A judges it pending and is held mid-send
        let cp = Cp::new(memory());
        let mut dp = Dp::start().await;
        let provider = dp.provider.clone();
        let (cp_link, mut dp_link) = connect(&cp, &mut dp, Mode::Normal).await;
        let saga = cp.saga();
        let saga_worker = cp.worker(NeverFires);
        cp.start(1).await;
        let (open, gate) = tokio::sync::watch::channel(false);
        let tap_a = Tapped {
            gate: Some(gate),
            ..Tapped::new(cp_link.directives.clone())
        };
        let tap_b = Tapped::new(cp_link.directives.clone());
        let (log_a, log_b) = (tap_a.log(), tap_b.log());
        let a_in_send = std::sync::Arc::clone(&tap_a.entered);
        let relay_a = cp.relay(tap_a, NeverFires);
        let worker_a = cp.worker(NeverFires);
        let relay_b = cp.relay(tap_b, NeverFires);
        let worker_b = cp.worker(NeverFires);
        saga_worker
            .catch_up(&saga)
            .await
            .unwrap_or_else(|e| panic!("{e}"));

        // when: A's lease runs out; B sends 5, it is answered, 6 is issued, sent and
        // answered; then A's send of 5 lands
        let takeover = async {
            let (mut cp_link, relay_b) = (cp_link, relay_b);
            // A is inside its send (judged 5 pending, held at the gate) before its lease runs out
            a_in_send.notified().await;
            cp.store_clock
                .advance(Millis::new(cp.config.lease_ttl.get() + 1));
            worker_b
                .catch_up(&relay_b)
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            let cloned = cp_link.next_answer().await;
            cp.record(1, &cloned).await;
            saga_worker
                .catch_up(&saga)
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            worker_b
                .catch_up(&relay_b)
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            let booted = cp_link.next_answer().await;
            cp.record(1, &booted).await;
            let before = cp.state(1).await;
            open.send_replace(true);
            let late = cp_link.next_answer().await;
            cp.record(1, &late).await;
            drop((relay_b, cp_link));
            (cloned, booted, before, late)
        };
        let script = async {
            let ((cloned, booted, before, late), report_a) =
                tokio::join!(takeover, worker_a.catch_up(&relay_a));
            drop(relay_a);
            (cloned, booted, before, late, report_a)
        };
        let (end, (cloned, booted, before, late, report_a)) = tokio::join!(
            dp.exec
                .serve(&mut dp_link.directives, &dp_link.observations),
            script
        );

        // then: 5 reached the data plane twice, last after 6, and ran once; its late answer is
        // recorded as stale and leaves the attempt where 6 put it
        assert_eq!(end, closed());
        let report_a = report_a.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(report_a.lost, vec![stream(1)]);
        let directives = cp.directives(1).await;
        let (five, six) = (directives[0].directive_id, directives[1].directive_id);
        let b: Vec<_> = log_b.sent().iter().map(|d| d.directive_id).collect();
        let a: Vec<_> = log_a.sent().iter().map(|d| d.directive_id).collect();
        assert_eq!((b, a), (vec![five, six], vec![five]));
        assert_eq!(provider.calls(Op::Clone), 1);
        assert_eq!(provider.calls(Op::Start), 1);
        assert!(matches!(cloned.kind, ObservationKind::VmCloned { .. }));
        assert!(matches!(booted.kind, ObservationKind::VmBooted));
        assert_eq!(late.kind, cloned.kind);
        let after = cp.state(1).await;
        assert_eq!(after, before);
        assert_eq!(after.phase, Phase::Verifying);
        assert!(matches!(
            cp.events(1).await.last(),
            Some(AttemptEvent::StaleObservationIgnored(ignored))
                if ignored.staleness == Staleness::Directive { directive_id: five }
        ));
    });
}

#[test]
fn should_hold_create_and_destroy_kinds_and_never_time_them_out_when_in_safe_mode() {
    run(async {
        // given: the control plane holds the host in safe mode
        let cp = Cp::new(memory());
        let mut dp = Dp::start().await;
        let provider = dp.provider.clone();
        let (mut cp_link, mut dp_link) = connect(&cp, &mut dp, Mode::Safe).await;
        let saga = cp.saga();
        cp.start(1).await;
        let tap = Tapped::new(cp_link.directives.clone());
        let log = tap.log();
        let relay = cp.relay(tap, NeverFires);
        let worker = cp.worker(NeverFires);

        // when: the clone is issued and relayed in safe mode, safe mode lasts far past the
        // clone's timeout, then it is left and the clone relayed again
        let script = async {
            let saga_worker = cp.worker(NeverFires);
            saga_worker
                .catch_up(&saga)
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            let held = worker
                .catch_up(&relay)
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            let sent_in_safe_mode = log.sent().len();
            cp.clock
                .advance(Millis::new(10 * past_clone_deadline().get()));
            saga_worker
                .catch_up(&saga)
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            let timeouts = saga.expire_due().await;
            let tries_held = cp.state(1).await.tries;
            cp.sessions.set_mode(&HostId::sample(), Mode::Normal);
            cp.store_clock
                .advance(Millis::new(cp.config.backoff_cap.get() + 1));
            let released = worker
                .catch_up(&relay)
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            let answer = cp_link.next_answer().await;
            let sent_at = cp.clock.now();
            let tries_sent = cp.state(1).await.tries;
            drop((relay, cp_link));
            (
                (held, sent_in_safe_mode, released, answer),
                (timeouts, tries_held, tries_sent, sent_at),
            )
        };
        let (
            end,
            (
                (held, sent_in_safe_mode, released, answer),
                (timeouts, tries_held, tries_sent, sent_at),
            ),
        ) = tokio::join!(
            dp.exec
                .serve(&mut dp_link.directives, &dp_link.observations),
            script
        );

        // then: nothing reached the data plane in safe mode; the same clone went out after
        assert_eq!(end, closed());
        assert_eq!(sent_in_safe_mode, 0);
        assert_eq!(held.failed.len(), 1, "{held:?}");
        assert_eq!(held.failed[0].0, stream(1));
        assert!(released.failed.is_empty(), "{released:?}");
        let frames = log.sent();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].kind.tag(), DirectiveTag::CloneVm);
        assert!(frames[0].kind.held_in_safe_mode());
        assert_eq!(
            frames[0].attempt.map(|a| (a.attempt_id, a.generation)),
            Some((AttemptId::from_u128(1), Generation::FIRST))
        );
        assert_eq!(provider.calls(Op::Clone), 1);
        assert!(matches!(answer.kind, ObservationKind::VmCloned { .. }));
        // held unsent, its outcome stayed known: no timeout, no Unknown, no try spent
        assert_eq!(timeouts, Default::default());
        assert_eq!((tries_held, tries_sent), (1, 1));
        assert!(
            !cp.events(1)
                .await
                .iter()
                .any(|e| matches!(e, AttemptEvent::DirectiveOutcomeUnknown(..)))
        );
        // the deadline runs from the send
        assert_eq!(
            frames[0].deadline,
            Deadline::after(sent_at, SagaConfig::default().timeouts.clone)
        );
    });
}
