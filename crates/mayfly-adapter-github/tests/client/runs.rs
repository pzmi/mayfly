//! The REST half of ADR 0090's per-job check (GH-10) against `mayfly-fake-github` in process,
//! under a paused Tokio clock: the `WorkflowRuns` contract (`mayfly_testkit::contracts::runs`),
//! a fork's run decoded as one (its classification is the aggregate's, pinned in
//! `mayfly-control/tests/store/listener.rs`), a lookup timeout answered failed, a cancel's 202,
//! 409 and lost answer, the held installation token, and no token in an error. The same suite
//! replayed over the recorded fixtures is `replay.rs`.

use std::sync::Arc;

use mayfly_adapter_github::{DEFAULT_CALL_POLICY, Operation};
use mayfly_control::github::{
    CancelAnswer, EventName, Failure, JobKey, RepositoryName, RunFacts, RunLookup, WorkflowJobId,
    WorkflowRunId, WorkflowRuns as _, cancel_observation, run_lookup,
};
use mayfly_control::store::Clock;
use mayfly_domain::Millis;
use mayfly_fake_github::{FakeGithub, Fault, JobSpec, Route};
use mayfly_testkit::clock::FakeClock;
use mayfly_testkit::contracts::runs::{
    CASES, FORK, LABEL, REPOSITORY, RunFault, RunSpec, RunsCase, RunsWorld, check_runs_contract,
};
use mayfly_testkit::contracts::scaleset::Flow;

use crate::contract::{Client, FakeWorld, client};
use crate::support::{InProcess, START, fake_config};

/// The wire name of `event` (the domain's serde form is GitHub's name).
fn wire_event(event: EventName) -> String {
    serde_json::to_value(event)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn job_spec(run: &RunSpec) -> JobSpec {
    let (owner, name) = run
        .repository
        .0
        .split_once('/')
        .unwrap_or_else(|| panic!("an owner/name repository"));
    JobSpec {
        owner: owner.to_owned(),
        repository: name.to_owned(),
        run_id: run.id,
        event: wire_event(run.event),
        head_repository: run.head_repository.0.clone(),
        actor: run.actor.clone(),
        labels: vec![LABEL.to_owned()],
        ..JobSpec::default()
    }
}

impl RunsWorld for FakeWorld {
    type Runs = Client;

    fn runs(&self) -> &Client {
        &self.client
    }

    async fn start_run(&self, run: &RunSpec) {
        self.fake.queue_job(job_spec(run));
    }

    async fn inject(&self, fault: RunFault) {
        match fault {
            RunFault::LookupStalls { .. } => {
                // One for the call and one for each of the policy's retries.
                for _ in 0..=DEFAULT_CALL_POLICY.retry.max_retries {
                    self.fake.fail_next(Route::WorkflowRun, Fault::Stall);
                }
            }
            RunFault::CancelAnswerLost { .. } => {
                self.fake
                    .fail_next(Route::CancelRun, Fault::DropAfterEffect);
            }
        }
    }
}

fn base() -> RepositoryName {
    RepositoryName(REPOSITORY.to_owned())
}

/// A fake whose installation holds `permissions` only, and a client against it.
fn world_granting(permissions: &[(&str, &str)]) -> (FakeGithub, FakeClock, Client) {
    let clock = FakeClock::at(START);
    let reader = clock.clone();
    let mut config = fake_config("https://github.com", 3600);
    config.permissions = permissions
        .iter()
        .map(|(n, l)| ((*n).to_owned(), (*l).to_owned()))
        .collect();
    let fake = FakeGithub::new(config, Arc::new(move || reader.now().get()));
    let client = client(InProcess(fake.clone()), &clock);
    (fake, clock, client)
}

fn attempts(client: &Client, op: Operation) -> u64 {
    client
        .auth()
        .metrics()
        .snapshot()
        .iter()
        .filter(|c| c.op == op)
        .map(|c| c.count)
        .sum()
}

#[tokio::test(start_paused = true)]
async fn should_pass_the_runs_contract_when_run_against_the_fake() {
    // given
    let world = |_case: RunsCase| async move { FakeWorld::new(Flow::Direct) };

    // when
    let report = check_runs_contract(world).await;

    // then
    assert!(report.failed.is_empty(), "{:#?}", report.failed);
    assert_eq!(report.passed.len(), CASES.len());
}

#[tokio::test(start_paused = true)]
async fn should_decode_a_forks_run_as_one_when_it_is_looked_up() {
    // given: a fork's pull request and a push to the same repository
    let world = FakeWorld::new(Flow::Direct);
    world.start_run(&RunSpec::fork_pull_request(21)).await;
    world.start_run(&RunSpec::push(22)).await;

    // when
    let fork = world.client.workflow_run(&base(), WorkflowRunId(21)).await;
    let push = world.client.workflow_run(&base(), WorkflowRunId(22)).await;

    // then: the fork's head is not its base, the push's is; both are found
    let facts = |head: &str, event| {
        RunLookup::Found(RunFacts {
            repository: Some(base()),
            head_repository: Some(RepositoryName(head.to_owned())),
            event_name: event,
        })
    };
    assert_eq!(run_lookup(&fork), facts(FORK, EventName::PullRequest));
    assert_eq!(run_lookup(&push), facts(REPOSITORY, EventName::Push));
    let actor = fork.ok().and_then(|run| run.actor);
    assert_eq!(actor.as_deref(), Some("contributor"));
    assert_eq!(world.fake.counts().run_lookups, 2);
}

#[tokio::test(start_paused = true)]
async fn should_answer_failed_when_a_lookup_times_out() {
    // given: every attempt of the lookup stalls
    let world = FakeWorld::new(Flow::Direct);
    world.start_run(&RunSpec::push(23)).await;
    world.inject(RunFault::LookupStalls { run: 23 }).await;
    let started = tokio::time::Instant::now();

    // when
    let looked = world.client.workflow_run(&base(), WorkflowRunId(23)).await;

    // then: unavailable within the call's bounds, read as a failed lookup (the job is refused)
    let error = looked.as_ref().expect_err("stalled");
    assert_eq!(error.failure, Failure::Unavailable);
    assert_eq!(run_lookup(&looked), RunLookup::Failed);
    let attempts_made = u64::from(DEFAULT_CALL_POLICY.retry.max_retries) + 1;
    assert_eq!(attempts(&world.client, Operation::RunLookup), attempts_made);
    let per_attempt = DEFAULT_CALL_POLICY.attempt_timeout.get();
    assert!(started.elapsed().as_millis() <= u128::from(per_attempt * attempts_made));
}

#[tokio::test(start_paused = true)]
async fn should_record_202_and_409_and_nothing_else_when_cancels_are_answered_or_lost() {
    // given: two runs of refused jobs, the second's cancel answer lost after its effect
    let world = FakeWorld::new(Flow::Direct);
    world.start_run(&RunSpec::fork_pull_request(24)).await;
    world.start_run(&RunSpec::fork_pull_request(25)).await;
    let job = |run: u64| JobKey {
        job_id: WorkflowJobId("1".to_owned()),
        run_id: WorkflowRunId(run),
    };

    // when
    let accepted = world.client.cancel_run(&base(), WorkflowRunId(24)).await;
    let over = world.client.cancel_run(&base(), WorkflowRunId(24)).await;
    world.inject(RunFault::CancelAnswerLost { run: 25 }).await;
    let lost = world.client.cancel_run(&base(), WorkflowRunId(25)).await;

    // then: 202 and 409 are observations recorded once; the lost answer records nothing and
    // was sent once (never retried), though it took effect at GitHub
    assert_eq!(accepted, Ok(CancelAnswer::Accepted));
    assert_eq!(over, Ok(CancelAnswer::Refused));
    assert_eq!(
        lost.as_ref().map_err(|e| e.failure),
        Err(Failure::Unavailable)
    );
    assert!(cancel_observation(&job(24), &accepted).is_some());
    assert!(cancel_observation(&job(24), &over).is_some());
    assert_eq!(cancel_observation(&job(25), &lost), None);
    assert_eq!(attempts(&world.client, Operation::RunCancel), 3);
    assert!(world.client.auth().client().clock().sleeps().is_empty());
    let cancelled: Vec<bool> = world
        .fake
        .world()
        .runs
        .values()
        .map(|r| r.cancelled)
        .collect();
    assert_eq!(cancelled, [true, true]);
}

#[tokio::test(start_paused = true)]
async fn should_refuse_rest_calls_when_the_installation_lacks_their_permission() {
    // given: "Actions: Read" only, then no "Actions" at all (ADR 0045)
    let (read_fake, _clock, read_only) = world_granting(&[("actions", "read")]);
    read_fake.queue_job(job_spec(&RunSpec::fork_pull_request(26)));
    let (none_fake, _clock2, without) = world_granting(&[("metadata", "read")]);
    none_fake.queue_job(job_spec(&RunSpec::push(27)));

    // when
    let looked = read_only.workflow_run(&base(), WorkflowRunId(26)).await;
    let cancel = read_only.cancel_run(&base(), WorkflowRunId(26)).await;
    let refused = without.workflow_run(&base(), WorkflowRunId(27)).await;

    // then: the lookup needs read, the cancel write; a refused lookup fails the job
    assert!(looked.is_ok(), "{looked:?}");
    assert_eq!(cancel, Ok(CancelAnswer::Refused));
    assert_eq!(
        refused.as_ref().map_err(|e| e.failure),
        Err(Failure::Refused { status: 403 })
    );
    assert_eq!(run_lookup(&refused), RunLookup::Failed);
    assert!(!read_fake.world().runs.values().any(|r| r.cancelled));
}

#[tokio::test(start_paused = true)]
async fn should_mint_one_installation_token_when_runs_are_looked_up_again() {
    // given
    let world = FakeWorld::new(Flow::Direct);
    world.start_run(&RunSpec::push(28)).await;

    // when: three lookups, then one after the token entered its refresh margin, then one
    // answered 401 (a token revoked before its expiry)
    for _ in 0..3 {
        let looked = world.client.workflow_run(&base(), WorkflowRunId(28)).await;
        assert!(looked.is_ok(), "{looked:?}");
    }
    let held = world.fake.counts().installation_tokens;
    world.clock.advance(Millis::new(3_600_000 - 30_000));
    let late = world.client.workflow_run(&base(), WorkflowRunId(28)).await;
    let refreshed = world.fake.counts().installation_tokens;
    world.fake.fail_next(Route::WorkflowRun, Fault::status(401));
    let renewed = world.client.workflow_run(&base(), WorkflowRunId(28)).await;

    // then
    assert_eq!(held, 1);
    assert!(late.is_ok(), "{late:?}");
    assert_eq!(refreshed, 2);
    assert!(renewed.is_ok(), "{renewed:?}");
    assert_eq!(world.fake.counts().installation_tokens, 3);
}

#[tokio::test(start_paused = true)]
async fn should_keep_tokens_out_of_errors_when_rest_calls_fail() {
    // given: a lookup refused, one of an absent run, one stalled, a cancel lost, one of a
    // repository that is not a plain owner/name
    let (fake, _clock, without) = world_granting(&[("metadata", "read")]);
    fake.queue_job(job_spec(&RunSpec::push(29)));
    let world = FakeWorld::new(Flow::Direct);
    world.start_run(&RunSpec::push(30)).await;
    world.start_run(&RunSpec::fork_pull_request(31)).await;

    // when: each fault queued right before the call it bends (a route's faults are in order)
    let refused = without.workflow_run(&base(), WorkflowRunId(29)).await.err();
    let absent = world
        .client
        .workflow_run(&base(), WorkflowRunId(99))
        .await
        .err();
    world.inject(RunFault::LookupStalls { run: 30 }).await;
    let stalled = world
        .client
        .workflow_run(&base(), WorkflowRunId(30))
        .await
        .err();
    world.inject(RunFault::CancelAnswerLost { run: 31 }).await;
    let lost = world
        .client
        .cancel_run(&base(), WorkflowRunId(31))
        .await
        .err();
    let odd = RepositoryName("acme/../x".to_owned());
    let unsafe_path = world
        .client
        .workflow_run(&odd, WorkflowRunId(30))
        .await
        .err();
    let errors = [refused, absent, stalled, lost, unsafe_path];

    // then
    assert!(errors.iter().all(Option::is_some), "{errors:?}");
    let mut texts: Vec<String> = errors
        .iter()
        .flatten()
        .flat_map(|e| [format!("{e:?}"), e.to_string()])
        .collect();
    texts.push(format!("{:?}", world.client));
    texts.push(format!("{without:?}"));
    let tokens: Vec<String> = fake
        .issued_tokens()
        .into_iter()
        .chain(world.fake.issued_tokens())
        .collect();
    assert!(!tokens.is_empty());
    for token in &tokens {
        for text in &texts {
            assert!(!text.contains(token.as_str()), "a token leaked into {text}");
        }
    }
    assert_eq!(
        errors[4].as_ref().map(|e| e.failure),
        Some(Failure::ProtocolDrift)
    );
}

#[tokio::test(start_paused = true)]
async fn should_refuse_after_one_attempt_when_a_cancel_is_answered_503_or_429() {
    // given: answers the lookup's policy would retry
    let world = FakeWorld::new(Flow::Direct);
    world.start_run(&RunSpec::fork_pull_request(32)).await;
    world.fake.fail_next(Route::CancelRun, Fault::status(503));
    world
        .fake
        .fail_next(Route::CancelRun, Fault::retry_after(429, 1));

    // when
    let unavailable = world.client.cancel_run(&base(), WorkflowRunId(32)).await;
    let throttled = world.client.cancel_run(&base(), WorkflowRunId(32)).await;

    // then: each is an answer, recorded refused after exactly one attempt and no wait
    assert_eq!(unavailable, Ok(CancelAnswer::Refused));
    assert_eq!(throttled, Ok(CancelAnswer::Refused));
    assert_eq!(attempts(&world.client, Operation::RunCancel), 2);
    assert!(world.client.auth().client().clock().sleeps().is_empty());
    assert!(!world.fake.world().runs.values().any(|r| r.cancelled));
}

#[tokio::test(start_paused = true)]
async fn should_refuse_without_a_request_when_a_cancel_names_no_plain_repository() {
    // given: a repository no path can name
    let world = FakeWorld::new(Flow::Direct);
    let odd = RepositoryName("acme/../x".to_owned());

    // when
    let answer = world.client.cancel_run(&odd, WorkflowRunId(33)).await;

    // then: refused at once, so it never stays due; nothing was sent, not even a token mint
    assert_eq!(answer, Ok(CancelAnswer::Refused));
    let job = JobKey {
        job_id: WorkflowJobId("1".to_owned()),
        run_id: WorkflowRunId(33),
    };
    assert!(cancel_observation(&job, &answer).is_some());
    assert_eq!(world.fake.counts().requests, 0);
}

#[tokio::test(start_paused = true)]
async fn should_share_one_deadline_when_the_mint_and_the_lookup_wait() {
    // given: the token's mint waits 100 s for a rate limit, then the lookup is asked to
    let world = FakeWorld::new(Flow::Direct);
    world.start_run(&RunSpec::push(34)).await;
    world
        .fake
        .fail_next(Route::InstallationToken, Fault::retry_after(429, 100));
    world
        .fake
        .fail_next(Route::WorkflowRun, Fault::retry_after(429, 100));

    // when
    let looked = world.client.workflow_run(&base(), WorkflowRunId(34)).await;

    // then: the second wait would outlast the one two-minute deadline, so it is not taken
    assert_eq!(
        looked.as_ref().map_err(|e| e.failure),
        Err(Failure::Throttled)
    );
    assert_eq!(
        world.client.auth().client().clock().sleeps(),
        [Millis::new(100_000)]
    );
    assert_eq!(run_lookup(&looked), RunLookup::Failed);
}
