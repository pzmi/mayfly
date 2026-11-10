//! The `attempt_saga` consumer over a real store (component level, policies P6 and P7): it
//! issues each stage's directive once per event, judges a late event against the attempt's
//! current state, and turns a deadline that passed unanswered into `Unknown`, never into a
//! success or a retry under a new name.

use mayfly_control::delivery::{
    Consumer, ConsumerError, ConsumerName, Delivery, DeliveryKey, Kicks, LeaseFence,
};
use mayfly_control::journal::Journal;
use mayfly_control::saga::{
    AttemptSaga, DirectiveTimeouts, SagaConfig, command_for_event, directive_of, timeout_of,
};
use mayfly_control::sessions::{HostSession, HostSessions};
use mayfly_control::store::Clock as _;
use mayfly_control_domain::attempt::{
    AttemptCommand, AttemptEvent, DirectiveKind, DirectiveTag, Fact, Outcome, Phase, SagaStep,
};
use mayfly_domain::{
    Deadline, DirectiveId, Epoch, EventId, HostId, Millis, ProviderInstanceId, Sample,
};
use mayfly_protocol::Mode;
use mayfly_testkit::attempts::{instance, observe, save};
use mayfly_testkit::block_on;
use mayfly_testkit::clock::{FakeClock, SeededRng};
use mayfly_testkit::journal::MemoryJournal;

use crate::attempts::{EPOCH, Harness, NOW, harness, stream};

type Saga<'s> = AttemptSaga<'s, MemoryJournal, FakeClock, SeededRng, Kicks>;

/// Delivers the `nth` event (from 0) of attempt `n`'s stream to `saga`, as a worker would.
fn deliver(saga: &Saga<'_>, h: &Harness, n: u128, nth: usize) -> Result<(), ConsumerError> {
    let events =
        block_on(h.journal.read_stream(&stream(n), None, None)).unwrap_or_else(|e| panic!("{e}"));
    let event = &events[nth];
    let key = DeliveryKey {
        consumer: ConsumerName::new("attempt_saga").unwrap_or_else(|e| panic!("{e}")),
        event_id: event.event_id,
    };
    block_on(saga.handle(Delivery {
        event,
        fence: LeaseFence::new(1),
        key: &key,
    }))
}

fn cloned(directive_id: DirectiveId) -> Fact {
    Fact::VmCloned {
        directive_id,
        instance_id: ProviderInstanceId::sample(),
    }
}

#[test]
fn should_derive_the_golden_ids_when_an_event_id_is_given() {
    // given: SHA-256 of "mayfly-attempt-saga-v1\n", a tag line and a 128-bit id, computed
    // outside Rust
    let event = EventId::from_u128(0xe1);

    // when
    let command = command_for_event(event);
    let directive = directive_of(command);
    let timeout = timeout_of(directive);

    // then
    assert_eq!(command.to_string(), "ec1ff3de2ca55fb07a35e30160a03a50");
    assert_eq!(directive.to_string(), "26d618adf56ed4f9412b46ef9443dbe4");
    assert_eq!(timeout.to_string(), "14ff708f45310448fa96092ea302a86b");
}

#[test]
fn should_issue_the_next_directive_when_a_stage_completes() {
    // given
    let h = harness();
    let saga = AttemptSaga::new(&h.store, h.sessions.clone(), SagaConfig::default());
    h.start(1);

    // when: the start is delivered, then the clone's answer
    let first = block_on(h.worker.catch_up(&saga)).unwrap_or_else(|e| panic!("{e}"));
    let clone = h.directives(1);
    block_on(save(
        &h.store,
        1,
        0x6001,
        EPOCH,
        observe(1, EPOCH, cloned(clone[0].directive_id)),
    ));
    let second = block_on(h.worker.catch_up(&saga)).unwrap_or_else(|e| panic!("{e}"));

    // then
    assert!(
        first.failed.is_empty() && second.failed.is_empty(),
        "{first:?} {second:?}"
    );
    let issued = h.directives(1);
    assert_eq!(issued.len(), 2);
    let started = h.events(1);
    let started_id = block_on(h.journal.read_stream(&stream(1), None, None))
        .unwrap_or_else(|e| panic!("{e}"))[0]
        .event_id;
    assert!(matches!(started[0], AttemptEvent::AttemptStarted(..)));
    assert_eq!(issued[0].kind, DirectiveKind::CloneVm);
    assert_eq!(
        issued[0].directive_id,
        directive_of(command_for_event(started_id))
    );
    assert_eq!(
        issued[0].deadline,
        Deadline::after(NOW, SagaConfig::default().timeouts.clone)
    );
    assert_eq!(issued[1].kind, DirectiveKind::StartVm);
    assert_eq!(h.state(1).phase, Phase::Booting);
}

#[test]
fn should_issue_one_directive_when_an_event_is_delivered_twice() {
    // given: the start delivered once, the clone answered
    let h = harness();
    let saga = AttemptSaga::new(&h.store, h.sessions.clone(), SagaConfig::default());
    h.start(1);
    deliver(&saga, &h, 1, 0).unwrap_or_else(|e| panic!("{e}"));
    let again_in_flight = deliver(&saga, &h, 1, 0);
    let clone = h.directives(1)[0].directive_id;
    block_on(save(
        &h.store,
        1,
        0x6001,
        EPOCH,
        observe(1, EPOCH, cloned(clone)),
    ));
    h.clock.advance(Millis::new(1_000));

    // when: the start comes once more, late, after the clone was answered
    let late = deliver(&saga, &h, 1, 0);

    // then: neither repeat issued anything; the late one met its own receipt (another
    // deadline, so another intent) and was read as decided
    assert_eq!(again_in_flight, Ok(()));
    assert_eq!(late, Ok(()));
    assert_eq!(h.directives(1).len(), 1);
    assert_eq!(h.state(1).phase, Phase::Booting);
    assert_eq!(h.state(1).pending, None);
}

#[test]
fn should_make_the_outcome_unknown_when_the_deadline_passes_unanswered() {
    // given: the clone is issued (no timer yet), then sent, and the saga saw it
    let h = harness();
    let config = SagaConfig::default();
    let saga = AttemptSaga::new(&h.store, h.sessions.clone(), config);
    h.start(1);
    block_on(h.worker.catch_up(&saga)).unwrap_or_else(|e| panic!("{e}"));
    let unsent = saga.next_due();
    let clone = h.directives(1)[0].clone();
    h.sent(1, &clone);
    block_on(h.worker.catch_up(&saga)).unwrap_or_else(|e| panic!("{e}"));
    let due = Deadline::after(clone.deadline.instant(), config.reply_grace).instant();

    // when
    assert_eq!(saga.next_due(), Some(due));
    h.clock.advance(Millis::new(due.get() - NOW.get() - 1));
    let early = block_on(saga.expire_due());
    h.clock.advance(Millis::new(1));
    let expired = block_on(saga.expire_due());
    let after = block_on(h.worker.catch_up(&saga)).unwrap_or_else(|e| panic!("{e}"));

    // then: Unknown, not cloned, nothing issued again, no timer left
    assert_eq!(unsent, None);
    assert_eq!(early, Default::default());
    assert_eq!(expired.unknown, vec![clone.directive_id]);
    let state = h.state(1);
    assert!(matches!(
        state.pending.map(|p| (p.directive_id, p.outcome)),
        Some((d, Outcome::Unknown { .. })) if d == clone.directive_id
    ));
    assert_eq!(state.phase, Phase::Cloning);
    assert_eq!(state.instance_id, None);
    assert!(after.failed.is_empty(), "{after:?}");
    assert_eq!(h.directives(1).len(), 1);
    assert_eq!(saga.next_due(), None);
    assert!(
        h.events(1)
            .iter()
            .any(|e| matches!(e, AttemptEvent::DirectiveOutcomeUnknown(..)))
    );
}

#[test]
fn should_disarm_the_deadline_when_the_answer_comes_first() {
    // given
    let h = harness();
    let saga = AttemptSaga::new(&h.store, h.sessions.clone(), SagaConfig::default());
    h.start(1);
    block_on(h.worker.catch_up(&saga)).unwrap_or_else(|e| panic!("{e}"));
    h.sent(1, &h.directives(1)[0]);
    block_on(h.worker.catch_up(&saga)).unwrap_or_else(|e| panic!("{e}"));
    let clone = h.directives(1)[0].directive_id;
    assert!(saga.next_due().is_some());

    // when: the answer, then the start's own directive in flight again and its timer
    block_on(save(
        &h.store,
        1,
        0x6001,
        EPOCH,
        observe(1, EPOCH, cloned(clone)),
    ));
    block_on(h.worker.catch_up(&saga)).unwrap_or_else(|e| panic!("{e}"));
    let start = h.directives(1)[1].clone();
    h.sent(1, &start);
    block_on(h.worker.catch_up(&saga)).unwrap_or_else(|e| panic!("{e}"));
    h.clock.advance(Millis::new(10_000_000));
    let expired = block_on(saga.expire_due());

    // then: only the start, which is pending now, timed out; the answered clone did not
    assert_eq!(expired.unknown, vec![start.directive_id]);
    assert!(expired.moot.is_empty());
    assert_eq!(h.state(1).phase, Phase::Booting);
}

#[test]
fn should_answer_a_moot_timeout_when_the_answer_lands_after_the_timer_was_armed() {
    // given: the clone is sent and armed; the answer is recorded but not yet delivered
    let h = harness();
    let saga = AttemptSaga::new(&h.store, h.sessions.clone(), SagaConfig::default());
    h.start(1);
    block_on(h.worker.catch_up(&saga)).unwrap_or_else(|e| panic!("{e}"));
    h.sent(1, &h.directives(1)[0]);
    block_on(h.worker.catch_up(&saga)).unwrap_or_else(|e| panic!("{e}"));
    let clone = h.directives(1)[0].directive_id;
    block_on(save(
        &h.store,
        1,
        0x6001,
        EPOCH,
        observe(1, EPOCH, cloned(clone)),
    ));

    // when
    h.clock.advance(Millis::new(10_000_000));
    let expired = block_on(saga.expire_due());

    // then: refused `directive_not_pending`, nothing recorded, the timer gone
    assert_eq!(expired.moot, vec![clone]);
    assert!(expired.unknown.is_empty());
    assert_eq!(saga.next_due(), None);
    assert_eq!(h.state(1).phase, Phase::Booting);
}

#[test]
fn should_wait_without_a_session_when_the_host_has_none() {
    // given
    let h = harness();
    let saga = AttemptSaga::new(&h.store, HostSessions::new(), SagaConfig::default());
    h.start(1);

    // when
    let handled = deliver(&saga, &h, 1, 0);

    // then
    assert_eq!(handled, Err(ConsumerError::new("no_session")));
    assert!(h.directives(1).is_empty());
}

#[test]
fn should_retry_a_timeout_later_when_it_cannot_be_decided_now() {
    // given: the clone is sent and armed, then the host's session goes away
    let h = harness();
    let config = SagaConfig::default();
    let saga = AttemptSaga::new(&h.store, h.sessions.clone(), config);
    h.start(1);
    block_on(h.worker.catch_up(&saga)).unwrap_or_else(|e| panic!("{e}"));
    let clone = h.directives(1)[0].clone();
    h.sent(1, &clone);
    block_on(h.worker.catch_up(&saga)).unwrap_or_else(|e| panic!("{e}"));
    h.sessions.close(&HostId::sample());
    h.clock.advance(Millis::new(10_000_000));

    // when
    let failed = block_on(saga.expire_due());
    let next = saga.next_due();
    // an event of the stream re-arms the timer: it keeps its retry time
    block_on(saga.reconsider(&instance(1))).unwrap_or_else(|e| panic!("{e}"));
    let rearmed = saga.next_due();
    let again = block_on(saga.expire_due());

    // then: not decided, and not due again until one reply grace later
    assert_eq!(failed.failed, vec![(clone.directive_id, "no_session")]);
    assert_eq!(
        next,
        Some(Deadline::after(h.clock.now(), config.reply_grace).instant())
    );
    assert_eq!(rearmed, next);
    assert_eq!(again, Default::default());
    assert!(matches!(
        h.state(1).pending.map(|p| p.outcome),
        Some(Outcome::InFlight)
    ));
}

#[test]
fn should_give_each_kind_its_own_timeout_when_configured() {
    // given
    let config = DirectiveTimeouts {
        clone: Millis::new(1),
        start: Millis::new(2),
        verify: Millis::new(3),
        launch: Millis::new(4),
        diagnostics: Millis::new(5),
        stop: Millis::new(6),
        delete: Millis::new(7),
    };
    let tags = [
        DirectiveTag::CloneVm,
        DirectiveTag::StartVm,
        DirectiveTag::VerifyGuest,
        DirectiveTag::LaunchRunner,
        DirectiveTag::ExportDiagnostics,
        DirectiveTag::StopVm,
        DirectiveTag::DeleteVm,
    ];

    // when
    let timeouts: Vec<u64> = tags.iter().map(|t| config.timeout(*t).get()).collect();

    // then
    assert_eq!(timeouts, [1, 2, 3, 4, 5, 6, 7]);
}

#[test]
fn should_rearm_the_deadline_when_a_restarted_saga_reconsiders_an_attempt() {
    // given: the clone is in flight, and a new saga (a restart) has no timer
    let h = harness();
    let first = AttemptSaga::new(&h.store, h.sessions.clone(), SagaConfig::default());
    h.start(1);
    block_on(h.worker.catch_up(&first)).unwrap_or_else(|e| panic!("{e}"));
    h.sent(1, &h.directives(1)[0]);
    block_on(h.worker.catch_up(&first)).unwrap_or_else(|e| panic!("{e}"));
    let restarted = AttemptSaga::new(&h.store, h.sessions.clone(), SagaConfig::default());
    let before = restarted.next_due();

    // when
    let step = block_on(restarted.reconsider(&instance(1))).unwrap_or_else(|e| panic!("{e}"));

    // then
    let clone = h.directives(1)[0].clone();
    assert_eq!(before, None);
    assert_eq!(
        step,
        SagaStep::AwaitAnswer {
            directive_id: clone.directive_id,
            deadline: clone.deadline,
        }
    );
    assert_eq!(restarted.next_due(), first.next_due());
    assert!(format!("{restarted:?}").contains("AttemptSaga"));
}

#[test]
fn should_close_the_stream_when_the_vm_is_lost() {
    // given: the clone answered and the start in flight, then a complete inventory without
    // the VM
    let h = harness();
    let saga = AttemptSaga::new(&h.store, h.sessions.clone(), SagaConfig::default());
    h.start(1);
    block_on(h.worker.catch_up(&saga)).unwrap_or_else(|e| panic!("{e}"));
    let clone = h.directives(1)[0].directive_id;
    block_on(save(
        &h.store,
        1,
        0x6001,
        EPOCH,
        observe(1, EPOCH, cloned(clone)),
    ));
    block_on(h.worker.catch_up(&saga)).unwrap_or_else(|e| panic!("{e}"));
    block_on(save(
        &h.store,
        1,
        0x6003,
        EPOCH,
        observe(3, EPOCH, Fact::VmAbsent),
    ));

    // when
    let report = block_on(h.worker.catch_up(&saga)).unwrap_or_else(|e| panic!("{e}"));

    // then
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(h.state(1).phase, Phase::Closed);
}

#[test]
fn should_keep_the_timer_armed_when_a_timeout_is_refused_for_another_reason() {
    // given: the clone sent and armed; the host's session moved to epoch 2 and the attempt is
    // not adopted yet
    let h = harness();
    let config = SagaConfig::default();
    let saga = AttemptSaga::new(&h.store, h.sessions.clone(), config);
    h.start(1);
    block_on(h.worker.catch_up(&saga)).unwrap_or_else(|e| panic!("{e}"));
    let clone = h.directives(1)[0].clone();
    h.sent(1, &clone);
    block_on(h.worker.catch_up(&saga)).unwrap_or_else(|e| panic!("{e}"));
    let two = Epoch::new(2);
    h.sessions.open(
        HostId::sample(),
        HostSession {
            epoch: two,
            mode: Mode::Normal,
        },
    );
    h.clock.advance(Millis::new(10_000_000));

    // when: refused `not_adopted`; then adopted (the stream moves) and tried again
    let refused = block_on(saga.expire_due());
    let armed = saga.next_due();
    block_on(save(
        &h.store,
        1,
        0x6100,
        two,
        AttemptCommand::Adopt {
            inventory_ref: EventId::from_u128(0x1e),
        },
    ));
    h.clock.advance(config.reply_grace);
    let expired = block_on(saga.expire_due());

    // then: the refusal kept the timer; once adopted, the timeout made the outcome Unknown
    assert_eq!(
        refused.failed,
        vec![(clone.directive_id, "timeout_refused")]
    );
    assert!(armed.is_some());
    assert_eq!(expired.unknown, vec![clone.directive_id]);
    assert!(matches!(
        h.state(1).pending.map(|p| p.outcome),
        Some(Outcome::Unknown { .. })
    ));
}

#[test]
fn should_count_a_timeout_moot_when_the_outcome_is_already_unknown() {
    // given: the clone sent and armed; the data plane answered Unknown before the deadline
    let h = harness();
    let saga = AttemptSaga::new(&h.store, h.sessions.clone(), SagaConfig::default());
    h.start(1);
    block_on(h.worker.catch_up(&saga)).unwrap_or_else(|e| panic!("{e}"));
    let clone = h.directives(1)[0].clone();
    h.sent(1, &clone);
    block_on(h.worker.catch_up(&saga)).unwrap_or_else(|e| panic!("{e}"));
    block_on(save(
        &h.store,
        1,
        0x6001,
        EPOCH,
        observe(
            1,
            EPOCH,
            Fact::OutcomeUnknown {
                directive_id: clone.directive_id,
            },
        ),
    ));
    h.clock.advance(Millis::new(10_000_000));

    // when
    let expired = block_on(saga.expire_due());

    // then: accepted with no event, so moot, and disarmed
    assert_eq!(expired.moot, vec![clone.directive_id]);
    assert!(expired.unknown.is_empty());
    assert_eq!(saga.next_due(), None);
}
