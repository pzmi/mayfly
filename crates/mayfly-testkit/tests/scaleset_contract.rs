//! `FakeScaleSetApi` and `FakeRunnerRegistry` pass the GitHub ports' contract (ver §5), and each
//! deliberately broken variant fails the cases named for it. The adapter runs the same suite
//! from `mayfly-adapter-github/tests/client/contract.rs`.

use mayfly_control::github::{RunnerId, WorkflowJobId};
use mayfly_domain::Millis;
use mayfly_testkit::block_on;
use mayfly_testkit::clock::FakeClock;
use mayfly_testkit::contracts::scaleset::{
    CASES, Flow, ScaleSetCase, ScaleSetWorld, WorldFault, check_scaleset_contract,
};
use mayfly_testkit::github::{Broken, FakeRunnerRegistry, FakeScaleSetApi};

struct MemoryWorld {
    api: FakeScaleSetApi,
    registry: FakeRunnerRegistry,
}

impl MemoryWorld {
    fn new(flow: Flow, broken: Option<Broken>) -> Self {
        let clock = FakeClock::at(Millis::new(1_794_312_000_000));
        let api = match broken {
            None => FakeScaleSetApi::new(flow, clock),
            Some(b) => FakeScaleSetApi::broken(flow, clock, b),
        };
        let registry = api.registry();
        Self { api, registry }
    }
}

impl ScaleSetWorld for MemoryWorld {
    type Api = FakeScaleSetApi;
    type Registry = FakeRunnerRegistry;

    fn api(&self) -> &FakeScaleSetApi {
        &self.api
    }

    fn registry(&self) -> &FakeRunnerRegistry {
        &self.registry
    }

    async fn queue_jobs(&self, count: usize, label: &str, run_id: u64) {
        self.api.queue_jobs(count, label, run_id);
    }

    async fn start_job(&self, job: &WorkflowJobId, runner: RunnerId) {
        self.api.start_job(job, runner).expect("the job starts");
    }

    async fn inject(&self, fault: WorldFault) {
        self.api.inject(fault);
    }

    fn slept(&self) -> Vec<Millis> {
        self.api.clock().sleeps()
    }
}

fn run(broken: Option<Broken>) -> mayfly_testkit::contracts::scaleset::Report {
    block_on(check_scaleset_contract(|case: ScaleSetCase| async move {
        MemoryWorld::new(case.flow, broken)
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
    // given
    let cases = [
        (Broken::NoCursorApi, vec!["SS-04"]),
        (Broken::RetryAfterIgnoringApi, vec!["SS-06"]),
        (Broken::NameGuessingRegistry, vec!["RR-02"]),
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
