//! The store's codec over the real registry: derived event ids and intent digests are frozen
//! by goldens (a change would make a retry across an upgrade look like a new command), every
//! refusal round-trips through its receipt, and stored events decode exactly.

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

use mayfly_control::journal::{AggregateName, CommandType, Payload, RecordedEvent, StreamRevision};
use mayfly_control::store::{
    EventDecodeError, decode_event, decode_rejection, derive_event_id, encode_event,
    encode_rejection, intent_digest, stream_of,
};
use mayfly_control_domain::attempt::AttemptEvent;
use mayfly_control_domain::{
    AggregateRegistry, AggregateVisitor, CommandBody, ControlAggregates, DeterminismCases,
    Rejection,
};
use mayfly_domain::{AttemptId, CommandId, EventId, Millis};

/// The instance every case of the intents golden is addressed to.
const INSTANCE: &str = "000000000000000000000000000000a1";

fn intents_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/intents")
}

#[test]
fn should_derive_the_golden_event_ids_when_a_command_id_is_given() {
    // given
    let inputs = [(0_u128, 0_u32), (1, 0), (1, 1), (u128::MAX, u32::MAX)];

    // when
    let ids: Vec<String> = inputs
        .iter()
        .map(|(c, o)| derive_event_id(CommandId::from_u128(*c), *o).to_string())
        .collect();

    // then
    assert_eq!(
        ids,
        [
            "c09f0559f729a2e0742c30e275b4c1f9",
            "115a9548db698f82cd6b34d49682605f",
            "5b56c8492944bf4c71e8cd481a53a27f",
            "8044d4857467844432438903748cfcbb",
        ]
    );
}

#[test]
fn should_hash_the_documented_layout_when_an_intent_is_digested() {
    // given
    let aggregate = AggregateName::new("counter").unwrap_or_else(|e| panic!("{e}"));
    let command_type = CommandType::new("increment").unwrap_or_else(|e| panic!("{e}"));
    let stream = stream_of::<crate::counter::Counter>("1").unwrap_or_else(|e| panic!("{e}"));
    let body = crate::counter::CounterBody::Increment { by: 1, limit: 9 };

    // when
    let digest =
        intent_digest(&aggregate, &command_type, &stream, &body).unwrap_or_else(|e| panic!("{e}"));

    // then: SHA-256 of "mayfly-intent-v1\ncounter\nincrement\ncounter-1\n" and the body's JSON,
    // computed outside Rust
    assert_eq!(
        digest.to_string(),
        "b8cc4c323325c8d09b9cfcc9f4e62164465b03754e08d99659ee7324c2db8e44"
    );
}

#[test]
fn should_match_the_intents_golden_when_registry_cases_are_digested() {
    // given
    struct Intents(Vec<(String, String)>);
    impl AggregateVisitor for Intents {
        fn visit<A: DeterminismCases>(&mut self) {
            let aggregate = AggregateName::new(A::NAME).unwrap_or_else(|e| panic!("{e}"));
            let stream = stream_of::<A>(INSTANCE).unwrap_or_else(|e| panic!("{e}"));
            let mut text = format!(
                "# {}: command type and intent digest of each determinism case, addressed to {stream}\n",
                A::NAME
            );
            let mut cases = A::determinism_cases();
            cases.sort_by_key(|c| c.name);
            for case in cases {
                let body = &case.command.body;
                let command_type =
                    CommandType::new(body.command_type()).unwrap_or_else(|e| panic!("{e}"));
                let digest = intent_digest(&aggregate, &command_type, &stream, body)
                    .unwrap_or_else(|e| panic!("{e}"));
                let _ = writeln!(text, "case {} {command_type} {digest}", case.name);
            }
            self.0.push((A::NAME.to_owned(), text));
        }
    }
    let mut intents = Intents(Vec::new());

    // when
    ControlAggregates::accept(&mut intents);

    // then
    let mut problems = Vec::new();
    for (aggregate, actual) in intents.0 {
        let path = intents_dir().join(format!("{aggregate}.txt"));
        match fs::read_to_string(&path) {
            Ok(expected) if expected == actual => {}
            Ok(expected) => problems.push(format!(
                "{} differs; a changed digest makes a retried command look new, so change it only \
                 deliberately:\n--- golden\n{expected}--- run\n{actual}",
                path.display()
            )),
            Err(e) => problems.push(format!(
                "{}: {e}; if these digests are intended, commit it as:\n{actual}",
                path.display()
            )),
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn should_round_trip_every_refusal_when_registry_cases_are_encoded() {
    // given
    struct RoundTrips(Vec<String>);
    impl AggregateVisitor for RoundTrips {
        fn visit<A: DeterminismCases>(&mut self) {
            for case in A::determinism_cases() {
                let state = mayfly_control_domain::replay::<A>(&case.history);
                if let Err(rejection) = A::decide(&state, case.command) {
                    let encoded = encode_rejection(&rejection).unwrap_or_else(|e| panic!("{e}"));
                    let decoded: Result<A::Rejection, _> = decode_rejection(&encoded);
                    if decoded.as_ref().ok() != Some(&rejection) {
                        self.0
                            .push(format!("{}/{}: {decoded:?}", A::NAME, case.name));
                    }
                }
            }
        }
    }
    let mut trips = RoundTrips(Vec::new());

    // when
    ControlAggregates::accept(&mut trips);

    // then
    assert!(trips.0.is_empty(), "{:?}", trips.0);
}

#[test]
fn should_encode_attempt_refusals_as_their_codes_when_recorded() {
    // given
    let codes = <mayfly_control_domain::attempt::AttemptRejection as Rejection>::CODES;

    // when
    let decoded: Vec<Result<mayfly_control_domain::attempt::AttemptRejection, _>> = codes
        .iter()
        .map(|code| decode_rejection(&Payload::new(format!("\"{code}\""))))
        .collect();

    // then
    for (code, rejection) in codes.iter().zip(decoded) {
        let rejection = rejection.unwrap_or_else(|e| panic!("{code}: {e}"));
        assert_eq!(rejection.code(), *code);
        assert_eq!(
            encode_rejection(&rejection).map(Payload::into_string).ok(),
            Some(format!("\"{code}\""))
        );
    }
}

#[test]
fn should_decode_exactly_what_it_encodes_when_an_event_is_stored() {
    // given
    let events: Vec<AttemptEvent> =
        <mayfly_control_domain::attempt::Attempt as DeterminismCases>::determinism_cases()
            .into_iter()
            .flat_map(|c| c.history)
            .collect();
    let stream = stream_of::<mayfly_control_domain::attempt::Attempt>(
        &AttemptId::from_u128(0xa1).to_string(),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let recorded = |event: &AttemptEvent, payload: Option<&str>| {
        let new = encode_event(event, EventId::from_u128(1)).unwrap_or_else(|e| panic!("{e}"));
        RecordedEvent {
            stream: stream.clone(),
            revision: StreamRevision::FIRST,
            event_id: new.event_id,
            kind: new.kind,
            payload: payload.map_or(new.payload, Payload::new),
            recorded_at: Millis::new(1),
        }
    };

    // when
    let decoded: Vec<Result<AttemptEvent, EventDecodeError>> = events
        .iter()
        .map(|e| decode_event(&recorded(e, None)))
        .collect();
    let first = &events[0];
    let mut unknown = recorded(first, None);
    unknown.kind.schema_version = 99;
    let trailing = recorded(
        first,
        Some(&format!("{} {{}}", recorded(first, None).payload.as_str())),
    );

    // then
    assert!(!events.is_empty());
    assert!(
        events
            .iter()
            .zip(&decoded)
            .all(|(e, d)| d.as_ref() == Ok(e)),
        "{decoded:?}"
    );
    assert_eq!(
        decode_event::<AttemptEvent>(&unknown),
        Err(EventDecodeError::UnknownKind)
    );
    assert_eq!(
        decode_event::<AttemptEvent>(&trailing),
        Err(EventDecodeError::Malformed)
    );
}
