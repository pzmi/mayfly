//! The control plane's registry under the testkit harnesses: every registered event kind has
//! its golden samples under `tests/fixtures/events/`, and every aggregate's determinism cases
//! hold and match its decisions golden under `tests/fixtures/decisions/`. An aggregate joins
//! both by its one line in `ControlAggregates`.

use std::path::PathBuf;

use mayfly_control_domain::ControlAggregates;
use mayfly_testkit::{determinism, golden};

fn fixtures(family: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(family)
}

#[test]
fn should_match_golden_fixtures_when_control_events_are_registered() {
    // given
    let root = fixtures("events");
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
    let root = fixtures("decisions");
    // when
    let result = determinism::verify::<ControlAggregates>(&root);
    // then
    if let Err(failures) = result {
        panic!("{failures}");
    }
}
