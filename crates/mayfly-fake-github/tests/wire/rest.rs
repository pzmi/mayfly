//! The REST routes of ADR 0090's trust check and their permissions (spec §2, §6.2, §13).

use std::collections::BTreeMap;

use mayfly_fake_github::JobSpec;
use mayfly_fake_github::contract::contract_fake;

use crate::support::{Api, clock, fake, keys};

#[tokio::test(start_paused = true)]
async fn should_describe_a_forks_run_when_it_is_looked_up() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    fake.queue_job(JobSpec {
        run_id: 77,
        event: "pull_request".to_owned(),
        head_repository: "mallory/app".to_owned(),
        actor: "mallory".to_owned(),
        ..JobSpec::default()
    });

    // when
    let run = api.rest("GET", "/repos/acme/app/actions/runs/77").await;
    let elsewhere = api.rest("GET", "/repos/acme/other/actions/runs/77").await;
    let missing = api.rest("GET", "/repos/acme/app/actions/runs/78").await;

    // then
    let body = run.json();
    assert_eq!(run.status, 200);
    assert_eq!(body["event"], "pull_request");
    assert_eq!(body["repository"]["full_name"], "acme/app");
    assert_eq!(body["head_repository"]["full_name"], "mallory/app");
    assert_eq!(body["actor"]["login"], "mallory");
    assert_eq!((elsewhere.status, missing.status), (404, 404));
    assert_eq!(fake.counts().run_lookups, 1);
}

#[tokio::test(start_paused = true)]
async fn should_answer_409_when_cancelling_a_completed_run() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    let job = fake.queue_job(JobSpec {
        run_id: 77,
        ..JobSpec::default()
    });
    fake.complete_job(job, "succeeded").expect("complete");

    // when
    let refused = api
        .rest("POST", "/repos/acme/app/actions/runs/77/cancel")
        .await;

    // then
    assert_eq!(refused.status, 409);
    assert_eq!(fake.counts().run_cancels, 0);
}

#[tokio::test(start_paused = true)]
async fn should_cancel_every_open_job_of_the_run_when_cancelled() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    let jobs: Vec<u64> = (0..2)
        .map(|_| {
            fake.queue_job(JobSpec {
                run_id: 77,
                ..JobSpec::default()
            })
        })
        .collect();

    // when
    let cancelled = api
        .rest("POST", "/repos/acme/app/actions/runs/77/cancel")
        .await;
    let run = api.rest("GET", "/repos/acme/app/actions/runs/77").await;

    // then
    assert_eq!(cancelled.status, 202);
    assert_eq!(run.json()["conclusion"], "cancelled");
    let world = fake.world();
    for job in jobs {
        assert!(matches!(
            &world.jobs[&job].state,
            mayfly_fake_github::JobState::Finished { result } if result == "canceled"
        ));
    }
}

#[tokio::test(start_paused = true)]
async fn should_answer_the_fork_pr_setting_and_the_runner_group_when_read() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    fake.update_knobs(|k| {
        k.fork_pr_setting.run_workflows_from_fork_pull_requests = true;
        k.runner_groups[0].allows_public_repositories = true;
    });

    // when
    let setting = api
        .rest(
            "GET",
            "/orgs/acme/actions/permissions/fork-pr-workflows-private-repos",
        )
        .await;
    let group = api.rest("GET", "/orgs/acme/actions/runner-groups/1").await;
    let other_org = api
        .rest("GET", "/orgs/umbrella/actions/runner-groups/1")
        .await;

    // then
    assert_eq!(
        setting.json()["run_workflows_from_fork_pull_requests"],
        true
    );
    assert_eq!(group.json()["allows_public_repositories"], true);
    assert_eq!(other_org.status, 404);
}

#[tokio::test(start_paused = true)]
async fn should_answer_403_when_the_installation_lacks_the_permission() {
    // given
    let keys = keys();
    let (_, setup) = contract_fake(
        &keys.public_pem,
        &keys.private_pem,
        mayfly_fake_github::Flow::Direct,
        clock(),
    );
    let poor = with_permissions(BTreeMap::from([
        (
            "organization_self_hosted_runners".to_owned(),
            "write".to_owned(),
        ),
        ("actions".to_owned(), "read".to_owned()),
        ("administration".to_owned(), "read".to_owned()),
    ]));
    let api = Api::connect(&poor, &setup).await;
    poor.queue_job(JobSpec {
        run_id: 77,
        ..JobSpec::default()
    });

    // when
    let lookup = api.rest("GET", "/repos/acme/app/actions/runs/77").await;
    let cancel = api
        .rest("POST", "/repos/acme/app/actions/runs/77/cancel")
        .await;
    let setting = api
        .rest(
            "GET",
            "/orgs/acme/actions/permissions/fork-pr-workflows-private-repos",
        )
        .await;
    let anonymous = poor
        .handle(crate::support::req(
            "GET",
            "/repos/acme/app/actions/runs/77",
            None,
            None,
        ))
        .await;

    // then
    assert_eq!(lookup.status, 200, "Actions: Read suffices for the lookup");
    assert_eq!(cancel.status, 403, "a cancel needs Actions: Write");
    assert_eq!(
        setting.status, 403,
        "the setting needs the organisation's Administration: Read, not the repository's"
    );
    assert_eq!(anonymous.status, 401);
}

/// The contract fake's App and organisation with only `permissions`.
fn with_permissions(permissions: BTreeMap<String, String>) -> mayfly_fake_github::FakeGithub {
    let keys = keys();
    let config = mayfly_fake_github::FakeConfig {
        app_client_id: mayfly_fake_github::contract::CONTRACT_APP.to_owned(),
        app_public_key_pem: keys.public_pem.clone(),
        installation_id: 11,
        scope: mayfly_fake_github::Scope::Organization("acme".to_owned()),
        web_base: "https://github.com".to_owned(),
        service_url: "http://fake.invalid/service".to_owned(),
        admin_token_lifetime_secs: 3600,
        admin_token_key_pem: keys.private_pem.clone(),
        permissions,
    };
    mayfly_fake_github::FakeGithub::new(config, clock())
}
