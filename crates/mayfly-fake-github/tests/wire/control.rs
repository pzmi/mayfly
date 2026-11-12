//! The control API over HTTP (ver §7.3): a job scripted through its life, refusals, the dump.

use serde_json::{Value, json};

use mayfly_fake_github::FakeServer;
use mayfly_fake_github::contract::{Socket, WireRequest, WireResponse, WireTarget};

use crate::support::{Api, fake, message};

async fn call(target: &Socket, method: &str, path: &str, body: Option<Value>) -> WireResponse {
    target
        .send(WireRequest {
            method: method.to_owned(),
            url: format!("http://fake.invalid{path}"),
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: body.map(|b| b.to_string().into_bytes()).unwrap_or_default(),
        })
        .await
        .expect("answer")
}

#[tokio::test]
async fn should_script_a_job_through_its_life_when_driven_over_http() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    let set = api.scale_set().await;
    let s = api.open(set).await;
    let server = FakeServer::start(fake.clone()).await.expect("start");
    let target = Socket(
        server
            .base_url()
            .trim_start_matches("http://")
            .parse()
            .expect("addr"),
    );

    // when
    let queued = call(
        &target,
        "POST",
        "/_fake/jobs",
        Some(json!({"run_id": 5, "labels": ["mf"]})),
    )
    .await;
    let job = queued.json()["jobId"].as_u64().expect("job id");
    let (m, assigned) = message(&api.poll(&s, 1, None).await);
    let runner = api.jit(set, "mf-vm-1").await.json()["runner"]["id"]
        .as_u64()
        .expect("runner");
    let started = call(
        &target,
        "POST",
        &format!("/_fake/jobs/{job}/start"),
        Some(json!({"runnerId": runner})),
    )
    .await;
    let completed = call(
        &target,
        "POST",
        &format!("/_fake/jobs/{job}/complete"),
        Some(json!({"result": "succeeded"})),
    )
    .await;
    let (_, later) = message(&api.poll(&s, 1, Some(m)).await);
    let state = call(&target, "GET", "/_fake/state", None).await.json();
    server.stop().await.expect("stop");

    // then
    assert_eq!(
        (queued.status, started.status, completed.status),
        (201, 204, 204)
    );
    assert_eq!(assigned[0]["jobId"], job.to_string());
    let kinds: Vec<&str> = later
        .iter()
        .filter_map(|j| j["messageType"].as_str())
        .collect();
    assert_eq!(kinds, vec!["JobStarted", "JobCompleted"]);
    assert_eq!(state["world"]["jobs"][job.to_string()]["state"], "finished");
    assert_eq!(state["counts"]["jit_configs"], 1);
}

#[tokio::test]
async fn should_refuse_a_control_call_when_it_does_not_apply() {
    // given
    let (fake, _) = fake();
    let server = FakeServer::start(fake.clone()).await.expect("start");
    let target = Socket(
        server
            .base_url()
            .trim_start_matches("http://")
            .parse()
            .expect("addr"),
    );

    // when
    let unknown_job = call(&target, "POST", "/_fake/jobs/99/release", None).await;
    let bad_body = call(&target, "POST", "/_fake/jobs", Some(json!({"run_id": "x"}))).await;
    let no_session = call(&target, "POST", "/_fake/scale-sets/1/expire-session", None).await;
    let unknown = call(&target, "POST", "/_fake/nothing", None).await;
    server.stop().await.expect("stop");

    // then
    assert_eq!(
        (
            unknown_job.status,
            bad_body.status,
            no_session.status,
            unknown.status
        ),
        (409, 400, 409, 404)
    );
}

#[tokio::test]
async fn should_change_the_knobs_when_put_to_the_control_api() {
    // given
    let (fake, _) = fake();
    let server = FakeServer::start(fake.clone()).await.expect("start");
    let target = Socket(
        server
            .base_url()
            .trim_start_matches("http://")
            .parse()
            .expect("addr"),
    );
    let mut knobs = call(&target, "GET", "/_fake/knobs", None).await.json();
    knobs["flow"] = json!("acquire");
    knobs["busy_removal_status"] = json!(409);

    // when
    let put = call(&target, "PUT", "/_fake/knobs", Some(knobs)).await;
    server.stop().await.expect("stop");

    // then
    assert_eq!(put.status, 204);
    assert_eq!(fake.knobs().flow, mayfly_fake_github::Flow::Acquire);
    assert_eq!(fake.knobs().busy_removal_status, 409);
}
