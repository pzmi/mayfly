//! The control API: what a test, an e2e run or the fault lab does to the world that GitHub's
//! users and runners would do (queue, start, complete, release and cancel jobs), the service's
//! own events (session expiry, dormant deletion, a restarted numbering), faults and knobs, and a
//! JSON dump of the state. In process as methods of [`FakeGithub`]; over HTTP under `/_fake/`
//! (ver §7.3), which no real GitHub path uses.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::knobs::Knobs;
use crate::routes::Route;
use crate::state::{FakeGithub, FakeResponse, Fault};
use crate::wire::{empty, json, status};
use crate::world::{Job, JobSpec, JobState, Label, Run, Runner, ScaleSet, SessionState, World};

/// Why a control call was refused.
pub type ControlError = String;

impl FakeGithub {
    /// The knobs in force.
    #[must_use]
    pub fn knobs(&self) -> Knobs {
        self.state().knobs.clone()
    }

    /// Changes the knobs.
    pub fn update_knobs(&self, change: impl FnOnce(&mut Knobs)) {
        change(&mut self.state().knobs);
        self.settle();
    }

    /// A copy of the world, for a test to read.
    #[must_use]
    pub fn world(&self) -> World {
        self.state().world.clone()
    }

    /// The state as JSON: the world (no tokens, no JIT configs, no pending job bodies), the
    /// knobs and the counts. Pinned by:
    /// `tests/wire/scaleset.rs::should_keep_tokens_and_jit_configs_out_when_the_state_is_dumped`.
    #[must_use]
    pub fn state_json(&self) -> Value {
        let state = self.state();
        json!({
            "world": state.world,
            "knobs": state.knobs,
            "counts": state.counts,
        })
    }

    /// Re-runs assignment and wakes held polls.
    fn settle(&self) {
        let now = self.now();
        {
            let mut state = self.state();
            let knobs = state.knobs.clone();
            state.world.assign(&knobs, now);
        }
        self.changed();
    }

    /// Queues a job, registering its workflow run for the REST lookup when new; returns its id.
    pub fn queue_job(&self, spec: JobSpec) -> u64 {
        let now = self.now();
        let id = {
            let mut state = self.state();
            let id = state.world.next_id();
            state.world.runs.entry(spec.run_id).or_insert_with(|| Run {
                id: spec.run_id,
                owner: spec.owner.clone(),
                repository: spec.repository.clone(),
                event: spec.event.clone(),
                head_repository: spec.head_repository.clone(),
                actor: spec.actor.clone(),
                cancelled: false,
            });
            state.world.jobs.insert(
                id,
                Job {
                    id,
                    spec,
                    state: JobState::Queued,
                    releases: 0,
                    queued_at: now,
                    assigned_at: None,
                    started_at: None,
                    finished_at: None,
                },
            );
            id
        };
        self.settle();
        id
    }

    /// A runner of the job's scale set takes the assigned job: `JobStarted`.
    ///
    /// # Errors
    /// When the job is not assigned and waiting, or the runner is not an idle runner of its
    /// scale set.
    pub fn start_job(&self, job: u64, runner: u64) -> Result<(), ControlError> {
        let now = self.now();
        {
            let mut state = self.state();
            let world = &mut state.world;
            let Some(JobState::Assigned { scale_set, request }) =
                world.jobs.get(&job).map(|j| j.state.clone())
            else {
                return Err("the job is not assigned and waiting".to_owned());
            };
            let r = world
                .runners
                .get_mut(&runner)
                .filter(|r| r.scale_set == scale_set && r.job.is_none())
                .ok_or("no idle runner of the job's scale set has that id")?;
            r.job = Some(job);
            r.online = true;
            if let Some(j) = world.jobs.get_mut(&job) {
                j.state = JobState::Running {
                    scale_set,
                    request,
                    runner,
                };
                j.started_at = Some(now);
            }
            world.emit(scale_set, "JobStarted", job);
        }
        self.settle();
        Ok(())
    }

    /// Ends a job with `result` (`succeeded`, `failed`, `canceled`): `JobCompleted` when a
    /// scale set holds it, and its runner deregistered.
    ///
    /// # Errors
    /// When the job does not exist or is over.
    pub fn complete_job(&self, job: u64, result: &str) -> Result<(), ControlError> {
        let now = self.now();
        {
            let mut state = self.state();
            let knobs = state.knobs.clone();
            let held = match state.world.jobs.get(&job).map(|j| &j.state) {
                None | Some(JobState::Finished { .. }) => {
                    return Err("the job does not exist or is over".to_owned());
                }
                Some(JobState::Queued) => None,
                Some(
                    JobState::Offered { scale_set, .. }
                    | JobState::Assigned { scale_set, .. }
                    | JobState::Running { scale_set, .. },
                ) => Some(*scale_set),
            };
            match held {
                Some(set) => state.world.finish(job, set, result, &knobs, now),
                None => {
                    if let Some(j) = state.world.jobs.get_mut(&job) {
                        j.state = JobState::Finished {
                            result: result.to_owned(),
                        };
                        j.finished_at = Some(now);
                    }
                }
            }
        }
        self.settle();
        Ok(())
    }

    /// Cancels a job (`result: "canceled"`).
    ///
    /// # Errors
    /// As [`FakeGithub::complete_job`].
    pub fn cancel_job(&self, job: u64) -> Result<(), ControlError> {
        self.complete_job(job, "canceled")
    }

    /// GitHub releases an assigned job no runner took (spec §4.3).
    ///
    /// # Errors
    /// When the job is not assigned and waiting.
    pub fn release_job(&self, job: u64) -> Result<(), ControlError> {
        let now = self.now();
        {
            let mut state = self.state();
            let knobs = state.knobs.clone();
            state.world.release(job, &knobs, now)?;
        }
        self.settle();
        Ok(())
    }

    /// Marks a runner online or offline (it reported in, or stopped reporting).
    ///
    /// # Errors
    /// When there is no such runner.
    pub fn set_runner_online(&self, runner: u64, online: bool) -> Result<(), ControlError> {
        self.state()
            .world
            .runners
            .get_mut(&runner)
            .ok_or("no such runner")?
            .online = online;
        Ok(())
    }

    /// A scale set as another client, or an earlier run, left it: named `name` in the runner
    /// group named `group`, its name its one label; returns its id. Pinned by
    /// `tests/wire/scaleset.rs::should_list_a_runner_added_by_the_control_api_when_its_set_was_added`.
    ///
    /// # Errors
    /// When the knobs know no such group, or a live scale set of that name is in it.
    pub fn add_scale_set(&self, name: &str, group: &str) -> Result<u64, ControlError> {
        let now = self.now();
        let mut state = self.state();
        let group = state
            .knobs
            .runner_groups
            .iter()
            .find(|g| g.name == group)
            .cloned()
            .ok_or("no such runner group")?;
        if state
            .world
            .scale_sets
            .values()
            .any(|s| !s.deleted && s.runner_group_id == group.id && s.name == name)
        {
            return Err("a scale set of that name exists in the group".to_owned());
        }
        let id = state.world.next_id();
        let first = state.knobs.first_message_id;
        state.world.scale_sets.insert(
            id,
            ScaleSet {
                id,
                name: name.to_owned(),
                runner_group_id: group.id,
                runner_group_name: group.name,
                labels: vec![Label {
                    kind: "System".to_owned(),
                    name: name.to_owned(),
                }],
                disable_update: false,
                created_on: now,
                deleted: false,
                max_capacity: 0,
                pending: Vec::new(),
                queue: std::collections::VecDeque::new(),
                next_message_id: first,
                queue_generation: 0,
            },
        );
        Ok(id)
    }

    /// A runner registered in `scale_set` by a JIT config no client of this test minted (an
    /// operator's, or one whose mint was lost before it was recorded); offline, idle; returns its
    /// id.
    ///
    /// # Errors
    /// When there is no such live scale set, or a runner of that name exists.
    pub fn add_runner(&self, scale_set: u64, name: &str) -> Result<u64, ControlError> {
        let mut state = self.state();
        if !state
            .world
            .scale_sets
            .get(&scale_set)
            .is_some_and(|s| !s.deleted)
        {
            return Err("no such scale set".to_owned());
        }
        if state.world.runners.values().any(|r| r.name == name) {
            return Err("a runner of that name exists".to_owned());
        }
        let id = state.world.next_id();
        state.world.runners.insert(
            id,
            Runner {
                id,
                name: name.to_owned(),
                scale_set,
                online: false,
                job: None,
            },
        );
        Ok(id)
    }

    /// The service expires the scale set's live session: its token answers 401, a refresh or
    /// delete the expired exception (spec §3.1).
    ///
    /// # Errors
    /// When the scale set has no live session.
    pub fn expire_session(&self, scale_set: u64) -> Result<(), ControlError> {
        let now = self.now();
        {
            let mut state = self.state();
            let knobs = state.knobs.clone();
            let id = state
                .world
                .live_session(scale_set, &knobs, now)
                .map(|s| s.id.clone())
                .ok_or("the scale set has no live session")?;
            if let Some(session) = state.world.sessions.get_mut(&id) {
                session.state = SessionState::Expired;
            }
            if let Some(set) = state.world.scale_sets.get_mut(&scale_set) {
                set.max_capacity = 0;
            }
        }
        self.settle();
        Ok(())
    }

    /// The live session's queue token stops working now; a refresh mints a new one (spec §3.3).
    ///
    /// # Errors
    /// When the scale set has no live session.
    pub fn expire_queue_token(&self, scale_set: u64) -> Result<(), ControlError> {
        let now = self.now();
        {
            let mut state = self.state();
            let knobs = state.knobs.clone();
            let id = state
                .world
                .live_session(scale_set, &knobs, now)
                .map(|s| s.id.clone())
                .ok_or("the scale set has no live session")?;
            if let Some(session) = state.world.sessions.get_mut(&id) {
                session.token_expires = now;
            }
        }
        self.settle();
        Ok(())
    }

    /// GitHub deletes the dormant scale set (spec §3.1).
    ///
    /// # Errors
    /// When there is no such scale set.
    pub fn delete_dormant(&self, scale_set: u64) -> Result<(), ControlError> {
        self.state().world.delete_dormant(scale_set)?;
        self.settle();
        Ok(())
    }

    /// The scale set's message numbering restarts: its next message gets `next` (spec §4.1, A1).
    ///
    /// # Errors
    /// When there is no such scale set.
    pub fn restart_numbering(&self, scale_set: u64, next: u64) -> Result<(), ControlError> {
        self.state()
            .world
            .scale_sets
            .get_mut(&scale_set)
            .ok_or("no such scale set")?
            .next_message_id = next;
        Ok(())
    }
}

#[derive(Deserialize)]
struct StartBody {
    #[serde(rename = "runnerId")]
    runner_id: u64,
}

#[derive(Deserialize)]
struct CompleteBody {
    result: String,
}

#[derive(Deserialize)]
struct OnlineBody {
    online: bool,
}

#[derive(Deserialize)]
struct RestartBody {
    next: u64,
}

#[derive(Deserialize)]
struct FaultBody {
    route: Route,
    fault: Fault,
}

fn done(result: Result<(), ControlError>) -> FakeResponse {
    match result {
        Ok(()) => empty(204),
        Err(why) => status(409, &why),
    }
}

fn parse<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, FakeResponse> {
    serde_json::from_slice(body).map_err(|e| status(400, &format!("bad control body: {e}")))
}

/// Answers a control call; `rest` is the path after `/_fake/`. Pinned by
/// `tests/wire/control.rs::should_script_a_job_through_its_life_when_driven_over_http`.
pub(crate) fn handle_http(
    fake: &FakeGithub,
    method: &str,
    rest: &str,
    body: &[u8],
) -> FakeResponse {
    let segments: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
    let id = |s: &str| s.parse::<u64>().map_err(|_| status(400, "bad id"));
    let answer = || -> Result<FakeResponse, FakeResponse> {
        Ok(match (method, segments.as_slice()) {
            ("GET", ["state"]) => json(200, &fake.state_json()),
            ("GET", ["knobs"]) => json(200, &json!(fake.knobs())),
            ("PUT", ["knobs"]) => {
                let knobs: Knobs = parse(body)?;
                fake.update_knobs(|k| *k = knobs);
                empty(204)
            }
            ("POST", ["faults"]) => {
                let f: FaultBody = parse(body)?;
                fake.fail_next(f.route, f.fault);
                empty(204)
            }
            ("POST", ["jobs"]) => {
                let spec: JobSpec = parse(body)?;
                json(201, &json!({"jobId": fake.queue_job(spec)}))
            }
            ("POST", ["jobs", job, "start"]) => {
                let b: StartBody = parse(body)?;
                done(fake.start_job(id(job)?, b.runner_id))
            }
            ("POST", ["jobs", job, "complete"]) => {
                let b: CompleteBody = parse(body)?;
                done(fake.complete_job(id(job)?, &b.result))
            }
            ("POST", ["jobs", job, "cancel"]) => done(fake.cancel_job(id(job)?)),
            ("POST", ["jobs", job, "release"]) => done(fake.release_job(id(job)?)),
            ("POST", ["runners", runner, "online"]) => {
                let b: OnlineBody = parse(body)?;
                done(fake.set_runner_online(id(runner)?, b.online))
            }
            ("POST", ["scale-sets", set, "expire-session"]) => done(fake.expire_session(id(set)?)),
            ("POST", ["scale-sets", set, "expire-queue-token"]) => {
                done(fake.expire_queue_token(id(set)?))
            }
            ("POST", ["scale-sets", set, "delete-dormant"]) => done(fake.delete_dormant(id(set)?)),
            ("POST", ["scale-sets", set, "restart-numbering"]) => {
                let b: RestartBody = parse(body)?;
                done(fake.restart_numbering(id(set)?, b.next))
            }
            _ => status(404, "no such control call"),
        })
    };
    answer().unwrap_or_else(|refused| refused)
}
