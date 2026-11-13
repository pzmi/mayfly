//! `FakeScaleSetApi` and `FakeRunnerRegistry` (ver §7.1): the `ScaleSetApi` and
//! `RunnerRegistry` ports in memory, for the listener's and the registration manager's component
//! tests (GH-04, CP-05). One [`FakeScaleSetApi`] is one GitHub: its scale sets, sessions, job
//! queue and runners; [`FakeScaleSetApi::registry`] hands out the runner endpoints of the same
//! world.
//!
//! It behaves as `doc/github-scaleset.md` reads, the same reading `mayfly-fake-github` serves on
//! the wire: one live session per scale set (a second is a conflict), the poll assigning queued
//! jobs up to the advertised capacity (github.com's direct flow, `runnerRequestId` 0) or
//! offering them (GHES), redelivery of a message until it is acked, `lastMessageId` skipping
//! delivered messages at or below it, an acquire listing only the offered ids, 204 for an absent
//! runner and a busy runner refused. It passes `mayfly_testkit::contracts::scaleset`; each
//! [`Broken`] variant fails the cases named for it (ver §5).
//!
//! It answers at once: no hold, no network. A [`WorldFault`] queued with
//! [`FakeScaleSetApi::inject`] bends the next matching call; a `Retry-After` is waited on the
//! [`FakeClock`] it was built with, so the wait is observable and nothing sleeps.
//!
//! For the registration manager's tests (CP-05) a world can also lose a JIT answer after its
//! effect ([`FakeScaleSetApi::lose_next_jit_answer`]), whose runner the listings then show only
//! once they catch up ([`FakeScaleSetApi::catch_up_listings`]), as GitHub's may lag behind a
//! mint, and it counts the runner listings it answered ([`FakeScaleSetApi::runner_listings`]).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use mayfly_control::github::{
    AssignedJob, EventName, Failure, GitHubCallError, JitConfig, JobFact, JobKey, MessageId,
    MessageItem, Offer, OpenedSession, PolledMessage, RemovalOutcome, RepositoryName, RunnerId,
    RunnerInfo, RunnerRegistry, RunnerRequestId, ScaleSetApi, ScaleSetId, ScaleSetInfo,
    ScaleSetSpec, SessionId, Statistics, WorkflowJobId, WorkflowRunId,
};
use mayfly_control::store::Clock;
use mayfly_domain::{Millis, Secret};

use crate::clock::FakeClock;
use crate::contracts::scaleset::{Flow, WorldFault};

/// The most job messages one message holds (spec §5).
pub const MAX_JOBS_PER_MESSAGE: usize = 50;

/// The first message id of a scale set (spec §4.1: about `100000001` in 2026).
pub const FIRST_MESSAGE_ID: u64 = 100_000_001;

/// The runner group every fake world knows.
pub const DEFAULT_GROUP: &str = "Default";

/// Deliberately broken fakes, each failing named cases of the contract (ver §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Broken {
    /// `NoCursorApi`: omits `lastMessageId`, so a delivered message not yet acked comes back
    /// instead of the next one. Fails SS-04.
    NoCursorApi,
    /// `RetryAfterIgnoringApi`: waits its own one-second backoff instead of a `Retry-After`.
    /// Fails SS-06.
    RetryAfterIgnoringApi,
    /// `NameGuessingRegistry`: removes every runner whose name starts with the removed one's
    /// name. Fails RR-02 and RR-05 (each removes a runner whose name extends another's).
    NameGuessingRegistry,
    /// `StaleListingRegistry`: keeps listing a runner after its removal, as a cached listing
    /// would. Fails RR-05.
    StaleListingRegistry,
}

#[derive(Debug, Clone)]
enum JobState {
    Queued,
    Offered { set: u64, request: u64 },
    Assigned { set: u64, request: u64 },
    Running { set: u64 },
}

#[derive(Debug, Clone)]
struct Job {
    run_id: u64,
    label: String,
    state: JobState,
}

#[derive(Debug, Clone)]
struct Message {
    id: u64,
    items: Vec<MessageItem>,
    statistics: Statistics,
    delivered: bool,
}

#[derive(Debug, Clone)]
struct Set {
    info: ScaleSetInfo,
    deleted: bool,
    max_capacity: u32,
    pending: Vec<MessageItem>,
    queue: VecDeque<Message>,
    next_message: u64,
    live: Option<SessionId>,
    /// The live session's owner name (named in a conflict's message, as GitHub does).
    owner: String,
    /// Sessions open with a queue URL the client must refuse (`WorldFault::QueueUrlUnusable`).
    unusable_queue_url: bool,
}

#[derive(Debug, Clone)]
struct Runner {
    name: String,
    set: u64,
    job: Option<u64>,
    /// Whether it reports in (`status: online`), apart from whether it is busy: a runner that
    /// registered and has not taken a job yet can be online and idle.
    online: bool,
    /// Whether the listings (by name and all) show it yet: a runner registered by a JIT answer
    /// that was lost is listed only once they catch up.
    listed: bool,
}

#[derive(Debug)]
struct World {
    flow: Flow,
    broken: Option<Broken>,
    serial: u64,
    sets: BTreeMap<u64, Set>,
    held: BTreeMap<u64, SessionId>,
    jobs: BTreeMap<u64, Job>,
    runners: BTreeMap<u64, Runner>,
    /// Runners removed while `StaleListingRegistry` keeps listing them.
    stale: BTreeMap<u64, Runner>,
    faults: Vec<WorldFault>,
    /// The next JIT registers its runner and loses its answer.
    lose_jit_answer: bool,
    /// Runner listings answered (by name and all).
    listings: u32,
    /// Runner removals answered.
    removals: u32,
}

impl World {
    fn next(&mut self) -> u64 {
        self.serial += 1;
        self.serial
    }

    fn take_fault(&mut self, wanted: impl Fn(&WorldFault) -> bool) -> Option<WorldFault> {
        let at = self.faults.iter().position(wanted)?;
        Some(self.faults.remove(at))
    }

    fn live_set(&self, id: ScaleSetId) -> Result<&Set, GitHubCallError> {
        self.sets
            .get(&id.0)
            .filter(|s| !s.deleted)
            .ok_or_else(|| fail(Failure::ScaleSetGone, "the scale set was not found"))
    }

    fn statistics(&self, set: u64) -> Statistics {
        let count = |f: &dyn Fn(&JobState) -> bool| {
            u32::try_from(self.jobs.values().filter(|j| f(&j.state)).count()).unwrap_or(u32::MAX)
        };
        let registered = self.runners.values().filter(|r| r.set == set).count();
        let busy = self
            .runners
            .values()
            .filter(|r| r.set == set && r.job.is_some())
            .count();
        Statistics {
            total_available_jobs: count(
                &|s| matches!(s, JobState::Offered { set: x, .. } if *x == set),
            ),
            total_acquired_jobs: 0,
            total_assigned_jobs: count(
                &|s| matches!(s, JobState::Assigned { set: x, .. } | JobState::Running { set: x, .. } if *x == set),
            ),
            total_running_jobs: count(
                &|s| matches!(s, JobState::Running { set: x, .. } if *x == set),
            ),
            total_registered_runners: u32::try_from(registered).unwrap_or(u32::MAX),
            total_busy_runners: u32::try_from(busy).unwrap_or(u32::MAX),
            total_idle_runners: u32::try_from(registered - busy).unwrap_or(u32::MAX),
        }
    }

    fn held_jobs(&self, set: u64) -> usize {
        self.jobs
            .values()
            .filter(|j| match j.state {
                JobState::Offered { set: x, .. }
                | JobState::Assigned { set: x, .. }
                | JobState::Running { set: x, .. } => x == set,
                JobState::Queued => false,
            })
            .count()
    }

    /// Assigns (or offers) queued jobs to `set` while it holds fewer than its capacity.
    fn assign(&mut self, set: u64) {
        let queued: Vec<u64> = self
            .jobs
            .iter()
            .filter(|(_, j)| matches!(j.state, JobState::Queued))
            .map(|(id, _)| *id)
            .collect();
        for job_id in queued {
            let Some(s) = self.sets.get(&set) else { return };
            let label = self.jobs[&job_id].label.clone();
            let fits = self.held_jobs(set) < s.max_capacity as usize;
            if s.live.is_none()
                || !fits
                || !s.info.labels.iter().any(|l| l.eq_ignore_ascii_case(&label))
            {
                continue;
            }
            let item = match self.flow {
                Flow::Direct => {
                    let job = &self.jobs[&job_id];
                    let item = MessageItem::Fact(JobFact::Assigned {
                        request_id: RunnerRequestId(0),
                        job: Some(assigned(job_id, job)),
                    });
                    if let Some(j) = self.jobs.get_mut(&job_id) {
                        j.state = JobState::Assigned { set, request: 0 };
                    }
                    item
                }
                Flow::Acquire => {
                    let request = self.next();
                    if let Some(j) = self.jobs.get_mut(&job_id) {
                        j.state = JobState::Offered { set, request };
                    }
                    MessageItem::Offer(Offer {
                        request_id: RunnerRequestId(request),
                        event_name: EventName::Push,
                    })
                }
            };
            if let Some(s) = self.sets.get_mut(&set) {
                s.pending.push(item);
            }
        }
    }

    /// The oldest message not acked that the cursor does not pass, else a new one.
    fn take_message(&mut self, set: u64, last: Option<u64>) -> Option<PolledMessage> {
        let statistics = self.statistics(set);
        let s = self.sets.get_mut(&set)?;
        let skip = |m: &Message| m.delivered && last.is_some_and(|l| m.id <= l);
        if let Some(m) = s.queue.iter_mut().find(|m| !skip(m)) {
            m.delivered = true;
            return Some(polled(m));
        }
        if s.pending.is_empty() {
            return None;
        }
        let take = s.pending.len().min(MAX_JOBS_PER_MESSAGE);
        let items: Vec<MessageItem> = s.pending.drain(..take).collect();
        let message = Message {
            id: s.next_message,
            items,
            statistics,
            delivered: true,
        };
        s.next_message += 1;
        let answer = polled(&message);
        s.queue.push_back(message);
        Some(answer)
    }
}

fn polled(m: &Message) -> PolledMessage {
    PolledMessage {
        message_id: MessageId(m.id),
        statistics: Some(m.statistics),
        items: m.items.clone(),
    }
}

fn assigned(job_id: u64, job: &Job) -> AssignedJob {
    AssignedJob {
        key: key(job_id, job),
        repository: Some(RepositoryName("acme/app".to_owned())),
        event_name: EventName::Push,
        labels: vec![job.label.clone()],
    }
}

fn key(job_id: u64, job: &Job) -> JobKey {
    JobKey {
        job_id: WorkflowJobId(job_id.to_string()),
        run_id: WorkflowRunId(job.run_id),
    }
}

fn fail(failure: Failure, detail: &str) -> GitHubCallError {
    GitHubCallError::new(failure, detail)
}

fn runner_info(id: u64, runner: &Runner) -> RunnerInfo {
    RunnerInfo {
        id: RunnerId(id),
        name: runner.name.clone(),
        scale_set: Some(ScaleSetId(runner.set)),
        online: Some(runner.online),
        busy: Some(runner.job.is_some()),
    }
}

/// The in-memory GitHub (ver §7.1). Clones share one world.
#[derive(Debug, Clone)]
pub struct FakeScaleSetApi {
    world: Arc<Mutex<World>>,
    clock: FakeClock,
}

/// The runner endpoints of a [`FakeScaleSetApi`]'s world.
#[derive(Debug, Clone)]
pub struct FakeRunnerRegistry {
    api: FakeScaleSetApi,
}

impl FakeScaleSetApi {
    /// A world serving `flow`, waiting on `clock`.
    #[must_use]
    pub fn new(flow: Flow, clock: FakeClock) -> Self {
        Self::build(flow, clock, None)
    }

    /// A deliberately broken world (ver §5).
    #[must_use]
    pub fn broken(flow: Flow, clock: FakeClock, broken: Broken) -> Self {
        Self::build(flow, clock, Some(broken))
    }

    fn build(flow: Flow, clock: FakeClock, broken: Option<Broken>) -> Self {
        Self {
            world: Arc::new(Mutex::new(World {
                flow,
                broken,
                serial: 0,
                sets: BTreeMap::new(),
                held: BTreeMap::new(),
                jobs: BTreeMap::new(),
                runners: BTreeMap::new(),
                stale: BTreeMap::new(),
                faults: Vec::new(),
                lose_jit_answer: false,
                listings: 0,
                removals: 0,
            })),
            clock,
        }
    }

    fn world(&self) -> MutexGuard<'_, World> {
        self.world.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The runner endpoints of this world.
    #[must_use]
    pub fn registry(&self) -> FakeRunnerRegistry {
        FakeRunnerRegistry { api: self.clone() }
    }

    /// The clock the fake waits on.
    #[must_use]
    pub const fn clock(&self) -> &FakeClock {
        &self.clock
    }

    /// Queues `count` jobs asking for `label`, of workflow run `run_id` (a push to `acme/app`);
    /// they are assigned or offered on the next poll that has room. Returns their ids.
    pub fn queue_jobs(&self, count: usize, label: &str, run_id: u64) -> Vec<WorkflowJobId> {
        let mut world = self.world();
        (0..count)
            .map(|_| {
                let id = world.next();
                world.jobs.insert(
                    id,
                    Job {
                        run_id,
                        label: label.to_owned(),
                        state: JobState::Queued,
                    },
                );
                WorkflowJobId(id.to_string())
            })
            .collect()
    }

    /// The runner `runner` takes the assigned job `job`: `JobStarted` follows.
    ///
    /// # Errors
    /// When the job is not assigned and waiting, or the runner is not idle in its scale set.
    pub fn start_job(&self, job: &WorkflowJobId, runner: RunnerId) -> Result<(), String> {
        let mut world = self.world();
        let id: u64 = job.0.parse().map_err(|_| "not a fake job id".to_owned())?;
        let Some(JobState::Assigned { set, request }) =
            world.jobs.get(&id).map(|j| j.state.clone())
        else {
            return Err("the job is not assigned and waiting".to_owned());
        };
        let fits = world
            .runners
            .get(&runner.0)
            .is_some_and(|r| r.set == set && r.job.is_none());
        if !fits {
            return Err("no idle runner of the job's scale set has that id".to_owned());
        }
        if let Some(r) = world.runners.get_mut(&runner.0) {
            r.job = Some(id);
            r.online = true;
        }
        let started_key = world.jobs.get(&id).map(|j| key(id, j));
        if let Some(j) = world.jobs.get_mut(&id) {
            j.state = JobState::Running { set };
        }
        if let Some(s) = world.sets.get_mut(&set) {
            s.pending.push(MessageItem::Fact(JobFact::Started {
                request_id: RunnerRequestId(request),
                runner_id: Some(runner),
                runner_name: None,
                job: started_key,
            }));
        }
        Ok(())
    }

    /// The runner `runner` starts (or stops) reporting in, as its process would.
    ///
    /// # Errors
    /// When there is no such runner.
    pub fn set_runner_online(&self, runner: RunnerId, online: bool) -> Result<(), String> {
        let mut world = self.world();
        let r = world
            .runners
            .get_mut(&runner.0)
            .ok_or_else(|| "no such runner".to_owned())?;
        r.online = online;
        Ok(())
    }

    /// The next JIT generation registers its runner and loses its answer: the caller gets
    /// [`Failure::Unavailable`], and the listings show the runner only after
    /// [`FakeScaleSetApi::catch_up_listings`] (F06, F07).
    pub fn lose_next_jit_answer(&self) {
        self.world().lose_jit_answer = true;
    }

    /// The listings catch up: every runner is listed.
    pub fn catch_up_listings(&self) {
        for runner in self.world().runners.values_mut() {
            runner.listed = true;
        }
    }

    /// How many runner listings (by name and all) the world answered.
    #[must_use]
    pub fn runner_listings(&self) -> u32 {
        self.world().listings
    }

    /// How many runner removals the world answered.
    #[must_use]
    pub fn runner_removals(&self) -> u32 {
        self.world().removals
    }

    /// The runner `runner`'s job ends and it goes idle (its process may still be registered).
    ///
    /// # Errors
    /// When there is no such runner.
    pub fn finish_job(&self, runner: RunnerId) -> Result<(), String> {
        let mut world = self.world();
        let r = world
            .runners
            .get_mut(&runner.0)
            .ok_or_else(|| "no such runner".to_owned())?;
        r.job = None;
        Ok(())
    }

    /// The runner `runner` as GitHub holds it, listed or not.
    #[must_use]
    pub fn runner(&self, runner: RunnerId) -> Option<RunnerInfo> {
        self.world()
            .runners
            .get(&runner.0)
            .map(|r| runner_info(runner.0, r))
    }

    /// Registers a runner named `name` in `scale_set` that no JIT of this caller asked for (a
    /// runner of another installation, or one a person added), listed and online.
    pub fn add_runner(&self, scale_set: ScaleSetId, name: &str) -> RunnerId {
        let mut world = self.world();
        let id = world.next();
        world.runners.insert(
            id,
            Runner {
                name: name.to_owned(),
                set: scale_set.0,
                job: None,
                online: true,
                listed: true,
            },
        );
        RunnerId(id)
    }

    /// Queues `fault` for the next call it bends.
    pub fn inject(&self, fault: WorldFault) {
        let mut world = self.world();
        if let WorldFault::QueueUrlUnusable { scale_set }
        | WorldFault::QueueUrlRestored { scale_set } = fault
        {
            let unusable = matches!(fault, WorldFault::QueueUrlUnusable { .. });
            if let Some(s) = world.sets.get_mut(&scale_set.0) {
                s.unusable_queue_url = unusable;
            }
            return;
        }
        if let WorldFault::ScaleSetDeleted { scale_set } = fault {
            if let Some(s) = world.sets.get_mut(&scale_set.0) {
                s.deleted = true;
                s.live = None;
            }
            return;
        }
        if let WorldFault::SessionExpired { scale_set } = fault
            && let Some(s) = world.sets.get_mut(&scale_set.0)
        {
            s.live = None;
        }
        world.faults.push(fault);
    }

    /// The group's id, or a refusal.
    fn group(name: &str) -> Result<u64, GitHubCallError> {
        if name == DEFAULT_GROUP {
            Ok(1)
        } else {
            Err(fail(
                Failure::Refused { status: 404 },
                "the runner group was not found",
            ))
        }
    }
}

impl ScaleSetApi for FakeScaleSetApi {
    async fn find_scale_set(
        &self,
        spec: &ScaleSetSpec,
    ) -> Result<Option<ScaleSetInfo>, GitHubCallError> {
        let group = Self::group(&spec.runner_group)?;
        Ok(self
            .world()
            .sets
            .values()
            .find(|s| !s.deleted && s.info.runner_group_id == group && s.info.name == spec.name)
            .map(|s| s.info.clone()))
    }

    async fn get_scale_set(&self, id: ScaleSetId) -> Result<ScaleSetInfo, GitHubCallError> {
        self.world().live_set(id).map(|s| s.info.clone())
    }

    async fn create_scale_set(&self, spec: &ScaleSetSpec) -> Result<ScaleSetInfo, GitHubCallError> {
        let group = Self::group(&spec.runner_group)?;
        let mut world = self.world();
        let taken = world
            .sets
            .values()
            .any(|s| !s.deleted && s.info.runner_group_id == group && s.info.name == spec.name);
        if taken {
            return Err(fail(Failure::Refused { status: 409 }, "the name is taken"));
        }
        let id = world.next();
        let labels = if spec.labels.is_empty() {
            vec![spec.name.clone()]
        } else {
            spec.labels.clone()
        };
        let info = ScaleSetInfo {
            id: ScaleSetId(id),
            name: spec.name.clone(),
            runner_group_id: group,
            labels,
        };
        world.sets.insert(
            id,
            Set {
                info: info.clone(),
                deleted: false,
                max_capacity: 0,
                pending: Vec::new(),
                queue: VecDeque::new(),
                next_message: FIRST_MESSAGE_ID,
                live: None,
                owner: String::new(),
                unusable_queue_url: false,
            },
        );
        Ok(info)
    }

    async fn create_session(
        &self,
        scale_set: ScaleSetId,
        owner: &str,
    ) -> Result<OpenedSession, GitHubCallError> {
        let mut world = self.world();
        let set = world.live_set(scale_set)?;
        if set.live.is_some() {
            return Err(GitHubCallError::new(
                Failure::SessionConflict,
                format!(
                    "the scale set already has an active session for owner {}",
                    set.owner
                ),
            ));
        }
        if set.unusable_queue_url {
            // The client refuses the answer and deletes the session it names: none stays live.
            return Err(fail(
                Failure::ProtocolDrift,
                "the messageQueueUrl is not https; the unusable session was deleted",
            ));
        }
        let serial = world.next();
        let session_id = SessionId::from_u128(0x4000_8000_0000_0000_0000 | u128::from(serial));
        let statistics = world.statistics(scale_set.0);
        if let Some(s) = world.sets.get_mut(&scale_set.0) {
            s.live = Some(session_id);
            s.owner = owner.to_owned();
            s.max_capacity = 0;
        }
        world.held.insert(scale_set.0, session_id);
        Ok(OpenedSession {
            session_id,
            statistics,
        })
    }

    async fn delete_session(
        &self,
        scale_set: ScaleSetId,
        session: SessionId,
    ) -> Result<(), GitHubCallError> {
        let mut world = self.world();
        // A 404 (`WorldFault::SessionDeleteNotFound`, or a scale set GitHub deleted) is a session
        // already gone: done either way.
        let _not_found = world.take_fault(
            |f| matches!(f, WorldFault::SessionDeleteNotFound { scale_set: s } if *s == scale_set),
        );
        if let Some(s) = world.sets.get_mut(&scale_set.0)
            && s.live == Some(session)
        {
            s.live = None;
            s.max_capacity = 0;
        }
        if world.held.get(&scale_set.0) == Some(&session) {
            world.held.remove(&scale_set.0);
        }
        Ok(())
    }

    async fn poll(
        &self,
        scale_set: ScaleSetId,
        last: Option<MessageId>,
        max_capacity: u32,
    ) -> Result<Option<PolledMessage>, GitHubCallError> {
        let wait = {
            let mut world = self.world();
            if !world.held.contains_key(&scale_set.0) {
                return Err(fail(Failure::NoSession, "no session is held"));
            }
            let is_for = |f: &WorldFault, set: ScaleSetId| match f {
                WorldFault::SessionExpired { scale_set }
                | WorldFault::QueueTokenExpired { scale_set }
                | WorldFault::PollRateLimited { scale_set, .. }
                | WorldFault::PollEmptyOk { scale_set } => *scale_set == set,
                _ => false,
            };
            match world.take_fault(|f| is_for(f, scale_set)) {
                Some(WorldFault::SessionExpired { .. }) => {
                    world.held.remove(&scale_set.0);
                    return Err(fail(Failure::SessionExpired, "the session is gone"));
                }
                Some(WorldFault::PollEmptyOk { .. }) => {
                    return Err(fail(Failure::Unavailable, "the poll answered an empty 200"));
                }
                Some(WorldFault::PollRateLimited {
                    retry_after_secs, ..
                }) => Some(if world.broken == Some(Broken::RetryAfterIgnoringApi) {
                    Millis::new(1_000)
                } else {
                    Millis::new(retry_after_secs.saturating_mul(1000))
                }),
                // A queue token's expiry is refreshed inside the client: nothing to see.
                _ => None,
            }
        };
        if let Some(wait) = wait {
            self.clock.sleep(wait).await;
        }
        let mut world = self.world();
        world.live_set(scale_set)?;
        if let Some(s) = world.sets.get_mut(&scale_set.0) {
            s.max_capacity = max_capacity;
        }
        world.assign(scale_set.0);
        let cursor = if world.broken == Some(Broken::NoCursorApi) {
            None
        } else {
            last.map(|m| m.0).filter(|m| *m > 0)
        };
        Ok(world.take_message(scale_set.0, cursor))
    }

    async fn ack(&self, scale_set: ScaleSetId, message: MessageId) -> Result<(), GitHubCallError> {
        let mut world = self.world();
        if !world.held.contains_key(&scale_set.0) {
            return Err(fail(Failure::NoSession, "no session is held"));
        }
        if let Some(s) = world.sets.get_mut(&scale_set.0) {
            s.queue.retain(|m| m.id != message.0);
        }
        let lost = world.take_fault(
            |f| matches!(f, WorldFault::AckAnswerLost { scale_set: s } if *s == scale_set),
        );
        if lost.is_some() {
            return Err(fail(Failure::Unavailable, "the ack's answer was lost"));
        }
        Ok(())
    }

    async fn acquire_jobs(
        &self,
        scale_set: ScaleSetId,
        requests: &BTreeSet<RunnerRequestId>,
    ) -> Result<BTreeSet<RunnerRequestId>, GitHubCallError> {
        let mut world = self.world();
        if !world.held.contains_key(&scale_set.0) {
            return Err(fail(Failure::NoSession, "no session is held"));
        }
        let mut acquired = BTreeSet::new();
        let ids: Vec<u64> = world.jobs.keys().copied().collect();
        for id in ids {
            let state = world.jobs[&id].state.clone();
            match state {
                JobState::Offered { set, request }
                    if set == scale_set.0 && requests.contains(&RunnerRequestId(request)) =>
                {
                    if let Some(j) = world.jobs.get_mut(&id) {
                        j.state = JobState::Assigned { set, request };
                    }
                    acquired.insert(RunnerRequestId(request));
                }
                JobState::Assigned { set, request }
                    if set == scale_set.0
                        && request != 0
                        && requests.contains(&RunnerRequestId(request)) =>
                {
                    acquired.insert(RunnerRequestId(request));
                }
                _ => {}
            }
        }
        Ok(acquired)
    }

    async fn acquirable_jobs(&self, scale_set: ScaleSetId) -> Result<Vec<Offer>, GitHubCallError> {
        let world = self.world();
        world.live_set(scale_set)?;
        Ok(world
            .jobs
            .values()
            .filter_map(|j| match j.state {
                JobState::Offered { set, request } if set == scale_set.0 => Some(Offer {
                    request_id: RunnerRequestId(request),
                    event_name: EventName::Push,
                }),
                _ => None,
            })
            .collect())
    }

    async fn generate_jit_config(
        &self,
        scale_set: ScaleSetId,
        runner_name: &str,
        _work_folder: &str,
    ) -> Result<JitConfig, GitHubCallError> {
        let mut world = self.world();
        world.live_set(scale_set)?;
        let throttled = world.take_fault(
            |f| matches!(f, WorldFault::JitThrottled { scale_set: s } if *s == scale_set),
        );
        if throttled.is_some() {
            return Err(fail(
                Failure::Throttled,
                "too many runners are being configured",
            ));
        }
        if world.runners.values().any(|r| r.name == runner_name) {
            return Err(fail(Failure::NameTaken, "the runner name is taken"));
        }
        let lost = std::mem::take(&mut world.lose_jit_answer);
        let id = world.next();
        let runner = Runner {
            name: runner_name.to_owned(),
            set: scale_set.0,
            job: None,
            online: false,
            listed: !lost,
        };
        let info = runner_info(id, &runner);
        world.runners.insert(id, runner);
        if lost {
            return Err(fail(
                Failure::Unavailable,
                "the connection broke after the runner was registered",
            ));
        }
        Ok(JitConfig {
            runner: info,
            encoded: Secret::new(format!("fake-jit-config-{id:08}")),
        })
    }
}

impl RunnerRegistry for FakeRunnerRegistry {
    async fn get_runner(&self, id: RunnerId) -> Result<Option<RunnerInfo>, GitHubCallError> {
        Ok(self
            .api
            .world()
            .runners
            .get(&id.0)
            .map(|r| runner_info(id.0, r)))
    }

    async fn find_runners(&self, name: &str) -> Result<Vec<RunnerInfo>, GitHubCallError> {
        let mut world = self.api.world();
        world.listings += 1;
        Ok(world
            .runners
            .iter()
            .filter(|(_, r)| r.listed && r.name == name)
            .map(|(id, r)| runner_info(*id, r))
            .collect())
    }

    async fn list_runners(&self) -> Result<Vec<RunnerInfo>, GitHubCallError> {
        let mut world = self.api.world();
        world.listings += 1;
        Ok(world
            .runners
            .iter()
            .chain(world.stale.iter())
            .filter(|(_, r)| r.listed)
            .map(|(id, r)| runner_info(*id, r))
            .collect())
    }

    async fn remove_runner(&self, id: RunnerId) -> Result<RemovalOutcome, GitHubCallError> {
        let mut world = self.api.world();
        world.removals += 1;
        let Some(runner) = world.runners.get(&id.0).cloned() else {
            return Ok(RemovalOutcome::Gone);
        };
        if runner.job.is_some() {
            return Ok(RemovalOutcome::Busy);
        }
        if world.broken == Some(Broken::NameGuessingRegistry) {
            world
                .runners
                .retain(|_, r| !r.name.starts_with(&runner.name) || r.job.is_some());
        } else {
            world.runners.remove(&id.0);
        }
        if world.broken == Some(Broken::StaleListingRegistry) {
            world.stale.insert(id.0, runner);
        }
        Ok(RemovalOutcome::Gone)
    }
}
