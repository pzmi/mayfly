//! The control plane's registry under the testkit harnesses: every registered event kind has
//! its golden fixture under `tests/fixtures/events/`, and every aggregate's determinism cases
//! hold. An aggregate joins both by its one line in `ControlAggregates`.

use std::path::PathBuf;

use mayfly_control_domain::ControlAggregates;
use mayfly_testkit::{determinism, golden};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/events")
}

#[test]
fn should_match_golden_fixtures_when_control_events_are_registered() {
    // given
    let root = fixtures();
    // when
    let result = golden::verify::<ControlAggregates>(&root);
    // then
    if let Err(failures) = result {
        panic!("{failures}");
    }
}

#[test]
fn should_decide_deterministically_when_control_aggregates_run_their_cases() {
    // given
    let registry = determinism::verify::<ControlAggregates>;
    // when
    let result = registry();
    // then
    if let Err(failures) = result {
        panic!("{failures}");
    }
}
