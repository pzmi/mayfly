//! The wire contract (ver §5) against the fake, in process and on a socket, in both flows; and
//! against deliberately broken fakes, each of which must fail the cases named for it.

use std::future::Future;
use std::time::Duration;

use mayfly_fake_github::contract::broken::{
    AckingOnPoll, CapacityIgnoring, SessionStealing, Truncating, TypeNameDropping,
};
use mayfly_fake_github::contract::{
    CASES, FakeWorld, InProcess, Report, Socket, WireRequest, WireResponse, WireSetup, WireTarget,
    WireWorld, check_wire_contract, contract_fake,
};
use mayfly_fake_github::{FakeGithub, FakeServer, Flow};

use crate::support::{clock, keys};

fn new_fake(flow: Flow) -> (FakeGithub, WireSetup) {
    let keys = keys();
    contract_fake(&keys.public_pem, &keys.private_pem, flow, clock())
}

async fn in_process<T: WireTarget>(wrap: impl Fn(InProcess) -> T) -> Report {
    check_wire_contract(|flow| {
        let (fake, setup) = new_fake(flow);
        let target = wrap(InProcess(fake.clone()));
        async move {
            Some(FakeWorld {
                fake,
                target,
                setup,
            })
        }
    })
    .await
}

#[tokio::test(start_paused = true)]
async fn should_pass_every_case_when_run_against_the_fake_in_process() {
    // given
    let all: Vec<&str> = CASES.iter().map(|c| c.id).collect();

    // when
    let report = in_process(|t| t).await;

    // then
    assert_eq!(report.failed, Vec::new());
    assert_eq!(report.not_run, Vec::new());
    assert_eq!(report.passed, all);
}

/// A fake world served on a loopback socket, the server stopped with the world.
struct Served {
    world: FakeWorld<Socket>,
    _server: FakeServer,
}

impl WireTarget for Served {
    fn send(
        &self,
        request: WireRequest,
    ) -> impl Future<Output = Result<WireResponse, String>> + Send {
        self.world.send(request)
    }
}

impl WireWorld for Served {
    type Script = FakeGithub;

    fn setup(&self) -> &WireSetup {
        self.world.setup()
    }

    fn script(&self) -> Option<&FakeGithub> {
        self.world.script()
    }
}

/// Real sockets on a real clock: a paused clock would advance while bytes are in flight and fire
/// a hold early. A short hold keeps the run fast; the only waits are the fake's own (ADR 0023).
#[tokio::test]
async fn should_pass_every_case_when_run_against_the_fake_on_a_socket() {
    // given
    let world = |flow| async move {
        let (fake, setup) = new_fake(flow);
        fake.update_knobs(|k| k.poll_hold = Duration::from_millis(100));
        let server = FakeServer::start(fake.clone()).await.ok()?;
        let target = Socket(
            server
                .base_url()
                .trim_start_matches("http://")
                .parse()
                .ok()?,
        );
        Some(Served {
            world: FakeWorld {
                fake,
                target,
                setup,
            },
            _server: server,
        })
    };

    // when
    let report = check_wire_contract(world).await;

    // then
    assert_eq!(report.failed, Vec::new());
    assert_eq!(report.passed.len(), CASES.len());
}

/// The fake as a world without a script, as a live organisation is to the suite.
struct Unscripted(FakeWorld<InProcess>);

impl WireTarget for Unscripted {
    fn send(
        &self,
        request: WireRequest,
    ) -> impl Future<Output = Result<WireResponse, String>> + Send {
        self.0.send(request)
    }
}

impl WireWorld for Unscripted {
    type Script = FakeGithub;

    fn setup(&self) -> &WireSetup {
        self.0.setup()
    }

    fn script(&self) -> Option<&FakeGithub> {
        None
    }
}

#[tokio::test(start_paused = true)]
async fn should_report_scripted_cases_as_not_run_when_the_world_has_no_script() {
    // given
    let world = |flow| {
        let (fake, setup) = new_fake(flow);
        let inner = FakeWorld {
            fake: fake.clone(),
            target: InProcess(fake),
            setup,
        };
        async move { (flow == Flow::Direct).then_some(Unscripted(inner)) }
    };

    // when
    let report = check_wire_contract(world).await;

    // then
    let not_run: Vec<&str> = report.not_run.iter().map(|(id, _)| *id).collect();
    let scripted: Vec<&str> = CASES
        .iter()
        .filter(|c| c.needs_script)
        .map(|c| c.id)
        .collect();
    assert_eq!(not_run, scripted);
    assert_eq!(report.failed, Vec::new());
    assert_eq!(report.passed.len() + not_run.len(), CASES.len());
}

/// The report failed exactly `cases`, each for a reason containing its text.
fn assert_fails_exactly(report: &Report, cases: &[(&str, &str)]) {
    let ids: Vec<&str> = cases.iter().map(|(id, _)| *id).collect();
    assert_eq!(report.failed_ids(), ids, "{report:?}");
    for ((id, why), (_, reason)) in cases.iter().zip(&report.failed) {
        assert!(
            reason.contains(why),
            "{id} failed for another reason: {reason}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn should_fail_wc03_when_the_fake_steals_a_live_session() {
    // given
    let broken = SessionStealing::new;

    // when
    let report = in_process(broken).await;

    // then
    assert_fails_exactly(&report, &[("WC-03", "a second session opened")]);
}

#[tokio::test(start_paused = true)]
async fn should_fail_the_redelivery_and_ack_cases_when_the_fake_never_redelivers() {
    // given
    let broken = AckingOnPoll;

    // when
    let report = in_process(broken).await;

    // then
    assert_fails_exactly(
        &report,
        &[
            ("WC-06", "poll: expected 200, got 202"),
            ("WC-07", "ack: expected 204, got 404"),
            ("WC-15", "ack: expected 204, got 404"),
        ],
    );
}

#[tokio::test(start_paused = true)]
async fn should_fail_wc08_when_the_fake_ignores_the_advertised_capacity() {
    // given
    let broken = CapacityIgnoring;

    // when
    let report = in_process(broken).await;

    // then
    assert_fails_exactly(
        &report,
        &[("WC-08", "poll at capacity 0: expected 202, got 200")],
    );
}

#[tokio::test(start_paused = true)]
async fn should_fail_the_typed_error_cases_when_the_fake_drops_type_names() {
    // given
    let broken = TypeNameDropping;

    // when
    let report = in_process(broken).await;

    // then
    let why = "expected a typeName containing";
    assert_fails_exactly(
        &report,
        &[
            ("WC-03", why),
            ("WC-04", why),
            ("WC-10", why),
            ("WC-12", why),
        ],
    );
}

#[tokio::test(start_paused = true)]
async fn should_fail_wc07_when_the_fake_drops_a_backlog() {
    // given
    let broken = Truncating;

    // when
    let report = in_process(broken).await;

    // then
    assert_fails_exactly(&report, &[("WC-07", "20 of 60 assignments delivered")]);
}
