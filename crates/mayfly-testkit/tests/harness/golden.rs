//! The golden-fixture harness against fixture trees that match and break the registry.

use std::fs;
use std::path::{Path, PathBuf};

use mayfly_control_domain::EventKind;
use mayfly_testkit::golden::{GoldenFailure, GoldenSummary, verify};
use tempfile::TempDir;

use crate::support::{
    DOOR_CLOSED_V1, DOOR_OPENED_V1, DOOR_OPENED_V1_EXPECTED, DOOR_OPENED_V2, Doors, DoorsAndFlaky,
    Gates, NoAggregates,
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
    ]
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
            legacy: 1
        })
    );
}

#[test]
fn should_fail_when_a_registered_kind_has_no_fixture() {
    // given
    let files: Vec<_> = complete()
        .into_iter()
        .filter(|(path, _)| *path != "door_closed/v1.json")
        .collect();
    let dir = tree(&files);
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
fn should_fail_when_fixtures_exist_but_nothing_is_registered() {
    // given
    let dir = tree(&complete());
    // when
    let failures = verify::<NoAggregates>(dir.path()).unwrap_err().0;
    // then
    assert_eq!(failures.len(), 4, "{failures:?}");
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
            legacy: 0
        })
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
    let files: Vec<_> = complete()
        .into_iter()
        .filter(|(path, _)| *path != "door_opened/v1.expected.json")
        .collect();
    let dir = tree(&files);
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
