//! `FakeScaleSetApi` passes the `WorkflowRuns` contract (ver §5), and each deliberately broken
//! variant fails the case named for it. The adapter runs the same suite from
//! `mayfly-adapter-github/tests/client/runs.rs` and, replayed, `replay.rs`.

use mayfly_domain::Millis;
use mayfly_testkit::block_on;
use mayfly_testkit::clock::FakeClock;
use mayfly_testkit::contracts::runs::{
    CASES, LABEL, Report, RunFault, RunSpec, RunsCase, RunsWorld, check_runs_contract,
};
use mayfly_testkit::contracts::scaleset::Flow;
use mayfly_testkit::github::{Broken, FakeScaleSetApi};

struct MemoryWorld {
    api: FakeScaleSetApi,
}

impl RunsWorld for MemoryWorld {
    type Runs = FakeScaleSetApi;

    fn runs(&self) -> &FakeScaleSetApi {
        &self.api
    }

    async fn start_run(&self, run: &RunSpec) {
        self.api.start_run(run, 1, LABEL);
    }

    async fn inject(&self, fault: RunFault) {
        self.api.inject_run(fault);
    }
}

fn run(broken: Option<Broken>) -> Report {
    block_on(check_runs_contract(|_case: RunsCase| async move {
        let clock = FakeClock::at(Millis::new(1_794_312_000_000));
        let api = match broken {
            None => FakeScaleSetApi::new(Flow::Direct, clock),
            Some(b) => FakeScaleSetApi::broken(Flow::Direct, clock, b),
        };
        MemoryWorld { api }
    }))
}

#[test]
fn should_pass_the_contract_when_the_fake_is_whole() {
    // given
    let broken = None;

    // when
    let report = run(broken);

    // then
    assert!(report.failed.is_empty(), "{:#?}", report.failed);
    assert_eq!(report.passed.len(), CASES.len());
}

#[test]
fn should_fail_the_named_cases_when_the_fake_is_broken() {
    // given: a head read as the base, and a lost cancel resent
    let cases = [
        (Broken::ForkBlindRuns, vec!["WR-02"]),
        (Broken::RetryingCancelRuns, vec!["CR-02"]),
    ];

    // when
    let failed: Vec<Vec<&str>> = cases
        .iter()
        .map(|(broken, _)| run(Some(*broken)).failed_ids())
        .collect();

    // then
    let expected: Vec<Vec<&str>> = cases.iter().map(|(_, ids)| ids.clone()).collect();
    assert_eq!(failed, expected);
}
