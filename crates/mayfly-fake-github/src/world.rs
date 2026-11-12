//! The service's world: scale sets, sessions, jobs, runners, workflow runs and the message
//! queues, with the assignment rules of spec §5 and §6.1. Plain data and functions over it; the
//! handlers in `service` and `rest` and the control API in `control` change it.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::knobs::{Flow, Knobs, StatisticsAt};
use crate::timefmt::rfc3339;

/// The zero time GitHub writes for an instant not reached yet (spec §2: it must decode).
pub const ZERO_TIME: &str = "0001-01-01T00:00:00Z";

/// What a job asks for and where it comes from, as the control API queues it (spec §4.3, §6.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobSpec {
    /// The repository's owner (`ownerName`).
    pub owner: String,
    /// The repository's name without its owner (`repositoryName`).
    pub repository: String,
    /// The workflow run's id; the run is registered for the REST lookup if new.
    pub run_id: u64,
    /// `eventName`, and the run's `event`.
    pub event: String,
    /// The run's head repository as `owner/name`; empty means the repository itself (no fork).
    pub head_repository: String,
    /// The run's actor's login.
    pub actor: String,
    /// `requestLabels`; a job matches a scale set that carries every one of them.
    pub labels: Vec<String>,
    /// `jobDisplayName`.
    pub display_name: String,
    /// `jobWorkflowRef`.
    pub workflow_ref: String,
    /// Fields left out of this job's `JobAssigned` and `JobAvailable`, to exercise A5
    /// (`[open: HW-02, items 1 and 8]`).
    pub omit: Vec<String>,
}

impl Default for JobSpec {
    fn default() -> Self {
        Self {
            owner: "acme".to_owned(),
            repository: "app".to_owned(),
            run_id: 1,
            event: "push".to_owned(),
            head_repository: String::new(),
            actor: "octo".to_owned(),
            labels: vec!["mayfly".to_owned()],
            display_name: "build".to_owned(),
            workflow_ref: "acme/app/.github/workflows/ci.yml@refs/heads/main".to_owned(),
            omit: Vec::new(),
        }
    }
}

/// Where a job is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum JobState {
    /// Waiting in GitHub's queue for a scale set.
    Queued,
    /// Offered to a scale set (GHES), not acquired.
    Offered {
        /// The scale set.
        scale_set: u64,
        /// The offer's `runnerRequestId`.
        request: u64,
    },
    /// Assigned to a scale set, waiting for a runner.
    Assigned {
        /// The scale set.
        scale_set: u64,
        /// `runnerRequestId`: 0 on github.com.
        request: u64,
    },
    /// Running on a runner of the scale set.
    Running {
        /// The scale set.
        scale_set: u64,
        /// `runnerRequestId`.
        request: u64,
        /// The runner's id.
        runner: u64,
    },
    /// Over, with its result.
    Finished {
        /// `succeeded`, `failed`, `canceled`, ...
        result: String,
    },
}

/// A workflow job.
#[derive(Debug, Clone, Serialize)]
pub struct Job {
    /// `jobId`, written as a string on the wire.
    pub id: u64,
    /// What was queued.
    pub spec: JobSpec,
    /// Where it is.
    #[serde(flatten)]
    pub state: JobState,
    /// How many times GitHub released it from a scale set no runner took it in.
    pub releases: u32,
    /// Unix-epoch seconds it was queued.
    pub queued_at: u64,
    /// When it was last assigned (or offered) to a scale set.
    pub assigned_at: Option<u64>,
    /// When a runner took it.
    pub started_at: Option<u64>,
    /// When it finished.
    pub finished_at: Option<u64>,
}

/// A label of a scale set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    /// `System` or `User`.
    #[serde(rename = "type", default)]
    pub kind: String,
    /// The label.
    pub name: String,
}

/// A scale set and its message queue.
#[derive(Debug, Clone, Serialize)]
pub struct ScaleSet {
    /// Its id.
    pub id: u64,
    /// Its name, unique within its runner group.
    pub name: String,
    /// Its runner group's id.
    pub runner_group_id: u64,
    /// Its runner group's name.
    pub runner_group_name: String,
    /// Its labels.
    pub labels: Vec<Label>,
    /// `RunnerSetting.disableUpdate`.
    pub disable_update: bool,
    /// Unix-epoch seconds it was created.
    pub created_on: u64,
    /// Deleted by GitHub (dormant, spec §3.1): every request about it answers "gone".
    pub deleted: bool,
    /// The `X-ScaleSetMaxCapacity` of the live session's last poll; 0 without one.
    pub max_capacity: u64,
    /// Job messages not yet put in a message, each with the scale set's statistics just after
    /// it was emitted.
    #[serde(skip)]
    pub pending: Vec<(Value, Value)>,
    /// Messages built and not acked, oldest first.
    pub queue: VecDeque<Message>,
    /// The id the next message gets.
    pub next_message_id: u64,
    /// The queue URL's generation, moved by a refresh when the knob says so.
    pub queue_generation: u64,
}

/// A message of the queue.
#[derive(Debug, Clone, Serialize)]
pub struct Message {
    /// `messageId`.
    pub id: u64,
    /// The job messages it carries, in order.
    #[serde(skip)]
    pub jobs: Vec<Value>,
    /// How many job messages it carries.
    pub job_count: usize,
    /// The scale set's statistics as [`Knobs::statistics_at`] takes them: at the first delivery,
    /// or just after its last job message was emitted; kept for every redelivery.
    #[serde(skip)]
    pub statistics: Value,
    /// Whether a poll has handed it out.
    pub delivered: bool,
}

/// How a session ended, if it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Live.
    Live,
    /// Expired by the service: refresh and delete answer the expired exception.
    Expired,
    /// Deleted by its client.
    Deleted,
}

/// A message session.
#[derive(Debug, Clone, Serialize)]
pub struct Session {
    /// `sessionId`, a UUID.
    pub id: String,
    /// The scale set.
    pub scale_set: u64,
    /// `ownerName`.
    pub owner: String,
    /// The message-queue token; never in the state dump.
    #[serde(skip)]
    pub token: String,
    /// When the token stops working, Unix-epoch seconds.
    pub token_expires: u64,
    /// When the session opened.
    pub opened_at: u64,
    /// Live or how it ended.
    pub state: SessionState,
}

/// A runner registered by a JIT config.
#[derive(Debug, Clone, Serialize)]
pub struct Runner {
    /// Its id.
    pub id: u64,
    /// Its name, unique.
    pub name: String,
    /// Its scale set.
    pub scale_set: u64,
    /// Whether it reported in (scripted).
    pub online: bool,
    /// The job it runs.
    pub job: Option<u64>,
}

/// A workflow run, for the REST lookup.
#[derive(Debug, Clone, Serialize)]
pub struct Run {
    /// Its id.
    pub id: u64,
    /// The base repository's owner.
    pub owner: String,
    /// The base repository's name.
    pub repository: String,
    /// `event`.
    pub event: String,
    /// `head_repository.full_name`.
    pub head_repository: String,
    /// `actor.login`.
    pub actor: String,
    /// Cancelled through the REST API.
    pub cancelled: bool,
}

/// The whole world.
#[derive(Debug, Clone, Default, Serialize)]
pub struct World {
    /// Scale sets by id.
    pub scale_sets: BTreeMap<u64, ScaleSet>,
    /// Sessions by id.
    pub sessions: BTreeMap<String, Session>,
    /// Jobs by id, in queue order.
    pub jobs: BTreeMap<u64, Job>,
    /// Runners by id.
    pub runners: BTreeMap<u64, Runner>,
    /// Workflow runs by id.
    pub runs: BTreeMap<u64, Run>,
    /// The last id handed out, shared by every kind of id.
    pub serial: u64,
}

/// The seven statistics of spec §2, all present.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Statistics {
    /// Offers not acquired.
    pub total_available_jobs: u64,
    /// Jobs acquired and not finished.
    pub total_acquired_jobs: u64,
    /// Jobs waiting for a runner and jobs running.
    pub total_assigned_jobs: u64,
    /// Jobs running.
    pub total_running_jobs: u64,
    /// Runners registered.
    pub total_registered_runners: u64,
    /// Runners with a job.
    pub total_busy_runners: u64,
    /// Runners without a job.
    pub total_idle_runners: u64,
}

impl World {
    /// The next id.
    pub fn next_id(&mut self) -> u64 {
        self.serial += 1;
        self.serial
    }

    /// The live session of `scale_set`, if any, at `now`.
    pub fn live_session(&self, scale_set: u64, knobs: &Knobs, now: u64) -> Option<&Session> {
        self.sessions
            .values()
            .find(|s| s.scale_set == scale_set && session_live(s, knobs, now))
    }

    /// The statistics of `scale_set`.
    #[must_use]
    pub fn statistics(&self, scale_set: u64) -> Statistics {
        let mut stats = Statistics::default();
        for job in self.jobs.values() {
            match &job.state {
                JobState::Offered { scale_set: s, .. } if *s == scale_set => {
                    stats.total_available_jobs += 1;
                }
                JobState::Assigned {
                    scale_set: s,
                    request,
                } if *s == scale_set => {
                    stats.total_assigned_jobs += 1;
                    if *request != 0 {
                        stats.total_acquired_jobs += 1;
                    }
                }
                JobState::Running {
                    scale_set: s,
                    request,
                    ..
                } if *s == scale_set => {
                    stats.total_assigned_jobs += 1;
                    stats.total_running_jobs += 1;
                    if *request != 0 {
                        stats.total_acquired_jobs += 1;
                    }
                }
                _ => {}
            }
        }
        for runner in self.runners.values().filter(|r| r.scale_set == scale_set) {
            stats.total_registered_runners += 1;
            if runner.job.is_some() {
                stats.total_busy_runners += 1;
            } else {
                stats.total_idle_runners += 1;
            }
        }
        stats
    }

    /// Hands queued jobs to scale sets with a live session and room under their advertised
    /// capacity (spec §5: the header caps assignment, 0 assigns nothing; §6.1: direct
    /// assignment on github.com, offers on GHES). Jobs go in queue order, each to the matching
    /// scale set with the lowest id. Pinned by: `tests/wire/scaleset.rs::should_cap_assignment_at_the_advertised_capacity_when_polled`
    /// and the contract's WC-08 and WC-15.
    pub fn assign(&mut self, knobs: &Knobs, now: u64) {
        let queued: Vec<u64> = self
            .jobs
            .values()
            .filter(|j| j.state == JobState::Queued)
            .map(|j| j.id)
            .collect();
        for job_id in queued {
            let Some(set) = self.target_for(job_id, knobs, now) else {
                continue;
            };
            let request = match knobs.flow {
                Flow::Direct => 0,
                Flow::Acquire => self.next_id(),
            };
            let Some(job) = self.jobs.get_mut(&job_id) else {
                continue;
            };
            job.assigned_at = Some(now);
            let kind = match knobs.flow {
                Flow::Direct => {
                    job.state = JobState::Assigned {
                        scale_set: set,
                        request,
                    };
                    "JobAssigned"
                }
                Flow::Acquire => {
                    job.state = JobState::Offered {
                        scale_set: set,
                        request,
                    };
                    "JobAvailable"
                }
            };
            self.emit(set, kind, job_id);
        }
    }

    fn target_for(&self, job_id: u64, knobs: &Knobs, now: u64) -> Option<u64> {
        let job = self.jobs.get(&job_id)?;
        self.scale_sets
            .values()
            .filter(|s| !s.deleted)
            .filter(|s| self.live_session(s.id, knobs, now).is_some())
            .filter(|s| {
                job.spec
                    .labels
                    .iter()
                    .all(|l| s.labels.iter().any(|sl| sl.name.eq_ignore_ascii_case(l)))
            })
            .find(|s| self.held(s.id) < s.max_capacity)
            .map(|s| s.id)
    }

    /// Jobs a scale set holds against its capacity: offered, assigned or running.
    fn held(&self, scale_set: u64) -> u64 {
        let held = self
            .jobs
            .values()
            .filter(|j| match &j.state {
                JobState::Offered { scale_set: s, .. }
                | JobState::Assigned { scale_set: s, .. }
                | JobState::Running { scale_set: s, .. } => *s == scale_set,
                _ => false,
            })
            .count();
        u64::try_from(held).unwrap_or(u64::MAX)
    }

    /// Appends a job message about `job_id` of `kind` to `scale_set`'s pending list.
    pub fn emit(&mut self, scale_set: u64, kind: &str, job_id: u64) {
        let Some(job) = self.jobs.get(&job_id) else {
            return;
        };
        let message = job_message(kind, job, &self.runners);
        self.pend(scale_set, message);
    }

    /// Appends `message` to `scale_set`'s pending list with the statistics as they are now.
    fn pend(&mut self, scale_set: u64, message: Value) {
        let statistics = serde_json::to_value(self.statistics(scale_set)).unwrap_or(Value::Null);
        if let Some(set) = self.scale_sets.get_mut(&scale_set) {
            set.pending.push((message, statistics));
        }
    }

    /// Takes the next message for a poll of `scale_set` that passed `last_message_id`: the
    /// oldest one not acked (redelivery until delete, spec §4.4), skipping those already handed
    /// out at or below `last_message_id`; else a new one from up to
    /// [`Knobs::max_jobs_per_message`] pending job messages (spec §5). Pinned by: the contract's
    /// WC-06 and WC-07, `tests/wire/scaleset.rs::should_skip_a_delivered_message_when_last_message_id_passes_it`.
    pub fn take_message(
        &mut self,
        scale_set: u64,
        last_message_id: Option<u64>,
        knobs: &Knobs,
    ) -> Option<Message> {
        let statistics = serde_json::to_value(self.statistics(scale_set)).ok()?;
        let set = self.scale_sets.get_mut(&scale_set)?;
        if let (true, Some(last)) = (knobs.last_message_id_acks, last_message_id) {
            set.queue.retain(|m| m.id > last);
        }
        let skip = |m: &Message| m.delivered && last_message_id.is_some_and(|l| m.id <= l);
        if let Some(message) = set.queue.iter_mut().find(|m| !skip(m)) {
            message.delivered = true;
            return Some(message.clone());
        }
        if set.pending.is_empty() {
            return None;
        }
        let take = set.pending.len().min(knobs.max_jobs_per_message.max(1));
        let (jobs, at_event): (Vec<Value>, Vec<Value>) = set.pending.drain(..take).unzip();
        let statistics = match knobs.statistics_at {
            StatisticsAt::Build => statistics,
            StatisticsAt::Event => at_event.last().cloned().unwrap_or(statistics),
        };
        let message = Message {
            id: set.next_message_id,
            job_count: jobs.len(),
            jobs,
            statistics,
            delivered: true,
        };
        set.next_message_id += 1;
        set.queue.push_back(message.clone());
        Some(message)
    }

    /// Releases an assigned job no runner took (spec §4.3): `JobCompleted` `canceled`, then
    /// requeued, until the [`Knobs::max_releases`]th release fails it. Pinned by:
    /// `tests/wire/scaleset.rs::should_requeue_until_the_last_release_fails_it_when_an_assigned_job_is_released`.
    ///
    /// # Errors
    /// When the job is not assigned and waiting.
    pub fn release(&mut self, job_id: u64, knobs: &Knobs, now: u64) -> Result<(), String> {
        let job = self.jobs.get(&job_id).ok_or("no such job")?;
        let JobState::Assigned { scale_set, .. } = job.state else {
            return Err("the job is not assigned and waiting".to_owned());
        };
        self.finish(job_id, scale_set, "canceled", knobs, now);
        let job = self.jobs.get_mut(&job_id).ok_or("no such job")?;
        job.releases += 1;
        if job.releases < knobs.max_releases {
            job.state = JobState::Queued;
            job.finished_at = None;
        } else {
            job.state = JobState::Finished {
                result: "failed".to_owned(),
            };
        }
        Ok(())
    }

    /// Ends `job_id` with `result`: a `JobCompleted` when a scale set holds it, its runner
    /// deregistered (an ephemeral runner that ran a job, spec §6.3).
    pub fn finish(&mut self, job_id: u64, scale_set: u64, result: &str, knobs: &Knobs, now: u64) {
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return;
        };
        let runner = match job.state {
            JobState::Running { runner, .. } => Some(runner),
            _ => None,
        };
        job.finished_at = Some(now);
        job.state = JobState::Finished {
            result: result.to_owned(),
        };
        let mut message = job_message("JobCompleted", job, &self.runners);
        if let Some(fields) = message.as_object_mut() {
            fields.insert("result".to_owned(), json!(result));
            if let Some(runner) = runner.and_then(|r| self.runners.get(&r)) {
                fields.insert("runnerId".to_owned(), json!(runner.id));
                fields.insert("runnerName".to_owned(), json!(runner.name));
            }
            if knobs.completed_names_no_job {
                fields.remove("jobId");
                fields.remove("workflowRunId");
            }
        }
        if let Some(runner) = runner {
            self.runners.remove(&runner);
        }
        self.pend(scale_set, message);
    }

    /// GitHub deletes a scale set no session connected to for a week (spec §3.1): its sessions
    /// end, and the jobs assigned or offered to it and not to a runner go back to the queue.
    /// Pinned by: `tests/wire/scaleset.rs::should_answer_gone_and_requeue_its_jobs_when_a_dormant_scale_set_is_deleted`.
    pub fn delete_dormant(&mut self, scale_set: u64) -> Result<(), String> {
        let set = self
            .scale_sets
            .get_mut(&scale_set)
            .filter(|s| !s.deleted)
            .ok_or("no such scale set")?;
        set.deleted = true;
        set.max_capacity = 0;
        set.pending.clear();
        set.queue.clear();
        for session in self.sessions.values_mut() {
            if session.scale_set == scale_set && session.state == SessionState::Live {
                session.state = SessionState::Deleted;
            }
        }
        for job in self.jobs.values_mut() {
            if let JobState::Offered { scale_set: s, .. } | JobState::Assigned { scale_set: s, .. } =
                job.state
                && s == scale_set
            {
                job.state = JobState::Queued;
            }
        }
        Ok(())
    }
}

/// Whether `session` still works at `now`.
pub fn session_live(session: &Session, knobs: &Knobs, now: u64) -> bool {
    session.state == SessionState::Live
        && knobs
            .session_lifetime_secs
            .is_none_or(|life| now < session.opened_at + life)
}

/// One job message of spec §4.3, in arrival shape.
pub fn job_message(kind: &str, job: &Job, runners: &BTreeMap<u64, Runner>) -> Value {
    let request = match job.state {
        JobState::Offered { request, .. }
        | JobState::Assigned { request, .. }
        | JobState::Running { request, .. } => request,
        _ => 0,
    };
    let at = |t: Option<u64>| t.map_or_else(|| ZERO_TIME.to_owned(), rfc3339);
    let mut fields = json!({
        "messageType": kind,
        "runnerRequestId": request,
        "repositoryName": job.spec.repository,
        "ownerName": job.spec.owner,
        // A string: the spec does not state the type; the reference client declares one
        // (client-assumed, `[open: HW-02, item 1]`).
        "jobId": job.id.to_string(),
        "jobWorkflowRef": job.spec.workflow_ref,
        "jobDisplayName": job.spec.display_name,
        "workflowRunId": job.spec.run_id,
        "eventName": job.spec.event,
        "requestLabels": job.spec.labels,
        "queueTime": rfc3339(job.queued_at),
        "scaleSetAssignTime": at(job.assigned_at),
        "runnerAssignTime": at(job.started_at),
        "finishTime": at(job.finished_at),
    });
    let Some(object) = fields.as_object_mut() else {
        return fields;
    };
    if kind == "JobAvailable" {
        // Relative, and unused by mayfly, which acquires through the scale set's route
        // (client-assumed shape).
        object.insert(
            "acquireJobUrl".to_owned(),
            json!(format!("acquirejobs/{request}")),
        );
    }
    if let JobState::Running { runner, .. } = job.state
        && let Some(runner) = runners.get(&runner)
    {
        object.insert("runnerId".to_owned(), json!(runner.id));
        object.insert("runnerName".to_owned(), json!(runner.name));
    }
    if matches!(kind, "JobAssigned" | "JobAvailable") {
        for name in &job.spec.omit {
            object.remove(name);
        }
    }
    fields
}
