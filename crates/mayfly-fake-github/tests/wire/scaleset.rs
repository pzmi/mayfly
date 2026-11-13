//! The Scale Set routes and the knobs that bend them (spec §2–§7).

use std::time::Duration;

use serde_json::json;

use mayfly_fake_github::{AbsentRemoval, Flow, Knobs, PartialAcquire, StatisticsAt};

use crate::support::{Api, fake, fake_with, job, knobs, message, session};

#[tokio::test(start_paused = true)]
async fn should_answer_202_when_the_hold_expires_with_nothing_to_deliver() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    let s = api.open(api.scale_set().await).await;
    let started = tokio::time::Instant::now();

    // when
    let answer = api.poll(&s, 1, None).await;

    // then
    assert_eq!(answer.status, 202);
    assert!(answer.body.is_empty());
    assert_eq!(started.elapsed(), Duration::from_secs(50));
}

#[tokio::test(start_paused = true)]
async fn should_wake_a_held_poll_when_a_job_is_assigned_during_the_hold() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    let s = api.open(api.scale_set().await).await;
    let first = api.poll(&s, 1, None).await;
    assert_eq!(first.status, 202);
    let started = tokio::time::Instant::now();
    let poll = {
        let (api, s) = (clone_api(&api), s.clone());
        tokio::spawn(async move { api.poll(&s, 1, None).await })
    };
    tokio::task::yield_now().await;
    assert!(!poll.is_finished(), "the poll holds");

    // when
    fake.queue_job(job(5));
    let answer = poll.await.expect("poll task");

    // then
    let (_, jobs) = message(&answer);
    assert_eq!(jobs[0]["messageType"], "JobAssigned");
    assert!(started.elapsed() < Duration::from_secs(50));
}

fn clone_api(api: &Api) -> Api {
    Api {
        fake: api.fake.clone(),
        installation: api.installation.clone(),
        service: api.service.clone(),
        admin: api.admin.clone(),
    }
}

#[tokio::test(start_paused = true)]
async fn should_redeliver_an_unacked_message_to_the_next_session_when_one_reopens() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    let set = api.scale_set().await;
    let s = api.open(set).await;
    fake.queue_job(job(5));
    let (sent, _) = message(&api.poll(&s, 1, None).await);
    let deleted = api
        .svc(
            "DELETE",
            &format!("runtime/runnerscalesets/{set}/sessions/{}", s.id),
            None,
        )
        .await;
    assert_eq!(deleted.status, 204);

    // when
    let next = api.open(set).await;
    let (again, _) = message(&api.poll(&next, 1, None).await);

    // then
    assert_eq!(again, sent);
    assert_eq!(sent, 100_000_001);
}

#[tokio::test(start_paused = true)]
async fn should_skip_a_delivered_message_when_last_message_id_passes_it() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    let s = api.open(api.scale_set().await).await;
    fake.queue_job(job(5));
    let (first, _) = message(&api.poll(&s, 2, None).await);
    fake.queue_job(job(6));

    // when
    let (second, jobs) = message(&api.poll(&s, 2, Some(first)).await);
    let (again, _) = message(&api.poll(&s, 2, None).await);

    // then
    assert_eq!(second, first + 1);
    assert_eq!(jobs[0]["workflowRunId"], 6);
    assert_eq!(
        again, first,
        "without lastMessageId the oldest unacked comes back"
    );
}

#[tokio::test(start_paused = true)]
async fn should_ack_earlier_messages_when_the_last_message_id_knob_acks() {
    // given
    let (fake, setup) = fake_with(Knobs {
        last_message_id_acks: true,
        ..Knobs::default()
    });
    let api = Api::connect(&fake, &setup).await;
    let s = api.open(api.scale_set().await).await;
    fake.queue_job(job(5));
    let (first, _) = message(&api.poll(&s, 1, None).await);

    // when
    let after = api.poll(&s, 1, Some(first)).await;
    let ack = api.ack(&s, first).await;

    // then
    assert_eq!(after.status, 202);
    assert_eq!(ack.status, 404, "already acked by lastMessageId");
}

#[tokio::test(start_paused = true)]
async fn should_answer_the_knob_status_when_acking_an_unknown_message() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    let s = api.open(api.scale_set().await).await;

    // when
    let default = api.ack(&s, 7).await;
    fake.update_knobs(|k| k.ack_of_unknown_status = 204);
    let lenient = api.ack(&s, 7).await;

    // then
    assert_eq!(default.status, 404);
    assert_eq!(lenient.status, 204);
}

#[tokio::test(start_paused = true)]
async fn should_number_from_the_restart_when_the_numbering_restarts() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    let set = api.scale_set().await;
    let s = api.open(set).await;
    fake.queue_job(job(5));
    let (first, _) = message(&api.poll(&s, 2, None).await);
    assert_eq!(api.ack(&s, first).await.status, 204);

    // when
    fake.restart_numbering(set, 3).expect("restart");
    fake.queue_job(job(6));
    let (next, _) = message(&api.poll(&s, 2, Some(first)).await);

    // then
    assert_eq!(next, 3);
}

#[tokio::test(start_paused = true)]
async fn should_cap_assignment_at_the_advertised_capacity_when_polled() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    let s = api.open(api.scale_set().await).await;
    for run in 1..=3 {
        fake.queue_job(job(run));
    }

    // when
    let (id, jobs) = message(&api.poll(&s, 2, None).await);
    let stats = api.poll(&s, 2, None).await.json()["statistics"].clone();
    assert_eq!(api.ack(&s, id).await.status, 204);
    let held = api.poll(&s, 2, Some(id)).await;

    // then
    assert_eq!(jobs.len(), 2);
    assert_eq!(stats["totalAssignedJobs"], 2);
    assert_eq!(held.status, 202, "the third job waits for capacity");
}

#[tokio::test(start_paused = true)]
async fn should_requeue_until_the_last_release_fails_it_when_an_assigned_job_is_released() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    let s = api.open(api.scale_set().await).await;
    let id = fake.queue_job(job(5));
    let mut last = None;
    let mut kinds = Vec::new();

    // when
    for _ in 0..3 {
        let (m, jobs) = message(&api.poll(&s, 1, last).await);
        kinds.extend(
            jobs.iter()
                .map(|j| (j["messageType"].clone(), j["result"].clone())),
        );
        assert_eq!(api.ack(&s, m).await.status, 204);
        last = Some(m);
        fake.release_job(id).expect("release");
    }
    let (_, jobs) = message(&api.poll(&s, 1, last).await);
    kinds.extend(
        jobs.iter()
            .map(|j| (j["messageType"].clone(), j["result"].clone())),
    );

    // then
    let assigned = kinds.iter().filter(|(k, _)| k == "JobAssigned").count();
    let canceled = kinds
        .iter()
        .filter(|(k, r)| k == "JobCompleted" && r == "canceled")
        .count();
    assert_eq!((assigned, canceled), (3, 3));
    assert_eq!(fake.world().jobs[&id].releases, 3);
    assert!(matches!(
        &fake.world().jobs[&id].state,
        mayfly_fake_github::JobState::Finished { result } if result == "failed"
    ));
}

#[tokio::test(start_paused = true)]
async fn should_answer_gone_and_requeue_its_jobs_when_a_dormant_scale_set_is_deleted() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    let set = api.scale_set().await;
    let s = api.open(set).await;
    let id = fake.queue_job(job(5));
    let _ = api.poll(&s, 1, None).await;

    // when
    fake.delete_dormant(set).expect("delete");
    let read = api
        .svc("GET", &format!("runtime/runnerscalesets/{set}"), None)
        .await;
    let open = api
        .svc(
            "POST",
            &format!("runtime/runnerscalesets/{set}/sessions"),
            Some(json!({})),
        )
        .await;
    let recreated = api.scale_set().await;

    // then
    assert_eq!(read.status, 404);
    assert_eq!(open.status, 404);
    assert_ne!(recreated, set);
    assert_eq!(
        fake.world().jobs[&id].state,
        mayfly_fake_github::JobState::Queued
    );
}

#[tokio::test(start_paused = true)]
async fn should_expire_a_session_when_its_lifetime_passes() {
    // given
    let (fake, setup) = fake_with(Knobs {
        session_lifetime_secs: Some(600),
        ..Knobs::default()
    });
    let api = Api::connect(&fake, &setup).await;
    let set = api.scale_set().await;
    let s = api.open(set).await;

    // when
    tokio::time::advance(Duration::from_secs(601)).await;
    let refresh = api
        .svc(
            "PATCH",
            &format!("runtime/runnerscalesets/{set}/sessions/{}", s.id),
            None,
        )
        .await;
    let poll = api.poll(&s, 1, None).await;
    let reopened = api
        .svc(
            "POST",
            &format!("runtime/runnerscalesets/{set}/sessions"),
            Some(json!({})),
        )
        .await;

    // then
    assert_eq!(refresh.status, 400);
    assert!(
        refresh.json()["typeName"]
            .as_str()
            .unwrap_or_default()
            .contains("SessionExpired")
    );
    assert_eq!(poll.status, 401);
    assert_eq!(reopened.status, 200);
}

#[tokio::test(start_paused = true)]
async fn should_move_the_queue_url_when_a_refresh_moves_it() {
    // given
    let (fake, setup) = fake_with(Knobs {
        refresh_moves_queue_url: true,
        ..Knobs::default()
    });
    let api = Api::connect(&fake, &setup).await;
    let set = api.scale_set().await;
    let s = api.open(set).await;

    // when
    let refreshed = api
        .svc(
            "PATCH",
            &format!("runtime/runnerscalesets/{set}/sessions/{}", s.id),
            None,
        )
        .await;
    let moved = session(&refreshed.json());
    let old = api
        .poll(
            &crate::support::Session {
                token: moved.token.clone(),
                ..s.clone()
            },
            1,
            None,
        )
        .await;
    let new = api.poll(&moved, 1, None).await;

    // then
    assert_ne!(moved.queue, s.queue);
    assert_eq!(old.status, 404);
    assert_eq!(new.status, 202);
}

#[tokio::test(start_paused = true)]
async fn should_list_a_runner_added_by_the_control_api_when_its_set_was_added() {
    // given: a scale set and a runner nobody in this test minted
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    let set = fake.add_scale_set("left-behind", "Default").expect("a set");
    let runner = fake.add_runner(set, "mf-left-1").expect("a runner");

    // when
    let all = api.svc("GET", "distributedtask/pools/0/agents", None).await;
    let by_name = api
        .svc(
            "GET",
            "distributedtask/pools/0/agents?agentName=mf-left-1",
            None,
        )
        .await;
    let again = fake.add_scale_set("left-behind", "Default");
    let unknown_group = fake.add_scale_set("other", "no-such-group");
    let duplicate = fake.add_runner(set, "mf-left-1");

    // then: listed with its id and set; a full listing counted, a listing by name not
    assert_eq!(all.status, 200);
    let listed = all.json()["value"].clone();
    assert!(
        listed
            .as_array()
            .is_some_and(|v| v.iter().any(|r| r["id"].as_u64() == Some(runner))),
        "{listed}"
    );
    assert_eq!(by_name.status, 200);
    assert_eq!(fake.counts().runner_listings, 1);
    assert!(again.is_err() && unknown_group.is_err() && duplicate.is_err());
    assert_eq!(fake.world().runners[&runner].scale_set, set);
}

#[tokio::test(start_paused = true)]
async fn should_answer_the_knob_when_removing_an_absent_runner() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;

    // when
    let default = api
        .svc("DELETE", "distributedtask/pools/0/agents/424242", None)
        .await;
    fake.update_knobs(|k| k.absent_removal = AbsentRemoval::NotFound);
    let strict = api
        .svc("DELETE", "distributedtask/pools/0/agents/424242", None)
        .await;

    // then
    assert_eq!(default.status, 204);
    assert_eq!(strict.status, 404);
    assert!(
        strict.json()["typeName"]
            .as_str()
            .unwrap_or_default()
            .contains("AgentNotFoundException")
    );
}

#[tokio::test(start_paused = true)]
async fn should_refuse_removal_with_the_knob_status_when_the_runner_is_busy() {
    // given
    let (fake, setup) = fake_with(Knobs {
        busy_removal_status: 409,
        ..Knobs::default()
    });
    let api = Api::connect(&fake, &setup).await;
    let set = api.scale_set().await;
    let s = api.open(set).await;
    let id = fake.queue_job(job(5));
    let _ = api.poll(&s, 1, None).await;
    let runner = api.jit(set, "mf-vm-1").await.json()["runner"]["id"]
        .as_u64()
        .expect("id");
    fake.start_job(id, runner).expect("start");

    // when
    let refused = api
        .svc(
            "DELETE",
            &format!("distributedtask/pools/0/agents/{runner}"),
            None,
        )
        .await;
    fake.complete_job(id, "succeeded").expect("complete");
    let gone = api
        .svc(
            "GET",
            &format!("distributedtask/pools/0/agents/{runner}"),
            None,
        )
        .await;

    // then
    assert_eq!(refused.status, 409);
    assert!(
        refused.json()["typeName"]
            .as_str()
            .unwrap_or_default()
            .contains("JobStillRunningException")
    );
    assert_eq!(
        gone.status, 404,
        "an ephemeral runner is deregistered after its job"
    );
}

#[tokio::test(start_paused = true)]
async fn should_leave_out_the_omitted_fields_when_a_job_spec_names_them() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    let s = api.open(api.scale_set().await).await;
    let mut spec = job(5);
    spec.omit = vec!["eventName".to_owned(), "ownerName".to_owned()];

    // when
    fake.queue_job(spec);
    let (_, jobs) = message(&api.poll(&s, 1, None).await);

    // then
    assert!(jobs[0].get("eventName").is_none());
    assert!(jobs[0].get("ownerName").is_none());
    assert_eq!(jobs[0]["runnerRequestId"], 0);
}

#[tokio::test(start_paused = true)]
async fn should_tie_started_and_completed_to_the_runner_when_a_job_runs() {
    // given
    let (fake, setup) = fake_with(Knobs {
        completed_names_no_job: true,
        ..Knobs::default()
    });
    let api = Api::connect(&fake, &setup).await;
    let set = api.scale_set().await;
    let s = api.open(set).await;
    let id = fake.queue_job(job(5));
    let (m, _) = message(&api.poll(&s, 1, None).await);
    assert_eq!(api.ack(&s, m).await.status, 204);
    let runner = api.jit(set, "mf-vm-1").await.json()["runner"]["id"]
        .as_u64()
        .expect("id");

    // when
    fake.start_job(id, runner).expect("start");
    fake.complete_job(id, "succeeded").expect("complete");
    let (_, jobs) = message(&api.poll(&s, 1, Some(m)).await);

    // then
    assert_eq!(jobs[0]["messageType"], "JobStarted");
    assert_eq!(jobs[0]["runnerId"], runner);
    assert_eq!(jobs[1]["messageType"], "JobCompleted");
    assert_eq!(jobs[1]["result"], "succeeded");
    assert_eq!(jobs[1]["runnerName"], "mf-vm-1");
    assert!(jobs[1].get("jobId").is_none(), "the knob names no job");
}

#[tokio::test(start_paused = true)]
async fn should_refuse_service_calls_when_the_admin_token_or_api_version_is_wrong() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    let forged = Api {
        admin: "not-a-token".to_owned(),
        ..clone_api(&api)
    };

    // when
    let refused = forged.svc("GET", "runtime/runnerscalesets", None).await;
    let unversioned = fake
        .handle(crate::support::req(
            "GET",
            &format!("{}/_apis/runtime/runnerscalesets", api.service),
            Some(&format!("Bearer {}", api.admin)),
            None,
        ))
        .await;
    tokio::time::advance(Duration::from_secs(3601)).await;
    let expired = api.svc("GET", "runtime/runnerscalesets", None).await;

    // then
    assert_eq!(refused.status, 401);
    assert_eq!(unversioned.status, 400);
    assert_eq!(expired.status, 401);
}

#[tokio::test(start_paused = true)]
async fn should_fail_the_whole_acquire_when_the_partial_acquire_knob_says_so() {
    // given
    let (fake, setup) = fake_with(Knobs {
        partial_acquire: PartialAcquire::FailRequest,
        ..knobs(Flow::Acquire)
    });
    let api = Api::connect(&fake, &setup).await;
    let set = api.scale_set().await;
    let s = api.open(set).await;
    fake.queue_job(job(5));
    let (_, jobs) = message(&api.poll(&s, 1, None).await);
    let request = jobs[0]["runnerRequestId"].as_u64().expect("request");
    let acquire = |ids: serde_json::Value| {
        let mut r = crate::support::req(
            "POST",
            &format!(
                "{}/_apis/runtime/runnerscalesets/{set}/acquirejobs?api-version=6.0-preview",
                api.service
            ),
            Some(&format!("Bearer {}", s.token)),
            Some(ids),
        );
        r.headers
            .push(("content-type".to_owned(), "application/json".to_owned()));
        fake.handle(r)
    };

    // when
    let partial = acquire(json!([request, 999_999])).await;
    let whole = acquire(json!([request])).await;
    let again = acquire(json!([request])).await;

    // then
    assert_eq!(partial.status, 400);
    assert_eq!(whole.json()["value"], json!([request]));
    assert_eq!(
        again.json()["value"],
        json!([request]),
        "re-acquire lists it again"
    );
    assert_eq!(fake.world().statistics(set).total_acquired_jobs, 1);
}

#[tokio::test(start_paused = true)]
async fn should_answer_204_when_no_job_is_acquirable() {
    // given
    let (fake, setup) = fake_with(knobs(Flow::Acquire));
    let api = Api::connect(&fake, &setup).await;
    let set = api.scale_set().await;

    // when
    let none = api
        .svc(
            "GET",
            &format!("runtime/runnerscalesets/{set}/acquirablejobs"),
            None,
        )
        .await;

    // then
    assert_eq!(none.status, 204);
}

#[tokio::test(start_paused = true)]
async fn should_keep_tokens_and_jit_configs_out_when_the_state_is_dumped() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    let set = api.scale_set().await;
    let _ = api.open(set).await;
    let _ = api.jit(set, "mf-vm-1").await;

    // when
    let dump = fake.state_json().to_string();

    // then
    let issued = fake.issued_tokens();
    assert!(issued.len() >= 5);
    for secret in issued {
        assert!(
            !dump.contains(&secret),
            "the state dump holds an issued secret"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn should_answer_409_or_the_existing_set_when_a_name_is_created_twice() {
    // given
    let (fake, setup) = fake();
    let api = Api::connect(&fake, &setup).await;
    let first = api.scale_set().await;

    // when
    let conflict = api
        .svc(
            "POST",
            "runtime/runnerscalesets",
            Some(json!({"name": "mf", "runnerGroupId": 1})),
        )
        .await;
    fake.update_knobs(|k| k.duplicate_create_returns_existing = true);
    let existing = api
        .svc(
            "POST",
            "runtime/runnerscalesets",
            Some(json!({"name": "mf", "runnerGroupId": 1})),
        )
        .await;

    // then
    assert_eq!(conflict.status, 409);
    assert_eq!(existing.json()["id"], first);
}

/// The `totalRegisteredRunners` of a message whose job was assigned before a runner registered,
/// with statistics taken `at`.
async fn runners_counted(at: StatisticsAt) -> serde_json::Value {
    let (fake, setup) = fake_with(Knobs {
        statistics_at: at,
        ..Knobs::default()
    });
    let api = Api::connect(&fake, &setup).await;
    let set = api.scale_set().await;
    let s = api.open(set).await;
    assert_eq!(api.poll(&s, 1, None).await.status, 202);
    fake.queue_job(job(5));
    assert_eq!(api.jit(set, "mf-vm-1").await.status, 200);
    api.poll(&s, 1, None).await.json()["statistics"]["totalRegisteredRunners"].clone()
}

#[tokio::test(start_paused = true)]
async fn should_carry_the_counts_of_its_last_event_when_statistics_are_taken_at_the_event() {
    // given
    let modes = [StatisticsAt::Build, StatisticsAt::Event];

    // when
    let mut counts = Vec::new();
    for at in modes {
        counts.push(runners_counted(at).await);
    }

    // then
    assert_eq!(
        counts,
        vec![json!(1), json!(0)],
        "build time, then event time (A6)"
    );
}

#[tokio::test(start_paused = true)]
async fn should_open_the_next_session_on_an_empty_queue_when_the_queue_does_not_survive() {
    // given
    let (fake, setup) = fake_with(Knobs {
        queue_survives_session: false,
        ..Knobs::default()
    });
    let api = Api::connect(&fake, &setup).await;
    let set = api.scale_set().await;
    let s = api.open(set).await;
    fake.queue_job(job(5));
    let _ = message(&api.poll(&s, 1, None).await);
    let deleted = api
        .svc(
            "DELETE",
            &format!("runtime/runnerscalesets/{set}/sessions/{}", s.id),
            None,
        )
        .await;
    assert_eq!(deleted.status, 204);

    // when
    let next = api.open(set).await;
    let answer = api.poll(&next, 1, None).await;

    // then
    assert_eq!(
        answer.status, 202,
        "the unacked message did not survive the session"
    );
}
