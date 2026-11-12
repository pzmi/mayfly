//! The Scale Set client passes the GitHub ports' contract (ver §5,
//! `mayfly_testkit::contracts::scaleset`) against `mayfly-fake-github` in process, in both flows,
//! under a paused Tokio clock (the fake's 50 s hold passes at once). The same suite replayed over
//! the recorded fixtures is `replay.rs`.

use std::sync::Arc;

use mayfly_adapter_github::{ApiClient, AppAuth, DEFAULT_CALL_POLICY, ScaleSetClient};
use mayfly_control::github::{RunnerId, ScaleSetApi as _, ScaleSetSpec, WorkflowJobId};
use mayfly_control::store::Clock;
use mayfly_domain::Millis;
use mayfly_fake_github::{FakeGithub, Fault, Flow as FakeFlow, JobSpec, Knobs, Route};
use mayfly_testkit::clock::{FakeClock, SeededRng};
use mayfly_testkit::contracts::scaleset::{
    CASES, Flow, ScaleSetCase, ScaleSetWorld, WorldFault, check_scaleset_contract,
};
use mayfly_testkit::secrets::MemorySecretProvider;

use crate::support::{InProcess, START, app_config, fake_config, key_ref, keys};

/// A queue base no token may be sent to: plain http to a remote host (spec §1).
pub const UNUSABLE_QUEUE_BASE: &str = "http://broker.example.com/q";

pub type Client<T = InProcess> = ScaleSetClient<T, FakeClock, SeededRng, MemorySecretProvider>;

/// A client over `transport`, on `clock`, with the synthetic App key installed.
pub fn client<T: mayfly_adapter_github::Transport>(transport: T, clock: &FakeClock) -> Client<T> {
    let secrets = MemorySecretProvider::new();
    secrets.set(&key_ref(), keys().private_pem.as_bytes());
    let api = ApiClient::new(
        transport,
        clock.clone(),
        SeededRng::new(7),
        DEFAULT_CALL_POLICY,
    );
    let auth = AppAuth::new(app_config("https://api.github.com"), api, secrets).expect("config");
    ScaleSetClient::new(auth)
}

pub struct FakeWorld {
    pub fake: FakeGithub,
    pub clock: FakeClock,
    pub client: Client,
}

impl FakeWorld {
    pub fn new(flow: Flow) -> Self {
        let clock = FakeClock::at(START);
        let reader = clock.clone();
        let knobs = Knobs {
            flow: match flow {
                Flow::Direct => FakeFlow::Direct,
                Flow::Acquire => FakeFlow::Acquire,
            },
            ..Knobs::default()
        };
        let fake = FakeGithub::with_knobs(
            fake_config("https://github.com", 3600),
            knobs,
            Arc::new(move || reader.now().get()),
        );
        let client = client(InProcess(fake.clone()), &clock);
        Self {
            fake,
            clock,
            client,
        }
    }
}

impl ScaleSetWorld for FakeWorld {
    type Api = Client;
    type Registry = Client;

    fn api(&self) -> &Client {
        &self.client
    }

    fn registry(&self) -> &Client {
        &self.client
    }

    async fn queue_jobs(&self, count: usize, label: &str, run_id: u64) {
        for _ in 0..count {
            self.fake.queue_job(JobSpec {
                run_id,
                labels: vec![label.to_owned()],
                ..JobSpec::default()
            });
        }
    }

    async fn start_job(&self, job: &WorkflowJobId, runner: RunnerId) {
        let job = job.0.parse().expect("a fake job id");
        self.fake.start_job(job, runner.0).expect("the job starts");
    }

    async fn inject(&self, fault: WorldFault) {
        match fault {
            WorldFault::QueueTokenExpired { scale_set } => self
                .fake
                .expire_queue_token(scale_set.0)
                .expect("a live session"),
            WorldFault::SessionExpired { scale_set } => self
                .fake
                .expire_session(scale_set.0)
                .expect("a live session"),
            WorldFault::PollRateLimited {
                retry_after_secs, ..
            } => self
                .fake
                .fail_next(Route::Poll, Fault::retry_after(429, retry_after_secs)),
            WorldFault::PollEmptyOk { .. } => self.fake.fail_next(Route::Poll, Fault::empty_ok()),
            WorldFault::AckAnswerLost { .. } => {
                self.fake.fail_next(Route::Ack, Fault::DropAfterEffect);
            }
            WorldFault::JitThrottled { .. } => {
                // One for the call and one for each of the policy's retries.
                for _ in 0..=DEFAULT_CALL_POLICY.retry.max_retries {
                    self.fake
                        .fail_next(Route::GenerateJitConfig, Fault::too_many_runners(Some(1)));
                }
            }
            WorldFault::SessionDeleteNotFound { .. } => self
                .fake
                .fail_next(Route::DeleteSession, Fault::status(404)),
            WorldFault::QueueUrlUnusable { .. } => self.fake.update_knobs(|k| {
                k.queue_url_base = Some(UNUSABLE_QUEUE_BASE.to_owned());
            }),
            WorldFault::QueueUrlRestored { .. } => {
                self.fake.update_knobs(|k| k.queue_url_base = None);
            }
            WorldFault::ScaleSetDeleted { scale_set } => self
                .fake
                .delete_dormant(scale_set.0)
                .expect("the scale set exists"),
        }
    }

    fn slept(&self) -> Vec<Millis> {
        self.clock.sleeps()
    }
}

#[tokio::test(start_paused = true)]
async fn should_pass_the_ports_contract_when_run_against_the_fake() {
    // given
    let world = |case: ScaleSetCase| async move { FakeWorld::new(case.flow) };

    // when
    let report = check_scaleset_contract(world).await;

    // then
    assert!(report.failed.is_empty(), "{:#?}", report.failed);
    assert_eq!(report.passed.len(), CASES.len());
}

#[tokio::test(start_paused = true)]
async fn should_keep_tokens_out_of_debug_and_errors_when_sessions_run() {
    // given: a session open, its queue token expired and refreshed, a JIT config minted
    let world = FakeWorld::new(Flow::Direct);
    let api = &world.client;
    let spec = ScaleSetSpec {
        name: "mayfly-redaction".to_owned(),
        runner_group: "Default".to_owned(),
        labels: vec!["mayfly".to_owned()],
    };
    let set = api.create_scale_set(&spec).await.expect("create");
    api.create_session(set.id, "host/pool").await.expect("open");
    world.fake.expire_queue_token(set.id.0).expect("live");
    api.poll(set.id, None, 0).await.expect("refreshed poll");
    let jit = api
        .generate_jit_config(set.id, "mf-redaction", "")
        .await
        .expect("jit");
    world.fake.expire_session(set.id.0).expect("live");

    // when
    let lost = api.poll(set.id, None, 0).await.expect_err("expired");
    let texts = [
        format!("{api:?}"),
        format!("{jit:?}"),
        format!("{lost:?}"),
        lost.to_string(),
    ];

    // then
    for token in world.fake.issued_tokens() {
        for text in &texts {
            assert!(!text.contains(&token), "a token leaked into {text}");
        }
    }
}
