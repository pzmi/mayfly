//! The scale-set listener (plan §7.2, `doc/github-scaleset.md` §3–§6 and §9): one pool's scale
//! set and its message session, driven one [`Listener::tick`] at a time, and the job facts its
//! messages carry turned into registration and attempt commands ([`JobFacts`], policy P12).
//!
//! The listener follows `formal/scaleset.qnt`'s listener actions one to one: `recreateScaleSet`
//! and ensure, `deleteStale`, `openSession` (with its 409 and lost answer), `poll`, `record`,
//! `ack` and `acquireCall`, `sessionLoss` and `restart`. It decides nothing itself: every fact is
//! a `ScaleSet` command, decided by the aggregate, and the listener acts on what was journaled.
//!
//! - **Record, then ack** (I12): a polled message becomes one `RecordMessage` unit consuming the
//!   inbox key `{gh-<session>, <message id>}` under the command id `(session, message)`, and the
//!   message is acked only once a session of the stream recorded it: the save accepted it (or its
//!   receipt answered), or it is the cursor's duplicate. A refused message is never acked (the
//!   model's `ack` requires the record; acking it would be its `ackBeforeRecord` mutant): its
//!   refusal consumed no key (ADR 0041 rule 6), so GitHub delivers it again and the next session
//!   records it; a `stale_run` refusal means a newer run owns the stream, and the listener stops
//!   ([`ListenerError::Fenced`]). A failed save acks nothing either. Pinned by
//!   `tests/store/listener.rs::should_ack_only_after_the_record_commits_when_the_store_fails`,
//!   `should_leave_a_refused_message_unacked_and_record_it_in_the_next_session_when_the_session_was_lost`
//!   and `should_stop_fenced_when_a_newer_run_owns_the_stream`.
//! - **Duplicates** (KER-06G): a message is a `DuplicateMessage`, acked without a write, only when
//!   its key was consumed **and** its id is the cursor's ([`ScaleSetState::is_duplicate`]); a
//!   consumed key for any other id is a reused id, recorded anew under a command id and key
//!   derived with the stream's head, so the aggregate sees it (it applies nothing and degrades
//!   the session) instead of a receipt dropping it silently. Pinned by
//!   `should_ack_without_a_write_when_the_cursor_message_comes_again` and
//!   `should_record_a_reused_id_anew_and_degrade_when_its_receipt_exists`.
//! - **A message without statistics** (GH-03 review) is recorded without them (an older count
//!   is never replaced by none) and counted as protocol drift, as an unknown type is: the
//!   listener adds one [`MessageItem::Unknown`] to its batch, so the aggregate's drift count and
//!   threshold apply (plan §7.2 step 6). Pinned by
//!   `should_count_drift_and_keep_the_statistics_when_a_message_has_none`.
//! - **The wire max** (KER-06G): each poll sends [`ScaleSetState::wire_max`] of the stream as
//!   loaded right before it (the capacity input's reported max, pinned at the assigned count while
//!   a refusal sink is up), never the snapshot `CapacityApplied.wire_max`; pause, drain and safe
//!   mode change it without closing the session (ADR 0029). Pinned by
//!   `should_send_the_streams_wire_max_when_a_sink_pins_it` and
//!   `should_keep_the_session_and_send_the_mode_wire_max_when_paused_draining_or_in_safe_mode`.
//! - **Acquire** (GHES, I23, I32): exactly the `acquire` list of the `MessageRecorded` the save
//!   journaled (planned by `PlanAcquire` inside the unit, before any call), after the ack; the
//!   answer is `RecordAcquireResult`. A lost answer leaves the ids pending (A4): the next tick
//!   asks again for the stream's pending set, only while the pool is open (the model's
//!   `acquireCall`) and no failed acquire holds (its own backoff, which an answered poll does not
//!   end), and GitHub answers a repeat for a job it already gave as acquired. An acquire answered
//!   400 is a definite refusal of the batch. Pinned by
//!   `should_acquire_again_and_serve_the_job_once_when_the_acquire_answer_was_lost` (F27).
//! - **Empty poll** (plan §7.2 step 5): a 202 still reconciles: the listener kicks its scale-set
//!   stream, so the reconciler's next tick passes (pinned by
//!   `should_kick_the_reconciler_when_a_poll_is_empty`).
//! - **Sessions** (spec §3): a journaled stale session is deleted before a create (§3.2, A3);
//!   a create answers `RecordSessionOpened` with its initial statistics, a 409
//!   `RecordSessionLoss{conflicted}`, a 404 `RecordScaleSetVanished` (the next tick past the
//!   backoff re-creates the set: `RecordScaleSetRecreated`), anything else `RecordSessionLoss{failed}`, a
//!   `ProtocolDrift` included (the client already deleted the session it could not use: nothing
//!   about it is journaled; a create whose answer the journal then fails to record deletes that
//!   session, best effort, A3). A poll answered
//!   `SessionExpired` is `RecordSessionLoss{expired}`; `NoSession` means this process never
//!   opened the journaled session, a restart: `RecordSessionLoss{restarted}`, deleted before the
//!   next create (F42). A poll that fails without a definite answer (a dropped connection, a
//!   throttle the client did not outlast) keeps the session (F08, F25). Pinned
//!   by `should_delete_the_journaled_session_and_reopen_when_the_daemon_restarted`,
//!   `should_back_off_and_open_after_the_wait_when_a_create_answers_conflict`,
//!   `should_recreate_the_scale_set_when_a_create_answers_gone` and
//!   `should_journal_no_session_when_the_create_answer_is_unusable`.
//! - **Backoff** (spec §8, plan §7.2): every failed GitHub call (ensure, a stale delete, a create,
//!   a poll without an answer, an ack, an acquire, a close) holds every later call for
//!   [`ListenerConfig::call_backoff`], doubling per failure in a row to
//!   [`ListenerConfig::call_backoff_cap`]; a tick then answers [`Tick::Waiting`] with the instant,
//!   the driver's only sleep. Only a poll that answers ends the run of failures, so a session that
//!   opens and fails at once is reopened at most once per backoff. A session found expired, or
//!   never opened by this process (a restart), reopens at once the first time. The port carries no
//!   `Retry-After` past the client's own deadline, so a longer throttle is waited out by this
//!   backoff; and the aggregate has no degraded cause for repeated call failures, so they are
//!   backed off, never degraded. Pinned by
//!   `should_wait_before_polling_again_when_a_poll_is_throttled`,
//!   `should_wait_before_ensuring_again_when_the_credentials_fail` and
//!   `should_reopen_at_most_once_per_backoff_when_every_poll_is_refused`.
//! - **The gap** (spec §3.4): `totalAssignedJobs − totalRunningJobs` beyond the jobs the stream
//!   knows of (pending, trusted, refused, acquiring, acquired) is reported on every poll
//!   ([`Polled::gap`]); one that stays above zero for [`ListenerConfig::gap_alarm_after`] is
//!   logged once at WARN (`assigned_jobs_gap_persists`), an operator alarm, never a reason to
//!   delete the scale set (ADR 0040). Pinned by `should_alarm_once_when_the_assigned_gap_persists`.
//! - **Never blocks on VM work** (spec §9): a tick makes GitHub calls and saves only; the effects
//!   that follow (VMs, JIT) are other consumers' and directives.
//!
//! The listener keeps in memory only what a restart may lose: the message whose ack is pending
//! (the cursor's message is acked again on a fresh listener's first poll of a session, since
//! the journal shows it recorded), the call backoff and the gap's start. The cursor and the
//! session id are journaled (spec §3.2, §9): a restarted listener polls with `lastMessageId` =
//! the stream's cursor.
//!
//! Left to other cards: the classification of assigned jobs through run lookups (GH-08, GH-10),
//! the listing of `acquirablejobs` when a GHES session opens (GH-11, spec §6.1), P12's grace for a
//! job that ended while its runner never reports its exit (GH-12), and the gap's metric (RT-04).
//! The daemon's driver and its local lease are `mayfly_composition::github` (RT-07).

mod jobs;

pub use jobs::{JOB_FACTS, JobFacts, JobFactsError, JobFactsReport};

use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard, PoisonError};

use mayfly_control_domain::demand::WireMode;
use mayfly_control_domain::pool::TrustTier;
use mayfly_control_domain::scaleset::{
    EnsureScaleSet, LossCause, Phase, RecordMessage, ScaleSet, ScaleSetCommand, ScaleSetEvent,
    ScaleSetRejection, ScaleSetState,
};
use mayfly_control_domain::{Actor, Command, Rejection};
use mayfly_domain::{CommandId, Epoch, Millis, PoolName};

use crate::delivery::Kicks;
use crate::github::{
    Failure, GitHubCallError, MessageId, MessageItem, PolledMessage, RunnerRequestId, ScaleSetApi,
    ScaleSetId, ScaleSetSpec, SessionId,
};
use crate::ids::derive;
use crate::journal::{
    Appended, InboxKey, InboxSource, Journal, JournalValueError, MessageId as InboxMessageId,
    StreamRevision,
};
use crate::store::{
    AggregateStore, Clock, Outcome, Rng, SaveError, SaveRequest, StoreNotifier, decode_event,
    stream_of,
};

/// The listener's name: the actor of its commands.
pub const LISTENER: &str = "scale_set_listener";

const ID_DOMAIN: &[u8] = b"mayfly-scale-set-listener-v1\n";

/// One pool's listener settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenerConfig {
    /// The pool (the stream is `scaleset-<pool>`).
    pub pool: PoolName,
    /// The scale set ensured for it (ADR 0040: found by name in its group, or created).
    pub spec: ScaleSetSpec,
    /// The pool's tier (I32).
    pub tier: TrustTier,
    /// The session's owner name: the host and pool, never a secret (spec §3.1).
    pub owner: String,
    /// The run epoch the listener issues under (I18).
    pub run_epoch: Epoch,
    /// The wait after a failed GitHub call (spec §3.1, §8: near 30 s), doubled per failure in a
    /// row until a poll answers.
    pub call_backoff: Millis,
    /// The longest wait between calls.
    pub call_backoff_cap: Millis,
    /// How long the assigned-running gap stays above zero before it is alarmed (spec §3.4).
    pub gap_alarm_after: Millis,
}

impl ListenerConfig {
    /// The defaults: a 30 s call backoff doubling to 5 min, the gap alarmed after 15 min.
    #[must_use]
    pub const fn new(
        pool: PoolName,
        spec: ScaleSetSpec,
        tier: TrustTier,
        owner: String,
        run_epoch: Epoch,
    ) -> Self {
        Self {
            pool,
            spec,
            tier,
            owner,
            run_epoch,
            call_backoff: Millis::new(30_000),
            call_backoff_cap: Millis::new(300_000),
            gap_alarm_after: Millis::new(900_000),
        }
    }
}

/// How a polled message was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// The unit committed, or its receipt answered (`replayed`); `anew` when the id's key was
    /// consumed already and the message was recorded under a derived command id (a reused id).
    Accepted {
        /// Answered from the receipt: nothing written now.
        replayed: bool,
        /// Recorded anew under a derived command id.
        anew: bool,
    },
    /// The cursor's message again, its key consumed: acked without a write.
    Duplicate,
    /// The aggregate refused it (its code): no session of this stream recorded it, so it is not
    /// acked and comes again (the model's `ack` requires the record).
    Refused(&'static str),
}

/// What one poll did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Polled {
    /// The message polled; `None` for a 202 or a failed poll.
    pub message: Option<MessageId>,
    /// How it was recorded.
    pub recorded: Option<Recorded>,
    /// Whether it was acked.
    pub acked: bool,
    /// The ids this poll's message planned to acquire and asked for.
    pub acquire: Vec<RunnerRequestId>,
    /// What the acquire answered, when it answered.
    pub acquired: Option<BTreeSet<RunnerRequestId>>,
    /// Why the poll answered nothing definite (a failure class), the session kept.
    pub poll_failure: Option<&'static str>,
    /// `totalAssignedJobs − totalRunningJobs` beyond the jobs the stream knows of (spec §3.4).
    pub gap: u32,
}

/// What one [`Listener::tick`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tick {
    /// The scale set was ensured (found or created) or re-created, under this id.
    Ensured(ScaleSetId),
    /// The journaled stale session was deleted.
    StaleDeleted(SessionId),
    /// A session opened.
    Opened(SessionId),
    /// A create did not open a session (`session_conflict`, `scale_set_gone` or the failure's
    /// class); the next create waits.
    OpenFailed(&'static str),
    /// Nothing to do before this instant (an open backoff).
    Waiting(Millis),
    /// One poll.
    Polled(Polled),
    /// The session was lost, and why.
    SessionLost(LossCause),
    /// The session was deleted in order.
    Closed,
    /// A GitHub call outside the poll failed without changing anything (its class).
    Failed(&'static str),
}

/// Why a tick could not go on: the journal, or a command the listener needs refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenerError {
    /// The store could not read or commit.
    Store(SaveError),
    /// The aggregate refused a command the listener cannot go on without (its code).
    Refused(&'static str),
    /// A stream or inbox name was invalid (a programming error).
    Name(JournalValueError),
    /// The stream accepted a newer run epoch (I18): this listener is fenced and must stop.
    Fenced,
}

impl std::fmt::Display for ListenerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "store: {e}"),
            Self::Refused(code) => write!(f, "refused: {code}"),
            Self::Name(e) => write!(f, "name: {e}"),
            Self::Fenced => f.write_str("fenced: a newer run owns the scale set"),
        }
    }
}

impl std::error::Error for ListenerError {}

impl From<SaveError> for ListenerError {
    fn from(error: SaveError) -> Self {
        Self::Store(error)
    }
}

impl From<JournalValueError> for ListenerError {
    fn from(error: JournalValueError) -> Self {
        Self::Name(error)
    }
}

/// What a restart may lose (module docs).
#[derive(Debug, Default)]
struct Local {
    /// The session this listener last polled in.
    session: Option<SessionId>,
    /// A recorded message whose ack did not answer.
    unacked: Option<MessageId>,
    /// GitHub calls failed in a row since a poll last answered.
    failures: u32,
    /// No GitHub call before this instant.
    next_call: Option<Millis>,
    /// Acquires failed in a row since one was answered and recorded.
    acquire_failures: u32,
    /// No acquire before this instant.
    next_acquire: Option<Millis>,
    /// When the gap was first seen above zero, in this episode.
    gap_since: Option<Millis>,
    /// Whether this episode was alarmed.
    gap_alarmed: bool,
}

/// One pool's listener over `store` and the scale-set port `api`.
pub struct Listener<'s, J, C, R, N, A> {
    store: &'s AggregateStore<J, C, R, N>,
    api: &'s A,
    config: ListenerConfig,
    kicks: Option<Kicks>,
    local: Mutex<Local>,
}

impl<J, C, R, N, A> std::fmt::Debug for Listener<'_, J, C, R, N, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener")
            .field("pool", &self.config.pool)
            .finish_non_exhaustive()
    }
}

/// The inbox key of `message` polled in `session`; `anew` names the stream revision a reused
/// id is recorded at.
///
/// # Errors
/// Never in practice: the source is `gh-` and 32 hex digits, the id decimal.
pub fn inbox_key(
    session: SessionId,
    message: MessageId,
    anew: Option<u64>,
) -> Result<InboxKey, JournalValueError> {
    let id = match anew {
        None => message.0.to_string(),
        Some(revision) => format!("{}/{revision}", message.0),
    };
    Ok(InboxKey {
        source: InboxSource::new(&format!("gh-{:032x}", session.as_u128()))?,
        message_id: InboxMessageId::new(&id)?,
    })
}

/// The command id of `RecordMessage` for `message` polled in `session`: `(session, message)`.
#[must_use]
pub fn message_command_id(session: SessionId, message: MessageId) -> CommandId {
    CommandId::from_u128(derive(
        ID_DOMAIN,
        &[
            b"message",
            &session.as_u128().to_be_bytes(),
            &message.0.to_be_bytes(),
        ],
    ))
}

fn revision_bytes(head: Option<StreamRevision>) -> [u8; 8] {
    head.map_or(u64::MAX, StreamRevision::get).to_be_bytes()
}

impl<'s, J: Journal, C: Clock, R: Rng, N: StoreNotifier, A: ScaleSetApi>
    Listener<'s, J, C, R, N, A>
{
    /// The listener of `config.pool` over `store` and `api`.
    pub fn new(store: &'s AggregateStore<J, C, R, N>, api: &'s A, config: ListenerConfig) -> Self {
        Self {
            store,
            api,
            config,
            kicks: None,
            local: Mutex::new(Local::default()),
        }
    }

    /// Kicks the scale-set stream in `kicks` on an empty poll, so the reconciler passes (plan
    /// §7.2 step 5).
    #[must_use]
    pub fn with_kicks(mut self, kicks: Kicks) -> Self {
        self.kicks = Some(kicks);
        self
    }

    fn local(&self) -> MutexGuard<'_, Local> {
        self.local.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts a failed GitHub call and, unless it is the first failure since a poll answered and
    /// `wait_first` is off, holds every call until [`ListenerConfig::call_backoff`] doubled per
    /// failure in a row, at most [`ListenerConfig::call_backoff_cap`]. Pinned by
    /// `tests/store/listener.rs::should_wait_before_polling_again_when_a_poll_is_throttled`.
    fn back_off(&self, wait_first: bool) -> Millis {
        let now = self.store.clock().now();
        let mut local = self.local();
        let n = local.failures;
        local.failures = n.saturating_add(1);
        if n == 0 && !wait_first {
            return now;
        }
        let wait = self
            .config
            .call_backoff
            .get()
            .saturating_mul(1_u64 << n.min(16))
            .min(self.config.call_backoff_cap.get());
        let until = Millis::new(now.get().saturating_add(wait));
        local.next_call = Some(until);
        until
    }

    /// Whether a failed acquire still holds the next one.
    fn acquire_held(&self) -> bool {
        let now = self.store.clock().now();
        self.local().next_acquire.is_some_and(|until| until > now)
    }

    /// Holds acquires after a failed one, on their own count: a poll that answers does not end
    /// it (a throttled acquire must not be asked again on every message of a backlog), only a
    /// recorded acquire answer does. [`ListenerConfig::call_backoff`] doubled per failure in a
    /// row, at most [`ListenerConfig::call_backoff_cap`]. Pinned by
    /// `tests/store/listener.rs::should_ask_a_throttled_acquire_at_most_once_per_backoff_when_messages_keep_coming`.
    fn hold_acquire(&self) {
        let now = self.store.clock().now();
        let mut local = self.local();
        let n = local.acquire_failures;
        local.acquire_failures = n.saturating_add(1);
        let wait = self
            .config
            .call_backoff
            .get()
            .saturating_mul(1_u64 << n.min(16))
            .min(self.config.call_backoff_cap.get());
        local.next_acquire = Some(Millis::new(now.get().saturating_add(wait)));
    }

    /// A poll answered: the failures in a row end.
    fn answered(&self) {
        let mut local = self.local();
        local.failures = 0;
        local.next_call = None;
    }

    async fn load(&self) -> Result<(ScaleSetState, Option<StreamRevision>), ListenerError> {
        Ok(self
            .store
            .load_with_head::<ScaleSet>(self.config.pool.as_str())
            .await?)
    }

    fn command_id(&self, tag: &[u8], parts: &[&[u8]]) -> CommandId {
        let pool = self.config.pool.as_str().as_bytes();
        let mut all: Vec<&[u8]> = vec![tag, pool];
        all.extend_from_slice(parts);
        CommandId::from_u128(derive(ID_DOMAIN, &all))
    }

    async fn save(
        &self,
        id: CommandId,
        body: ScaleSetCommand,
        inbox: Option<InboxKey>,
    ) -> Result<Outcome<ScaleSetRejection>, SaveError> {
        let command = Command::new(
            id,
            self.store.clock().now(),
            self.config.run_epoch,
            Actor::Automation { policy: LISTENER },
            body,
        );
        let mut request = SaveRequest::new(self.config.pool.as_str(), command);
        if let Some(key) = inbox {
            request = request.with_inbox(key);
        }
        self.store.execute::<ScaleSet>(request).await
    }

    /// Saves a command the listener needs accepted; a refusal is an error naming its code.
    async fn require(&self, id: CommandId, body: ScaleSetCommand) -> Result<(), ListenerError> {
        match self.save(id, body, None).await? {
            Outcome::Accepted { .. } => Ok(()),
            Outcome::Refused {
                rejection: ScaleSetRejection::StaleRun,
                ..
            } => Err(ListenerError::Fenced),
            Outcome::Refused { rejection, .. } => Err(ListenerError::Refused(rejection.code())),
        }
    }

    /// One step of the session's life (module docs): ensure, delete a stale session, open,
    /// wait, poll once, or finish a close.
    ///
    /// # Errors
    /// [`ListenerError`] when the journal fails or a needed command is refused; nothing was
    /// acked then.
    pub async fn tick(&self) -> Result<Tick, ListenerError> {
        let now = self.store.clock().now();
        if let Some(until) = self.local().next_call.filter(|until| *until > now) {
            return Ok(Tick::Waiting(until));
        }
        let (state, head) = self.load().await?;
        match state.phase {
            Phase::Unconfigured => self.ensure(&state, head).await,
            Phase::Active | Phase::Degraded(_) => self.poll_step(&state).await,
            Phase::Closing => self.finish_close(&state).await,
            Phase::Ensured | Phase::Opening | Phase::Backoff(_) | Phase::Closed => {
                self.open(&state, head).await
            }
        }
    }

    /// The scale set found by name or created, journaled (`EnsureScaleSet`), or, when the
    /// journaled one vanished, re-created under its new id (`RecordScaleSetRecreated`; ADR 0040:
    /// mayfly re-creates its own set, never deletes one).
    async fn ensure(
        &self,
        state: &ScaleSetState,
        head: Option<StreamRevision>,
    ) -> Result<Tick, ListenerError> {
        let found = match self.api.find_scale_set(&self.config.spec).await {
            Ok(found) => found,
            Err(e) => return Ok(self.call_failed(&e)),
        };
        let info = match found {
            Some(info) => info,
            None => match self.api.create_scale_set(&self.config.spec).await {
                Ok(info) => info,
                Err(e) => return Ok(self.call_failed(&e)),
            },
        };
        let id_bytes = info.id.0.to_be_bytes();
        if state.vanished() {
            let id = self.command_id(b"recreated", &[&id_bytes, &revision_bytes(head)]);
            self.require(
                id,
                ScaleSetCommand::RecordScaleSetRecreated { github_id: info.id },
            )
            .await?;
        } else {
            let id = self.command_id(b"ensure", &[&id_bytes]);
            let body = ScaleSetCommand::EnsureScaleSet(EnsureScaleSet {
                pool: self.config.pool.clone(),
                github_id: info.id,
                tier: self.config.tier,
            });
            self.require(id, body).await?;
        }
        Ok(Tick::Ensured(info.id))
    }

    /// Deletes a journaled stale session, then opens one (`BeginOpen`, the create, its answer),
    /// once the backoff of the last failed open passed.
    async fn open(
        &self,
        state: &ScaleSetState,
        head: Option<StreamRevision>,
    ) -> Result<Tick, ListenerError> {
        let Some(set) = state.github_id else {
            return Err(ListenerError::Refused(ScaleSetRejection::NotEnsured.code()));
        };
        if let Some(stale) = state.stale_session {
            return match self.api.delete_session(set, stale).await {
                Ok(()) => {
                    let id = self.command_id(b"deleted", &[&stale.as_u128().to_be_bytes()]);
                    self.require(
                        id,
                        ScaleSetCommand::RecordSessionDeleted { session_id: stale },
                    )
                    .await?;
                    Ok(Tick::StaleDeleted(stale))
                }
                Err(e) => Ok(self.call_failed(&e)),
            };
        }
        if state.phase != Phase::Opening {
            let id = self.command_id(b"begin_open", &[&revision_bytes(head)]);
            self.require(id, ScaleSetCommand::BeginOpen).await?;
        }
        let answer = self.api.create_session(set, &self.config.owner).await;
        let (_, head) = self.load().await?;
        let failed = match answer {
            Ok(opened) => {
                let session = opened.session_id;
                let id = self.command_id(b"opened", &[&session.as_u128().to_be_bytes()]);
                let body = ScaleSetCommand::RecordSessionOpened {
                    session_id: session,
                    statistics: opened.statistics,
                };
                if let Err(error) = self.require(id, body).await {
                    // A3: mayfly made this session and journaled nothing of it; delete it, best
                    // effort, so it does not block the next create (a 409 there backs off anyway)
                    if self.api.delete_session(set, session).await.is_err() {
                        tracing::warn!(error = %error, "unrecorded_session_not_deleted");
                    }
                    return Err(error);
                }
                // the failures in a row end only once a poll answers (a session that opens and
                // fails at once must not reopen at once)
                return Ok(Tick::Opened(session));
            }
            Err(GitHubCallError {
                failure: Failure::ScaleSetGone,
                ..
            }) => {
                let id =
                    self.command_id(b"vanished", &[&set.0.to_be_bytes(), &revision_bytes(head)]);
                self.require(
                    id,
                    ScaleSetCommand::RecordScaleSetVanished { github_id: set },
                )
                .await?;
                self.back_off(true);
                return Ok(Tick::OpenFailed(Failure::ScaleSetGone.as_str()));
            }
            Err(e) => e.failure,
        };
        let cause = match failed {
            Failure::SessionConflict => LossCause::Conflicted,
            Failure::SessionExpired
            | Failure::NoSession
            | Failure::ScaleSetGone
            | Failure::NameTaken
            | Failure::Throttled
            | Failure::Refused { .. }
            | Failure::Credentials
            | Failure::Unavailable
            | Failure::ProtocolDrift => LossCause::Failed,
        };
        let id = self.command_id(b"open_lost", &[&revision_bytes(head)]);
        self.require(id, ScaleSetCommand::RecordSessionLoss { cause })
            .await?;
        self.back_off(true);
        Ok(Tick::OpenFailed(failed.as_str()))
    }

    /// A GitHub call outside the poll failed: nothing changed, every call waits.
    fn call_failed(&self, error: &GitHubCallError) -> Tick {
        self.back_off(true);
        Tick::Failed(error.failure.as_str())
    }

    /// Records the loss of the live session, under an id derived from the head it is decided at.
    /// A session found expired or never opened by this process reopens at once the first time;
    /// any other loss, or a loss again before a poll answered, waits (a session that opens and
    /// fails each time is retried at most once per backoff).
    async fn lose(&self, cause: LossCause) -> Result<Tick, ListenerError> {
        self.back_off(!matches!(cause, LossCause::Expired | LossCause::Restarted));
        let (_, head) = self.load().await?;
        let id = self.command_id(b"lost", &[&revision_bytes(head)]);
        self.require(id, ScaleSetCommand::RecordSessionLoss { cause })
            .await?;
        let mut local = self.local();
        local.session = None;
        local.unacked = None;
        Ok(Tick::SessionLost(cause))
    }

    /// One poll of a live session: a pending ack and pending acquisitions first, then the long
    /// poll, the record, the ack and the acquire (module docs).
    async fn poll_step(&self, state: &ScaleSetState) -> Result<Tick, ListenerError> {
        let (Some(set), Some(session)) = (state.github_id, state.session) else {
            return Err(ListenerError::Refused(ScaleSetRejection::NoSession.code()));
        };
        self.adopt_session(state, session).await?;
        if let Some(lost) = self.retry_ack(set, session).await {
            return self.lose(lost).await;
        }
        if self.local().unacked.is_some() {
            // the ack did not answer: it backed off, and no poll goes before it is asked again
            let until = self
                .local()
                .next_call
                .unwrap_or_else(|| self.store.clock().now());
            return Ok(Tick::Waiting(until));
        }
        let open = state.capacity.mode == WireMode::Open;
        if open && !state.acquiring.is_empty() && !self.acquire_held() {
            let pending: Vec<RunnerRequestId> = state.acquiring.iter().copied().collect();
            if let Err(lost) = self.acquire(set, &pending).await {
                return self.lose(lost).await;
            }
        }
        let (state, head) = self.load().await?;
        let mut polled = Polled::default();
        let answer = self.api.poll(set, state.cursor, state.wire_max()).await;
        if answer.is_ok() {
            self.answered();
        }
        let message = match answer {
            Ok(Some(message)) => message,
            Ok(None) => {
                if let Some(kicks) = &self.kicks {
                    kicks.kick(&stream_of::<ScaleSet>(self.config.pool.as_str())?);
                }
                polled.gap = self.check_gap(&state);
                return Ok(Tick::Polled(polled));
            }
            Err(e) => {
                return match e.failure {
                    Failure::SessionExpired => self.lose(LossCause::Expired).await,
                    Failure::NoSession => self.lose(LossCause::Restarted).await,
                    Failure::Unavailable | Failure::Throttled => {
                        // no definite answer, nothing to record: the session is kept and the next
                        // poll waits (a throttle the client could not outlast within its deadline
                        // must not turn into a poll per tick)
                        self.back_off(true);
                        polled.poll_failure = Some(e.failure.as_str());
                        Ok(Tick::Polled(polled))
                    }
                    Failure::SessionConflict
                    | Failure::ScaleSetGone
                    | Failure::NameTaken
                    | Failure::Refused { .. }
                    | Failure::Credentials
                    | Failure::ProtocolDrift => self.lose(LossCause::Failed).await,
                };
            }
        };
        polled.message = Some(message.message_id);
        let message_id = message.message_id;
        let (recorded, acquire) = self.record(&state, head, session, message).await?;
        polled.recorded = Some(recorded);
        if let Recorded::Refused(code) = recorded {
            // no session of this stream recorded it: never acked (the model's `ack` requires the
            // record), so GitHub delivers it again, to the next session if this one is gone
            if code == ScaleSetRejection::StaleRun.code() {
                return Err(ListenerError::Fenced);
            }
            // a refusal no longer stops the next poll asking for it again: hold the calls
            self.back_off(true);
            return Ok(Tick::Polled(polled));
        }
        self.local().unacked = Some(message_id);
        if let Some(lost) = self.retry_ack(set, session).await {
            return self.lose(lost).await;
        }
        polled.acked = self.local().unacked.is_none();
        // while a failed acquire holds, the new plan stays pending and is asked with the rest
        if !acquire.is_empty() && !self.acquire_held() {
            polled.acquire.clone_from(&acquire);
            match self.acquire(set, &acquire).await {
                Ok(answer) => polled.acquired = answer,
                Err(lost) => return self.lose(lost).await,
            }
        }
        let (state, _) = self.load().await?;
        polled.gap = self.check_gap(&state);
        Ok(Tick::Polled(polled))
    }

    /// On the first poll of a session by this listener: the cursor's message, recorded in this
    /// session, may have lost its ack in a crash; it is acked again (a 404 counts as done).
    async fn adopt_session(
        &self,
        state: &ScaleSetState,
        session: SessionId,
    ) -> Result<(), ListenerError> {
        if self.local().session == Some(session) {
            return Ok(());
        }
        let mut unacked = None;
        if let Some(cursor) = state.cursor {
            let key = inbox_key(session, cursor, None)?;
            let recorded = self
                .store
                .journal()
                .inbox_contains(&key)
                .await
                .map_err(SaveError::Storage)?;
            unacked = recorded.then_some(cursor);
        }
        let mut local = self.local();
        local.session = Some(session);
        local.unacked = unacked;
        Ok(())
    }

    /// Acks the pending message, if any: `Some(cause)` when the session is gone.
    async fn retry_ack(&self, set: ScaleSetId, session: SessionId) -> Option<LossCause> {
        let message = self.local().unacked?;
        match self.api.ack(set, message).await {
            Ok(()) => {
                let mut local = self.local();
                if local.session == Some(session) && local.unacked == Some(message) {
                    local.unacked = None;
                }
                None
            }
            Err(e) => match e.failure {
                Failure::SessionExpired => Some(LossCause::Expired),
                Failure::NoSession => Some(LossCause::Restarted),
                Failure::SessionConflict
                | Failure::ScaleSetGone
                | Failure::NameTaken
                | Failure::Throttled
                | Failure::Refused { .. }
                | Failure::Credentials
                | Failure::Unavailable
                | Failure::ProtocolDrift => {
                    tracing::warn!(failure = e.failure.as_str(), "scale_set_ack_failed");
                    self.back_off(true);
                    None
                }
            },
        }
    }

    /// `RecordMessage` in one unit with its inbox key (module docs): how it was recorded and the
    /// acquire list its `MessageRecorded` journaled.
    async fn record(
        &self,
        state: &ScaleSetState,
        head: Option<StreamRevision>,
        session: SessionId,
        message: PolledMessage,
    ) -> Result<(Recorded, Vec<RunnerRequestId>), ListenerError> {
        let id = message.message_id;
        let key = inbox_key(session, id, None)?;
        let consumed = self
            .store
            .journal()
            .inbox_contains(&key)
            .await
            .map_err(SaveError::Storage)?;
        if consumed && state.is_duplicate(id) {
            return Ok((Recorded::Duplicate, Vec::new()));
        }
        let (command_id, key) = if consumed {
            let revision = head.map_or(u64::MAX, StreamRevision::get);
            let command_id = CommandId::from_u128(derive(
                ID_DOMAIN,
                &[
                    b"message_anew",
                    &session.as_u128().to_be_bytes(),
                    &id.0.to_be_bytes(),
                    &revision.to_be_bytes(),
                ],
            ));
            (command_id, inbox_key(session, id, Some(revision))?)
        } else {
            (message_command_id(session, id), key)
        };
        let mut items = message.items;
        if message.statistics.is_none() {
            // an envelope without its counts is protocol drift, counted as an unknown type is
            items.push(MessageItem::Unknown);
        }
        let body = ScaleSetCommand::RecordMessage(RecordMessage {
            session_id: session,
            message_id: id,
            statistics: message.statistics,
            items,
        });
        match self.save(command_id, body, Some(key)).await {
            Ok(Outcome::Accepted { appended, replayed }) => {
                let acquire = self.journaled_acquire(&appended).await?;
                Ok((
                    Recorded::Accepted {
                        replayed,
                        anew: consumed,
                    },
                    acquire,
                ))
            }
            Ok(Outcome::Refused { rejection, .. }) => {
                Ok((Recorded::Refused(rejection.code()), Vec::new()))
            }
            Err(SaveError::DuplicateMessage(_)) => Ok((Recorded::Duplicate, Vec::new())),
            Err(error) => Err(error.into()),
        }
    }

    /// The `acquire` list of the `MessageRecorded` in `appended`.
    async fn journaled_acquire(
        &self,
        appended: &Appended,
    ) -> Result<Vec<RunnerRequestId>, ListenerError> {
        let stream = stream_of::<ScaleSet>(self.config.pool.as_str())?;
        let mut acquire = Vec::new();
        for part in appended.streams.iter().filter(|s| s.stream == stream) {
            let after = part.first.get().checked_sub(1).map(StreamRevision::new);
            let events = self
                .store
                .journal()
                .read_stream(&stream, after, Some(part.last))
                .await
                .map_err(SaveError::Storage)?;
            for recorded in &events {
                let event =
                    decode_event::<ScaleSetEvent>(recorded).map_err(|_| SaveError::Codec {
                        stream: stream.clone(),
                        what: "event_payload",
                    })?;
                if let ScaleSetEvent::MessageRecorded(e) = event {
                    acquire.extend(e.acquire);
                }
            }
        }
        Ok(acquire)
    }

    /// `AcquireJobs` for `requests`, and `RecordAcquireResult` when it answered: `Ok(None)` when
    /// no answer came (the ids stay pending), `Err(cause)` when the session is gone.
    async fn acquire(
        &self,
        set: ScaleSetId,
        requests: &[RunnerRequestId],
    ) -> Result<Option<BTreeSet<RunnerRequestId>>, LossCause> {
        let requested: BTreeSet<RunnerRequestId> = requests.iter().copied().collect();
        let acquired = match self.api.acquire_jobs(set, &requested).await {
            Ok(acquired) => acquired,
            // a 400 is a definite refusal of the batch (spec §6.1); any other status leaves it
            // pending, asked again after the backoff
            Err(GitHubCallError {
                failure: Failure::Refused { status: 400 },
                ..
            }) => BTreeSet::new(),
            Err(e) => {
                return match e.failure {
                    Failure::SessionExpired => Err(LossCause::Expired),
                    Failure::NoSession => Err(LossCause::Restarted),
                    Failure::SessionConflict
                    | Failure::ScaleSetGone
                    | Failure::NameTaken
                    | Failure::Throttled
                    | Failure::Refused { .. }
                    | Failure::Credentials
                    | Failure::Unavailable
                    | Failure::ProtocolDrift => {
                        self.hold_acquire();
                        Ok(None)
                    }
                };
            }
        };
        let mut parts: Vec<[u8; 8]> = Vec::new();
        parts.extend(requested.iter().map(|r| r.0.to_be_bytes()));
        parts.push(u64::MAX.to_be_bytes());
        parts.extend(acquired.iter().map(|r| r.0.to_be_bytes()));
        let slices: Vec<&[u8]> = parts.iter().map(<[u8; 8]>::as_slice).collect();
        let id = self.command_id(b"acquired", &slices);
        let body = ScaleSetCommand::RecordAcquireResult {
            requested,
            acquired: acquired.clone(),
        };
        // a refusal or a store failure records nothing: the ids stay pending, asked again
        match self.save(id, body, None).await {
            Ok(Outcome::Accepted { .. }) => {
                let mut local = self.local();
                local.acquire_failures = 0;
                local.next_acquire = None;
                Ok(Some(acquired))
            }
            Ok(Outcome::Refused { rejection, .. }) => {
                tracing::warn!(code = rejection.code(), "acquire_result_refused");
                self.hold_acquire();
                Ok(None)
            }
            Err(error) => {
                tracing::warn!(error = %error, "acquire_result_not_recorded");
                self.hold_acquire();
                Ok(None)
            }
        }
    }

    /// The gap of spec §3.4, alarmed once per episode past [`ListenerConfig::gap_alarm_after`].
    fn check_gap(&self, state: &ScaleSetState) -> u32 {
        let gap = gap_of(state);
        let now = self.store.clock().now();
        let mut local = self.local();
        if gap == 0 {
            local.gap_since = None;
            local.gap_alarmed = false;
            return 0;
        }
        let since = *local.gap_since.get_or_insert(now);
        let persisted = now.get().saturating_sub(since.get()) >= self.config.gap_alarm_after.get();
        if persisted && !local.gap_alarmed {
            local.gap_alarmed = true;
            tracing::warn!(
                pool = %self.config.pool,
                gap,
                assigned = state.statistics.map_or(0, |s| s.total_assigned_jobs),
                running = state.statistics.map_or(0, |s| s.total_running_jobs),
                "assigned_jobs_gap_persists"
            );
        }
        gap
    }

    /// Whether this episode of the gap was alarmed.
    #[must_use]
    pub fn gap_alarmed(&self) -> bool {
        self.local().gap_alarmed
    }

    /// Starts an orderly close (`StartClosing`) and deletes the live session; a tick finishes a
    /// close a failed delete left.
    ///
    /// # Errors
    /// As [`Listener::tick`].
    pub async fn close(&self) -> Result<Tick, ListenerError> {
        let (state, head) = self.load().await?;
        match state.phase {
            Phase::Active | Phase::Degraded(_) => {
                let id = self.command_id(b"closing", &[&revision_bytes(head)]);
                self.require(id, ScaleSetCommand::StartClosing).await?;
                let (state, _) = self.load().await?;
                self.finish_close(&state).await
            }
            Phase::Closing => self.finish_close(&state).await,
            Phase::Unconfigured
            | Phase::Ensured
            | Phase::Opening
            | Phase::Backoff(_)
            | Phase::Closed => Ok(Tick::Closed),
        }
    }

    async fn finish_close(&self, state: &ScaleSetState) -> Result<Tick, ListenerError> {
        let (Some(set), Some(session)) = (state.github_id, state.session) else {
            return Err(ListenerError::Refused(ScaleSetRejection::NoSession.code()));
        };
        if let Err(e) = self.api.delete_session(set, session).await {
            return Ok(self.call_failed(&e));
        }
        let id = self.command_id(b"closed", &[&session.as_u128().to_be_bytes()]);
        self.require(
            id,
            ScaleSetCommand::RecordSessionDeleted {
                session_id: session,
            },
        )
        .await?;
        let mut local = self.local();
        local.session = None;
        local.unacked = None;
        Ok(Tick::Closed)
    }
}

/// `totalAssignedJobs − totalRunningJobs` beyond the jobs the stream knows of: those it holds
/// pending, trusted or refused, and its open reservations (spec §3.4). Zero before any
/// statistics. A job counted in two of those sets lowers the gap, never raises it.
#[must_use]
pub fn gap_of(state: &ScaleSetState) -> u32 {
    let Some(statistics) = state.statistics else {
        return 0;
    };
    let known =
        state.pending.len() + state.trusted.len() + state.refused.len() + state.reserved().len();
    let known = u32::try_from(known).unwrap_or(u32::MAX);
    statistics
        .total_assigned_jobs
        .saturating_sub(statistics.total_running_jobs)
        .saturating_sub(known)
}
