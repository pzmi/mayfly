//! Fault knobs (ver §7.3): replaced answers, stalls, delays and answers lost after their effect.

use std::time::Duration;

use serde_json::json;

use mayfly_fake_github::{Fault, Route};

use crate::support::{Api, fake, job, message};

#[tokio::test(start_paused = true)]
async fn should_answer_each_queued_fault_once_in_order_when_faults_are_queued() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    fake.fail_next(Route::ScaleSetByName, Fault::retry_after(429, 30));
    fake.fail_next(
        Route::ScaleSetByName,
        Fault::rate_limited_403(1_794_315_600),
    );
    fake.fail_next(Route::ScaleSetByName, Fault::status(422));
    fake.fail_next(Route::ScaleSetByName, Fault::status(401));

    // when
    let mut answers = Vec::new();
    for _ in 0..5 {
        answers.push(
            api.svc("GET", "runtime/runnerscalesets?name=mf", None)
                .await,
        );
    }

    // then
    let statuses: Vec<u16> = answers.iter().map(|a| a.status).collect();
    assert_eq!(statuses, vec![429, 403, 422, 401, 200]);
    assert!(
        answers[0]
            .headers
            .contains(&("retry-after".to_owned(), "30".to_owned()))
    );
    assert!(
        answers[1]
            .headers
            .contains(&("x-ratelimit-remaining".to_owned(), "0".to_owned()))
    );
}

#[tokio::test(start_paused = true)]
async fn should_throttle_jit_with_too_many_runners_when_faulted() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    let set = api.scale_set().await;
    fake.fail_next(Route::GenerateJitConfig, Fault::too_many_runners(Some(600)));

    // when
    let throttled = api.jit(set, "mf-vm-1").await;
    let minted = api.jit(set, "mf-vm-1").await;

    // then
    assert_eq!(throttled.status, 429);
    assert!(
        throttled.json()["typeName"]
            .as_str()
            .unwrap_or_default()
            .contains("ConfigureTooManyRunnersException")
    );
    assert_eq!(
        minted.status, 200,
        "the throttled call took no effect, so the name is free"
    );
}

#[tokio::test(start_paused = true)]
async fn should_redeliver_the_message_when_a_poll_answered_200_with_an_empty_body() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    let s = api.open(api.scale_set().await).await;
    fake.queue_job(job(5));
    fake.fail_next(Route::Poll, Fault::empty_ok());

    // when
    let empty = api.poll(&s, 1, None).await;
    let (id, jobs) = message(&api.poll(&s, 1, None).await);

    // then
    assert_eq!(empty.status, 200);
    assert!(empty.body.is_empty());
    assert_eq!((id, jobs.len()), (100_000_001, 1));
}

#[tokio::test(start_paused = true)]
async fn should_take_the_effect_and_never_answer_when_dropped_after_effect() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    let set = api.scale_set().await;
    fake.fail_next(Route::CreateSession, Fault::DropAfterEffect);

    let lost = {
        let request = crate::support::req(
            "POST",
            &format!(
                "{}/_apis/runtime/runnerscalesets/{set}/sessions?api-version=6.0-preview",
                api.service
            ),
            Some(&format!("Bearer {}", api.admin)),
            Some(json!({})),
        );
        let fake = fake.clone();
        tokio::spawn(async move { fake.handle(request).await })
    };

    // when
    tokio::time::advance(Duration::from_secs(600)).await;
    let second = api
        .svc(
            "POST",
            &format!("runtime/runnerscalesets/{set}/sessions"),
            Some(json!({})),
        )
        .await;

    // then
    assert!(!lost.is_finished(), "the answer was lost");
    assert_eq!(second.status, 409, "the lost create opened the session");
    assert_eq!(fake.counts().sessions_created, 1);
}

#[tokio::test(start_paused = true)]
async fn should_end_a_stalled_poll_with_503_when_the_fake_closes() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    let s = api.open(api.scale_set().await).await;
    fake.fail_next(Route::Poll, Fault::Stall);
    let stalled = {
        let fake = fake.clone();
        let request =
            crate::support::req("GET", &s.queue, Some(&format!("Bearer {}", s.token)), None);
        tokio::spawn(async move { fake.handle(request).await })
    };
    tokio::time::advance(Duration::from_secs(600)).await;
    assert!(!stalled.is_finished(), "a stall outlasts any hold");

    // when
    fake.close();
    let answer = stalled.await.expect("stalled task");

    // then
    assert_eq!(answer.status, 503);
}

#[tokio::test(start_paused = true)]
async fn should_answer_after_the_delay_when_delayed() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    fake.fail_next(Route::ListRunners, Fault::Delay { ms: 7_000 });
    let started = tokio::time::Instant::now();

    // when
    let answer = api.svc("GET", "distributedtask/pools/0/agents", None).await;

    // then
    assert_eq!(answer.status, 200);
    assert_eq!(started.elapsed(), Duration::from_secs(7));
}

#[tokio::test(start_paused = true)]
async fn should_queue_a_fault_when_posted_to_the_control_api() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    let body = json!({
        "route": "scale_set_by_name",
        "fault": {"kind": "respond", "status": 503, "headers": [["retry-after", "5"]]},
    });

    // when
    let queued = fake
        .handle(crate::support::req(
            "POST",
            "/_fake/faults",
            None,
            Some(body),
        ))
        .await;
    let answer = api.svc("GET", "runtime/runnerscalesets", None).await;

    // then
    assert_eq!(queued.status, 204);
    assert_eq!(answer.status, 503);
    assert!(
        answer
            .headers
            .contains(&("retry-after".to_owned(), "5".to_owned()))
    );
}
