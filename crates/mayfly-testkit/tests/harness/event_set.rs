//! The codec `event_set!` generates, driven through serde_json.

use mayfly_control_domain::{
    Aggregate, DecodeError, DeterminismCases, EventKind, EventSet, Rejection, known_kinds, replay,
};
use mayfly_testkit::golden::{decode, encode};

use crate::support::{
    Door, DoorClosed, DoorEvent, DoorOpened, DoorRejection, Doors, Gate, GateEvent, GateSwung,
};

#[test]
fn should_refuse_unknown_kind_when_type_or_version_is_not_registered() {
    // given
    let unknown_type = ("door_slammed", 1);
    let newer_version = ("door_opened", 3);
    // when
    let results = [unknown_type, newer_version]
        .map(|(event_type, version)| decode::<DoorEvent>(event_type, version, "{}"));
    // then
    assert!(matches!(
        &results[0],
        Err(DecodeError::UnknownKind { event_type, schema_version: 1 }) if event_type == "door_slammed"
    ));
    assert!(matches!(
        &results[1],
        Err(DecodeError::UnknownKind { event_type, schema_version: 3 }) if event_type == "door_opened"
    ));
}

#[test]
fn should_report_malformed_when_payload_has_the_wrong_shape() {
    // given
    let payload = "{\"by\": -1}";
    // when
    let result = decode::<DoorEvent>("door_opened", 2, payload);
    // then
    assert!(
        matches!(result, Err(DecodeError::Malformed(_))),
        "{result:?}"
    );
}

#[test]
fn should_refuse_trailing_data_when_payload_is_followed_by_more() {
    // given
    let payload = "{} {}";
    // when
    let result = decode::<DoorEvent>("door_closed", 1, payload);
    // then
    assert!(
        matches!(result, Err(DecodeError::Malformed(_))),
        "{result:?}"
    );
}

#[test]
fn should_upcast_on_load_when_payload_is_a_legacy_version() {
    // given
    let legacy = "{}";
    // when
    let event = decode::<DoorEvent>("door_opened", 1, legacy).unwrap();
    // then
    assert_eq!(event, DoorEvent::Opened(DoorOpened { by: 0 }));
    assert_eq!(
        event.kind(),
        EventKind {
            event_type: "door_opened",
            schema_version: 2
        }
    );
}

#[test]
fn should_write_the_payload_alone_when_event_is_encoded() {
    // given
    let event = DoorEvent::Opened(DoorOpened { by: 4 });
    // when
    let json = encode(&event).unwrap();
    // then
    assert_eq!(json, "{\n  \"by\": 4\n}\n");
}

#[test]
fn should_list_current_and_legacy_kinds_when_registry_is_walked() {
    // given
    let registry = known_kinds::<Doors>;
    // when
    let kinds: Vec<String> = registry().iter().map(ToString::to_string).collect();
    // then
    assert_eq!(
        kinds,
        vec!["door_opened/v2", "door_closed/v1", "door_opened/v1"]
    );
}

#[test]
fn should_fold_history_in_order_when_stream_is_replayed() {
    // given
    let history = [
        DoorEvent::Opened(DoorOpened { by: 1 }),
        DoorEvent::Closed(DoorClosed { reason: None }),
        DoorEvent::Opened(DoorOpened { by: 2 }),
    ];
    // when
    let open = replay::<Door>(&history);
    let closed = replay::<Door>(&history[..2]);
    // then
    assert!(open);
    assert!(!closed);
}

#[test]
fn should_expose_stable_codes_when_rejections_are_named() {
    // given
    let rejections = [DoorRejection::AlreadyOpen, DoorRejection::AlreadyClosed];
    // when
    let codes = rejections.map(|r| r.code());
    // then
    assert_eq!(codes, ["already_open", "already_closed"]);
    assert_eq!(codes, DoorRejection::CODES);
}

#[test]
fn should_decide_from_command_alone_when_aggregate_has_no_state() {
    // given
    let command = crate::support::command(crate::support::Nothing);
    // when
    let events = Gate::decide(&Gate::initial(), command);
    // then
    assert_eq!(events, Ok(vec![GateEvent::Swung(GateSwung {})]));
}

#[test]
fn should_trace_current_and_legacy_kinds_and_no_other_when_shape_is_asked() {
    // given
    let legacy = EventKind {
        event_type: "door_opened",
        schema_version: 1,
    };
    let unknown = EventKind {
        event_type: "door_slammed",
        schema_version: 1,
    };
    // when
    let shapes = [legacy, unknown].map(DoorEvent::shape);
    // then
    assert!(shapes[0].as_ref().is_some_and(|s| s.atoms().is_empty()));
    assert_eq!(shapes[1], None);
}

#[test]
fn should_show_aggregate_name_and_history_when_case_is_debug_formatted() {
    // given
    let case = Door::determinism_cases().swap_remove(1);
    // when
    let shown = format!("{:?}", case.clone());
    // then
    assert!(
        shown.starts_with("DeterminismCase { aggregate: \"door\", name: \"close_open_door\""),
        "{shown}"
    );
    assert!(shown.contains("Opened(DoorOpened { by: 1 })"), "{shown}");
}
