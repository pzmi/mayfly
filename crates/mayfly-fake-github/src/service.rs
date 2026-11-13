//! The Actions service and the message queue (spec §2–§7).

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::knobs::{AbsentRemoval, PartialAcquire};
use crate::routes::{Params, Route};
use crate::state::{
    Ctx, FakeGithub, FakeRequest, FakeResponse, State, admin_ok, not_found, unauthorized,
};
use crate::wire::{
    AGENT_EXISTS, AGENT_NOT_FOUND, JOB_STILL_RUNNING, SESSION_CONFLICT, SESSION_EXPIRED, bearer,
    empty, header, json, status, typed,
};
use crate::world::{
    JobState, Label, Runner, ScaleSet, Session, SessionState, job_message, session_live,
};

/// Answers every service route but the poll: ack and acquire on the queue token (spec §2), the
/// rest on a live admin token.
pub(crate) fn answer(state: &mut State, route: Route, ctx: &Ctx<'_>) -> FakeResponse {
    match route {
        Route::Ack => return ack(state, ctx),
        Route::AcquireJobs => return acquire(state, ctx),
        _ => {}
    }
    if !admin_ok(state, ctx.request, ctx.now) {
        return unauthorized();
    }
    if state.knobs.require_api_version && !ctx.params.query.contains_key("api-version") {
        return status(400, "The api-version query parameter is required.");
    }
    match route {
        Route::RunnerGroupByName => runner_group_by_name(state, ctx),
        Route::ScaleSetByName => scale_set_by_name(state, ctx),
        Route::ScaleSetById => scale_set_by_id(state, ctx),
        Route::CreateScaleSet => create_scale_set(state, ctx),
        Route::UpdateScaleSet => update_scale_set(state, ctx),
        Route::CreateSession => create_session(state, ctx),
        Route::RefreshSession => refresh_session(state, ctx),
        Route::DeleteSession => delete_session(state, ctx),
        Route::AcquirableJobs => acquirable_jobs(state, ctx),
        Route::GenerateJitConfig => generate_jit_config(state, ctx),
        Route::RunnerById => runner_by_id(state, ctx),
        Route::ListRunners => list_runners(state, ctx),
        Route::RemoveRunner => remove_runner(state, ctx),
        _ => not_found(),
    }
}

fn list(values: Vec<Value>) -> FakeResponse {
    json(200, &json!({"count": values.len(), "value": values}))
}

/// The live scale set a path names, or the "gone" answer.
fn live_set(state: &State, id: Option<u64>) -> Result<&ScaleSet, FakeResponse> {
    id.and_then(|id| state.world.scale_sets.get(&id))
        .filter(|s| !s.deleted)
        .ok_or_else(|| {
            status(
                state.knobs.gone_scale_set_status,
                "The runner scale set was not found.",
            )
        })
}

fn scale_set_json(state: &State, set: &ScaleSet, ctx: &Ctx<'_>) -> Value {
    json!({
        "id": set.id,
        "name": set.name,
        "runnerGroupId": set.runner_group_id,
        "runnerGroupName": set.runner_group_name,
        "labels": set.labels,
        // `ephemeral` is not in the spec's shape; the fake writes it as the reference client
        // sends it (client-assumed).
        "RunnerSetting": {"ephemeral": true, "disableUpdate": set.disable_update},
        "createdOn": crate::timefmt::rfc3339(set.created_on),
        "runnerJitConfigUrl": format!(
            "{}/_apis/runtime/runnerscalesets/{}/generatejitconfig",
            ctx.service_url(),
            set.id
        ),
        "statistics": state.world.statistics(set.id),
    })
}

fn runner_group_by_name(state: &State, ctx: &Ctx<'_>) -> FakeResponse {
    let name = ctx
        .params
        .query
        .get("groupName")
        .cloned()
        .unwrap_or_default();
    list(
        state
            .knobs
            .runner_groups
            .iter()
            .filter(|g| g.name == name)
            .map(|g| json!({"id": g.id, "name": g.name, "isDefault": g.default}))
            .collect(),
    )
}

fn scale_set_by_name(state: &State, ctx: &Ctx<'_>) -> FakeResponse {
    let group: Option<u64> = ctx
        .params
        .query
        .get("runnerGroupId")
        .and_then(|g| g.parse().ok());
    let name = ctx.params.query.get("name");
    list(
        state
            .world
            .scale_sets
            .values()
            .filter(|s| !s.deleted)
            .filter(|s| group.is_none_or(|g| s.runner_group_id == g))
            .filter(|s| name.is_none_or(|n| &s.name == n))
            .map(|s| scale_set_json(state, s, ctx))
            .collect(),
    )
}

fn scale_set_by_id(state: &State, ctx: &Ctx<'_>) -> FakeResponse {
    match live_set(state, ctx.params.id) {
        Ok(set) => json(200, &scale_set_json(state, set, ctx)),
        Err(gone) => gone,
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ScaleSetBody {
    name: Option<String>,
    #[serde(rename = "runnerGroupId")]
    runner_group_id: Option<u64>,
    labels: Option<Vec<Label>>,
    #[serde(rename = "RunnerSetting")]
    runner_setting: Option<RunnerSetting>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RunnerSetting {
    #[serde(rename = "disableUpdate")]
    disable_update: bool,
}

fn create_scale_set(state: &mut State, ctx: &Ctx<'_>) -> FakeResponse {
    let Ok(body) = serde_json::from_slice::<ScaleSetBody>(&ctx.request.body) else {
        return status(400, "The request body is not a scale set.");
    };
    let Some(name) = body.name.filter(|n| !n.trim().is_empty()) else {
        return status(400, "The scale set needs a name.");
    };
    let group_id = body.runner_group_id.unwrap_or(1);
    let Some(group) = state
        .knobs
        .runner_groups
        .iter()
        .find(|g| g.id == group_id)
        .cloned()
    else {
        return status(404, "The runner group was not found.");
    };
    let existing = state
        .world
        .scale_sets
        .values()
        .find(|s| !s.deleted && s.runner_group_id == group_id && s.name == name)
        .map(|s| s.id);
    if let Some(id) = existing {
        if state.knobs.duplicate_create_returns_existing {
            let set = &state.world.scale_sets[&id];
            return json(200, &scale_set_json(state, set, ctx));
        }
        return status(409, "A runner scale set with this name already exists.");
    }
    let labels = body.labels.unwrap_or_else(|| {
        vec![Label {
            kind: "System".to_owned(),
            name: name.clone(),
        }]
    });
    let id = state.world.next_id();
    let first = state.knobs.first_message_id;
    let set = ScaleSet {
        id,
        name,
        runner_group_id: group.id,
        runner_group_name: group.name,
        labels,
        disable_update: body.runner_setting.is_some_and(|r| r.disable_update),
        created_on: ctx.now,
        deleted: false,
        max_capacity: 0,
        pending: Vec::new(),
        queue: std::collections::VecDeque::new(),
        next_message_id: first,
        queue_generation: 0,
    };
    state.world.scale_sets.insert(id, set);
    json(
        200,
        &scale_set_json(state, &state.world.scale_sets[&id], ctx),
    )
}

fn update_scale_set(state: &mut State, ctx: &Ctx<'_>) -> FakeResponse {
    if let Err(gone) = live_set(state, ctx.params.id) {
        return gone;
    }
    let Ok(body) = serde_json::from_slice::<ScaleSetBody>(&ctx.request.body) else {
        return status(400, "The request body is not a scale set.");
    };
    let id = ctx.params.id.unwrap_or_default();
    if let Some(set) = state.world.scale_sets.get_mut(&id) {
        if let Some(name) = body.name.filter(|n| !n.trim().is_empty()) {
            set.name = name;
        }
        if let Some(labels) = body.labels {
            set.labels = labels;
        }
        if let Some(setting) = body.runner_setting {
            set.disable_update = setting.disable_update;
        }
    }
    json(
        200,
        &scale_set_json(state, &state.world.scale_sets[&id], ctx),
    )
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SessionBody {
    #[serde(rename = "ownerName")]
    owner_name: String,
}

fn queue_url(state: &State, set: &ScaleSet, ctx: &Ctx<'_>) -> String {
    let base = state
        .knobs
        .queue_url_base
        .as_deref()
        .unwrap_or(ctx.service_url())
        .trim_end_matches('/')
        .to_owned();
    format!("{base}/_queue/{}/{}/messages", set.id, set.queue_generation)
}

fn session_json(state: &State, session: &Session, ctx: &Ctx<'_>) -> Value {
    let set = &state.world.scale_sets[&session.scale_set];
    json!({
        "sessionId": session.id,
        "ownerName": session.owner,
        "runnerScaleSet": scale_set_json(state, set, ctx),
        "messageQueueUrl": queue_url(state, set, ctx),
        "messageQueueAccessToken": session.token,
        "statistics": state.world.statistics(set.id),
    })
}

fn new_queue_token(state: &mut State) -> String {
    let serial = state.serial();
    let token = format!("fake-queue-token-{serial:08}");
    state.issued.push(token.clone());
    token
}

/// One live session per scale set: a second answers 409 `RunnerScaleSetSessionConflictException`
/// (spec §3.1). Pinned by: the contract's WC-03.
fn create_session(state: &mut State, ctx: &Ctx<'_>) -> FakeResponse {
    let set = match live_set(state, ctx.params.id) {
        Ok(set) => set,
        Err(gone) => return gone,
    };
    let (set_id, set_name) = (set.id, set.name.clone());
    let owner = serde_json::from_slice::<SessionBody>(&ctx.request.body)
        .map(|b| b.owner_name)
        .unwrap_or_default();
    if let Some(live) = state.world.live_session(set_id, &state.knobs, ctx.now) {
        return typed(
            409,
            SESSION_CONFLICT,
            &format!(
                "The actions runner scaleset {set_name} already has an active session for owner {}.",
                live.owner
            ),
        );
    }
    let serial = state.world.next_id();
    let token = new_queue_token(state);
    let session = Session {
        id: format!("00000000-0000-4000-8000-{serial:012x}"),
        scale_set: set_id,
        owner,
        token,
        token_expires: ctx.now + state.knobs.queue_token_lifetime_secs,
        opened_at: ctx.now,
        state: SessionState::Live,
    };
    let survives = state.knobs.queue_survives_session;
    if let Some(set) = state.world.scale_sets.get_mut(&set_id) {
        set.max_capacity = 0;
        if !survives {
            set.queue.clear();
        }
    }
    let body = session_json(state, &session, ctx);
    state.world.sessions.insert(session.id.clone(), session);
    state.counts.sessions_created += 1;
    json(200, &body)
}

fn expired(id: &str) -> FakeResponse {
    typed(
        400,
        SESSION_EXPIRED,
        &format!("The session identifier {id} is not valid."),
    )
}

/// The session a path names, live and of the path's scale set.
fn path_session<'a>(state: &'a State, ctx: &Ctx<'_>) -> Option<&'a Session> {
    let id = ctx.params.text.as_deref()?;
    state
        .world
        .sessions
        .get(id)
        .filter(|s| Some(s.scale_set) == ctx.params.id)
        .filter(|s| session_live(s, &state.knobs, ctx.now))
}

/// A new queue token; a session that is gone answers 400 `RunnerScaleSetSessionExpiredException`
/// (spec §3.1). Pinned by: the contract's WC-04 and WC-13,
/// `tests/wire/scaleset.rs::should_expire_a_session_when_its_lifetime_passes`.
fn refresh_session(state: &mut State, ctx: &Ctx<'_>) -> FakeResponse {
    if let Err(gone) = live_set(state, ctx.params.id) {
        return gone;
    }
    let Some(id) = path_session(state, ctx).map(|s| s.id.clone()) else {
        return expired(ctx.params.text.as_deref().unwrap_or_default());
    };
    let token = new_queue_token(state);
    let lifetime = state.knobs.queue_token_lifetime_secs;
    let moves = state.knobs.refresh_moves_queue_url;
    let Some(session) = state.world.sessions.get_mut(&id) else {
        return expired(&id);
    };
    session.token = token;
    session.token_expires = ctx.now + lifetime;
    let set_id = session.scale_set;
    if moves && let Some(set) = state.world.scale_sets.get_mut(&set_id) {
        set.queue_generation += 1;
    }
    let session = state.world.sessions[&id].clone();
    json(200, &session_json(state, &session, ctx))
}

fn delete_session(state: &mut State, ctx: &Ctx<'_>) -> FakeResponse {
    if let Err(gone) = live_set(state, ctx.params.id) {
        return gone;
    }
    let Some(id) = path_session(state, ctx).map(|s| s.id.clone()) else {
        return expired(ctx.params.text.as_deref().unwrap_or_default());
    };
    if let Some(session) = state.world.sessions.get_mut(&id) {
        session.state = SessionState::Deleted;
        if let Some(set) = state.world.scale_sets.get_mut(&session.scale_set) {
            set.max_capacity = 0;
        }
    }
    empty(204)
}

/// The live session whose queue token `request` carries, for scale set `set` at `now`; `Err`
/// is the answer to give instead.
fn queue_session(
    state: &State,
    request: &FakeRequest,
    params: &Params,
    now: u64,
) -> Result<String, FakeResponse> {
    let set = live_set(state, params.id)?;
    if params.sub.is_some_and(|g| g != set.queue_generation) {
        return Err(not_found());
    }
    let token = bearer(request).ok_or_else(unauthorized)?;
    let session = state
        .world
        .sessions
        .values()
        .find(|s| s.token == token && s.scale_set == set.id)
        .ok_or_else(unauthorized)?;
    if !session_live(session, &state.knobs, now) || session.token_expires <= now {
        return Err(status(401, "The message queue token is expired."));
    }
    Ok(session.id.clone())
}

/// The long poll (spec §4.1): records the advertised capacity, then answers the oldest message
/// not acked, a new one, or 202 once the hold expires. Pinned by
/// `tests/wire/scaleset.rs::should_answer_202_when_the_hold_expires_with_nothing_to_deliver`.
pub(crate) async fn poll(
    fake: &FakeGithub,
    params: &Params,
    request: &FakeRequest,
) -> FakeResponse {
    let capacity = match header(request, "x-scalesetmaxcapacity") {
        None => None,
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(n) => Some(n),
            Err(_) => return status(400, "X-ScaleSetMaxCapacity is not a number."),
        },
    };
    let last: Option<u64> = match params.query.get("lastMessageId") {
        None => None,
        Some(raw) => match raw.parse() {
            Ok(n) => Some(n),
            Err(_) => return status(400, "lastMessageId is not a number."),
        },
    };
    let hold = {
        let now = fake.now();
        let mut state = fake.state();
        if let Err(refused) = queue_session(&state, request, params, now) {
            return refused;
        }
        if let (Some(capacity), Some(set)) = (
            capacity,
            params.id.and_then(|id| state.world.scale_sets.get_mut(&id)),
        ) {
            set.max_capacity = capacity;
        }
        let knobs = state.knobs.clone();
        state.world.assign(&knobs, now);
        knobs.poll_hold
    };
    fake.changed();
    let deadline = tokio::time::Instant::now() + hold;
    let changes = fake.changes();
    loop {
        let notified = changes.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        {
            let now = fake.now();
            let mut state = fake.state();
            if let Err(refused) = queue_session(&state, request, params, now) {
                return refused;
            }
            let knobs = state.knobs.clone();
            let set = params.id.unwrap_or_default();
            if let Some(message) = state.world.take_message(set, last, &knobs) {
                state.counts.messages_delivered += 1;
                let body = serde_json::to_string(&message.jobs).unwrap_or_default();
                return json(
                    200,
                    &json!({
                        "messageId": message.id,
                        "messageType": "RunnerScaleSetJobMessages",
                        "body": body,
                        "statistics": message.statistics,
                    }),
                );
            }
        }
        tokio::select! {
            () = &mut notified => {}
            () = crate::wait::until(deadline) => {
                let body = fake.state().knobs.accepted_body.clone();
                return FakeResponse { status: 202, headers: Vec::new(), body: body.into_bytes() };
            }
            () = fake.wait_closed() => return status(503, "closed"),
        }
    }
}

fn ack(state: &mut State, ctx: &Ctx<'_>) -> FakeResponse {
    if let Err(refused) = queue_session(state, ctx.request, ctx.params, ctx.now) {
        return refused;
    }
    let message: Option<u64> = ctx
        .params
        .query
        .get("messageId")
        .and_then(|m| m.parse().ok());
    let unknown = state.knobs.ack_of_unknown_status;
    let Some(set) = ctx
        .params
        .id
        .and_then(|id| state.world.scale_sets.get_mut(&id))
    else {
        return not_found();
    };
    let before = set.queue.len();
    set.queue.retain(|m| Some(m.id) != message);
    if set.queue.len() == before {
        return if unknown == 204 {
            empty(204)
        } else {
            status(unknown, "The message was not found.")
        };
    }
    state.counts.acks += 1;
    empty(204)
}

fn acquire(state: &mut State, ctx: &Ctx<'_>) -> FakeResponse {
    if state.knobs.require_api_version && !ctx.params.query.contains_key("api-version") {
        return status(400, "The api-version query parameter is required.");
    }
    let sub = Params {
        id: ctx.params.id,
        sub: None,
        ..Params::default()
    };
    if let Err(refused) = queue_session(state, ctx.request, &sub, ctx.now) {
        return refused;
    }
    let Ok(ids) = serde_json::from_slice::<Vec<u64>>(&ctx.request.body) else {
        return status(400, "The body is not a list of request ids.");
    };
    let ids: std::collections::BTreeSet<u64> = ids.into_iter().collect();
    let set = ctx.params.id.unwrap_or_default();
    let mut take = Vec::new();
    let mut listed = Vec::new();
    let mut unavailable = false;
    for id in &ids {
        let job = state.world.jobs.values().find(|j| match j.state {
            JobState::Offered { request, .. }
            | JobState::Assigned { request, .. }
            | JobState::Running { request, .. } => request == *id && *id != 0,
            _ => false,
        });
        match job.map(|j| (j.id, &j.state)) {
            Some((job, JobState::Offered { scale_set, .. })) if *scale_set == set => {
                take.push(job);
                listed.push(*id);
            }
            Some((
                _,
                JobState::Assigned { scale_set, .. } | JobState::Running { scale_set, .. },
            )) if *scale_set == set => {
                if state.knobs.reacquire_lists_again {
                    listed.push(*id);
                }
            }
            _ => unavailable = true,
        }
    }
    if unavailable && state.knobs.partial_acquire == PartialAcquire::FailRequest {
        return status(400, "One or more jobs are no longer available.");
    }
    for job in take {
        if let Some(j) = state.world.jobs.get_mut(&job)
            && let JobState::Offered { scale_set, request } = j.state
        {
            j.state = JobState::Assigned { scale_set, request };
            state.world.emit(scale_set, "JobAssigned", job);
        }
    }
    json(200, &json!({"count": listed.len(), "value": listed}))
}

fn acquirable_jobs(state: &State, ctx: &Ctx<'_>) -> FakeResponse {
    let set = match live_set(state, ctx.params.id) {
        Ok(set) => set.id,
        Err(gone) => return gone,
    };
    let offers: Vec<Value> = state
        .world
        .jobs
        .values()
        .filter(|j| matches!(j.state, JobState::Offered { scale_set, .. } if scale_set == set))
        .map(|j| job_message("JobAvailable", j, &state.world.runners))
        .collect();
    if offers.is_empty() {
        return empty(204);
    }
    list(offers)
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct JitBody {
    name: String,
    #[serde(rename = "workFolder")]
    work_folder: String,
}

fn runner_json(runner: &Runner) -> Value {
    json!({
        "id": runner.id,
        "name": runner.name,
        "runnerScaleSetId": runner.scale_set,
        // `status` and `busy` are not in the spec's runner reference (client-assumed).
        "status": if runner.online { "online" } else { "offline" },
        "busy": runner.job.is_some(),
    })
}

fn generate_jit_config(state: &mut State, ctx: &Ctx<'_>) -> FakeResponse {
    let set = match live_set(state, ctx.params.id) {
        Ok(set) => set.id,
        Err(gone) => return gone,
    };
    let Ok(body) = serde_json::from_slice::<JitBody>(&ctx.request.body) else {
        return status(400, "The body is not a JIT request.");
    };
    if body.name.trim().is_empty() {
        return status(400, "The runner needs a name.");
    }
    if state.world.runners.values().any(|r| r.name == body.name) {
        return typed(
            409,
            AGENT_EXISTS,
            &format!("A runner with the name {} already exists.", body.name),
        );
    }
    let id = state.world.next_id();
    let runner = Runner {
        id,
        name: body.name.clone(),
        scale_set: set,
        online: false,
        job: None,
    };
    let serial = state.serial();
    let files = json!({
        ".runner": STANDARD.encode(json!({
            "agentId": id,
            "agentName": body.name,
            "poolId": 0,
            "serverUrl": ctx.service_url(),
            "workFolder": body.work_folder,
        }).to_string()),
        ".credentials": STANDARD.encode(format!("fake-runner-credential-{serial:08}")),
    });
    let encoded = STANDARD.encode(files.to_string());
    state.issued.push(encoded.clone());
    let answer = json!({"runner": runner_json(&runner), "encodedJITConfig": encoded});
    state.world.runners.insert(id, runner);
    state.counts.jit_configs += 1;
    json(200, &answer)
}

fn runner_by_id(state: &State, ctx: &Ctx<'_>) -> FakeResponse {
    match ctx.params.sub.and_then(|id| state.world.runners.get(&id)) {
        Some(runner) => json(200, &runner_json(runner)),
        None => typed(404, AGENT_NOT_FOUND, "The runner was not found."),
    }
}

fn list_runners(state: &mut State, ctx: &Ctx<'_>) -> FakeResponse {
    let name = ctx.params.query.get("agentName");
    if name.is_none() {
        state.counts.runner_listings += 1;
    }
    list(
        state
            .world
            .runners
            .values()
            .filter(|r| name.is_none_or(|n| &r.name == n))
            .map(runner_json)
            .collect(),
    )
}

/// 204 removed or absent (or 404 by knob); a busy runner refused with `JobStillRunningException`
/// (spec §7). Pinned by `tests/wire/scaleset.rs::should_answer_the_knob_when_removing_an_absent_runner`
/// and `should_refuse_removal_with_the_knob_status_when_the_runner_is_busy`.
fn remove_runner(state: &mut State, ctx: &Ctx<'_>) -> FakeResponse {
    let id = ctx.params.sub.unwrap_or_default();
    let Some(runner) = state.world.runners.get(&id) else {
        return match state.knobs.absent_removal {
            AbsentRemoval::NoContent => empty(204),
            AbsentRemoval::NotFound => typed(404, AGENT_NOT_FOUND, "The runner was not found."),
        };
    };
    if runner.job.is_some() {
        return typed(
            state.knobs.busy_removal_status,
            JOB_STILL_RUNNING,
            "The runner is running a job and cannot be removed.",
        );
    }
    state.world.runners.remove(&id);
    state.counts.runners_removed += 1;
    empty(204)
}
