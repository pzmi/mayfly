//! The Scale Set client's own promises beyond the ports' contract, against `mayfly-fake-github`
//! in process under a paused Tokio clock: racing 401s refresh once, a refresh answering 404 loses
//! the session, a queue URL a token could leak through is never called, and an admin token the
//! service refuses is minted again once.

use std::sync::{Arc, Mutex, PoisonError};

use mayfly_adapter_github::{
    HttpRequest, HttpResponse, Operation, ScaleSetClient, Transport, TransportError,
};
use mayfly_control::github::{Failure, ScaleSetApi as _, ScaleSetId, ScaleSetSpec};
use mayfly_control::store::Clock;
use mayfly_fake_github::{FakeGithub, Fault, Knobs, Route};
use mayfly_testkit::clock::{FakeClock, SeededRng};
use mayfly_testkit::contracts::scaleset::Flow;
use mayfly_testkit::secrets::MemorySecretProvider;

use crate::contract::{Client, FakeWorld, UNUSABLE_QUEUE_BASE, client};
use crate::support::{InProcess, START, fake_config};

fn spec() -> ScaleSetSpec {
    ScaleSetSpec {
        name: "mayfly-client".to_owned(),
        runner_group: "Default".to_owned(),
        labels: vec!["mayfly".to_owned()],
    }
}

async fn create<T: Transport>(
    api: &ScaleSetClient<T, FakeClock, SeededRng, MemorySecretProvider>,
) -> ScaleSetId {
    api.create_scale_set(&spec()).await.expect("create").id
}

async fn open(world: &FakeWorld) -> ScaleSetId {
    let set = create(&world.client).await;
    world
        .client
        .create_session(set, "host/pool")
        .await
        .expect("open");
    set
}

fn count(api: &Client, op: Operation) -> u64 {
    api.auth()
        .metrics()
        .snapshot()
        .iter()
        .filter(|c| c.op == op)
        .map(|c| c.count)
        .sum()
}

/// The in-process fake, recording the host of every request sent.
#[derive(Debug, Clone)]
struct Recording {
    inner: InProcess,
    hosts: Arc<Mutex<Vec<String>>>,
}

impl Transport for Recording {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.hosts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.url.host_str().unwrap_or_default().to_owned());
        self.inner.send(request).await
    }
}

/// The hosts every request was sent to.
type Hosts = Arc<Mutex<Vec<String>>>;

fn recording_world() -> (FakeGithub, Client<Recording>, Hosts) {
    let clock = FakeClock::at(START);
    let reader = clock.clone();
    let fake = FakeGithub::with_knobs(
        fake_config("https://github.com", 3600),
        Knobs::default(),
        Arc::new(move || reader.now().get()),
    );
    let hosts = Arc::new(Mutex::new(Vec::new()));
    let transport = Recording {
        inner: InProcess(fake.clone()),
        hosts: Arc::clone(&hosts),
    };
    (fake, client(transport, &clock), hosts)
}

#[tokio::test(start_paused = true)]
async fn should_refresh_once_when_queue_calls_race_on_an_expired_token() {
    // given: the queue token expired; the first refresh is held until the second poll waits
    let world = FakeWorld::new(Flow::Direct);
    let set = open(&world).await;
    world.fake.expire_queue_token(set.0).expect("live");
    let gate = world.fake.hold(Route::RefreshSession);
    let api = &world.client;

    // when
    let (first, second) = tokio::join!(api.poll(set, None, 0), async {
        gate.arrived().await;
        let (second, ()) = tokio::join!(api.poll(set, None, 0), async { gate.release() });
        second
    });

    // then
    assert_eq!(first.expect("first poll"), None);
    assert_eq!(second.expect("second poll"), None);
    assert_eq!(count(api, Operation::SessionRefresh), 1);
}

#[tokio::test(start_paused = true)]
async fn should_lose_the_session_when_its_refresh_answers_404() {
    // given
    let world = FakeWorld::new(Flow::Direct);
    let set = open(&world).await;
    world.fake.expire_queue_token(set.0).expect("live");
    world
        .fake
        .fail_next(Route::RefreshSession, Fault::status(404));

    // when
    let lost = world.client.poll(set, None, 0).await;

    // then
    assert_eq!(lost.expect_err("lost").failure, Failure::SessionExpired);
    assert!(!world.client.holds_session(set).await);
    assert!(world.client.get_scale_set(set).await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn should_refuse_a_queue_url_when_a_token_could_leak_through_it() {
    // given: queue bases a token must never reach (spec §1): plain http to a remote host, and
    // a URL with a query
    let bases = [UNUSABLE_QUEUE_BASE, "https://broker.example.net/q?tenant=1"];

    for base in bases {
        let (fake, api, hosts) = recording_world();
        let set = create(&api).await;
        fake.update_knobs(|k| k.queue_url_base = Some(base.to_owned()));

        // when
        let refused = api.create_session(set, "host/pool").await;
        fake.update_knobs(|k| k.queue_url_base = None);
        let next = api.create_session(set, "host/pool").await;

        // then: drift, nothing sent to the queue host, and the session deleted (no 409 next)
        let refused = refused.expect_err("refused");
        assert_eq!(refused.failure, Failure::ProtocolDrift, "{base}");
        assert!(refused.detail.contains("was deleted"), "{}", refused.detail);
        assert!(next.is_ok(), "{base}: {next:?}");
        let sent = hosts.lock().unwrap_or_else(PoisonError::into_inner).clone();
        assert!(
            sent.iter().all(|h| !h.starts_with("broker.")),
            "{base}: {sent:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn should_refuse_a_refreshed_queue_url_when_a_token_could_leak_through_it() {
    // given: a usable session whose refresh answers an unusable queue URL. The fake routes by
    // path alone, so the unusable base keeps the service URL's path and the old queue URL still
    // answers (its 401 is what starts the refresh).
    let (fake, api, hosts) = recording_world();
    let set = create(&api).await;
    api.create_session(set, "host/pool").await.expect("open");
    fake.update_knobs(|k| {
        k.queue_url_base = Some("http://broker.example.com/synthetic-service".to_owned());
    });
    fake.expire_queue_token(set.0).expect("live");

    // when
    let refused = api.poll(set, None, 0).await;

    // then: drift, nothing sent to the new host, and the session lost
    assert_eq!(
        refused.expect_err("refused").failure,
        Failure::ProtocolDrift
    );
    let sent = hosts.lock().unwrap_or_else(PoisonError::into_inner).clone();
    assert!(!sent.iter().any(|h| h == "broker.example.com"), "{sent:?}");
    assert!(!api.holds_session(set).await);
    assert_eq!(
        api.poll(set, None, 0).await.expect_err("lost").failure,
        Failure::NoSession
    );
}

#[tokio::test(start_paused = true)]
async fn should_mint_the_admin_token_again_once_when_a_service_call_answers_401() {
    // given: one scale set read refused once, another refused twice
    let world = FakeWorld::new(Flow::Direct);
    let set = create(&world.client).await;
    let minted = world.fake.counts().admin_exchanges;
    world
        .fake
        .fail_next(Route::ScaleSetById, Fault::status(401));

    // when
    let once = world.client.get_scale_set(set).await;
    let after_once = world.fake.counts().admin_exchanges;
    world
        .fake
        .fail_next(Route::ScaleSetById, Fault::status(401));
    world
        .fake
        .fail_next(Route::ScaleSetById, Fault::status(401));
    let twice = world.client.get_scale_set(set).await;

    // then
    assert!(once.is_ok(), "{once:?}");
    assert_eq!(after_once, minted + 1);
    assert_eq!(
        twice.expect_err("refused twice").failure,
        Failure::Refused { status: 401 }
    );
    assert_eq!(world.fake.counts().admin_exchanges, minted + 2);
}
