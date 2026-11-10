//! The `attempt_saga` durable consumer (`doc/event-store.md` §5.2; policies P6 and P7 of
//! `doc/domain-model.md` §7): every event of an `attempt-*` stream wakes it, it reads where the
//! attempt is now, and issues the next command the pure decision table
//! ([`mayfly_control_domain::attempt::next_step`]) names: `IssueNext`, `Close`, or nothing;
//! for a directive the relay sent (`DirectiveSent`) it arms a deadline timer whose expiry is
//! the `RecordDirectiveTimeout` command, which makes the outcome `Unknown`, never a success. A
//! directive issued and not sent (held in safe mode, without a session, before adoption) arms
//! nothing: its outcome is known, nothing happened.
//!
//! **Idempotent by event id.** The command an event causes has an id derived from
//! `(attempt_saga, event_id)` ([`command_for_event`]), and the directive it issues an id derived
//! from that command ([`directive_of`]): a redelivered event is answered by the command's
//! receipt, or, when the attempt moved on, judged against the newer state and refused or
//! skipped; a directive is never issued twice under two ids for one event. The deadline is
//! read from the clock at the decision, so a redelivery whose first decision committed may
//! carry another intent: [`SaveError::CommandIdReused`] then means "decided before", which is
//! all the saga needs (pinned by
//! `tests/store/saga.rs::should_issue_one_directive_when_an_event_is_delivered_twice`).
//!
//! **Deadlines are memory.** The timers live in this consumer, armed whenever an event shows a
//! directive in flight; a restart loses them. Startup recovery (CP-04) re-arms them with
//! [`AttemptSaga::reconsider`] for every attempt it adopts, after resolving `Unknown` outcomes
//! by inventory (plan §10).

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use mayfly_control_domain::attempt::{
    Attempt, AttemptCommand, AttemptRejection, AttemptState, DirectiveTag, SagaStep, next_step,
};
use mayfly_control_domain::{Actor, Command};
use mayfly_domain::{CommandId, Deadline, DirectiveId, EventId, HostId, Millis};
use sha2::{Digest, Sha256};

use crate::delivery::{Consumer, ConsumerError, Delivery};
use crate::journal::{Journal, StreamId};
use crate::sessions::HostSessions;
use crate::store::{AggregateStore, Clock, Outcome, Rng, SaveError, SaveRequest, StoreNotifier};

/// The saga's name: the delivery ledger's consumer and the actor of its commands.
pub const SAGA: &str = "attempt_saga";

const ID_DOMAIN: &[u8] = b"mayfly-attempt-saga-v1\n";

fn derive(tag: &[u8], id: u128) -> u128 {
    let mut hasher = Sha256::new();
    hasher.update(ID_DOMAIN);
    hasher.update(tag);
    hasher.update(id.to_be_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    let mut head = [0_u8; 16];
    head.copy_from_slice(&digest[..16]);
    u128::from_be_bytes(head)
}

/// The id of the command the saga issues for event `event_id`: the first 128 bits of SHA-256
/// over a domain tag, `event` and the event id. Pinned by
/// `tests/store/saga.rs::should_derive_the_golden_ids_when_an_event_id_is_given`.
#[must_use]
pub fn command_for_event(event_id: EventId) -> CommandId {
    CommandId::from_u128(derive(b"event\n", event_id.as_u128()))
}

/// The id of the directive an `IssueNext` with `command_id` issues: what the relay sends and
/// the data plane dedupes by, so every resend of it carries this one id.
#[must_use]
pub fn directive_of(command_id: CommandId) -> DirectiveId {
    DirectiveId::from_u128(derive(b"directive\n", command_id.as_u128()))
}

/// The id of the `RecordDirectiveTimeout` for `directive_id`: one per directive, so a timer
/// that fires twice is answered by the first one's receipt.
#[must_use]
pub fn timeout_of(directive_id: DirectiveId) -> CommandId {
    CommandId::from_u128(derive(b"timeout\n", directive_id.as_u128()))
}

/// The id of the `RecordDirectiveSent` for `directive_id`: one per directive, so every relay
/// worker that sends it records it under the one command (a second send is a receipt replay or,
/// under another deadline, `CommandIdReused`: recorded before).
#[must_use]
pub fn sent_of(directive_id: DirectiveId) -> CommandId {
    CommandId::from_u128(derive(b"sent\n", directive_id.as_u128()))
}

/// How long each directive kind may take once sent (P6: a deadline per stage): the relay's
/// sink fixes a directive's deadline at its first send from these, and the saga gives an
/// `IssueNext` the deadline it would have if it were sent at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectiveTimeouts {
    /// `CloneVm`: a copy of the image base.
    pub clone: Millis,
    /// `StartVm`: the VM boots under its shim.
    pub start: Millis,
    /// `VerifyGuest`: the guest answers its challenge.
    pub verify: Millis,
    /// `LaunchRunner`: the JIT delivery deadline (T_jit).
    pub launch: Millis,
    /// `ExportDiagnostics`.
    pub diagnostics: Millis,
    /// `StopVm`, its grace included.
    pub stop: Millis,
    /// `DeleteVm`.
    pub delete: Millis,
}

impl Default for DirectiveTimeouts {
    /// Ten minutes for a clone, two for every other kind.
    fn default() -> Self {
        let two_minutes = Millis::new(120_000);
        Self {
            clone: Millis::new(600_000),
            start: two_minutes,
            verify: two_minutes,
            launch: two_minutes,
            diagnostics: two_minutes,
            stop: two_minutes,
            delete: two_minutes,
        }
    }
}

impl DirectiveTimeouts {
    /// The time a directive of `tag` is given.
    #[must_use]
    pub const fn timeout(&self, tag: DirectiveTag) -> Millis {
        match tag {
            DirectiveTag::CloneVm => self.clone,
            DirectiveTag::StartVm => self.start,
            DirectiveTag::VerifyGuest => self.verify,
            DirectiveTag::LaunchRunner => self.launch,
            DirectiveTag::ExportDiagnostics => self.diagnostics,
            DirectiveTag::StopVm => self.stop,
            DirectiveTag::DeleteVm => self.delete,
        }
    }
}

/// The saga's timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SagaConfig {
    /// Each kind's time once sent.
    pub timeouts: DirectiveTimeouts,
    /// Past a deadline, the time an answer the data plane sent at the deadline may still take
    /// to arrive before the timeout makes the outcome `Unknown`.
    pub reply_grace: Millis,
}

impl Default for SagaConfig {
    /// The default timeouts and five seconds of grace.
    fn default() -> Self {
        Self {
            timeouts: DirectiveTimeouts::default(),
            reply_grace: Millis::new(5_000),
        }
    }
}

/// A deadline timer for the directive in flight on one attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Armed {
    instance: String,
    host: HostId,
    directive_id: DirectiveId,
    deadline: Deadline,
    /// After a timeout that could not be decided: not before this instant again, so a timer
    /// loop never spins on it.
    retry_at: Option<Millis>,
}

/// What [`AttemptSaga::expire_due`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TimeoutReport {
    /// Directives whose outcome the timeout made `Unknown`.
    pub unknown: Vec<DirectiveId>,
    /// Timeouts that changed nothing: the answer came first, or the outcome was already
    /// `Unknown`.
    pub moot: Vec<DirectiveId>,
    /// Timeouts that could not be decided now (no session, a store fault): kept, and due again
    /// one reply grace later (pinned by
    /// `tests/store/saga.rs::should_retry_a_timeout_later_when_it_cannot_be_decided_now`).
    pub failed: Vec<(DirectiveId, &'static str)>,
}

/// The attempt saga over an [`AggregateStore`], issuing commands under the run epoch of each
/// attempt's host session.
pub struct AttemptSaga<'s, J, C, R, N> {
    store: &'s AggregateStore<J, C, R, N>,
    sessions: HostSessions,
    config: SagaConfig,
    /// At most one per attempt stream (one directive in flight per attempt, I13).
    timers: Mutex<BTreeMap<StreamId, Armed>>,
}

impl<J, C, R, N> std::fmt::Debug for AttemptSaga<'_, J, C, R, N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttemptSaga")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// How a timeout ended: `Ok(true)` when it recorded `Unknown`; `Ok(false)` when it was moot
/// (the answer came first, `directive_not_pending`, or the outcome was already `Unknown`, an
/// acceptance with no event); an error for anything else, a refusal included (`not_adopted`,
/// `stale_run`, `directive_not_sent`): the timer stays armed and is tried again one reply grace
/// later, and since a refusal receipt answers only while its stream is unchanged, the retry is
/// decided afresh once the attempt moves (adopted, sent). Pinned by
/// `tests/store/saga.rs::should_keep_the_timer_armed_when_a_timeout_is_refused_for_another_reason`.
fn timed_out(saved: Result<Outcome<AttemptRejection>, SaveError>) -> Result<bool, &'static str> {
    match saved {
        Ok(Outcome::Accepted { appended, .. }) => Ok(!appended.streams.is_empty()),
        Ok(Outcome::Refused {
            rejection: AttemptRejection::DirectiveNotPending,
            ..
        }) => Ok(false),
        Ok(Outcome::Refused { .. }) => Err("timeout_refused"),
        Err(error) if error.is_retryable() => Err("conflict"),
        Err(_) => Err("save_failed"),
    }
}

/// How a saved command ended, for the saga: decided (accepted, refused, or decided before under
/// another intent), or not decided now.
fn decided<R>(saved: Result<Outcome<R>, SaveError>) -> Result<bool, &'static str> {
    match saved {
        Ok(Outcome::Accepted { .. }) => Ok(true),
        Ok(Outcome::Refused { .. }) | Err(SaveError::CommandIdReused { .. }) => Ok(false),
        Err(error) if error.is_retryable() => Err("conflict"),
        Err(_) => Err("save_failed"),
    }
}

impl<'s, J: Journal, C: Clock, R: Rng, N: StoreNotifier> AttemptSaga<'s, J, C, R, N> {
    /// A saga over `store`, reading run epochs from `sessions`.
    pub fn new(
        store: &'s AggregateStore<J, C, R, N>,
        sessions: HostSessions,
        config: SagaConfig,
    ) -> Self {
        Self {
            store,
            sessions,
            config,
            timers: Mutex::new(BTreeMap::new()),
        }
    }

    /// Reads attempt `instance` and acts on its state as if one of its events had been
    /// delivered, without issuing a command for any event: re-arms the deadline timer of a
    /// directive in flight, and disarms one that no longer is. Startup recovery (CP-04) calls it
    /// for every attempt it adopts, since timers do not survive a restart.
    ///
    /// # Errors
    /// [`SaveError`] when the stream cannot be read.
    pub async fn reconsider(&self, instance: &str) -> Result<SagaStep, SaveError> {
        let state = self.store.load::<Attempt>(instance).await?;
        let step = next_step(&state);
        let stream =
            crate::store::stream_of::<Attempt>(instance).map_err(SaveError::InvalidName)?;
        self.arm(&stream, instance, &state, &step);
        Ok(step)
    }

    /// When the earliest armed timeout falls due (deadline plus the reply grace): what the
    /// supervision tree's timer sleeps until before [`AttemptSaga::expire_due`].
    #[must_use]
    pub fn next_due(&self) -> Option<Millis> {
        self.lock().values().map(|armed| self.due(armed)).min()
    }

    /// Issues `RecordDirectiveTimeout` for every armed (sent) directive whose deadline plus the
    /// reply grace has passed by the control plane's clock. The aggregate makes a still-pending
    /// directive `Unknown` (never a success, I10), and the timer is disarmed; one already
    /// answered is moot and disarmed too. Anything else ([`timed_out`]) keeps the timer, due
    /// again one reply grace later. Pinned by
    /// `tests/store/saga.rs::should_make_the_outcome_unknown_when_the_deadline_passes_unanswered`.
    pub async fn expire_due(&self) -> TimeoutReport {
        let now = self.store.clock().now();
        let due: Vec<(StreamId, Armed)> = self
            .lock()
            .iter()
            .filter(|(_, armed)| self.due(armed) <= now)
            .map(|(stream, armed)| (stream.clone(), armed.clone()))
            .collect();
        let mut report = TimeoutReport::default();
        for (stream, armed) in due {
            let body = AttemptCommand::RecordDirectiveTimeout {
                directive_id: armed.directive_id,
            };
            let outcome = match self
                .command(
                    &armed.instance,
                    &armed.host,
                    timeout_of(armed.directive_id),
                    body,
                )
                .await
            {
                Ok(saved) => timed_out(saved),
                Err(code) => Err(code),
            };
            match outcome {
                Ok(true) => report.unknown.push(armed.directive_id),
                Ok(false) => report.moot.push(armed.directive_id),
                Err(code) => {
                    report.failed.push((armed.directive_id, code));
                    let retry = Deadline::after(now, self.config.reply_grace.max(Millis::new(1)));
                    if let Some(kept) = self
                        .lock()
                        .get_mut(&stream)
                        .filter(|kept| kept.directive_id == armed.directive_id)
                    {
                        kept.retry_at = Some(retry.instant());
                    }
                    continue;
                }
            }
            let mut timers = self.lock();
            if timers.get(&stream) == Some(&armed) {
                timers.remove(&stream);
            }
        }
        report
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<StreamId, Armed>> {
        self.timers.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn due(&self, armed: &Armed) -> Millis {
        let due = Deadline::after(armed.deadline.instant(), self.config.reply_grace).instant();
        armed.retry_at.map_or(due, |retry| due.max(retry))
    }

    fn arm(&self, stream: &StreamId, instance: &str, state: &AttemptState, step: &SagaStep) {
        let mut timers = self.lock();
        match (step, &state.identity) {
            (
                SagaStep::AwaitAnswer {
                    directive_id,
                    deadline,
                },
                Some(identity),
            ) => {
                // the same directive keeps its retry time, so a failed timeout is tried again
                // one reply grace later however many of its stream's events arrive meanwhile
                let retry_at = timers
                    .get(stream)
                    .filter(|kept| kept.directive_id == *directive_id)
                    .and_then(|kept| kept.retry_at);
                timers.insert(
                    stream.clone(),
                    Armed {
                        instance: instance.to_owned(),
                        host: identity.host_id.clone(),
                        directive_id: *directive_id,
                        deadline: *deadline,
                        retry_at,
                    },
                );
            }
            (SagaStep::AwaitAnswer { .. }, None)
            | (SagaStep::Issue(..) | SagaStep::Close | SagaStep::Idle, _) => {
                timers.remove(stream);
            }
        }
    }

    /// Saves `body` for attempt `instance` under its host's session epoch, issued now.
    async fn command(
        &self,
        instance: &str,
        host: &HostId,
        command_id: CommandId,
        body: AttemptCommand,
    ) -> Result<Result<Outcome<AttemptRejection>, SaveError>, &'static str> {
        let session = self.sessions.get(host).ok_or("no_session")?;
        let command = Command::new(
            command_id,
            self.store.clock().now(),
            session.epoch,
            Actor::Automation { policy: SAGA },
            body,
        );
        Ok(self
            .store
            .execute::<Attempt>(SaveRequest::new(instance, command))
            .await)
    }
}

impl<J: Journal, C: Clock, R: Rng, N: StoreNotifier> Consumer for AttemptSaga<'_, J, C, R, N> {
    const NAME: &'static str = SAGA;

    /// Reads the attempt the event belongs to and acts on its current state: issues the next
    /// directive or closes the stream under a command derived from the event's id, or arms or
    /// disarms the deadline timer. Events of other streams pass. Pinned by
    /// `tests/store/saga.rs::should_issue_the_next_directive_when_a_stage_completes`.
    async fn handle(&self, delivery: Delivery<'_>) -> Result<(), ConsumerError> {
        let event = delivery.event;
        let Some(instance) = event.stream.as_str().strip_prefix("attempt-") else {
            return Ok(());
        };
        let state = self
            .store
            .load::<Attempt>(instance)
            .await
            .map_err(|_| ConsumerError::new("attempt_unreadable"))?;
        let step = next_step(&state);
        self.arm(&event.stream, instance, &state, &step);
        let Some(identity) = &state.identity else {
            return Ok(());
        };
        let command_id = command_for_event(event.event_id);
        let body = match step {
            SagaStep::Issue(kind) => AttemptCommand::IssueNext {
                directive_id: directive_of(command_id),
                deadline: Deadline::after(
                    self.store.clock().now(),
                    self.config.timeouts.timeout(kind.tag()),
                ),
            },
            SagaStep::Close => AttemptCommand::Close,
            SagaStep::AwaitAnswer { .. } | SagaStep::Idle => return Ok(()),
        };
        let saved = self
            .command(instance, &identity.host_id, command_id, body)
            .await
            .map_err(ConsumerError::new)?;
        decided(saved).map(|_| ()).map_err(ConsumerError::new)
    }
}
