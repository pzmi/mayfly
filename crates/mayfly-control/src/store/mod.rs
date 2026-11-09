//! `AggregateStore`: the control plane's one write path (`doc/event-store.md` §3.7, fnd §6.3).
//!
//! [`AggregateStore::save`] loads an aggregate's stream, decides a command with the aggregate's
//! pure `decide`, and commits the outcome as one unit of work: the events and the receipt of an
//! acceptance, or the refusal receipt of a refusal, bound to every stream the decision read
//! (ADR 0034 E3, ADR 0041). It is the only caller of [`crate::journal::Journal::append`]
//! (`ARCH-single-append`), so every write the control plane makes goes through the steps below,
//! in this order:
//!
//! 1. **Epoch before receipt.** Read the head; a stream whose epoch exceeds this build's
//!    supported epoch is refused (`StreamEpochUnsupported`) before any receipt is consulted, so
//!    a rolled-back build never answers for a newer build's outcome
//!    (`tests/store/save.rs::should_refuse_a_newer_epoch_before_the_receipt_when_a_newer_build_wrote_the_stream`).
//! 2. **Receipt.** The command's intent digest ([`intent_digest`]) is computed and its receipt
//!    read: another intent is `CommandIdReused`; an accepted receipt answers for ever; a refusal
//!    receipt answers while every stream it is bound to is unchanged, and is decided afresh
//!    otherwise (`should_replay_a_refusal_until_its_stream_moves_when_a_command_is_repeated`).
//! 3. **Load.** The whole stream is read and folded; a kind this build does not decode refuses
//!    the command and raises safe mode through [`StoreNotifier::unknown_kind`]. Snapshots join
//!    with ES-08.
//! 4. **Decide** outside any transaction; event ids are derived from `(command_id, ordinal)`
//!    ([`derive_event_id`]), so a retried command writes the same ids. Inline projections join
//!    with ES-04.
//! 5. **Append** the unit. `WrongExpectedRevision` becomes [`SaveError::Conflict`], which only
//!    [`AggregateStore::with_revision_retry`] retries (load and decide inside its closure, three
//!    attempts, full-jitter backoff through the [`Rng`] and [`Clock`] ports); `Busy`, `Storage`
//!    and refusals are returned. ES-12's projection-generation conflict joins `Conflict` as the
//!    second retried error when it exists (`SaveError` is `#[non_exhaustive]`). `OutcomeUnknown` is resolved before answering by re-appending
//!    the identical unit: its receipt answers, or `DuplicateEventId` names an id that
//!    `lookup_event` finds exactly where this unit would have put it; an id found anywhere else
//!    is a genuine collision ([`SaveError::EventIdCollision`]), never a success.
//! 6. **After commit**, and only then, [`StoreNotifier::committed`] kicks the consumers' workers,
//!    at most once; a replayed answer kicks nothing (`doc/event-store.md` §5.5).
//!
//! A refusal's unit carries no inbox key: the message is not consumed, so a redelivery after
//! the bound streams moved is decided again rather than dropped as a duplicate.

mod codec;
mod error;
mod kinds;
mod ports;

use std::fmt;
use std::future::Future;

use mayfly_control_domain::{Aggregate, AggregateRegistry, Command, CommandBody};
use mayfly_domain::Millis;

pub use codec::{
    EventDecodeError, decode_event, decode_rejection, derive_event_id, encode_event,
    encode_rejection, intent_digest, stream_of,
};
pub use error::{Outcome, SaveError};
pub use kinds::{KindCheck, UnknownStoredKind, check_stored_kinds, supported_epochs};
pub use ports::{Clock, Committed, NoNotifier, Rng, StoreNotifier};

use crate::journal::{
    AggregateName, AppendError, Appended, AppendedStream, CommandReceipt, CommandType, Decision,
    EventLocation, ExpectedRevision, InboxKey, Journal, NewEvent, NewReceipt, ReadCondition,
    ReceiptOutcome, StreamId, SupportedEpochs, UnitOfWork,
};

/// How many times one save re-appends a unit whose commit reply was lost before it reports
/// [`SaveError::OutcomeUnknown`].
const RESOLVE_ATTEMPTS: u32 = 3;

/// How [`AggregateStore::with_revision_retry`] retries a conflict: up to `attempts` decisions
/// in all, waiting a uniformly random `0..=min(cap, base * 2^(n-1))` before the n-th retry (full
/// jitter), so writers that collided spread out instead of colliding again.
///
/// Pinned by: `tests/store/save.rs::should_retry_only_conflicts_when_a_stream_moves_under_the_decision`
/// (three decisions, two waits per exhausted run, every wait within the default 10 and 20 ms
/// ceilings) and `should_not_retry_when_the_policy_allows_one_decision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Decisions in all, the first included (at least 1).
    pub attempts: u32,
    /// The first backoff's ceiling.
    pub base: Millis,
    /// The largest ceiling.
    pub cap: Millis,
}

impl Default for RetryPolicy {
    /// Three decisions, backoff ceilings of 10 and 20 ms (`doc/event-store.md` §3.7 step 5).
    fn default() -> Self {
        Self {
            attempts: 3,
            base: Millis::new(10),
            cap: Millis::new(200),
        }
    }
}

/// One command addressed to one instance of an aggregate, with what else its decision read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveRequest<B> {
    /// The instance id; the stream is [`stream_of`] it.
    pub instance: String,
    /// The command.
    pub command: Command<B>,
    /// Other streams the caller read to build the command (a pool's configuration while
    /// starting an attempt), each at the revision read: the unit is refused if one moved, and
    /// a refusal receipt is bound to them too (ADR 0041).
    pub reads: Vec<ReadCondition>,
    /// The inbound message the command consumes on acceptance (ES-I6).
    pub inbox: Option<InboxKey>,
}

impl<B> SaveRequest<B> {
    /// `command` for instance `instance`, with no other read and no inbox key.
    pub fn new(instance: impl fmt::Display, command: Command<B>) -> Self {
        Self {
            instance: instance.to_string(),
            command,
            reads: Vec::new(),
            inbox: None,
        }
    }

    /// Adds a stream the decision read, at the revision read.
    #[must_use]
    pub fn with_read(mut self, stream: StreamId, expected: ExpectedRevision) -> Self {
        self.reads.push(ReadCondition { stream, expected });
        self
    }

    /// Sets the inbound message the command consumes.
    #[must_use]
    pub fn with_inbox(mut self, key: InboxKey) -> Self {
        self.inbox = Some(key);
        self
    }
}

/// The single write path over a [`Journal`], with the clock, randomness and notifications it
/// needs as ports (D8: generics, no `dyn`).
#[derive(Debug)]
pub struct AggregateStore<J, C, R, N> {
    journal: J,
    clock: C,
    rng: R,
    notifier: N,
    epochs: SupportedEpochs,
    retry: RetryPolicy,
}

/// What appending a unit settled to.
enum Settled {
    /// This call's unit committed (directly, or after a lost reply).
    Committed(Appended),
    /// A receipt answered: another run's outcome, or this call's own after a lost reply.
    Answered(Box<CommandReceipt>),
}

impl<J: Journal, C: Clock, R: Rng, N: StoreNotifier> AggregateStore<J, C, R, N> {
    /// A store over `journal` for the aggregates of registry `G`, whose supported epochs every
    /// unit carries ([`supported_epochs`]).
    ///
    /// # Errors
    /// [`SaveError::InvalidName`] when an aggregate's `NAME` is not a journal name.
    pub fn new<G: AggregateRegistry>(
        journal: J,
        clock: C,
        rng: R,
        notifier: N,
    ) -> Result<Self, SaveError> {
        Ok(Self {
            journal,
            clock,
            rng,
            notifier,
            epochs: supported_epochs::<G>().map_err(SaveError::InvalidName)?,
            retry: RetryPolicy::default(),
        })
    }

    /// The same store retrying conflicts by `retry`.
    #[must_use]
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// The journal, for reads (heads, streams, receipts); never for writes.
    pub const fn journal(&self) -> &J {
        &self.journal
    }

    /// This build's supported epoch per aggregate.
    pub const fn supported_epochs(&self) -> &SupportedEpochs {
        &self.epochs
    }

    /// Decides `request` once and commits its outcome (steps 1 to 6 of the module docs).
    ///
    /// # Errors
    /// [`SaveError`]; only [`SaveError::OutcomeUnknown`] leaves the commit undetermined.
    pub async fn save<A: Aggregate>(
        &self,
        request: SaveRequest<A::Body>,
    ) -> Result<Outcome<A::Rejection>, SaveError> {
        let aggregate = AggregateName::new(A::NAME).map_err(SaveError::InvalidName)?;
        let stream = stream_of::<A>(&request.instance).map_err(SaveError::InvalidName)?;
        if let Some(head) = self
            .journal
            .head(&stream)
            .await
            .map_err(SaveError::Storage)?
        {
            if head.aggregate != aggregate {
                return Err(SaveError::AggregateMismatch {
                    stream,
                    recorded: head.aggregate,
                });
            }
            if head.epoch > self.epochs.of(&head.aggregate) {
                return Err(SaveError::StreamEpochUnsupported {
                    stream,
                    epoch: head.epoch,
                });
            }
        }
        let command_id = request.command.command_id;
        let command_type = CommandType::new(request.command.body.command_type())
            .map_err(SaveError::InvalidName)?;
        let intent = intent_digest(&aggregate, &command_type, &stream, &request.command.body)
            .map_err(|_| SaveError::Codec {
                stream: stream.clone(),
                what: "command_body",
            })?;
        if let Some(receipt) = self
            .journal
            .receipt(command_id)
            .await
            .map_err(SaveError::Storage)?
        {
            if receipt.intent != intent {
                return Err(SaveError::CommandIdReused { command_id });
            }
            if self.still_answers(&receipt).await? {
                return answer::<A>(&stream, &receipt, true);
            }
        }
        let recorded = self
            .journal
            .read_stream(&stream, None, None)
            .await
            .map_err(SaveError::Storage)?;
        let expected = ExpectedRevision::of(recorded.last().map(|e| e.revision));
        let mut state = A::initial();
        for event in &recorded {
            match decode_event::<A::Event>(event) {
                Ok(event) => state = A::evolve(state, &event),
                Err(EventDecodeError::UnknownKind) => {
                    self.notifier.unknown_kind(&stream, &event.kind);
                    return Err(SaveError::UnknownKind {
                        stream,
                        kind: event.kind.clone(),
                    });
                }
                Err(EventDecodeError::Malformed) => {
                    return Err(SaveError::Codec {
                        stream,
                        what: "event_payload",
                    });
                }
            }
        }
        let receipt = |decision| NewReceipt {
            command_id,
            command_type: command_type.clone(),
            intent,
            decision,
        };
        let unit = UnitOfWork::new(self.clock.now(), self.epochs.clone());
        let decided = A::decide(&state, request.command);
        let (unit, refusal) = match decided {
            Ok(events) => {
                let events = events
                    .iter()
                    .zip(0_u32..)
                    .map(|(event, ordinal)| {
                        encode_event(event, derive_event_id(command_id, ordinal))
                    })
                    .collect::<Result<Vec<NewEvent>, _>>()
                    .map_err(|_| SaveError::Codec {
                        stream: stream.clone(),
                        what: "event_payload",
                    })?;
                let unit = if events.is_empty() {
                    unit.with_read(stream.clone(), expected)
                } else {
                    unit.with_append(stream.clone(), aggregate, expected, events)
                };
                let mut unit = unit.with_receipt(receipt(Decision::Accepted));
                unit.inbox = request.inbox;
                (unit, None)
            }
            Err(rejection) => {
                let encoded = encode_rejection(&rejection).map_err(|_| SaveError::Codec {
                    stream: stream.clone(),
                    what: "rejection_payload",
                })?;
                let unit = unit
                    .with_read(stream.clone(), expected)
                    .with_receipt(receipt(Decision::Refused { rejection: encoded }));
                (unit, Some(rejection))
            }
        };
        let mut unit = unit;
        unit.reads.extend(request.reads);
        match self.settle(unit).await? {
            Settled::Committed(appended) => {
                if !appended.streams.is_empty() {
                    self.notifier.committed(&Committed {
                        command_id,
                        appended: appended.clone(),
                    });
                }
                Ok(match refusal {
                    Some(rejection) => Outcome::Refused {
                        rejection,
                        replayed: false,
                    },
                    None => Outcome::Accepted {
                        appended,
                        replayed: false,
                    },
                })
            }
            Settled::Answered(receipt) => answer::<A>(&stream, &receipt, true),
        }
    }

    /// Runs `attempt` (which loads, decides and saves) until it does not end in a conflict, at
    /// most [`RetryPolicy::attempts`] times, with full-jitter backoff between attempts. Only
    /// [`SaveError::Conflict`] is retried: `Busy`, `Storage`, refusals and an unresolved
    /// `OutcomeUnknown` are returned as they are (pinned by
    /// `tests/store/save.rs::should_retry_only_conflicts_when_a_stream_moves_under_the_decision`).
    ///
    /// # Errors
    /// The last attempt's error.
    pub async fn with_revision_retry<T, F, Fut>(&self, mut attempt: F) -> Result<T, SaveError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, SaveError>>,
    {
        let mut tries = 1;
        loop {
            match attempt().await {
                Err(error) if error.is_retryable() && tries < self.retry.attempts => {
                    let ceiling = self
                        .retry
                        .base
                        .get()
                        .saturating_mul(1_u64 << (tries - 1).min(32))
                        .min(self.retry.cap.get());
                    let wait = self.rng.below(ceiling.saturating_add(1));
                    self.clock.sleep(Millis::new(wait)).await;
                    tries += 1;
                }
                other => return other,
            }
        }
    }

    /// [`AggregateStore::save`] under [`AggregateStore::with_revision_retry`]: the usual way to
    /// run a command whose decision reads only its own stream. A request with
    /// [`SaveRequest::reads`] is refused [`SaveError::ReadsOutsideRetry`] before any read: the
    /// retry would re-check the same stale revisions, so such a caller builds its reads inside
    /// its own `with_revision_retry` closure (pinned by
    /// `tests/store/save.rs::should_refuse_reads_in_execute_when_a_retry_could_not_rebuild_them`
    /// and `should_rebuild_reads_in_the_closure_when_a_read_stream_moves`).
    ///
    /// # Errors
    /// See [`AggregateStore::save`]; [`SaveError::ReadsOutsideRetry`] as above.
    pub async fn execute<A: Aggregate>(
        &self,
        request: SaveRequest<A::Body>,
    ) -> Result<Outcome<A::Rejection>, SaveError> {
        if !request.reads.is_empty() {
            return Err(SaveError::ReadsOutsideRetry);
        }
        self.with_revision_retry(|| self.save::<A>(request.clone()))
            .await
    }

    /// Whether a stored receipt with this command's intent still answers it: an acceptance
    /// always, a refusal while every bound stream is where the decision read it.
    async fn still_answers(&self, receipt: &CommandReceipt) -> Result<bool, SaveError> {
        let ReceiptOutcome::Refused { bound, .. } = &receipt.outcome else {
            return Ok(true);
        };
        for read in bound {
            let head = self
                .journal
                .head(&read.stream)
                .await
                .map_err(SaveError::Storage)?;
            if !read.expected.matches(head.map(|h| h.revision)) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Appends `unit` and settles what happened, resolving a lost commit reply by re-appending
    /// the identical unit (ES-I18), at most [`RESOLVE_ATTEMPTS`] times:
    ///
    /// - the first append answers directly: committed, replayed by a receipt (another run's
    ///   outcome, so `save` kicks nothing), or `DuplicateEventId`, which on a first append is
    ///   always a collision;
    /// - after a lost reply, a receipt answers as this call's own commit when [`own_commit`] says
    ///   it records exactly this unit, else as another run's; `DuplicateEventId` is this
    ///   unit's commit only when `lookup_event` finds the id exactly where this unit placed it,
    ///   anywhere else a collision; a definite error is returned as it is; replies lost on
    ///   every re-append end in [`SaveError::OutcomeUnknown`], never in success.
    ///
    /// Pinned by: `tests/store/save.rs::should_resolve_a_lost_reply_by_its_receipt_when_the_commit_landed_or_not`,
    /// `should_answer_from_the_twin_receipt_when_the_same_command_commits_first`,
    /// `should_confirm_a_lost_reply_by_lookup_when_the_id_sits_where_the_unit_wrote_it`,
    /// `should_report_a_collision_when_a_derived_id_lives_elsewhere` and
    /// `should_settle_a_lost_refusal_reply_as_its_own_commit_when_the_receipt_landed`.
    async fn settle(&self, unit: UnitOfWork) -> Result<Settled, SaveError> {
        let lost = match self.journal.append(unit.clone()).await {
            Ok(appended) => return Ok(Settled::Committed(appended)),
            Err(AppendError::CommandReplayed(receipt)) => return Ok(Settled::Answered(receipt)),
            Err(AppendError::OutcomeUnknown(lost)) => lost,
            Err(AppendError::DuplicateEventId(event_id)) => {
                let location = self
                    .journal
                    .lookup_event(event_id)
                    .await
                    .map_err(SaveError::Storage)?;
                return Err(SaveError::EventIdCollision { event_id, location });
            }
            Err(other) => return Err(definite(other)),
        };
        for _ in 0..RESOLVE_ATTEMPTS {
            match self.journal.append(unit.clone()).await {
                Ok(appended) => return Ok(Settled::Committed(appended)),
                Err(AppendError::CommandReplayed(receipt)) => {
                    return Ok(match own_commit(&unit, &receipt) {
                        Some(appended) => Settled::Committed(appended),
                        None => Settled::Answered(receipt),
                    });
                }
                Err(AppendError::DuplicateEventId(event_id)) => {
                    let location = self
                        .journal
                        .lookup_event(event_id)
                        .await
                        .map_err(SaveError::Storage)?;
                    return match placed(&unit) {
                        Some(appended)
                            if location.as_ref().is_some_and(|at| {
                                where_written(&appended, event_id).as_ref() == Some(at)
                            }) =>
                        {
                            Ok(Settled::Committed(appended))
                        }
                        _ => Err(SaveError::EventIdCollision { event_id, location }),
                    };
                }
                Err(AppendError::OutcomeUnknown(_)) => {}
                Err(other) => return Err(definite(other)),
            }
        }
        Err(SaveError::OutcomeUnknown(lost))
    }
}

/// The outcome a receipt records, as this aggregate's answer.
fn answer<A: Aggregate>(
    stream: &StreamId,
    receipt: &CommandReceipt,
    replayed: bool,
) -> Result<Outcome<A::Rejection>, SaveError> {
    match &receipt.outcome {
        ReceiptOutcome::Accepted(appended) => Ok(Outcome::Accepted {
            appended: appended.clone(),
            replayed,
        }),
        ReceiptOutcome::Refused { rejection, .. } => decode_rejection::<A::Rejection>(rejection)
            .map(|rejection| Outcome::Refused {
                rejection,
                replayed,
            })
            .map_err(|_| SaveError::Codec {
                stream: stream.clone(),
                what: "rejection_payload",
            }),
    }
}

/// A definite append error as the save's error. `CommandReplayed`, `OutcomeUnknown` and
/// `DuplicateEventId` are settled by `settle` before this is reached; they map
/// to what they would mean unresolved. The match is exhaustive on purpose (no fallback arm): this crate
/// defines `AppendError`, so a new variant (ES-12's generation conflict) fails to compile here
/// until its retry semantics are decided.
fn definite(error: AppendError) -> SaveError {
    match error {
        AppendError::Malformed(m) => SaveError::Malformed(m),
        AppendError::EpochAboveBuild { stream } => SaveError::EpochAboveBuild { stream },
        AppendError::CommandIdReused { command_id } => SaveError::CommandIdReused { command_id },
        AppendError::DuplicateMessage(key) => SaveError::DuplicateMessage(key),
        AppendError::DuplicateEventId(event_id) => SaveError::EventIdCollision {
            event_id,
            location: None,
        },
        AppendError::WrongExpectedRevision {
            stream,
            expected,
            actual,
        } => SaveError::Conflict {
            stream,
            expected,
            actual,
        },
        AppendError::StreamEpochUnsupported { stream, epoch } => {
            SaveError::StreamEpochUnsupported { stream, epoch }
        }
        AppendError::Busy => SaveError::Busy,
        AppendError::Storage(e) => SaveError::Storage(e),
        AppendError::OutcomeUnknown(e) => SaveError::OutcomeUnknown(e),
        AppendError::CommandReplayed(receipt) => SaveError::CommandIdReused {
            command_id: receipt.command_id,
        },
    }
}

/// What `unit` appends, at the revisions its expected revisions put it: where its events are
/// if it committed. `None` for a unit that appends nothing.
fn placed(unit: &UnitOfWork) -> Option<Appended> {
    let streams = unit
        .appends
        .iter()
        .map(|append| {
            let first = append.expected.next_revision()?;
            let mut last = first;
            for _ in 1..append.events.len() {
                last = last.next()?;
            }
            Some(AppendedStream {
                stream: append.stream.clone(),
                first,
                last,
                event_ids: append.events.iter().map(|e| e.event_id).collect(),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    (!streams.is_empty()).then_some(Appended { streams })
}

/// Where `event_id` lands if `appended` committed.
fn where_written(appended: &Appended, event_id: mayfly_domain::EventId) -> Option<EventLocation> {
    appended.streams.iter().find_map(|s| {
        let index = s.event_ids.iter().position(|id| *id == event_id)?;
        let mut revision = s.first;
        for _ in 0..index {
            revision = revision.next()?;
        }
        Some(EventLocation {
            stream: s.stream.clone(),
            revision,
        })
    })
}

/// Whether `receipt`, answering a re-append after a lost reply, records this unit's own commit:
/// an acceptance of exactly the events this unit placed where it placed them, or a refusal bound
/// to exactly this unit's reads. Then the save committed; otherwise another run answered.
///
/// An own acceptance returns what it appended, so `save` kicks the consumers once. An own
/// refusal returns `Appended::default()`, no stream: the refusal receipt is this call's write,
/// `save` answers `Outcome::Refused { replayed: false }` and kicks nothing, since no stream
/// moved. Pinned by:
/// `tests/store/save.rs::should_resolve_a_lost_reply_by_its_receipt_when_the_commit_landed_or_not`
/// (an own acceptance, kicked) and
/// `should_settle_a_lost_refusal_reply_as_its_own_commit_when_the_receipt_landed` (an own
/// refusal, not replayed, no kick).
fn own_commit(unit: &UnitOfWork, receipt: &CommandReceipt) -> Option<Appended> {
    let decision = &unit.receipt.as_ref()?.decision;
    match (&receipt.outcome, decision) {
        (ReceiptOutcome::Accepted(appended), Decision::Accepted) => {
            let mine = placed(unit).unwrap_or_default();
            (*appended == mine).then_some(mine)
        }
        (ReceiptOutcome::Refused { bound, rejection }, Decision::Refused { rejection: own }) => {
            let mut reads = unit.reads.clone();
            reads.sort_by(|a, b| a.stream.cmp(&b.stream));
            (*bound == reads && rejection == own).then(Appended::default)
        }
        _ => None,
    }
}
