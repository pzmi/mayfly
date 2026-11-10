//! The `directive_relay` consumer (component level, ADR 0034): it hands each committed
//! `DirectiveIssued` event to its sink with where it was recorded and its claim's fence, lets
//! every other event pass, and stops on a directive it cannot decode or a sink failure instead
//! of skipping it (ES-I15).

use std::sync::{Arc, Mutex, PoisonError};

use mayfly_control::delivery::{
    Consumer, ConsumerError, ConsumerName, Delivery, DeliveryKey, DirectiveRelay, DirectiveSink,
    LeaseFence, RelayedDirective, SinkError,
};
use mayfly_control::journal::{Payload, RecordedEvent, StoredKind, StreamId, StreamRevision};
use mayfly_domain::{EventId, Millis};
use mayfly_testkit::block_on;

/// The golden `directive_issued` v1 sample (`clone_vm`), compact.
const CLONE_VM: &str =
    r#"{"directive_id":"0000000000000000000000000000d001","kind":"clone_vm","deadline":60000}"#;

#[derive(Debug, Clone, Default)]
struct Sink {
    sent: Arc<Mutex<Vec<RelayedDirective>>>,
    refuse: Option<&'static str>,
}

impl DirectiveSink for Sink {
    async fn send(&self, directive: RelayedDirective) -> Result<(), SinkError> {
        if let Some(code) = self.refuse {
            return Err(SinkError::new(code));
        }
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(directive);
        Ok(())
    }
}

fn recorded(event_type: &str, schema_version: u32, payload: &str) -> RecordedEvent {
    RecordedEvent {
        stream: StreamId::new("attempt-0000a77e").unwrap_or_else(|e| panic!("{e}")),
        revision: StreamRevision::new(3),
        event_id: EventId::from_u128(0xe1),
        kind: StoredKind {
            event_type: event_type.to_owned(),
            schema_version,
        },
        payload: Payload::new(payload),
        recorded_at: Millis::new(1),
    }
}

fn handle(relay: &DirectiveRelay<Sink>, event: &RecordedEvent) -> Result<(), ConsumerError> {
    let key = DeliveryKey {
        consumer: ConsumerName::new(DirectiveRelay::<Sink>::NAME).unwrap_or_else(|e| panic!("{e}")),
        event_id: event.event_id,
    };
    block_on(relay.handle(Delivery {
        event,
        fence: LeaseFence::new(7),
        key: &key,
    }))
}

#[test]
fn should_hand_the_directive_to_the_sink_when_a_directive_issued_event_is_delivered() {
    // given
    let relay = DirectiveRelay::new(Sink::default());
    let event = recorded("directive_issued", 1, CLONE_VM);

    // when
    let handled = handle(&relay, &event);

    // then
    assert_eq!(handled, Ok(()));
    let sent = relay
        .sink()
        .sent
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (
            &sent[0].stream,
            sent[0].revision,
            sent[0].event_id,
            sent[0].fence
        ),
        (
            &event.stream,
            event.revision,
            event.event_id,
            LeaseFence::new(7)
        )
    );
    assert_eq!(
        serde_json::to_string(&sent[0].directive).unwrap_or_else(|e| panic!("{e}")),
        CLONE_VM
    );
}

#[test]
fn should_let_the_event_pass_when_it_is_not_a_directive() {
    // given
    let relay = DirectiveRelay::new(Sink::default());
    let event = recorded("attempt_started", 1, "{}");

    // when
    let handled = handle(&relay, &event);

    // then
    assert_eq!(handled, Ok(()));
    assert!(
        relay
            .sink()
            .sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    );
}

#[test]
fn should_stop_on_the_directive_when_this_build_cannot_decode_it() {
    // given
    let relay = DirectiveRelay::new(Sink::default());
    let newer = recorded("directive_issued", 2, CLONE_VM);

    // when
    let handled = handle(&relay, &newer);

    // then
    assert_eq!(handled, Err(ConsumerError::new("directive_undecodable")));
}

#[test]
fn should_fail_with_the_sink_code_when_the_sink_refuses() {
    // given
    let relay = DirectiveRelay::new(Sink {
        refuse: Some("link_down"),
        ..Sink::default()
    });
    let event = recorded("directive_issued", 1, CLONE_VM);

    // when
    let handled = handle(&relay, &event);

    // then
    assert_eq!(handled, Err(ConsumerError::new("link_down")));
}
