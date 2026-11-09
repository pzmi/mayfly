//! The golden-fixture harness against fixture trees that match and break the registry.

use std::fs;
use std::path::{Path, PathBuf};

use mayfly_control_domain::EventKind;
use mayfly_control_domain::shape::{Atom, Gap, trace};
use mayfly_testkit::golden::{GoldenFailure, GoldenFailures, GoldenSummary, observed, verify};
use tempfile::TempDir;

use crate::support::{
    DOOR_CLOSED_V1, DOOR_CLOSED_V1_HAND, DOOR_CLOSED_V1_WIND, DOOR_OPENED_V1,
    DOOR_OPENED_V1_EXPECTED, DOOR_OPENED_V2, Doors, DoorsAndFlaky, DoorsTwice, Gates, Hatches,
    Jammed, Ladders, Latches, Malformeds, Misrouted, NoAggregates, Shapelesses, SignalPosts, Twins,
};

const OPENED_V2: EventKind = EventKind {
    event_type: "door_opened",
    schema_version: 2,
};
const OPENED_V1: EventKind = EventKind {
    event_type: "door_opened",
    schema_version: 1,
};
const CLOSED_V1: EventKind = EventKind {
    event_type: "door_closed",
    schema_version: 1,
};

/// A fixture tree with `files` (relative path, content) written under a fresh tempdir.
fn tree(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, content) in files {
        let path = dir.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    dir
}

/// Door's complete fixture set.
fn complete() -> Vec<(&'static str, &'static str)> {
    vec![
        ("door_opened/v2.json", DOOR_OPENED_V2),
        ("door_opened/v1.json", DOOR_OPENED_V1),
        ("door_opened/v1.expected.json", DOOR_OPENED_V1_EXPECTED),
        ("door_closed/v1.json", DOOR_CLOSED_V1),
        ("door_closed/v1.wind.json", DOOR_CLOSED_V1_WIND),
        ("door_closed/v1.hand.json", DOOR_CLOSED_V1_HAND),
    ]
}

fn without(
    files: Vec<(&'static str, &'static str)>,
    gone: &[&str],
) -> Vec<(&'static str, &'static str)> {
    files
        .into_iter()
        .filter(|(path, _)| !gone.contains(path))
        .collect()
}

fn at(root: &Path, path: &str) -> PathBuf {
    root.join(path)
}

#[test]
fn should_pass_when_every_kind_has_its_byte_identical_fixture() {
    // given
    let dir = tree(&complete());
    // when
    let summary = verify::<Doors>(dir.path());
    // then
    assert_eq!(
        summary,
        Ok(GoldenSummary {
            aggregates: 1,
            current: 2,
            legacy: 1,
            samples: 5,
        })
    );
}

#[test]
fn should_fail_when_a_registered_kind_has_no_fixture() {
    // given
    let dir = tree(&without(complete(), &["door_closed/v1.json"]));
    // when
    let failures = verify::<Doors>(dir.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![GoldenFailure::MissingFixture {
            kind: CLOSED_V1,
            path: at(dir.path(), "door_closed/v1.json"),
        }]
    );
}

#[test]
fn should_report_every_kind_missing_when_root_does_not_exist() {
    // given
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("no-such-dir");
    // when
    let failures = verify::<Doors>(&root).unwrap_err().0;
    // then
    let missing: Vec<_> = failures
        .iter()
        .map(|f| match f {
            GoldenFailure::MissingFixture { kind, .. }
            | GoldenFailure::MissingUpcastExpectation { kind, .. } => *kind,
            other => panic!("unexpected failure {other:?}"),
        })
        .collect();
    assert_eq!(missing, vec![OPENED_V2, CLOSED_V1, OPENED_V1, OPENED_V1]);
}

#[test]
fn should_fail_when_a_fixture_is_not_registered() {
    // given
    let mut files = complete();
    files.push(("door_slammed/v1.json", "{}\n"));
    files.push(("door_closed/v2.json", "{}\n"));
    files.push(("README.md", "notes\n"));
    let dir = tree(&files);
    // when
    let failures = verify::<Doors>(dir.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![
            GoldenFailure::UnregisteredFixture {
                path: at(dir.path(), "README.md")
            },
            GoldenFailure::UnregisteredFixture {
                path: at(dir.path(), "door_closed/v2.json")
            },
            GoldenFailure::UnregisteredFixture {
                path: at(dir.path(), "door_slammed/v1.json")
            },
        ]
    );
}

#[test]
fn should_fail_when_a_fixture_name_is_outside_the_layout() {
    // given
    let mut files = complete();
    files.push(("door_opened/v02.json", DOOR_OPENED_V2));
    files.push(("door_opened/v2.Wide.json", DOOR_OPENED_V2));
    files.push(("door_opened/v2.expected.expected.json", DOOR_OPENED_V2));
    files.push(("door_opened/extra/v2.json", DOOR_OPENED_V2));
    let dir = tree(&files);
    // when
    let failures = verify::<Doors>(dir.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        [
            "door_opened/extra/v2.json",
            "door_opened/v02.json",
            "door_opened/v2.Wide.json",
            "door_opened/v2.expected.expected.json",
        ]
        .map(|path| GoldenFailure::UnregisteredFixture {
            path: at(dir.path(), path)
        })
    );
}

#[test]
fn should_fail_when_an_expectation_has_no_sample() {
    // given
    let mut files = complete();
    files.push(("door_opened/v1.lost.expected.json", DOOR_OPENED_V1_EXPECTED));
    files.push(("door_closed/v1.expected.json", DOOR_CLOSED_V1));
    let dir = tree(&files);
    // when
    let failures = verify::<Doors>(dir.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![
            GoldenFailure::UnregisteredFixture {
                path: at(dir.path(), "door_closed/v1.expected.json")
            },
            GoldenFailure::UnregisteredFixture {
                path: at(dir.path(), "door_opened/v1.lost.expected.json")
            },
        ]
    );
}

#[test]
fn should_fail_when_fixtures_exist_but_nothing_is_registered() {
    // given
    let dir = tree(&complete());
    // when
    let failures = verify::<NoAggregates>(dir.path()).unwrap_err().0;
    // then
    assert_eq!(failures.len(), complete().len(), "{failures:?}");
    assert!(
        failures
            .iter()
            .all(|f| matches!(f, GoldenFailure::UnregisteredFixture { .. }))
    );
}

#[test]
fn should_pass_vacuously_when_nothing_is_registered_and_root_is_absent() {
    // given
    let dir = tempfile::tempdir().unwrap();
    // when
    let summary = verify::<NoAggregates>(&dir.path().join("events"));
    // then
    assert_eq!(
        summary,
        Ok(GoldenSummary {
            aggregates: 0,
            current: 0,
            legacy: 0,
            samples: 0,
        })
    );
}

#[test]
fn should_fail_when_root_is_not_a_directory() {
    // given
    let dir = tree(&[("events", "not a directory\n")]);
    let root = dir.path().join("events");
    // when
    let failures = verify::<NoAggregates>(&root).unwrap_err().0;
    // then
    assert!(
        matches!(
            failures.as_slice(),
            [GoldenFailure::UnreadableDir { path, .. }] if *path == root
        ),
        "{failures:?}"
    );
}

#[test]
fn should_fail_when_a_fixture_is_not_utf8() {
    // given
    let dir = tree(&complete());
    let path = at(dir.path(), "door_closed/v1.json");
    fs::write(&path, [0xff, 0xfe, b'\n']).unwrap();
    // when
    let failures = verify::<Doors>(dir.path()).unwrap_err().0;
    // then
    assert!(
        matches!(
            failures.as_slice(),
            [GoldenFailure::Unreadable { path: p, .. }] if *p == path
        ),
        "{failures:?}"
    );
}

#[test]
fn should_fail_when_fixture_does_not_reencode_byte_for_byte() {
    // given
    let mut files = complete();
    files[0] = ("door_opened/v2.json", "{\"by\": 4}\n");
    let dir = tree(&files);
    // when
    let failures = verify::<Doors>(dir.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![GoldenFailure::NotByteIdentical {
            kind: OPENED_V2,
            path: at(dir.path(), "door_opened/v2.json"),
            expected: "{\"by\": 4}\n".to_owned(),
            actual: DOOR_OPENED_V2.to_owned(),
        }]
    );
}

#[test]
fn should_fail_when_an_extra_sample_does_not_reencode_byte_for_byte() {
    // given
    let mut files = complete();
    files[4] = ("door_closed/v1.wind.json", "{\"reason\":\"wind\"}\n");
    let dir = tree(&files);
    // when
    let failures = verify::<Doors>(dir.path()).unwrap_err().0;
    // then
    assert!(
        matches!(
            failures.as_slice(),
            [GoldenFailure::NotByteIdentical { kind, path, actual, .. }]
                if *kind == CLOSED_V1
                    && *path == at(dir.path(), "door_closed/v1.wind.json")
                    && actual == DOOR_CLOSED_V1_WIND
        ),
        "{failures:?}"
    );
}

#[test]
fn should_fail_when_fixture_has_a_field_the_event_drops() {
    // given
    let mut files = complete();
    files[3] = ("door_closed/v1.json", "{\n  \"slammed\": true\n}\n");
    let dir = tree(&files);
    // when
    let failures = verify::<Doors>(dir.path()).unwrap_err().0;
    // then
    assert!(
        matches!(
            failures.as_slice(),
            [GoldenFailure::NotByteIdentical { kind, .. }] if *kind == CLOSED_V1
        ),
        "{failures:?}"
    );
}

#[test]
fn should_fail_when_a_sample_holds_a_field_the_event_no_longer_has() {
    // given: `reason` was renamed `cause`, as the only sample of `wind` still says
    let mut files = complete();
    files[4] = ("door_closed/v1.wind.json", "{\n  \"cause\": \"wind\"\n}\n");
    let dir = tree(&files);
    // when
    let failures = verify::<Doors>(dir.path()).unwrap_err().0;
    // then
    assert!(
        matches!(
            failures.as_slice(),
            [
                GoldenFailure::NotByteIdentical { kind, .. },
                GoldenFailure::Uncovered { missing, .. },
            ] if *kind == CLOSED_V1 && *missing == ["variant `reason::wind`"]
        ),
        "{failures:?}"
    );
}

#[test]
fn should_fail_when_an_optional_field_is_in_no_sample() {
    // given
    let files = without(
        complete(),
        &["door_closed/v1.wind.json", "door_closed/v1.hand.json"],
    );
    let dir = tree(&files);
    // when
    let failures = verify::<Doors>(dir.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![GoldenFailure::Uncovered {
            kind: CLOSED_V1,
            missing: vec![
                "field `reason`".to_owned(),
                "field `reason::hand.by`".to_owned(),
                "variant `reason::hand`".to_owned(),
                "variant `reason::wind`".to_owned(),
            ],
        }]
    );
}

#[test]
fn should_fail_when_a_variant_is_in_no_sample() {
    // given
    let dir = tree(&without(complete(), &["door_closed/v1.hand.json"]));
    // when
    let failures = verify::<Doors>(dir.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![GoldenFailure::Uncovered {
            kind: CLOSED_V1,
            missing: vec![
                "field `reason::hand.by`".to_owned(),
                "variant `reason::hand`".to_owned(),
            ],
        }]
    );
}

#[test]
fn should_fail_when_payload_cannot_be_traced() {
    // given
    let dir = tree(&[(
        "signalled/v1.json",
        "{\n  \"signal\": {\n    \"kind\": \"bell\"\n  }\n}\n",
    )]);
    // when
    let failures = verify::<SignalPosts>(dir.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![GoldenFailure::Untraceable {
            kind: EventKind {
                event_type: "signalled",
                schema_version: 1
            },
            gaps: vec![("signal".to_owned(), Gap::SelfDescribing)],
        }]
    );
}

#[test]
fn should_fail_when_the_event_set_has_no_shape_for_a_registered_kind() {
    // given
    let dir = tree(&complete());
    // when
    let failures = verify::<Shapelesses>(dir.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![
            GoldenFailure::NoShape { kind: OPENED_V2 },
            GoldenFailure::NoShape { kind: CLOSED_V1 },
            GoldenFailure::NoShape { kind: OPENED_V1 },
        ]
    );
}

const CLIMBED_V1: EventKind = EventKind {
    event_type: "rung_climbed",
    schema_version: 1,
};

#[test]
fn should_refuse_an_alias_when_an_event_declares_one() {
    // given: the alias needs a sample, and a sample that uses it cannot re-encode as itself
    let without_alias = tree(&[("rung_climbed/v1.json", "{\n  \"rung\": 1\n}\n")]);
    let with_alias = tree(&[
        ("rung_climbed/v1.json", "{\n  \"rung\": 1\n}\n"),
        ("rung_climbed/v1.old.json", "{\n  \"step\": 1\n}\n"),
    ]);
    // when
    let uncovered = verify::<Ladders>(without_alias.path()).unwrap_err().0;
    let rewritten = verify::<Ladders>(with_alias.path()).unwrap_err().0;
    // then
    assert_eq!(
        uncovered,
        vec![GoldenFailure::Uncovered {
            kind: CLIMBED_V1,
            missing: vec!["field `step`".to_owned()],
        }]
    );
    assert!(
        matches!(
            rewritten.as_slice(),
            [GoldenFailure::NotByteIdentical { kind, path, .. }]
                if *kind == CLIMBED_V1 && *path == at(with_alias.path(), "rung_climbed/v1.old.json")
        ),
        "{rewritten:?}"
    );
}

#[test]
fn should_fail_when_fixture_does_not_decode() {
    // given
    let mut files = complete();
    files[0] = ("door_opened/v2.json", "{\n  \"by\": \"four\"\n}\n");
    let dir = tree(&files);
    // when
    let failures = verify::<Doors>(dir.path()).unwrap_err().0;
    // then
    assert!(
        matches!(
            failures.as_slice(),
            [GoldenFailure::Undecodable { kind, .. }] if *kind == OPENED_V2
        ),
        "{failures:?}"
    );
}

#[test]
fn should_fail_when_legacy_fixture_has_no_upcast_expectation() {
    // given
    let dir = tree(&without(complete(), &["door_opened/v1.expected.json"]));
    // when
    let failures = verify::<Doors>(dir.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![GoldenFailure::MissingUpcastExpectation {
            kind: OPENED_V1,
            path: at(dir.path(), "door_opened/v1.expected.json"),
        }]
    );
}

#[test]
fn should_upcast_every_legacy_sample_when_each_has_its_expectation() {
    // given
    let mut files = complete();
    files.push(("door_opened/v1.again.json", DOOR_OPENED_V1));
    files.push((
        "door_opened/v1.again.expected.json",
        DOOR_OPENED_V1_EXPECTED,
    ));
    let dir = tree(&files);
    // when
    let summary = verify::<Doors>(dir.path()).map(|s| s.samples);
    // then
    assert_eq!(summary, Ok(6));
}

#[test]
fn should_fail_when_legacy_extra_sample_has_no_upcast_expectation() {
    // given
    let mut files = complete();
    files.push(("door_opened/v1.again.json", DOOR_OPENED_V1));
    let dir = tree(&files);
    // when
    let failures = verify::<Doors>(dir.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![GoldenFailure::MissingUpcastExpectation {
            kind: OPENED_V1,
            path: at(dir.path(), "door_opened/v1.again.expected.json"),
        }]
    );
}

#[test]
fn should_fail_when_upcast_differs_from_its_expectation() {
    // given
    let mut files = complete();
    files[2] = ("door_opened/v1.expected.json", "{\n  \"by\": 9\n}\n");
    let dir = tree(&files);
    // when
    let failures = verify::<Doors>(dir.path()).unwrap_err().0;
    // then
    assert!(
        matches!(
            failures.as_slice(),
            [GoldenFailure::NotByteIdentical { kind, path, .. }]
                if *kind == OPENED_V1 && *path == at(dir.path(), "door_opened/v1.expected.json")
        ),
        "{failures:?}"
    );
}

#[test]
fn should_fail_when_upcast_lands_on_another_kind() {
    // given: the expectation even matches what the misrouted upcast writes
    let mut files = complete();
    files[2] = ("door_opened/v1.expected.json", DOOR_CLOSED_V1);
    let dir = tree(&files);
    // when
    let failures = verify::<Misrouted>(dir.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![GoldenFailure::KindMismatch {
            kind: OPENED_V1,
            path: at(dir.path(), "door_opened/v1.json"),
            expected: OPENED_V2,
            decoded: CLOSED_V1,
        }]
    );
}

#[test]
fn should_fail_when_evolve_panics_on_a_sample() {
    // given
    let dir = tree(&complete());
    // when
    let failures = verify::<Jammed>(dir.path()).unwrap_err().0;
    // then
    let jammed: Vec<_> = failures
        .iter()
        .map(|f| match f {
            GoldenFailure::EvolvePanicked {
                aggregate: "jammed_door",
                kind,
                path,
                message,
            } if *kind == CLOSED_V1 && message == "jammed" => {
                path.strip_prefix(dir.path()).unwrap().to_owned()
            }
            other => panic!("unexpected failure {other:?}"),
        })
        .collect();
    assert_eq!(
        jammed,
        [
            "door_closed/v1.json",
            "door_closed/v1.hand.json",
            "door_closed/v1.wind.json"
        ]
        .map(PathBuf::from)
    );
}

#[test]
fn should_fail_when_two_aggregates_register_the_same_kinds() {
    // given
    let dir = tree(&complete());
    // when
    let failures = verify::<DoorsAndFlaky>(dir.path()).unwrap_err().0;
    // then
    let duplicated: Vec<_> = failures
        .iter()
        .map(|f| match f {
            GoldenFailure::Duplicate { what } => what.as_str(),
            other => panic!("unexpected failure {other:?}"),
        })
        .collect();
    assert_eq!(
        duplicated,
        vec![
            "event kind door_opened/v2",
            "event kind door_closed/v1",
            "event kind door_opened/v1",
        ]
    );
}

#[test]
fn should_fail_when_a_kind_is_invalid() {
    // given
    let dir = tree(&[("Door-Opened/v1.json", "{}\n"), ("zeroed/v0.json", "{}\n")]);
    // when
    let failures = verify::<Malformeds>(dir.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![
            GoldenFailure::InvalidKind {
                kind: EventKind {
                    event_type: "Door-Opened",
                    schema_version: 1
                }
            },
            GoldenFailure::InvalidKind {
                kind: EventKind {
                    event_type: "zeroed",
                    schema_version: 0
                }
            },
        ]
    );
}

#[test]
fn should_fail_when_version_chain_has_a_gap() {
    // given
    let dir = tree(&[
        ("gate_swung/v3.json", "{}\n"),
        ("gate_swung/v1.json", "{}\n"),
        ("gate_swung/v1.expected.json", "{}\n"),
    ]);
    // when
    let failures = verify::<Gates>(dir.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![GoldenFailure::VersionChain {
            event_type: "gate_swung",
            detail: "versions [2] below the current version 3 have no upcaster".to_owned(),
        }]
    );
}

#[test]
fn should_fail_when_a_sample_needs_more_than_the_aggregate_supports() {
    // given
    let dir = tree(&[("hatch_sealed/v1.json", "{}\n")]);
    // when
    let failures = verify::<Hatches>(dir.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![GoldenFailure::EpochAboveSupported {
            aggregate: "hatch",
            kind: EventKind {
                event_type: "hatch_sealed",
                schema_version: 1,
            },
            path: at(dir.path(), "hatch_sealed/v1.json"),
            required: 2,
            supported: 1,
        }]
    );
}

#[test]
fn should_fail_when_legacy_version_is_not_older_than_current() {
    // given
    let dir = tree(&[
        ("latch_set/v2.json", "{}\n"),
        ("latch_set/v1.json", "{}\n"),
        ("latch_set/v1.expected.json", "{}\n"),
        ("latch_set/v3.json", "{}\n"),
        ("latch_set/v3.expected.json", "{}\n"),
    ]);
    // when
    let failures = verify::<Latches>(dir.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![GoldenFailure::VersionChain {
            event_type: "latch_set",
            detail: "legacy version 3 is not older than the current version 2".to_owned(),
        }]
    );
}

#[test]
fn should_fail_when_a_type_has_more_than_one_current_version() {
    // given
    let dir = tree(&[("latch_set/v1.json", "{}\n"), ("latch_set/v2.json", "{}\n")]);
    // when
    let failures = verify::<Twins>(dir.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![GoldenFailure::VersionChain {
            event_type: "latch_set",
            detail: "expected exactly one current version, found [1, 2]".to_owned(),
        }]
    );
}

#[test]
fn should_fail_when_two_aggregates_share_a_name() {
    // given
    let dir = tree(&complete());
    // when
    let failures = verify::<DoorsTwice>(dir.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures.first(),
        Some(&GoldenFailure::Duplicate {
            what: "aggregate door".to_owned()
        }),
        "{failures:?}"
    );
}

#[derive(serde::Deserialize, PartialEq)]
struct Inventory {
    items: Vec<crate::support::CloseReason>,
    labels: std::collections::BTreeMap<String, crate::support::CloseReason>,
}

#[test]
fn should_observe_elements_and_map_values_when_samples_hold_collections() {
    // given
    let shape = trace::<Inventory>();
    let sample = serde_json::json!({
        "items": ["wind"],
        "labels": { "front": { "hand": { "by": 1 } } },
    });
    // when
    let seen = observed(&shape, &[sample]);
    // then
    let missing: Vec<String> = shape
        .atoms()
        .difference(&seen)
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        missing,
        [
            "field `items[]::hand.by`",
            "variant `items[]::hand`",
            "variant `labels{}::wind`",
        ]
    );
    assert!(seen.contains(&Atom::Field("labels{}::hand.by".to_owned())));
}

#[test]
fn should_name_what_to_fix_when_each_failure_is_displayed() {
    // given
    let path = PathBuf::from("events/door_opened/v1.json");
    let failures = GoldenFailures(vec![
        GoldenFailure::UnreadableDir {
            path: path.clone(),
            error: "not a directory".to_owned(),
        },
        GoldenFailure::UnreadableEntry {
            path: path.clone(),
            error: "gone".to_owned(),
        },
        GoldenFailure::Unreadable {
            path: path.clone(),
            error: "invalid UTF-8".to_owned(),
        },
        GoldenFailure::KindMismatch {
            kind: OPENED_V1,
            path: path.clone(),
            expected: OPENED_V2,
            decoded: CLOSED_V1,
        },
        GoldenFailure::EvolvePanicked {
            aggregate: "door",
            kind: CLOSED_V1,
            path: path.clone(),
            message: "jammed".to_owned(),
        },
        GoldenFailure::Uncovered {
            kind: CLOSED_V1,
            missing: vec!["variant `reason::wind`".to_owned()],
        },
        GoldenFailure::Untraceable {
            kind: CLOSED_V1,
            gaps: vec![
                (String::new(), Gap::NotConverged),
                ("mode".to_owned(), Gap::OpaqueLeaf),
            ],
        },
        GoldenFailure::NoShape { kind: CLOSED_V1 },
        GoldenFailure::EpochAboveSupported {
            aggregate: "door",
            kind: CLOSED_V1,
            path: path.clone(),
            required: 2,
            supported: 1,
        },
    ]);
    // when
    let report = failures.to_string();
    // then
    for expected in [
        "9 golden fixture failure(s)",
        "door: door_closed/v1 (events/door_opened/v1.json) needs epoch 2, above the supported 1",
        "cannot list events/door_opened/v1.json: not a directory",
        "cannot read an entry of events/door_opened/v1.json: gone",
        "cannot read events/door_opened/v1.json: invalid UTF-8",
        "legacy door_opened/v1 (events/door_opened/v1.json) upcasts to door_closed/v1, not to the current door_opened/v2",
        "door: evolve(initial, door_closed/v1) panicked on events/door_opened/v1.json: jammed",
        "door_closed/v1: no sample shows variant `reason::wind`",
        "door_closed/v1: the payload cannot be traced: `<root>` did not settle",
        "; `mode` is an opaque leaf",
        "door_closed/v1: EventSet::shape has no shape for a registered kind",
    ] {
        assert!(report.contains(expected), "{expected:?} not in {report}");
    }
}
