//! `LinkSink`, the directive relay's sink over the link (component level, P7, I29): it sends a
//! directive only while it is the attempt's pending one, stamped with the host session's
//! epoch, holds create and destroy kinds in safe mode, and bounds each send below the relay's
//! handle budget (DC-26).

use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use mayfly_control::delivery::{
    DeliveryConfig, DirectiveDetails as _, DirectiveSink as _, Kicks, LeaseFence, LinkSink,
    RelayedDirective, SinkConfig, SinkError, compose, send_deadline,
};
use mayfly_control::journal::Journal;
use mayfly_control::saga::{AttemptSaga, DirectiveTimeouts, SagaConfig};
use mayfly_control::sessions::{HostSession, HostSessions};
use mayfly_control::store::Clock as _;
use mayfly_control::store::decode_event;
use mayfly_control_domain::attempt::{
    AttemptEvent, DirectiveKind as Decided, Fact, OwnershipProof,
};
use mayfly_domain::{
    AttemptId, Deadline, DirectiveId, Epoch, Generation, HostId, Millis, ProviderInstanceId,
    Sample, SecretHandleId,
};
use mayfly_protocol::{AttemptRef, Directive, DirectiveKind, LinkError, Mode};
use mayfly_testkit::attempts::{FixedDetails, observe, save};
use mayfly_testkit::block_on;
use mayfly_testkit::clock::{FakeClock, SeededRng};
use mayfly_testkit::journal::MemoryJournal;

use crate::attempts::{EPOCH, Harness, harness, stream};

/// The link's sending half: records each frame, or never completes a send while `blocked`.
#[derive(Debug, Clone, Default)]
struct Link {
    sent: Arc<Mutex<Vec<Directive>>>,
    blocked: bool,
}

impl Link {
    fn sent(&self) -> Vec<Directive> {
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl mayfly_protocol::DirectiveSink for Link {
    fn send(&self, directive: Directive) -> impl Future<Output = Result<(), LinkError>> + Send {
        let sent = Arc::clone(&self.sent);
        let blocked = self.blocked;
        async move {
            if blocked {
                std::future::pending::<()>().await;
            }
            sent.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(directive);
            Ok(())
        }
    }
}

type Sink<'s> =
    LinkSink<'s, MemoryJournal, FakeClock, SeededRng, Kicks, Link, FakeClock, FixedDetails>;

fn sink<'s>(h: &'s Harness, link: &Link, sessions: &HostSessions) -> Sink<'s> {
    LinkSink::new(
        &h.store,
        link.clone(),
        FakeClock::at(Millis::new(0)),
        FixedDetails::default(),
        sessions.clone(),
        SinkConfig::default(),
        &DeliveryConfig::default(),
    )
}

/// Every `DirectiveIssued` of attempt `n`, as the relay hands them over.
fn relayed(h: &Harness, n: u128) -> Vec<RelayedDirective> {
    block_on(h.journal.read_stream(&stream(n), None, None))
        .unwrap_or_else(|e| panic!("{e}"))
        .into_iter()
        .filter_map(|event| match decode_event::<AttemptEvent>(&event) {
            Ok(AttemptEvent::DirectiveIssued(directive)) => Some(RelayedDirective {
                stream: event.stream,
                revision: event.revision,
                event_id: event.event_id,
                fence: LeaseFence::new(1),
                directive,
            }),
            _ => None,
        })
        .collect()
}

fn issue(h: &Harness, n: u128, id: u128) {
    block_on(save(
        &h.store,
        n,
        0x7000 + id,
        EPOCH,
        mayfly_control_domain::attempt::AttemptCommand::IssueNext {
            directive_id: DirectiveId::from_u128(id),
            deadline: Deadline::at(Millis::new(1_800_000_000_000)),
        },
    ));
}

fn answer(h: &Harness, n: u128, observation: u128, fact: Fact) {
    block_on(save(
        &h.store,
        n,
        0x8000 + observation,
        EPOCH,
        observe(observation, EPOCH, fact),
    ));
}

#[test]
fn should_stamp_the_session_epoch_when_a_pending_directive_is_sent() {
    // given: the clone is pending, decided under epoch 1, and the host's session is at 1
    let h = harness();
    h.start(1);
    issue(&h, 1, 0xd1);
    let link = Link::default();
    let sink = sink(&h, &link, &h.sessions);
    let clone = relayed(&h, 1).remove(0);

    // when: sent, then sent again (a redelivery)
    let first = block_on(sink.send(clone.clone()));
    let again = block_on(sink.send(clone));

    // then: the same frame twice, under the one directive id
    assert_eq!((first, again), (Ok(()), Ok(())));
    let sent = link.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], sent[1]);
    assert_eq!(sent[0].directive_id, DirectiveId::from_u128(0xd1));
    assert_eq!(sent[0].epoch, EPOCH);
    assert_eq!(
        sent[0].attempt,
        Some(AttemptRef {
            attempt_id: AttemptId::from_u128(1),
            generation: Generation::FIRST,
        })
    );
    assert!(matches!(
        &sent[0].kind,
        DirectiveKind::CloneVm { vm_name, .. }
            if vm_name.is_for(&AttemptId::from_u128(1), Generation::FIRST)
    ));
    assert_eq!(sink.stats().sent, 2);
    let deadline = Deadline::after(h.clock.now(), DirectiveTimeouts::default().clone);
    assert_eq!(sent[0].deadline, deadline);
    let pending = h.state(1).pending.unwrap_or_else(|| panic!("pending"));
    assert!(pending.sent && pending.deadline == deadline);
    let recorded = h
        .events(1)
        .iter()
        .filter(|e| matches!(e, AttemptEvent::DirectiveSent(..)))
        .count();
    assert_eq!(recorded, 1);
}

#[test]
fn should_drop_a_late_directive_when_a_later_one_is_pending() {
    // given: directive 5 (the clone) was answered and directive 6 (the start) is pending
    let h = harness();
    h.start(1);
    issue(&h, 1, 5);
    answer(
        &h,
        1,
        1,
        Fact::VmCloned {
            directive_id: DirectiveId::from_u128(5),
            instance_id: ProviderInstanceId::sample(),
        },
    );
    issue(&h, 1, 6);
    let link = Link::default();
    let sink = sink(&h, &link, &h.sessions);
    let both = relayed(&h, 1);

    // when: 6 is sent, then a stale relay worker's 5 arrives late
    let six = block_on(sink.send(both[1].clone()));
    let five = block_on(sink.send(both[0].clone()));

    // then: only 6 went out; 5 was dropped, not an error (the relay must advance past it)
    assert_eq!((six, five), (Ok(()), Ok(())));
    let sent = link.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].directive_id, DirectiveId::from_u128(6));
    assert!(matches!(sent[0].kind, DirectiveKind::StartVm { .. }));
    assert_eq!(sink.stats().superseded, 1);
}

#[test]
fn should_hold_create_and_destroy_kinds_when_the_session_is_in_safe_mode() {
    // given: attempt 1's clone and attempt 2's verification are pending; safe mode
    let h = harness();
    h.start(1);
    issue(&h, 1, 0xd1);
    h.start(2);
    issue(&h, 2, 0xd2);
    answer(
        &h,
        2,
        2,
        Fact::VmCloned {
            directive_id: DirectiveId::from_u128(0xd2),
            instance_id: ProviderInstanceId::sample(),
        },
    );
    issue(&h, 2, 0xd3);
    answer(
        &h,
        2,
        3,
        Fact::VmBooted {
            directive_id: DirectiveId::from_u128(0xd3),
        },
    );
    issue(&h, 2, 0xd4);
    h.sessions.set_mode(&HostId::sample(), Mode::Safe);
    let link = Link::default();
    let sink = sink(&h, &link, &h.sessions);
    let clone = relayed(&h, 1).remove(0);
    let verify = relayed(&h, 2).remove(2);

    // when
    let held = block_on(sink.send(clone.clone()));
    let observed = block_on(sink.send(verify));
    h.sessions.set_mode(&HostId::sample(), Mode::Normal);
    let released = block_on(sink.send(clone));

    // then: the clone waited for normal mode; verification ran in safe mode
    assert_eq!(held, Err(SinkError::new("held_in_safe_mode")));
    assert_eq!((observed, released), (Ok(()), Ok(())));
    let kinds: Vec<_> = link.sent().iter().map(|d| d.kind.tag()).collect();
    assert_eq!(
        kinds,
        [
            mayfly_protocol::DirectiveTag::VerifyGuest,
            mayfly_protocol::DirectiveTag::CloneVm
        ]
    );
    assert_eq!(sink.stats().held, 1);
}

#[test]
fn should_wait_for_adoption_when_the_session_is_of_another_epoch() {
    // given: the clone was decided under epoch 1; the host's session is now at 2
    let h = harness();
    h.start(1);
    issue(&h, 1, 0xd1);
    let sessions = HostSessions::new();
    sessions.open(
        HostId::sample(),
        HostSession {
            epoch: Epoch::new(2),
            mode: Mode::Normal,
        },
    );
    let link = Link::default();
    let other_epoch = sink(&h, &link, &sessions);
    let no_session = sink(&h, &link, &HostSessions::new());

    // when
    let sent = block_on(other_epoch.send(relayed(&h, 1).remove(0)));
    let without = block_on(no_session.send(relayed(&h, 1).remove(0)));

    // then
    assert_eq!(sent, Err(SinkError::new("attempt_not_adopted")));
    assert_eq!(without, Err(SinkError::new("no_session")));
    assert!(link.sent().is_empty());
}

#[test]
fn should_bound_the_send_below_the_handle_budget_when_built() {
    // given
    let delivery = DeliveryConfig::default();
    let short = DeliveryConfig {
        lease_ttl: Millis::new(4),
        handle_deadline: Millis::new(4),
        ..DeliveryConfig::default()
    };

    // when
    let wanted = send_deadline(Millis::new(5_000), &delivery);
    let capped = send_deadline(Millis::new(60_000), &delivery);
    let tiny = send_deadline(Millis::new(60_000), &short);

    // then: below the 20 s budget, at most half of it, never zero
    assert_eq!(wanted, Millis::new(5_000));
    assert_eq!(capped, Millis::new(10_000));
    assert!(capped < delivery.handle_budget());
    assert_eq!(tiny, Millis::new(1));
}

#[test]
fn should_fail_at_its_own_deadline_when_the_link_does_not_take_the_frame() {
    // given: a link whose send never completes, and a timer that fires at once
    let h = harness();
    h.start(1);
    issue(&h, 1, 0xd1);
    let link = Link {
        blocked: true,
        ..Link::default()
    };
    let sink = sink(&h, &link, &h.sessions);

    // when
    let sent = block_on(sink.send(relayed(&h, 1).remove(0)));

    // then: a sink error the relay backs off on, not a handle cut at the relay's budget
    assert_eq!(sent, Err(SinkError::new("send_deadline")));
}

#[test]
fn should_refuse_to_compose_a_clone_when_the_image_is_not_materialised() {
    // given
    let h = harness();
    h.start(1);
    issue(&h, 1, 0xd1);
    let link = Link::default();
    let sink = LinkSink::new(
        &h.store,
        link.clone(),
        FakeClock::at(Millis::new(0)),
        FixedDetails { base: None },
        h.sessions.clone(),
        SinkConfig::default(),
        &DeliveryConfig::default(),
    );

    // when
    let sent = block_on(sink.send(relayed(&h, 1).remove(0)));

    // then
    assert_eq!(sent, Err(SinkError::new("image_base_unknown")));
    assert!(link.sent().is_empty());
}

#[test]
fn should_compose_every_kind_from_the_stream_and_the_details_when_sent() {
    // given: attempt 1's identity, a recorded instance and its proof
    let h = harness();
    h.start(1);
    let identity = h.state(1).identity.unwrap_or_else(|| panic!("started"));
    let proof = OwnershipProof {
        attempt_id: identity.attempt_id,
        host_id: identity.host_id.clone(),
        slot: identity.slot,
        generation: identity.generation,
        vm_name: identity.vm_name.clone(),
        instance_id: ProviderInstanceId::sample(),
    };
    let handle_id = SecretHandleId::from_u128(0x5ec);
    let kinds = [
        Decided::CloneVm,
        Decided::StartVm,
        Decided::VerifyGuest,
        Decided::LaunchRunner { handle_id },
        Decided::ExportDiagnostics,
        Decided::StopVm {
            proof: proof.clone(),
        },
        Decided::DeleteVm {
            proof: proof.clone(),
        },
    ];
    let details = FixedDetails::default();
    let id = DirectiveId::from_u128(0xd1);

    // when
    let composed: Vec<DirectiveKind> = kinds
        .iter()
        .map(|kind| compose(&identity, id, kind, &details).unwrap_or_else(|e| panic!("{e}")))
        .collect();

    // then: every VM kind names the stream's VM; the proof is the stream's; only the handle
    // crosses for a launch
    assert!(
        composed
            .iter()
            .all(|k| k.vm_name() == Some(&identity.vm_name)),
        "{composed:?}"
    );
    assert!(matches!(
        &composed[1],
        DirectiveKind::StartVm { run_options, .. } if *run_options == details.run_options(&identity.pool).unwrap_or_else(|| panic!("known"))
    ));
    assert!(matches!(
        &composed[2],
        DirectiveKind::VerifyGuest { nonce, .. } if *nonce == details.nonce(id)
    ));
    assert!(matches!(
        &composed[3],
        DirectiveKind::LaunchRunner { launch, .. } if launch.handle_id == handle_id
    ));
    assert!(matches!(
        &composed[5],
        DirectiveKind::StopVm { grace, proof: p }
            if *grace == Millis::new(10_000) && p.instance_id == proof.instance_id
    ));
    assert!(matches!(
        &composed[6],
        DirectiveKind::DeleteVm { proof: p } if p.vm_name == proof.vm_name
    ));
}

#[test]
fn should_report_its_send_deadline_when_built_or_shown() {
    // given
    let h = harness();
    let link = Link::default();

    // when
    let sink = sink(&h, &link, &h.sessions);
    let shown = format!("{sink:?}");

    // then
    assert_eq!(sink.deadline(), Millis::new(5_000));
    assert!(shown.contains("send_deadline"), "{shown}");
}

#[test]
fn should_fix_the_deadline_at_the_first_send_when_a_directive_was_held() {
    // given: the clone is held for an hour of safe mode
    let h = harness();
    h.start(1);
    issue(&h, 1, 0xd1);
    h.sessions.set_mode(&HostId::sample(), Mode::Safe);
    let link = Link::default();
    let sink = sink(&h, &link, &h.sessions);
    let clone = relayed(&h, 1).remove(0);
    let held = block_on(sink.send(clone.clone()));
    let tries = h.state(1).tries;
    h.clock.advance(Millis::new(3_600_000));
    let still_held = h.state(1).pending;

    // when: safe mode ends and the relay delivers it again
    h.sessions.set_mode(&HostId::sample(), Mode::Normal);
    let sent = block_on(sink.send(clone));

    // then: its deadline runs from the send, not from the decision an hour earlier, and
    // nothing was counted against it while held
    assert_eq!(held, Err(SinkError::new("held_in_safe_mode")));
    assert!(still_held.is_some_and(|p| !p.sent));
    assert_eq!(sent, Ok(()));
    let deadline = Deadline::after(h.clock.now(), DirectiveTimeouts::default().clone);
    assert_eq!(link.sent()[0].deadline, deadline);
    let state = h.state(1);
    assert!(
        state
            .pending
            .is_some_and(|p| p.sent && p.deadline == deadline)
    );
    assert_eq!(state.tries, tries);

    // and the saga times it out only past that deadline (plus the reply grace)
    let config = SagaConfig::default();
    let saga = AttemptSaga::new(&h.store, h.sessions.clone(), config);
    block_on(h.worker.catch_up(&saga)).unwrap_or_else(|e| panic!("{e}"));
    let due = Deadline::after(deadline.instant(), config.reply_grace).instant();
    assert_eq!(saga.next_due(), Some(due));
    h.clock
        .advance(Millis::new(due.get() - h.clock.now().get() - 1));
    assert_eq!(block_on(saga.expire_due()), Default::default());
    h.clock.advance(Millis::new(1));
    assert_eq!(
        block_on(saga.expire_due()).unknown,
        vec![DirectiveId::from_u128(0xd1)]
    );
}
