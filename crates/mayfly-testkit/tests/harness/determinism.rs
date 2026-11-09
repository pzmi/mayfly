//! The determinism harness against aggregates that keep and break its contract.

use std::fs;
use std::path::Path;

use mayfly_control_domain::Expect;
use mayfly_testkit::determinism::{
    DeterminismFailure, DeterminismFailures, DeterminismSummary, assert_deterministic, verify,
};
use tempfile::TempDir;

use crate::support::{
    DOOR_DECISIONS, Doors, DoorsAndBoastful, DoorsAndConfused, DoorsAndDrifting, DoorsAndEcho,
    DoorsAndFlaky, DoorsAndForgetful, DoorsAndGarbled, DoorsAndGrudging, DoorsAndMangled,
    DoorsAndMoody, DoorsAndMuted, DoorsAndOptimist, DoorsAndSilent, DoorsAndTaciturn,
    DoorsAndTally, DoorsAndUnlisted, DoorsAndVeiled, DoorsAndWordy, DoorsTwice, NoAggregates,
};

/// A decisions directory holding `files` (name, content).
fn decisions_with(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, content) in files {
        fs::write(dir.path().join(name), content).unwrap();
    }
    dir
}

/// A decisions directory holding `Door`'s golden only.
fn decisions() -> TempDir {
    decisions_with(&[("door.txt", DOOR_DECISIONS)])
}

/// The aggregate and case of each failure, which must all be `NonDeterministicDecide`.
fn nondeterministic_decides(failures: &[DeterminismFailure]) -> Vec<(&'static str, &'static str)> {
    failures
        .iter()
        .map(|f| match f {
            DeterminismFailure::NonDeterministicDecide {
                aggregate, case, ..
            } => (*aggregate, *case),
            other => panic!("unexpected failure {other:?}"),
        })
        .collect()
}

/// The aggregate and case of each failure, which must all be `NonDeterministicEvolve`.
fn nondeterministic_evolves(failures: &[DeterminismFailure]) -> Vec<(&'static str, &'static str)> {
    failures
        .iter()
        .map(|f| match f {
            DeterminismFailure::NonDeterministicEvolve {
                aggregate, case, ..
            } => (*aggregate, *case),
            other => panic!("unexpected failure {other:?}"),
        })
        .collect()
}

#[test]
fn should_pass_when_every_case_is_deterministic_and_as_declared() {
    // given
    let root = decisions();
    // when
    let summary = verify::<Doors>(root.path());
    // then
    assert_eq!(
        summary,
        Ok(DeterminismSummary {
            aggregates: 1,
            cases: 4
        })
    );
}

#[test]
fn should_pass_vacuously_when_registry_is_empty() {
    // given
    let root = tempfile::tempdir().unwrap();
    // when
    let summary = verify::<NoAggregates>(&root.path().join("decisions"));
    // then
    assert_eq!(
        summary,
        Ok(DeterminismSummary {
            aggregates: 0,
            cases: 0
        })
    );
}

#[test]
fn should_fail_when_decide_is_not_deterministic() {
    // given
    let root = decisions();
    // when
    let failures = verify::<DoorsAndFlaky>(root.path()).unwrap_err().0;
    // then
    assert_eq!(
        nondeterministic_decides(&failures),
        vec![
            ("flaky_door", "open_closed_door"),
            ("flaky_door", "close_open_door")
        ]
    );
}

#[test]
fn should_fail_when_refusal_is_not_deterministic() {
    // given
    let root = decisions();
    // when
    let failures = verify::<DoorsAndGrudging>(root.path()).unwrap_err().0;
    // then
    assert_eq!(
        nondeterministic_decides(&failures),
        vec![
            ("grudging_door", "open_open_door"),
            ("grudging_door", "close_closed_door")
        ]
    );
}

#[test]
fn should_fail_when_only_the_refusal_code_differs() {
    // given
    let root = decisions();
    // when
    let failures = verify::<DoorsAndMoody>(root.path()).unwrap_err().0;
    // then
    assert_eq!(
        nondeterministic_decides(&failures),
        vec![
            ("moody_door", "open_open_door"),
            ("moody_door", "close_closed_door")
        ]
    );
}

#[test]
fn should_fail_when_only_the_encoded_event_differs() {
    // given
    let root = decisions();
    // when
    let failures = verify::<DoorsAndMuted>(root.path()).unwrap_err().0;
    // then
    assert_eq!(
        nondeterministic_decides(&failures),
        vec![("muted_door", "open")]
    );
}

#[test]
fn should_fail_when_evolve_is_not_deterministic() {
    // given
    let root = decisions();
    // when
    let failures = verify::<DoorsAndDrifting>(root.path()).unwrap_err().0;
    // then
    assert_eq!(
        nondeterministic_evolves(&failures),
        vec![
            ("drifting_door", "open_closed_door"),
            ("drifting_door", "close_open_door"),
            ("drifting_door", "open_open_door"),
        ]
    );
}

#[test]
fn should_fail_when_state_equality_hides_a_field_decide_reads() {
    // given
    let root = decisions();
    // when
    let failures = verify::<DoorsAndVeiled>(root.path()).unwrap_err().0;
    // then
    assert_eq!(
        nondeterministic_evolves(&failures),
        vec![
            ("veiled_door", "open_closed_door"),
            ("veiled_door", "close_open_door"),
            ("veiled_door", "open_open_door"),
        ]
    );
}

#[test]
fn should_fail_when_aggregate_module_has_no_cases() {
    // given
    let root = decisions();
    // when
    let failures = verify::<DoorsAndSilent>(root.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![
            DeterminismFailure::NoAcceptedCase {
                aggregate: "silent_door"
            },
            DeterminismFailure::NoRefusedCase {
                aggregate: "silent_door"
            },
            DeterminismFailure::NoHistoryCase {
                aggregate: "silent_door"
            },
        ]
    );
}

#[test]
fn should_fail_when_aggregate_module_has_no_refused_case() {
    // given
    let root = decisions();
    // when
    let failures = verify::<DoorsAndOptimist>(root.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![DeterminismFailure::NoRefusedCase {
            aggregate: "optimist_door"
        }]
    );
}

#[test]
fn should_fail_when_no_case_has_a_history() {
    // given
    let root = decisions();
    // when
    let failures = verify::<DoorsAndForgetful>(root.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![DeterminismFailure::NoHistoryCase {
            aggregate: "forgetful_door"
        }]
    );
}

#[test]
fn should_fail_when_case_name_is_declared_twice() {
    // given
    let root = decisions();
    // when
    let failures = verify::<DoorsAndEcho>(root.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![DeterminismFailure::DuplicateCase {
            aggregate: "echo_door",
            case: "open_closed_door"
        }]
    );
}

#[test]
fn should_fail_when_case_declares_the_wrong_outcome() {
    // given
    let root = decisions();
    // when
    let failures = verify::<DoorsAndConfused>(root.path()).unwrap_err().0;
    // then
    let declared: Vec<_> = failures
        .iter()
        .map(|f| match f {
            DeterminismFailure::UnexpectedOutcome {
                aggregate: "confused_door",
                case,
                expected,
                ..
            } => (*case, *expected),
            other => panic!("unexpected failure {other:?}"),
        })
        .collect();
    assert_eq!(
        declared,
        vec![
            ("open_closed_door", Expect::Refused),
            ("close_open_door", Expect::Refused),
            ("open_open_door", Expect::Accepted),
            ("close_closed_door", Expect::Accepted),
        ]
    );
}

#[test]
fn should_fail_when_refusal_code_is_not_declared() {
    // given
    let root = decisions();
    // when
    let failures = verify::<DoorsAndTaciturn>(root.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![DeterminismFailure::UndeclaredCode {
            aggregate: "taciturn_door",
            case: "open_open_door",
            code: "already_open"
        }]
    );
}

#[test]
fn should_fail_when_a_declared_code_has_no_refused_case() {
    // given
    let root = decisions();
    // when
    let failures = verify::<DoorsAndWordy>(root.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![DeterminismFailure::UncoveredCode {
            aggregate: "wordy_door",
            code: "jammed"
        }]
    );
}

#[test]
fn should_fail_when_refusal_codes_are_malformed_or_repeated() {
    // given
    let root = decisions();
    // when
    let failures = verify::<DoorsAndGarbled>(root.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![
            DeterminismFailure::InvalidCodes {
                aggregate: "garbled_door",
                detail: "has \"Already-Open\", which is not [a-z][a-z0-9_]*".to_owned()
            },
            DeterminismFailure::InvalidCodes {
                aggregate: "garbled_door",
                detail: "lists already_closed twice".to_owned()
            },
        ]
    );
}

#[test]
fn should_fail_when_command_type_is_not_declared() {
    // given
    let root = decisions();
    // when
    let failures = verify::<DoorsAndUnlisted>(root.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![
            DeterminismFailure::UndeclaredCommandType {
                aggregate: "unlisted_door",
                case: "close_open_door",
                command_type: "close"
            },
            DeterminismFailure::UndeclaredCommandType {
                aggregate: "unlisted_door",
                case: "close_closed_door",
                command_type: "close"
            },
        ]
    );
}

#[test]
fn should_fail_when_a_declared_command_type_has_no_case() {
    // given
    let root = decisions_with(&[
        ("door.txt", DOOR_DECISIONS),
        (
            "boastful_door.txt",
            &DOOR_DECISIONS.replacen("# door:", "# boastful_door:", 1),
        ),
    ]);
    // when
    let failures = verify::<DoorsAndBoastful>(root.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![DeterminismFailure::UncoveredCommandType {
            aggregate: "boastful_door",
            command_type: "slam"
        }]
    );
}

#[test]
fn should_fail_when_command_types_are_malformed() {
    // given
    let root = decisions_with(&[
        ("door.txt", DOOR_DECISIONS),
        (
            "mangled_door.txt",
            &DOOR_DECISIONS.replacen("# door:", "# mangled_door:", 1),
        ),
    ]);
    // when
    let failures = verify::<DoorsAndMangled>(root.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![
            DeterminismFailure::InvalidCommandTypes {
                aggregate: "mangled_door",
                detail: "has \"Open\", which is not [a-z][a-z0-9_]*".to_owned()
            },
            DeterminismFailure::InvalidCommandTypes {
                aggregate: "mangled_door",
                detail: "lists close twice".to_owned()
            },
        ]
    );
}

#[test]
fn should_fail_when_aggregate_is_registered_twice() {
    // given
    let root = decisions();
    // when
    let failures = verify::<DoorsTwice>(root.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![DeterminismFailure::DuplicateAggregate { aggregate: "door" }]
    );
}

#[test]
fn should_fail_with_the_text_to_commit_when_decisions_golden_is_missing() {
    // given
    let root = decisions_with(&[]);
    // when
    let failures = verify::<Doors>(root.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![DeterminismFailure::MissingDecisions {
            aggregate: "door",
            path: root.path().join("door.txt"),
            actual: DOOR_DECISIONS.to_owned(),
        }]
    );
}

#[test]
fn should_fail_when_decisions_differ_from_the_golden() {
    // given: a golden from before `already_open` was renamed
    let golden = DOOR_DECISIONS.replace("already_open", "open_already");
    let root = decisions_with(&[("door.txt", &golden)]);
    // when
    let failures = verify::<Doors>(root.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![DeterminismFailure::DecisionsDiffer {
            aggregate: "door",
            path: root.path().join("door.txt"),
            expected: golden,
            actual: DOOR_DECISIONS.to_owned(),
        }]
    );
}

#[test]
fn should_fail_when_decisions_golden_belongs_to_no_aggregate() {
    // given
    let root = decisions_with(&[("door.txt", DOOR_DECISIONS), ("gate.txt", "")]);
    // when
    let failures = verify::<Doors>(root.path()).unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![DeterminismFailure::UnregisteredDecisions {
            path: root.path().join("gate.txt")
        }]
    );
}

#[test]
fn should_fail_when_decisions_root_is_not_a_directory() {
    // given
    let dir = decisions();
    let root = dir.path().join("door.txt");
    // when
    let failures = verify::<NoAggregates>(&root).unwrap_err().0;
    // then
    assert!(
        matches!(
            failures.as_slice(),
            [DeterminismFailure::UnreadableDecisions { path, .. }] if path == Path::new(&root)
        ),
        "{failures:?}"
    );
}

#[test]
fn should_name_aggregate_and_case_when_failure_is_reported() {
    // given
    let root = decisions();
    let failures = verify::<DoorsAndFlaky>(root.path()).unwrap_err();
    // when
    let report = failures.to_string();
    // then
    assert!(
        report.contains("flaky_door/open_closed_door: decide is not deterministic"),
        "{report}"
    );
}

#[test]
fn should_fail_when_an_accepted_event_does_not_encode() {
    // given
    let root = decisions();
    // when
    let failures = verify::<DoorsAndTally>(root.path()).unwrap_err().0;
    // then
    assert!(
        matches!(
            failures.as_slice(),
            [DeterminismFailure::Unencodable {
                aggregate: "tally",
                case: "count",
                ..
            }]
        ),
        "{failures:?}"
    );
}

#[test]
fn should_return_the_summary_when_assert_deterministic_passes() {
    // given
    let root = decisions();
    // when
    let summary = assert_deterministic::<Doors>(root.path());
    // then
    assert_eq!(summary.cases, 4);
}

#[test]
fn should_fail_when_decisions_golden_is_unreadable() {
    // given
    let root = decisions_with(&[]);
    fs::create_dir(root.path().join("door.txt")).unwrap();
    // when
    let failures = verify::<Doors>(root.path()).unwrap_err().0;
    // then
    assert!(
        matches!(
            failures.as_slice(),
            [DeterminismFailure::UnreadableDecisions { path, .. }]
                if *path == root.path().join("door.txt")
        ),
        "{failures:?}"
    );
}

#[test]
fn should_name_what_to_fix_when_each_failure_is_displayed() {
    // given
    let path = Path::new("decisions/door.txt").to_owned();
    let failures = DeterminismFailures(vec![
        DeterminismFailure::NoHistoryCase { aggregate: "door" },
        DeterminismFailure::InvalidCodes {
            aggregate: "door",
            detail: "lists x twice".to_owned(),
        },
        DeterminismFailure::Unencodable {
            aggregate: "door",
            case: "count",
            error: "key must be a string".to_owned(),
        },
        DeterminismFailure::UndeclaredCode {
            aggregate: "door",
            case: "open_open_door",
            code: "already_open",
        },
        DeterminismFailure::UncoveredCode {
            aggregate: "door",
            code: "jammed",
        },
        DeterminismFailure::MissingDecisions {
            aggregate: "door",
            path: path.clone(),
            actual: "code a\n".to_owned(),
        },
        DeterminismFailure::DecisionsDiffer {
            aggregate: "door",
            path: path.clone(),
            expected: "code a\n".to_owned(),
            actual: "code b\n".to_owned(),
        },
        DeterminismFailure::UnreadableDecisions {
            path: path.clone(),
            error: "denied".to_owned(),
        },
        DeterminismFailure::UnregisteredDecisions { path },
        DeterminismFailure::InvalidCommandTypes {
            aggregate: "door",
            detail: "lists open twice".to_owned(),
        },
        DeterminismFailure::UndeclaredCommandType {
            aggregate: "door",
            case: "slam_door",
            command_type: "slam",
        },
        DeterminismFailure::UncoveredCommandType {
            aggregate: "door",
            command_type: "lock",
        },
    ]);
    // when
    let report = failures.to_string();
    // then
    for expected in [
        "12 determinism failure(s)",
        "door: CommandBody::TYPES lists open twice",
        "door/slam_door: command type slam is not in CommandBody::TYPES",
        "door: no case decides command type lock",
        "door: no refused case returns code jammed",
        "door: no determinism case has a history",
        "door: Rejection::CODES lists x twice",
        "door/count: an event does not encode: key must be a string",
        "door/open_open_door: refusal code already_open is not in Rejection::CODES",
        "missing decisions golden decisions/door.txt",
        "--- golden\ncode a\n--- run\ncode b",
        "cannot read decisions/door.txt: denied",
        "decisions golden of no registered aggregate: decisions/door.txt",
    ] {
        assert!(report.contains(expected), "{expected:?} not in {report}");
    }
}
