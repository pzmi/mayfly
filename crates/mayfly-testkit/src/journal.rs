//! `MemoryJournal`: the `Journal` port in memory, with the transactional semantics every
//! journal promises (`doc/event-store.md` §3.3). It passes the journal contract
//! (`tests/journal_contract.rs`: `should_pass_journal_contract_when_journal_is_in_memory`).
//!
//! A unit is judged and applied under one lock in one poll, so the memory journal never
//! interleaves two units: it shows the contract's outcomes, not its concurrency. Faults are
//! injected through [`MemoryJournal::inject`], which is how the contract's unknown-outcome and
//! crash cases (JC-23, JC-25) run on memory.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use mayfly_control::journal::{
    AggregateName, AppendError, Appended, AppendedStream, CommandReceipt, Decision, EventLocation,
    ExpectedRevision, InboxKey, Journal, MAX_PAGE, Payload, ProjectionName, ReceiptOutcome,
    RecordedEvent, StorageError, StoredKind, StreamHead, StreamId, StreamRevision, UnitOfWork,
};
use mayfly_domain::{CommandId, EventId};

use crate::contracts::journal::{InjectedFault, JournalFactory};

#[derive(Debug, Clone, Default)]
struct State {
    streams: BTreeMap<StreamId, Vec<RecordedEvent>>,
    heads: BTreeMap<StreamId, StreamHead>,
    identities: BTreeMap<EventId, EventLocation>,
    kinds: BTreeSet<StoredKind>,
    receipts: BTreeMap<CommandId, CommandReceipt>,
    inbox: BTreeSet<InboxKey>,
    projections: BTreeMap<(ProjectionName, String), Payload>,
}

impl State {
    fn revision(&self, stream: &StreamId) -> Option<StreamRevision> {
        self.heads.get(stream).map(|h| h.revision)
    }

    /// The first refusal of `unit` in the port's reporting order, or `None` when it commits.
    fn refusal(&self, unit: &UnitOfWork) -> Option<AppendError> {
        if let Err(malformed) = unit.check_shape() {
            return Some(AppendError::Malformed(malformed));
        }
        if let Some(stream) = unit.epoch_above_build() {
            return Some(AppendError::EpochAboveBuild {
                stream: stream.clone(),
            });
        }
        if let Some(new) = &unit.receipt
            && let Some(stored) = self.receipts.get(&new.command_id)
        {
            if stored.intent != new.intent {
                return Some(AppendError::CommandIdReused {
                    command_id: new.command_id,
                });
            }
            let answers = match &stored.outcome {
                ReceiptOutcome::Accepted(_) => true,
                ReceiptOutcome::Refused { bound, .. } => bound
                    .iter()
                    .all(|read| read.expected.matches(self.revision(&read.stream))),
            };
            if answers {
                // Judged with the receipt, under the same lock: an older build is never
                // answered from a newer build's outcome (JC-33, §3.3).
                if let Some(refusal) =
                    self.above_build(stored.replay_streams(unit.named_streams()).iter(), unit)
                {
                    return Some(refusal);
                }
                return Some(AppendError::CommandReplayed(Box::new(stored.clone())));
            }
        }
        if let Some(key) = unit.inbox.as_ref().filter(|k| self.inbox.contains(k)) {
            return Some(AppendError::DuplicateMessage(key.clone()));
        }
        if let Some(id) = unit
            .event_ids()
            .filter(|id| self.identities.contains_key(id))
            .min()
        {
            return Some(AppendError::DuplicateEventId(id));
        }
        let mut conditions: Vec<(&StreamId, ExpectedRevision)> = unit
            .appends
            .iter()
            .map(|a| (&a.stream, a.expected))
            .chain(unit.reads.iter().map(|r| (&r.stream, r.expected)))
            .collect();
        conditions.sort_by_key(|(stream, _)| *stream);
        for (stream, expected) in conditions {
            let actual = self.revision(stream);
            if !expected.matches(actual) {
                return Some(AppendError::WrongExpectedRevision {
                    stream: stream.clone(),
                    expected,
                    actual,
                });
            }
        }
        let mut appended: Vec<&StreamId> = unit.appends.iter().map(|a| &a.stream).collect();
        appended.sort();
        self.above_build(appended.into_iter(), unit)
    }

    /// The first of `streams` (given in `StreamId` order) whose epoch exceeds what `unit`'s
    /// build supports for its recorded aggregate, as `StreamEpochUnsupported`.
    fn above_build<'a>(
        &self,
        streams: impl Iterator<Item = &'a StreamId>,
        unit: &UnitOfWork,
    ) -> Option<AppendError> {
        streams.into_iter().find_map(|stream| {
            let head = self.heads.get(stream)?;
            (head.epoch > unit.supported_epochs.of(&head.aggregate)).then(|| {
                AppendError::StreamEpochUnsupported {
                    stream: stream.clone(),
                    epoch: head.epoch,
                }
            })
        })
    }

    /// The state after `unit`, or its refusal. `self` is never touched, which is what makes
    /// the commit all-or-nothing.
    fn after(&self, unit: UnitOfWork) -> Result<(Self, Appended), AppendError> {
        if let Some(refusal) = self.refusal(&unit) {
            return Err(refusal);
        }
        let mut next = self.clone();
        let mut appended = Appended::default();
        for append in unit.appends {
            let mut revision =
                append
                    .expected
                    .next_revision()
                    .ok_or(AppendError::Storage(StorageError::new(
                        "revision_exhausted",
                    )))?;
            let first = revision;
            let mut last = revision;
            let mut event_ids = Vec::new();
            let mut epoch = self.heads.get(&append.stream).map_or(0, |h| h.epoch);
            for (i, event) in append.events.into_iter().enumerate() {
                if i > 0 {
                    revision = revision
                        .next()
                        .ok_or(AppendError::Storage(StorageError::new(
                            "revision_exhausted",
                        )))?;
                }
                last = revision;
                epoch = epoch.max(event.required_epoch);
                event_ids.push(event.event_id);
                next.identities.insert(
                    event.event_id,
                    EventLocation {
                        stream: append.stream.clone(),
                        revision,
                    },
                );
                next.kinds.insert(event.kind.clone());
                next.streams
                    .entry(append.stream.clone())
                    .or_default()
                    .push(RecordedEvent {
                        stream: append.stream.clone(),
                        revision,
                        event_id: event.event_id,
                        kind: event.kind,
                        payload: event.payload,
                        recorded_at: unit.recorded_at,
                    });
            }
            let aggregate: AggregateName = self
                .heads
                .get(&append.stream)
                .map_or(append.aggregate, |h| h.aggregate.clone());
            next.heads.insert(
                append.stream.clone(),
                StreamHead {
                    stream: append.stream.clone(),
                    revision: last,
                    aggregate,
                    epoch,
                },
            );
            appended.streams.push(AppendedStream {
                stream: append.stream,
                first,
                last,
                event_ids,
            });
        }
        if let Some(receipt) = unit.receipt {
            let outcome = match receipt.decision {
                Decision::Accepted => ReceiptOutcome::Accepted(appended.clone()),
                Decision::Refused { rejection } => {
                    let mut bound = unit.reads;
                    bound.sort_by(|a, b| a.stream.cmp(&b.stream));
                    ReceiptOutcome::Refused { bound, rejection }
                }
            };
            next.receipts.insert(
                receipt.command_id,
                CommandReceipt {
                    command_id: receipt.command_id,
                    command_type: receipt.command_type,
                    intent: receipt.intent,
                    outcome,
                },
            );
        }
        if let Some(key) = unit.inbox {
            next.inbox.insert(key);
        }
        for write in unit.projections {
            let key = (write.projection, write.key);
            match write.row {
                Some(row) => next.projections.insert(key, row),
                None => next.projections.remove(&key),
            };
        }
        Ok((next, appended))
    }
}

#[derive(Debug, Default)]
struct Shared {
    state: State,
    fault: Option<InjectedFault>,
}

/// The in-memory journal. Clones share one store: a clone is a second handle on the same
/// journal, and the same journal "reopened" (what one handle committed, every other reads).
#[derive(Debug, Clone, Default)]
pub struct MemoryJournal {
    shared: Arc<Mutex<Shared>>,
}

impl MemoryJournal {
    /// An empty journal.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Arms `fault` for the next append through any handle; returns whether this journal can
    /// inject it (memory has two crash points: before the commit and after it).
    pub fn inject(&self, fault: InjectedFault) -> bool {
        if matches!(fault, InjectedFault::CrashAfterStatement(n) if n > 1) {
            return false;
        }
        self.lock().fault = Some(fault);
        true
    }

    /// The refusal `unit` would get now, or `None` when it would commit; nothing is written
    /// and no injected fault is consumed. For broken journals built over this one.
    #[must_use]
    pub fn judge(&self, unit: &UnitOfWork) -> Option<AppendError> {
        self.read(|s| s.refusal(unit))
    }

    /// Stores `receipt` over whatever receipt its command has, judging nothing: for broken
    /// journals built over this one (the blind receipt upsert JC-29b must catch).
    pub fn overwrite_receipt(&self, receipt: CommandReceipt) {
        self.lock()
            .state
            .receipts
            .insert(receipt.command_id, receipt);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn read<T>(&self, f: impl FnOnce(&State) -> T) -> T {
        f(&self.lock().state)
    }

    /// Judges `unit` first and only then lets an armed fault strike, so a refusal is reported
    /// ahead of the store's own `Busy` and `Storage`, as the reporting order says (JC-09); the
    /// fault is consumed either way.
    fn commit(&self, unit: UnitOfWork) -> Result<Appended, AppendError> {
        let mut shared = self.lock();
        let fault = shared.fault.take();
        let (next, appended) = shared.state.after(unit)?;
        match fault {
            Some(InjectedFault::StorageBeforeCommit | InjectedFault::CrashAfterStatement(0)) => {
                return Err(AppendError::Storage(StorageError::new("injected_failure")));
            }
            Some(InjectedFault::Busy) => return Err(AppendError::Busy),
            _ => {}
        }
        let lost = AppendError::OutcomeUnknown(StorageError::new("injected_lost_reply"));
        match fault {
            Some(InjectedFault::ReplyLostBeforeCommit) => Err(lost),
            Some(InjectedFault::ReplyLostAfterCommit | InjectedFault::CrashAfterStatement(_)) => {
                shared.state = next;
                Err(lost)
            }
            _ => {
                shared.state = next;
                Ok(appended)
            }
        }
    }
}

impl Journal for MemoryJournal {
    async fn append(&self, unit: UnitOfWork) -> Result<Appended, AppendError> {
        self.commit(unit)
    }

    async fn head(&self, stream: &StreamId) -> Result<Option<StreamHead>, StorageError> {
        Ok(self.read(|s| s.heads.get(stream).cloned()))
    }

    async fn read_stream(
        &self,
        stream: &StreamId,
        after: Option<StreamRevision>,
        through: Option<StreamRevision>,
    ) -> Result<Vec<RecordedEvent>, StorageError> {
        Ok(self.read(|s| {
            s.streams
                .get(stream)
                .into_iter()
                .flatten()
                .filter(|e| after.is_none_or(|a| e.revision > a))
                .filter(|e| through.is_none_or(|t| e.revision <= t))
                .cloned()
                .collect()
        }))
    }

    async fn lookup_event(&self, event_id: EventId) -> Result<Option<EventLocation>, StorageError> {
        Ok(self.read(|s| s.identities.get(&event_id).cloned()))
    }

    async fn receipt(&self, command_id: CommandId) -> Result<Option<CommandReceipt>, StorageError> {
        Ok(self.read(|s| s.receipts.get(&command_id).cloned()))
    }

    async fn inbox_contains(&self, key: &InboxKey) -> Result<bool, StorageError> {
        Ok(self.read(|s| s.inbox.contains(key)))
    }

    async fn streams(
        &self,
        after: Option<&StreamId>,
        limit: u32,
    ) -> Result<Vec<StreamHead>, StorageError> {
        let limit = usize::try_from(limit.min(MAX_PAGE)).unwrap_or(0);
        Ok(self.read(|s| {
            s.heads
                .values()
                .filter(|h| after.is_none_or(|a| h.stream > *a))
                .take(limit)
                .cloned()
                .collect()
        }))
    }

    async fn stored_kinds(&self) -> Result<Vec<StoredKind>, StorageError> {
        Ok(self.read(|s| s.kinds.iter().cloned().collect()))
    }

    async fn projection(
        &self,
        projection: &ProjectionName,
        key: &str,
    ) -> Result<Option<Payload>, StorageError> {
        Ok(self.read(|s| {
            s.projections
                .get(&(projection.clone(), key.to_owned()))
                .cloned()
        }))
    }
}

/// Makes [`MemoryJournal`]s for the journal contract: reopening, a second handle and a crash
/// all hand back the same store, and faults are injected with [`MemoryJournal::inject`].
#[derive(Debug, Clone, Copy, Default)]
pub struct MemoryJournalFactory;

impl JournalFactory for MemoryJournalFactory {
    type Journal = MemoryJournal;

    async fn fresh(&self) -> MemoryJournal {
        MemoryJournal::new()
    }

    async fn reopen(&self, journal: MemoryJournal) -> MemoryJournal {
        journal
    }

    async fn second_handle(&self, journal: &MemoryJournal) -> MemoryJournal {
        journal.clone()
    }

    async fn inject(&self, journal: &MemoryJournal, fault: InjectedFault) -> bool {
        journal.inject(fault)
    }

    async fn crash(&self, journal: MemoryJournal) -> MemoryJournal {
        journal
    }
}
