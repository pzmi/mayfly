//! The determinism harness against aggregates that keep and break its contract.

use crate::support::{
    Doors, DoorsAndConfused, DoorsAndFlaky, DoorsAndOptimist, DoorsAndSilent, DoorsTwice,
    NoAggregates,
};
use mayfly_control_domain::Expect;
use mayfly_testkit::determinism::{DeterminismFailure, DeterminismSummary, verify};

#[test]
fn should_pass_when_every_case_is_deterministic_and_as_declared() {
    // given
    let registry = verify::<Doors>;
    // when
    let summary = registry();
    // then
    assert_eq!(
        summary,
        Ok(DeterminismSummary {
            aggregates: 1,
            cases: 3
        })
    );
}

#[test]
fn should_pass_vacuously_when_registry_is_empty() {
    // given
    let registry = verify::<NoAggregates>;
    // when
    let summary = registry();
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
    let registry = verify::<DoorsAndFlaky>;
    // when
    let failures = registry().unwrap_err().0;
    // then
    let cases: Vec<_> = failures
        .iter()
        .map(|f| match f {
            DeterminismFailure::NonDeterministicDecide {
                aggregate: "flaky_door",
                case,
                ..
            } => *case,
            other => panic!("unexpected failure {other:?}"),
        })
        .collect();
    assert_eq!(cases, vec!["open_closed_door", "close_open_door"]);
}

#[test]
fn should_fail_when_aggregate_module_has_no_cases() {
    // given
    let registry = verify::<DoorsAndSilent>;
    // when
    let failures = registry().unwrap_err().0;
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
        ]
    );
}

#[test]
fn should_fail_when_aggregate_module_has_no_refused_case() {
    // given
    let registry = verify::<DoorsAndOptimist>;
    // when
    let failures = registry().unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![DeterminismFailure::NoRefusedCase {
            aggregate: "optimist_door"
        }]
    );
}

#[test]
fn should_fail_when_case_declares_the_wrong_outcome() {
    // given
    let registry = verify::<DoorsAndConfused>;
    // when
    let failures = registry().unwrap_err().0;
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
        ]
    );
}

#[test]
fn should_fail_when_aggregate_is_registered_twice() {
    // given
    let registry = verify::<DoorsTwice>;
    // when
    let failures = registry().unwrap_err().0;
    // then
    assert_eq!(
        failures,
        vec![DeterminismFailure::DuplicateAggregate { aggregate: "door" }]
    );
}

#[test]
fn should_name_aggregate_and_case_when_failure_is_reported() {
    // given
    let failures = verify::<DoorsAndFlaky>().unwrap_err();
    // when
    let report = failures.to_string();
    // then
    assert!(
        report.contains("flaky_door/open_closed_door: decide is not deterministic"),
        "{report}"
    );
}
