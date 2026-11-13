//! [`RegistrationManager`]: policies P9 (the mint), P10 (an unknown issue), P11 (deregister
//! before stop) and P13 (presumed leaked) over one attempt's two streams, as a durable consumer
//! and a level-triggered pass (module docs of [`crate::registration`]).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard, PoisonError};

use mayfly_control_domain::attempt::{Attempt, AttemptCommand, AttemptEvent, AttemptState, Jit};
use mayfly_control_domain::attempt::{Phase as AttemptPhase, RetireReason, StopDeregistration};
use mayfly_control_domain::registration::{
    DeregistrationAnswer, DeregistrationReason, LeakReason, Phase, RegistrationCommand,
    RegistrationEvent, RegistrationState, RequestRegistration, RetirementStep, RunnerId,
    RunnerRegistration, SessionHealth, retirement_step,
};
use mayfly_control_domain::{Actor, Aggregate, Command, Rejection};
use mayfly_domain::{AttemptId, Deadline, Epoch, EventId, Millis};
use mayfly_protocol::Mode;

use super::broker::{JitBroker, JitScope};
use super::reaper::SweepReport;
use super::{
    MIN_LISTING_GRACE, REGISTRATION_MANAGER, RegistrationConfig, ScaleSetDirectory, command_of,
    handle_of,
};
use crate::delivery::{Consumer, ConsumerError, Delivery};
use crate::github::{Failure, RemovalOutcome, RunnerRegistry, ScaleSetApi};
use crate::journal::{Journal, MAX_PAGE, StreamId, StreamRevision};
use crate::sessions::HostSessions;
use crate::store::{
    AggregateStore, Clock, Outcome, Rng, SaveError, SaveRequest, StoreNotifier, decode_event,
    stream_of,
};

/// How many steps one call takes at most before it returns: each step records one command or
/// makes one call, so a delivery drives an attempt as far as it can go now (a removal, then the
/// confirmation) without ever spinning.
const MAX_ROUNDS: usize = 8;

/// What a call to [`RegistrationManager::step`] ended on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Nothing to do for the attempt now, and nothing done.
    Idle,
    /// It recorded commands or made calls, and has nothing more to do now (or took its eight
    /// rounds).
    Moved,
    /// It waits for time to pass or for another call in flight (the online deadline, the
    /// listing grace, a mint another delivery is making, a busy runner): the pass comes back.
    Waiting(&'static str),
    /// It cannot go on now for a fault (GitHub unreachable, no data-plane session, safe mode):
    /// the consumer answers an error, so the event is delivered again after a backoff.
    Blocked(&'static str),
}

/// What one [`RegistrationManager::pass`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PassReport {
    /// The registrations visited.
    pub visited: usize,
    /// Those that moved.
    pub moved: Vec<AttemptId>,
    /// Those waiting or blocked, and why.
    pub waiting: Vec<(AttemptId, &'static str)>,
    /// Those that could not be read or written.
    pub failed: Vec<(AttemptId, &'static str)>,
}

/// Memory that does not survive a restart, on purpose: a restart only waits longer (a new
/// online deadline, a new first listing) or presumes a minted config leaked, never the
/// reverse.
#[derive(Debug, Default)]
pub(super) struct Local {
    /// Attempts a step is being taken for: steps of one attempt never overlap, so no step
    /// decides on a state another step is changing (a stale `Requested` would otherwise drop a
    /// freshly minted config).
    stepping: BTreeSet<AttemptId>,
    /// After GitHub answered busy to the removal of a leaked runner: not asked again before
    /// `until`, unless the job's end shows up in the journal meanwhile.
    busy: BTreeMap<AttemptId, BusyBackoff>,
    /// When the manager first saw each registration delivered (the online deadline runs from
    /// there).
    delivered_seen: BTreeMap<AttemptId, Millis>,
    /// When the reaper's first empty listing by name of each unknown issue was taken.
    empty_listing: BTreeMap<AttemptId, Millis>,
    /// When the last listing of every runner was taken (P14's budget).
    pub(super) last_listing: Option<Millis>,
    /// The last sweep's report, for `doctor` and status.
    pub(super) last_sweep: Option<SweepReport>,
}

/// A busy answer's backoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BusyBackoff {
    /// Not asked again before this instant.
    until: Millis,
    /// Whether the job's end was already in the journal when GitHub answered busy.
    job_ended: bool,
}

/// Whether the journal shows the runner's job over: the runner exited, or its job completed.
const fn job_ended(a: &AttemptState, r: &RegistrationState) -> bool {
    a.runner_exited || matches!(r.phase, Phase::Completed)
}

/// The attempt's history the decision needs beyond its folded state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct History {
    /// The latest `RetirementRequested` or `StopDeregistrationRequested`: P11's key.
    key: Option<EventId>,
    /// A `RunnerLaunched` was recorded.
    launched: bool,
    /// A `RunnerLaunchUnknown` was recorded.
    launch_unknown: bool,
}

/// The next thing to do for one attempt (the decision table of [`next`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Next {
    Idle,
    RecordIssueUnknown,
    Remove(RunnerId),
    PresumeLeaked(LeakReason),
    RequestDeregistration(DeregistrationReason),
    Confirm,
    Withdraw,
    Retire(RetireReason),
    RecordHandle,
    RecordDelivered,
    ConfirmOnline(RunnerId),
    Mint,
    /// A leaked runner GitHub answered busy for waits out its backoff.
    WaitBusy,
}

/// Whether the registration holds a runner id GitHub may still list.
pub(super) const fn holds_runner(r: &RegistrationState) -> bool {
    r.runner_id.is_some()
        && matches!(
            r.phase,
            Phase::Issued
                | Phase::Delivered
                | Phase::Connected
                | Phase::Consumed
                | Phase::Completed
        )
}

/// Whether a retirement or a quarantine stop waits for P11's answer.
fn retirement_pending(a: &AttemptState) -> bool {
    match a.phase {
        AttemptPhase::Activating
        | AttemptPhase::Listening
        | AttemptPhase::Backoff { .. }
        | AttemptPhase::Finished => a.retirement_requested.is_some(),
        AttemptPhase::Quarantined { .. } => a.stop_deregistration == StopDeregistration::Requested,
        AttemptPhase::NotStarted
        | AttemptPhase::Reserved
        | AttemptPhase::Cloning
        | AttemptPhase::Booting
        | AttemptPhase::Verifying
        | AttemptPhase::Prepared
        | AttemptPhase::Busy
        | AttemptPhase::Retiring
        | AttemptPhase::Deleting
        | AttemptPhase::Deleted
        | AttemptPhase::Lost
        | AttemptPhase::Closed => false,
    }
}

/// Whether the attempt can be asked to retire for its registration's sake.
const fn retirable(a: &AttemptState) -> bool {
    a.retirement_requested.is_none()
        && matches!(
            a.phase,
            AttemptPhase::Activating | AttemptPhase::Listening | AttemptPhase::Backoff { .. }
        )
}

/// The decision, pure: what the attempt `a` and its registration `r` need next, given the
/// attempt's history, whether the broker holds its config and whether a busy answer's backoff
/// holds back the removal of its leaked runner. Steps of one attempt never overlap, so a
/// registration a step finds `Requested` has no mint in flight. In order:
///
/// 1. a `Requested` is an unknown issue (a crash or a dropped call between the request and the
///    record): never minted again (I19);
/// 2. a pending removal is made, by the journaled id;
/// 3. P13: an orphaned handle, a launch of unknown outcome, or an issued config that reached no
///    handle and is held nowhere is presumed leaked;
/// 4. P11: a pending retirement or quarantine stop follows `retirement_step` (I21);
/// 5. a leaked runner is removed by its id, unless GitHub answered busy for it and the backoff
///    holds (a busy runner is never asked again on every event: its job may last hours);
/// 6. the attempt of a deregistered leaked, a lost or an absent registration is retired, never
///    launched again;
/// 7. a config issued and held but whose handle is not recorded yet gets it recorded;
/// 8. a launched runner is recorded delivered, then confirmed online (ADR 0038);
/// 9. an activating attempt with its grant and a fresh verification is minted for, once.
fn next(a: &AttemptState, r: &RegistrationState, h: History, held: bool, backed_off: bool) -> Next {
    if r.phase == Phase::Requested {
        return Next::RecordIssueUnknown;
    }
    if let (Some(_), Some(id)) = (r.deregistration, r.runner_id) {
        return Next::Remove(id);
    }
    let holds = holds_runner(r);
    if holds && r.leaked.is_none() {
        if matches!(a.jit, Jit::Orphaned(..)) {
            return Next::PresumeLeaked(LeakReason::HandleOrphaned);
        }
        if h.launch_unknown {
            return Next::PresumeLeaked(LeakReason::LaunchUnknown);
        }
        if r.phase == Phase::Issued && a.jit == Jit::None && !held {
            return Next::PresumeLeaked(LeakReason::HandleOrphaned);
        }
    }
    if let Some(key) = h.key.filter(|_| retirement_pending(a)) {
        return match retirement_step(r, key) {
            RetirementStep::Deregister => {
                Next::RequestDeregistration(DeregistrationReason::Retirement { retirement: key })
            }
            RetirementStep::Confirm => Next::Confirm,
            RetirementStep::Withdraw => Next::Withdraw,
            RetirementStep::Wait => Next::Idle,
        };
    }
    if holds && r.leaked.is_some() {
        if backed_off {
            return Next::WaitBusy;
        }
        return Next::RequestDeregistration(DeregistrationReason::PresumedLeaked);
    }
    if retirable(a) {
        if r.leaked.is_some() {
            return Next::Retire(RetireReason::JitPresumedLeaked);
        }
        let lost = matches!(r.phase, Phase::IssueUnknown | Phase::Absent);
        if lost && a.phase == AttemptPhase::Activating && a.jit == Jit::None {
            return Next::Retire(RetireReason::JitLost);
        }
    }
    if r.phase == Phase::Issued && r.leaked.is_none() && a.jit == Jit::None && held {
        return Next::RecordHandle;
    }
    if h.launched && matches!(a.jit, Jit::Taken(..)) && r.phase == Phase::Issued {
        return Next::RecordDelivered;
    }
    if r.phase == Phase::Delivered
        && r.leaked.is_none()
        && let Some(id) = r.runner_id
    {
        return Next::ConfirmOnline(id);
    }
    let activating = a.phase == AttemptPhase::Activating
        && a.grant_ref.is_some()
        && a.fresh_verification
        && a.jit == Jit::None
        && a.retirement_requested.is_none();
    if activating && r.phase == Phase::Unrequested {
        return Next::Mint;
    }
    Next::Idle
}

/// Whether a failed mint certainly created no runner: a definite refusal (I19 holds either way;
/// this only decides between `RecordAbsent` and `RecordIssueUnknown`).
const fn definite(failure: Failure) -> bool {
    matches!(
        failure,
        Failure::Throttled | Failure::Refused { .. } | Failure::Credentials | Failure::ScaleSetGone
    )
}

/// How a save ended, for the manager: `Ok(true)` when it appended (the state moved),
/// `Ok(false)` when it was decided without a change (a refusal, an acceptance with no event, a
/// decision recorded before under another intent), an error code when nothing was decided.
fn progress<R>(saved: Result<Outcome<R>, SaveError>) -> Result<bool, &'static str> {
    match saved {
        Ok(Outcome::Accepted { appended, .. }) => Ok(!appended.streams.is_empty()),
        Ok(Outcome::Refused { .. }) | Err(SaveError::CommandIdReused { .. }) => Ok(false),
        Err(error) if error.is_retryable() => Err("conflict"),
        Err(_) => Err("save_failed"),
    }
}

const fn moved(appended: bool) -> Step {
    if appended { Step::Moved } else { Step::Idle }
}

/// Holds an attempt in [`Local::stepping`] while a step is taken for it, and lets go however the
/// call ends: dropped mid-way (the delivery worker's handle deadline) during a mint, the
/// registration stays `Requested`, which the next step records as an unknown issue.
struct Stepping<'l> {
    local: &'l Mutex<Local>,
    attempt: AttemptId,
}

impl<'l> Stepping<'l> {
    fn enter(local: &'l Mutex<Local>, attempt: AttemptId) -> Option<Self> {
        let entered = local
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .stepping
            .insert(attempt);
        // `then`, not `then_some`: a guard built for a refused entry would drop and release
        // the holder's entry
        entered.then(|| Self { local, attempt })
    }
}

impl Drop for Stepping<'_> {
    fn drop(&mut self) {
        self.local
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .stepping
            .remove(&self.attempt);
    }
}

/// The registration manager over an [`AggregateStore`] and the GitHub ports, issuing commands
/// under the run epoch of each attempt's host session (I18).
pub struct RegistrationManager<'s, J, C, R, N, A, G, D> {
    pub(super) store: &'s AggregateStore<J, C, R, N>,
    sessions: HostSessions,
    api: &'s A,
    pub(super) registry: &'s G,
    pub(super) directory: &'s D,
    broker: &'s JitBroker,
    pub(super) config: RegistrationConfig,
    local: Mutex<Local>,
}

impl<J, C, R, N, A, G, D> std::fmt::Debug for RegistrationManager<'_, J, C, R, N, A, G, D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistrationManager")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl<'s, J, C, R, N, A, G, D> RegistrationManager<'s, J, C, R, N, A, G, D>
where
    J: Journal,
    C: Clock,
    R: Rng,
    N: StoreNotifier,
    A: ScaleSetApi,
    G: RunnerRegistry,
    D: ScaleSetDirectory,
{
    /// A manager over `store`, reading run epochs from `sessions`, minting through `api`,
    /// reading and removing runners through `registry`, finding each pool's scale set in
    /// `directory`, and holding minted configs in `broker`.
    pub fn new(
        store: &'s AggregateStore<J, C, R, N>,
        sessions: HostSessions,
        api: &'s A,
        registry: &'s G,
        directory: &'s D,
        broker: &'s JitBroker,
        config: RegistrationConfig,
    ) -> Self {
        Self {
            store,
            sessions,
            api,
            registry,
            directory,
            broker,
            config,
            local: Mutex::new(Local::default()),
        }
    }

    pub(super) fn local(&self) -> MutexGuard<'_, Local> {
        self.local.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The run epoch commands for attempt `a` are issued under: its host's session (I18). An
    /// attempt with no session, or naming no host, waits for one.
    pub(super) fn epoch(&self, a: &AttemptState) -> Option<Epoch> {
        let identity = a.identity.as_ref()?;
        self.sessions.get(&identity.host_id).map(|s| s.epoch)
    }

    /// Takes `attempt`'s next steps until none is left now (at most eight rounds). A step
    /// already being taken for the attempt answers [`Step::Blocked`] `stepping` at once, so
    /// two steps never decide on one state (pinned by
    /// `tests/store/registration.rs::should_block_a_second_step_when_a_mint_is_in_flight_for_the_attempt`).
    ///
    /// # Errors
    /// A fixed code when a stream cannot be read or a command not saved.
    pub async fn step(&self, attempt: AttemptId) -> Result<Step, &'static str> {
        let Some(_stepping) = Stepping::enter(&self.local, attempt) else {
            return Ok(Step::Blocked("stepping"));
        };
        self.steps(attempt).await
    }

    async fn steps(&self, attempt: AttemptId) -> Result<Step, &'static str> {
        let mut moved_any = false;
        for _ in 0..MAX_ROUNDS {
            match self.once(attempt).await? {
                Step::Moved => moved_any = true,
                Step::Idle if moved_any => return Ok(Step::Moved),
                ended => return Ok(ended),
            }
        }
        Ok(Step::Moved)
    }

    /// Visits every registration that is not settled, or whose attempt still waits on it (a
    /// deregistered or absent registration is skipped only when its attempt has no retirement
    /// to confirm and nothing to retire, so the pass stays the safety net of a consumer that
    /// failed): P10's listing by name for an unknown issue, then its
    /// steps (a removal GitHub did not answer, a busy runner's backoff, the online deadline, a
    /// pending retirement). The supervision tree runs it at most once a minute (RT-07).
    pub async fn pass(&self) -> PassReport {
        let mut report = PassReport::default();
        let attempts = match self.registration_attempts().await {
            Ok(attempts) => attempts,
            Err(code) => {
                report.failed.push((AttemptId::from_u128(0), code));
                return report;
            }
        };
        for attempt in attempts {
            match self
                .store
                .load::<RunnerRegistration>(&attempt.to_string())
                .await
            {
                Ok(r) if matches!(r.phase, Phase::Deregistered | Phase::Absent) => {
                    match self.store.load::<Attempt>(&attempt.to_string()).await {
                        Ok(a) if !retirement_pending(&a) && !retirable(&a) => continue,
                        Ok(_) => {}
                        Err(_) => {
                            report.failed.push((attempt, "attempt_unreadable"));
                            continue;
                        }
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    report.failed.push((attempt, "registration_unreadable"));
                    continue;
                }
            }
            report.visited += 1;
            let Some(_stepping) = Stepping::enter(&self.local, attempt) else {
                report.waiting.push((attempt, "stepping"));
                continue;
            };
            let step = match self.reap_by_name(attempt).await {
                Ok(Step::Moved) => self.steps(attempt).await.map(|s| match s {
                    Step::Idle => Step::Moved,
                    other => other,
                }),
                Ok(Step::Idle) => self.steps(attempt).await,
                other => other,
            };
            match step {
                Ok(Step::Moved) => report.moved.push(attempt),
                Ok(Step::Idle) => {}
                Ok(Step::Waiting(why) | Step::Blocked(why)) => report.waiting.push((attempt, why)),
                Err(code) => report.failed.push((attempt, code)),
            }
        }
        report
    }

    /// Every attempt with a registration stream, in the journal's key order.
    pub(super) async fn registration_attempts(&self) -> Result<Vec<AttemptId>, &'static str> {
        const PREFIX: &str = "registration-";
        let mut after = StreamId::new("registration").map_err(|_| "invalid_stream")?;
        let mut attempts = Vec::new();
        loop {
            let page = self
                .store
                .journal()
                .streams(Some(&after), MAX_PAGE)
                .await
                .map_err(|_| "journal_unreadable")?;
            let Some(last) = page.last() else {
                return Ok(attempts);
            };
            for head in &page {
                match head.stream.as_str().strip_prefix(PREFIX) {
                    Some(id) => attempts.extend(id.parse::<AttemptId>().ok()),
                    None if head.stream.as_str() > PREFIX => return Ok(attempts),
                    None => {}
                }
            }
            after = last.stream.clone();
        }
    }

    async fn once(&self, attempt: AttemptId) -> Result<Step, &'static str> {
        let instance = attempt.to_string();
        let (a, a_head) = self
            .store
            .load_with_head::<Attempt>(&instance)
            .await
            .map_err(|_| "attempt_unreadable")?;
        let (r, r_head) = self
            .store
            .load_with_head::<RunnerRegistration>(&instance)
            .await
            .map_err(|_| "registration_unreadable")?;
        let history = self.history(&instance).await?;
        self.tidy(attempt, &a, &r);
        let held = self.broker.holds(handle_of(attempt));
        let now = self.store.clock().now();
        let ended = job_ended(&a, &r);
        let backed_off =
            self.local().busy.get(&attempt).is_some_and(|b| {
                !Deadline::at(b.until).is_expired(now) && !(ended && !b.job_ended)
            });
        let decided = next(&a, &r, history, held, backed_off);
        if decided == Next::Idle {
            return Ok(Step::Idle);
        }
        let Some(epoch) = self.epoch(&a) else {
            return Ok(Step::Blocked("no_session"));
        };
        let a_rev = a_head.map(StreamRevision::get);
        let r_rev = r_head.map(StreamRevision::get);
        let (tag, body) = match decided {
            Next::Idle => return Ok(Step::Idle),
            Next::WaitBusy => return Ok(Step::Waiting("runner_busy")),
            Next::Remove(id) => {
                // a busy answer to a retirement's request is final for it (I21); any other is
                // asked again only after a backoff
                let backoff = r
                    .deregistration
                    .filter(|reason| reason.retirement().is_none())
                    .map(|_| ended);
                return self.remove(attempt, epoch, r_rev, id, backoff).await;
            }
            Next::RecordHandle => return self.record_handle(attempt, epoch).await,
            Next::ConfirmOnline(id) => {
                return self.confirm_online(attempt, epoch, r_rev, id).await;
            }
            Next::Mint => return self.mint(attempt, &a, epoch).await,
            Next::Confirm => {
                let saved = self
                    .attempt(
                        attempt,
                        epoch,
                        "confirm",
                        a_rev,
                        AttemptCommand::ConfirmRetirement,
                    )
                    .await;
                return progress(saved).map(moved);
            }
            Next::Withdraw => {
                let saved = self
                    .attempt(
                        attempt,
                        epoch,
                        "withdraw",
                        a_rev,
                        AttemptCommand::WithdrawRetirement,
                    )
                    .await;
                return progress(saved).map(moved);
            }
            Next::Retire(reason) => {
                let body = AttemptCommand::RequestRetirement { reason };
                let saved = self.attempt(attempt, epoch, "retire", a_rev, body).await;
                return progress(saved).map(moved);
            }
            // the same id as the mint's own record: a record whose commit was in doubt is
            // answered by its receipt, never recorded twice
            Next::RecordIssueUnknown => {
                tracing::warn!(attempt = %attempt, "registration_issue_unknown");
                let body = RegistrationCommand::RecordIssueUnknown;
                let saved = self
                    .registration(attempt, epoch, "outcome", None, body)
                    .await;
                return progress(saved).map(moved);
            }
            Next::PresumeLeaked(reason) => {
                tracing::warn!(
                    attempt = %attempt,
                    reason = leak_name(reason),
                    "registration_presumed_leaked"
                );
                ("leak", RegistrationCommand::PresumeLeaked { reason })
            }
            Next::RequestDeregistration(reason) => (
                "deregister",
                RegistrationCommand::RequestDeregistration { reason },
            ),
            Next::RecordDelivered => ("delivered", RegistrationCommand::RecordDelivered),
        };
        let saved = self.registration(attempt, epoch, tag, r_rev, body).await;
        progress(saved).map(moved)
    }

    /// Lets go of what the attempt no longer needs: a config whose launch will never come (the
    /// registration is not issued, is leaked, or its handle was taken or orphaned), the broker's
    /// memory of a settled registration's handle, and the deadlines of phases it left.
    fn tidy(&self, attempt: AttemptId, a: &AttemptState, r: &RegistrationState) {
        let handle = handle_of(attempt);
        let launchable = r.phase == Phase::Issued
            && r.leaked.is_none()
            && matches!(a.jit, Jit::None | Jit::Issued(..));
        if matches!(r.phase, Phase::Deregistered | Phase::Absent) {
            // never minted again (I19): the handle can be forgotten
            self.broker.forget(handle);
        } else if !launchable {
            self.broker.discard(handle);
        }
        let mut local = self.local();
        if !holds_runner(r) {
            local.busy.remove(&attempt);
        }
        if r.phase != Phase::Delivered {
            local.delivered_seen.remove(&attempt);
        }
        if r.phase != Phase::IssueUnknown {
            local.empty_listing.remove(&attempt);
        }
    }

    /// The attempt's history: P11's key and the launch's outcome.
    async fn history(&self, instance: &str) -> Result<History, &'static str> {
        let stream = stream_of::<Attempt>(instance).map_err(|_| "invalid_stream")?;
        let events = self
            .store
            .journal()
            .read_stream(&stream, None, None)
            .await
            .map_err(|_| "attempt_unreadable")?;
        let mut history = History::default();
        for recorded in &events {
            match decode_event::<AttemptEvent>(recorded).map_err(|_| "attempt_undecodable")? {
                AttemptEvent::RetirementRequested(..)
                | AttemptEvent::StopDeregistrationRequested(..) => {
                    history.key = Some(recorded.event_id);
                }
                AttemptEvent::RunnerLaunched(..) => history.launched = true,
                AttemptEvent::RunnerLaunchUnknown(..) => history.launch_unknown = true,
                _ => {}
            }
        }
        Ok(history)
    }

    /// The registration's `RegistrationIssued` event: the attempt's `registration_ref`.
    async fn issued_event(&self, instance: &str) -> Result<Option<EventId>, &'static str> {
        let stream = stream_of::<RunnerRegistration>(instance).map_err(|_| "invalid_stream")?;
        let events = self
            .store
            .journal()
            .read_stream(&stream, None, None)
            .await
            .map_err(|_| "registration_unreadable")?;
        for recorded in &events {
            if let RegistrationEvent::RegistrationIssued(..) =
                decode_event::<RegistrationEvent>(recorded)
                    .map_err(|_| "registration_undecodable")?
            {
                return Ok(Some(recorded.event_id));
            }
        }
        Ok(None)
    }

    /// Saves `body` on `attempt`'s registration under a command id derived from `tag` and the
    /// revision the decision read.
    pub(super) async fn registration(
        &self,
        attempt: AttemptId,
        epoch: Epoch,
        tag: &str,
        revision: Option<u64>,
        body: RegistrationCommand,
    ) -> Result<Outcome<<RunnerRegistration as Aggregate>::Rejection>, SaveError> {
        let command = Command::new(
            command_of(tag, attempt, revision),
            self.store.clock().now(),
            epoch,
            Actor::Automation {
                policy: REGISTRATION_MANAGER,
            },
            body,
        );
        self.store
            .execute::<RunnerRegistration>(SaveRequest::new(attempt, command))
            .await
    }

    /// Saves `body` on the attempt under a command id derived from `tag` and the revision the
    /// decision read.
    async fn attempt(
        &self,
        attempt: AttemptId,
        epoch: Epoch,
        tag: &str,
        revision: Option<u64>,
        body: AttemptCommand,
    ) -> Result<Outcome<<Attempt as Aggregate>::Rejection>, SaveError> {
        let command = Command::new(
            command_of(tag, attempt, revision),
            self.store.clock().now(),
            epoch,
            Actor::Automation {
                policy: REGISTRATION_MANAGER,
            },
            body,
        );
        self.store
            .execute::<Attempt>(SaveRequest::new(attempt, command))
            .await
    }

    /// P9: asks for the registration once, mints the JIT config once, holds it in the broker
    /// and records the answer. The config never reaches the journal, a log or an error.
    /// Pinned by `tests/store/registration.rs::should_mint_once_and_record_only_the_handle_when_an_attempt_is_activating`
    /// and `should_retire_the_attempt_and_never_mint_again_when_the_jit_answer_is_lost`.
    async fn mint(
        &self,
        attempt: AttemptId,
        a: &AttemptState,
        epoch: Epoch,
    ) -> Result<Step, &'static str> {
        let Some(identity) = &a.identity else {
            return Ok(Step::Idle);
        };
        if self
            .sessions
            .get(&identity.host_id)
            .is_some_and(|s| s.mode == Mode::Safe)
        {
            return Ok(Step::Blocked("safe_mode"));
        }
        let Some((scale_set, health)) = self.directory.scale_set(&identity.pool) else {
            return Ok(Step::Blocked("no_scale_set"));
        };
        if health == SessionHealth::Degraded {
            // P21: minting is frozen; the aggregate would refuse it too
            return Ok(Step::Blocked("scale_set_degraded"));
        }
        let request = RegistrationCommand::RequestRegistration(RequestRegistration {
            attempt_id: attempt,
            runner_name: identity.vm_name.clone(),
            scale_set,
            session: SessionHealth::Healthy,
        });
        match self
            .registration(attempt, epoch, "request", None, request)
            .await
        {
            Ok(Outcome::Accepted {
                appended,
                replayed: false,
            }) if !appended.streams.is_empty() => {}
            // asked before, by another run: its outcome is not this call's
            Ok(Outcome::Accepted { .. }) => return Ok(Step::Moved),
            Ok(Outcome::Refused { rejection, .. }) => return Ok(Step::Waiting(rejection.code())),
            Err(error) if error.is_retryable() => return Err("conflict"),
            Err(_) => return Err("save_failed"),
        }
        let name = identity.vm_name.to_string();
        let minted = self
            .api
            .generate_jit_config(scale_set, &name, &self.config.work_folder)
            .await;
        match minted {
            Ok(jit) => {
                let handle = handle_of(attempt);
                // a handle already used means this attempt minted before: the config is not
                // held, and the next step presumes it leaked
                let _ = self
                    .broker
                    .put(handle, JitScope { attempt, epoch }, jit.encoded);
                let saved = self
                    .registration(
                        attempt,
                        epoch,
                        "outcome",
                        None,
                        RegistrationCommand::RecordIssued {
                            runner_id: jit.runner.id,
                        },
                    )
                    .await;
                match progress(saved) {
                    Ok(appended) => Ok(moved(appended)),
                    Err(code) => {
                        self.broker.discard(handle);
                        Err(code)
                    }
                }
            }
            Err(error) => {
                tracing::warn!(
                    attempt = %attempt,
                    failure = error.failure.as_str(),
                    "registration_mint_failed"
                );
                let body = if definite(error.failure) {
                    RegistrationCommand::RecordAbsent
                } else {
                    RegistrationCommand::RecordIssueUnknown
                };
                let saved = self
                    .registration(attempt, epoch, "outcome", None, body)
                    .await;
                progress(saved).map(moved)
            }
        }
    }

    /// Records the held config's handle on the attempt (`RecordJitHandle`, once per attempt). A
    /// refusal (the attempt left activation meanwhile) drops the config: the next step presumes
    /// the registration leaked and removes its runner.
    async fn record_handle(&self, attempt: AttemptId, epoch: Epoch) -> Result<Step, &'static str> {
        let handle = handle_of(attempt);
        let Some(registration_ref) = self.issued_event(&attempt.to_string()).await? else {
            return Ok(Step::Idle);
        };
        let saved = self
            .attempt(
                attempt,
                epoch,
                "handle",
                None,
                AttemptCommand::RecordJitHandle {
                    handle_id: handle,
                    registration_ref,
                },
            )
            .await;
        match saved {
            Ok(Outcome::Accepted { .. }) => Ok(Step::Moved),
            Ok(Outcome::Refused { .. }) | Err(SaveError::CommandIdReused { .. }) => {
                self.broker.discard(handle);
                Ok(Step::Moved)
            }
            Err(error) if error.is_retryable() => Err("conflict"),
            Err(_) => Err("save_failed"),
        }
    }

    /// Removes the runner by its journaled id and records GitHub's answer; an unanswered call
    /// leaves the request pending (asked again by id, which is idempotent). A busy answer to a
    /// request that serves no retirement (`backoff` says whether the job's end was in the
    /// journal) holds the next request back for [`RegistrationConfig::busy_retry`] (pinned by
    /// `tests/store/registration.rs::should_bound_the_removals_when_a_leaked_runner_stays_busy`).
    async fn remove(
        &self,
        attempt: AttemptId,
        epoch: Epoch,
        revision: Option<u64>,
        id: RunnerId,
        backoff: Option<bool>,
    ) -> Result<Step, &'static str> {
        let answer = match self.registry.remove_runner(id).await {
            // 204 or not found: gone, which is not proof that mayfly removed it (spec §7); the
            // port does not tell them apart, and either deregisters
            Ok(RemovalOutcome::Gone) => DeregistrationAnswer::Removed,
            Ok(RemovalOutcome::Busy) => DeregistrationAnswer::Busy,
            Err(error) => return Ok(Step::Blocked(error.failure.as_str())),
        };
        let saved = self
            .registration(
                attempt,
                epoch,
                "answer",
                revision,
                RegistrationCommand::RecordDeregistration { answer },
            )
            .await;
        let appended = progress(saved)?;
        if answer == DeregistrationAnswer::Busy {
            if let Some(job_ended) = backoff {
                let until =
                    Deadline::after(self.store.clock().now(), self.config.busy_retry).instant();
                self.local()
                    .busy
                    .insert(attempt, BusyBackoff { until, job_ended });
            }
            return Ok(Step::Waiting("runner_busy"));
        }
        Ok(moved(appended))
    }

    /// The online confirmation (ADR 0038): the issued id online (or already busy) is
    /// `RecordConnected`; not seen by the deadline, the registration is presumed leaked.
    async fn confirm_online(
        &self,
        attempt: AttemptId,
        epoch: Epoch,
        revision: Option<u64>,
        id: RunnerId,
    ) -> Result<Step, &'static str> {
        let now = self.store.clock().now();
        let since = *self.local().delivered_seen.entry(attempt).or_insert(now);
        let seen = match self.registry.get_runner(id).await {
            Ok(Some(runner)) => {
                runner.id == id && (runner.online == Some(true) || runner.busy == Some(true))
            }
            Ok(None) => false,
            Err(error) => return Ok(Step::Blocked(error.failure.as_str())),
        };
        if seen {
            let saved = self
                .registration(
                    attempt,
                    epoch,
                    "connected",
                    revision,
                    RegistrationCommand::RecordConnected { runner_id: id },
                )
                .await;
            return progress(saved).map(moved);
        }
        if !Deadline::after(since, self.config.online_deadline).is_expired(now) {
            return Ok(Step::Waiting("not_online_yet"));
        }
        tracing::warn!(attempt = %attempt, "registration_not_seen_online");
        let saved = self
            .registration(
                attempt,
                epoch,
                "leak",
                revision,
                RegistrationCommand::PresumeLeaked {
                    reason: LeakReason::NotSeenOnline,
                },
            )
            .await;
        progress(saved).map(moved)
    }

    /// P10: lists runners by an unknown issue's exact name. One listed is journaled
    /// (`RecordFoundByName`, presumed leaked; the steps remove it by that id); none is recorded
    /// absent only on a second empty listing at least the listing grace after the first, since
    /// GitHub's listing may lag behind a lost mint (`formal/registration.qnt`,
    /// `absentOnlyWhenNoRunner`). Pinned by
    /// `tests/store/registration.rs::should_find_the_runner_by_name_on_the_second_listing_when_the_first_lagged`
    /// and `should_record_absent_only_after_a_second_empty_listing_when_the_grace_passed`.
    pub(super) async fn reap_by_name(&self, attempt: AttemptId) -> Result<Step, &'static str> {
        let instance = attempt.to_string();
        let (r, r_head) = self
            .store
            .load_with_head::<RunnerRegistration>(&instance)
            .await
            .map_err(|_| "registration_unreadable")?;
        let Some(name) = r
            .runner_name
            .as_ref()
            .filter(|_| r.phase == Phase::IssueUnknown)
        else {
            return Ok(Step::Idle);
        };
        let a = self
            .store
            .load::<Attempt>(&instance)
            .await
            .map_err(|_| "attempt_unreadable")?;
        let Some(epoch) = self.epoch(&a) else {
            return Ok(Step::Blocked("no_session"));
        };
        let name = name.to_string();
        let listed = match self.registry.find_runners(&name).await {
            Ok(listed) => listed,
            Err(error) => return Ok(Step::Blocked(error.failure.as_str())),
        };
        let found = listed.iter().find(|runner| {
            runner.name == name && runner.scale_set.is_none_or(|s| Some(s) == r.scale_set)
        });
        let revision = r_head.map(StreamRevision::get);
        if let Some(runner) = found {
            let saved = self
                .registration(
                    attempt,
                    epoch,
                    "found",
                    revision,
                    RegistrationCommand::RecordFoundByName {
                        runner_id: runner.id,
                    },
                )
                .await;
            return progress(saved).map(moved);
        }
        let now = self.store.clock().now();
        let first = *self.local().empty_listing.entry(attempt).or_insert(now);
        let grace = self.config.listing_grace.max(MIN_LISTING_GRACE);
        if first == now || !Deadline::after(first, grace).is_expired(now) {
            return Ok(Step::Waiting("listing_grace"));
        }
        let saved = self
            .registration(
                attempt,
                epoch,
                "absent",
                revision,
                RegistrationCommand::RecordAbsent,
            )
            .await;
        progress(saved).map(moved)
    }
}

const fn leak_name(reason: LeakReason) -> &'static str {
    match reason {
        LeakReason::LaunchUnknown => "launch_unknown",
        LeakReason::HandleOrphaned => "handle_orphaned",
        LeakReason::NotSeenOnline => "not_seen_online",
        LeakReason::IdentityDoubt => "identity_doubt",
        LeakReason::RedactionViolation => "redaction_violation",
        LeakReason::IssueUnknown => "issue_unknown",
    }
}

impl<J, C, R, N, A, G, D> Consumer for RegistrationManager<'_, J, C, R, N, A, G, D>
where
    J: Journal,
    C: Clock,
    R: Rng,
    N: StoreNotifier,
    A: ScaleSetApi,
    G: RunnerRegistry,
    D: ScaleSetDirectory,
{
    const NAME: &'static str = REGISTRATION_MANAGER;

    /// Takes the next steps of the attempt an `attempt-*` or `registration-*` event belongs to;
    /// events of other streams pass. A step blocked by a fault answers an error, so the event
    /// comes again after a backoff; a wait for time is the pass's.
    async fn handle(&self, delivery: Delivery<'_>) -> Result<(), ConsumerError> {
        let stream = delivery.event.stream.as_str();
        let Some(id) = stream
            .strip_prefix("attempt-")
            .or_else(|| stream.strip_prefix("registration-"))
        else {
            return Ok(());
        };
        let Ok(attempt) = id.parse::<AttemptId>() else {
            return Ok(());
        };
        match self.step(attempt).await.map_err(ConsumerError::new)? {
            Step::Blocked(code) => Err(ConsumerError::new(code)),
            Step::Idle | Step::Moved | Step::Waiting(..) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use mayfly_control_domain::attempt::Phase as P;
    use mayfly_domain::SecretHandleId;

    use super::*;

    fn activating() -> AttemptState {
        let mut a = AttemptState::initial();
        a.phase = P::Activating;
        a.grant_ref = Some(EventId::from_u128(1));
        a.fresh_verification = true;
        a
    }

    fn registration(phase: Phase, runner: Option<u64>) -> RegistrationState {
        let mut r = RegistrationState::initial();
        r.phase = phase;
        r.runner_id = runner.map(RunnerId);
        r
    }

    #[test]
    fn should_mint_only_when_activating_with_a_grant_and_a_fresh_verification() {
        // given
        let ready = activating();
        let mut stale = activating();
        stale.fresh_verification = false;
        let mut ungranted = activating();
        ungranted.grant_ref = None;
        let none = registration(Phase::Unrequested, None);

        // when
        let decided =
            [&ready, &stale, &ungranted].map(|a| next(a, &none, History::default(), false, false));

        // then
        assert_eq!(decided, [Next::Mint, Next::Idle, Next::Idle]);
    }

    #[test]
    fn should_record_an_unknown_issue_when_a_step_finds_a_request_unanswered() {
        // given: steps never overlap, so no mint answers a request a step finds
        let a = activating();
        let requested = registration(Phase::Requested, None);

        // when
        let decided = next(&a, &requested, History::default(), false, false);

        // then
        assert_eq!(decided, Next::RecordIssueUnknown);
    }

    #[test]
    fn should_wait_out_the_backoff_when_github_answered_busy_for_a_leaked_runner() {
        // given
        let a = activating();
        let mut leaked = registration(Phase::Connected, Some(7));
        leaked.leaked = Some(LeakReason::NotSeenOnline);

        // when
        let backed_off = next(&a, &leaked, History::default(), false, true);
        let after = next(&a, &leaked, History::default(), false, false);

        // then
        assert_eq!(backed_off, Next::WaitBusy);
        assert_eq!(
            after,
            Next::RequestDeregistration(DeregistrationReason::PresumedLeaked)
        );
    }

    #[test]
    fn should_presume_leaked_when_an_issued_config_is_held_nowhere_and_has_no_handle() {
        // given
        let a = activating();
        let issued = registration(Phase::Issued, Some(7));

        // when
        let held = next(&a, &issued, History::default(), true, false);
        let lost = next(&a, &issued, History::default(), false, false);

        // then
        assert_eq!(held, Next::RecordHandle);
        assert_eq!(lost, Next::PresumeLeaked(LeakReason::HandleOrphaned));
    }

    #[test]
    fn should_never_mint_again_when_the_issue_was_unknown_or_absent() {
        // given
        let a = activating();

        // when
        let decided = [Phase::IssueUnknown, Phase::Absent].map(|phase| {
            next(
                &a,
                &registration(phase, None),
                History::default(),
                false,
                false,
            )
        });

        // then: the attempt is retired, never minted for again (I19)
        assert_eq!(decided, [Next::Retire(RetireReason::JitLost); 2]);
    }

    #[test]
    fn should_deregister_before_confirming_when_a_retirement_waits_on_a_registered_runner() {
        // given
        let mut a = activating();
        a.jit = Jit::Issued(SecretHandleId::from_u128(1));
        a.retirement_requested = Some(RetireReason::Surplus);
        let key = EventId::from_u128(9);
        let history = History {
            key: Some(key),
            ..History::default()
        };
        let issued = registration(Phase::Issued, Some(7));
        let gone = registration(Phase::Deregistered, Some(7));

        // when
        let first = next(&a, &issued, history, true, false);
        let after = next(&a, &gone, history, false, false);

        // then
        assert_eq!(
            first,
            Next::RequestDeregistration(DeregistrationReason::Retirement { retirement: key })
        );
        assert_eq!(after, Next::Confirm);
    }
}
