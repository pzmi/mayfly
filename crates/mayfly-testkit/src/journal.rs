//! `MemoryJournal`: the `Journal` port in memory, with the same transactional semantics the
//! SQLite journal promises (ver §7.1). It passes the journal contract
//! (`tests/journal_contract.rs`: `should_pass_journal_contract_when_journal_is_in_memory`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use mayfly_control::journal::{
    AppendError, Appended, CommandReceipt, EventLocation, InboxEntry, Journal, OutboxRow,
    OutboxStatus, OutboxWrite, Payload, ProjectionName, RecordedEvent, RecordedKind, StorageError,
    StreamId, StreamRevision, UnitOfWork,
};
use mayfly_domain::{CommandId, DirectiveId, EventId, Millis};

use crate::contracts::journal::JournalFactory;

#[derive(Debug, Clone, Default)]
struct State {
    streams: BTreeMap<StreamId, Vec<RecordedEvent>>,
    event_ids: BTreeMap<EventId, EventLocation>,
    kinds: BTreeSet<RecordedKind>,
    receipts: BTreeMap<CommandId, CommandReceipt>,
    inbox: BTreeMap<InboxEntry, Millis>,
    outbox: BTreeMap<DirectiveId, OutboxRow>,
    projections: BTreeMap<(ProjectionName, String), Payload>,
}

impl State {
    fn revision(&self, stream: &StreamId) -> StreamRevision {
        self.streams
            .get(stream)
            .and_then(|events| events.last())
            .map_or(StreamRevision::EMPTY, |e| e.revision)
    }

    /// The state after `unit`, or the first refusal in the port's check order. `self` is never
    /// touched, which is what makes the commit all-or-nothing.
    fn after(&self, unit: UnitOfWork) -> Result<(Self, Appended), AppendError> {
        unit.check_shape().map_err(AppendError::Malformed)?;
        if let Some(existing) = unit
            .receipt
            .as_ref()
            .and_then(|r| self.receipts.get(&r.command_id))
        {
            return Err(AppendError::DuplicateCommand(Box::new(existing.clone())));
        }
        if let Some(entry) = unit.inbox.as_ref().filter(|e| self.inbox.contains_key(e)) {
            return Err(AppendError::DuplicateMessage(entry.clone()));
        }
        for append in &unit.streams {
            let actual = self.revision(&append.stream);
            if actual != append.expected {
                return Err(AppendError::WrongExpectedRevision {
                    stream: append.stream.clone(),
                    expected: append.expected,
                    actual,
                });
            }
        }
        if let Some(event) = unit
            .streams
            .iter()
            .flat_map(|s| &s.events)
            .find(|e| self.event_ids.contains_key(&e.event_id))
        {
            return Err(AppendError::DuplicateEventId(event.event_id));
        }

        let mut next = self.clone();
        for write in unit.outbox {
            next.write_outbox(write)?;
        }
        let mut recorded = Vec::new();
        for append in unit.streams {
            let mut revision = append.expected;
            for event in append.events {
                revision = revision.next().ok_or_else(exhausted)?;
                next.event_ids.insert(
                    event.event_id,
                    EventLocation {
                        stream: append.stream.clone(),
                        revision,
                    },
                );
                next.kinds.insert(RecordedKind {
                    event_type: event.event_type.clone(),
                    schema_version: event.schema_version,
                });
                recorded.push(RecordedEvent {
                    stream: append.stream.clone(),
                    revision,
                    event_id: event.event_id,
                    event_type: event.event_type,
                    schema_version: event.schema_version,
                    payload: event.payload,
                    recorded_at: unit.recorded_at,
                });
            }
        }
        for event in &recorded {
            next.streams
                .entry(event.stream.clone())
                .or_default()
                .push(event.clone());
        }
        if let Some(receipt) = unit.receipt {
            next.receipts.insert(receipt.command_id, receipt);
        }
        if let Some(entry) = unit.inbox {
            next.inbox.insert(entry, unit.recorded_at);
        }
        for write in unit.projections {
            let key = (write.projection, write.key);
            match write.value {
                Some(value) => next.projections.insert(key, value),
                None => next.projections.remove(&key),
            };
        }
        Ok((next, Appended { events: recorded }))
    }

    fn write_outbox(&mut self, write: OutboxWrite) -> Result<(), AppendError> {
        match write {
            OutboxWrite::Insert(directive) => {
                if self.outbox.contains_key(&directive.directive_id) {
                    return Err(AppendError::DuplicateDirective(directive.directive_id));
                }
                self.outbox
                    .insert(directive.directive_id, OutboxRow::pending(directive));
            }
            OutboxWrite::Transition(transition) => {
                let id = transition.directive_id;
                let row = self
                    .outbox
                    .remove(&id)
                    .ok_or(AppendError::UnknownDirective(id))?;
                if row.status != transition.from {
                    return Err(AppendError::OutboxStatusMismatch {
                        directive_id: id,
                        expected: transition.from,
                        actual: row.status,
                    });
                }
                if !row.status.allows(transition.to) {
                    return Err(AppendError::IllegalOutboxTransition {
                        directive_id: id,
                        from: row.status,
                        to: transition.to,
                    });
                }
                self.outbox.insert(id, row.moved(&transition));
            }
        }
        Ok(())
    }
}

fn exhausted() -> AppendError {
    AppendError::Storage(StorageError::new("stream revision exhausted"))
}

/// The in-memory journal. Clones share one store, so a clone is the same journal "reopened":
/// what one handle committed, every other reads.
#[derive(Debug, Clone, Default)]
pub struct MemoryJournal {
    state: Arc<Mutex<State>>,
}

impl MemoryJournal {
    /// An empty journal.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn read<T>(&self, f: impl FnOnce(&State) -> T) -> T {
        f(&self.state.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn commit(&self, unit: UnitOfWork) -> Result<Appended, AppendError> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (next, appended) = state.after(unit)?;
        *state = next;
        Ok(appended)
    }
}

impl Journal for MemoryJournal {
    async fn append(&self, unit: UnitOfWork) -> Result<Appended, AppendError> {
        self.commit(unit)
    }

    async fn stream_revision(&self, stream: &StreamId) -> Result<StreamRevision, StorageError> {
        Ok(self.read(|s| s.revision(stream)))
    }

    async fn read_stream(
        &self,
        stream: &StreamId,
        after: StreamRevision,
        limit: usize,
    ) -> Result<Vec<RecordedEvent>, StorageError> {
        Ok(self.read(|s| {
            s.streams
                .get(stream)
                .into_iter()
                .flatten()
                .filter(|e| e.revision > after)
                .take(limit)
                .cloned()
                .collect()
        }))
    }

    async fn streams(
        &self,
        after: Option<&StreamId>,
        limit: usize,
    ) -> Result<Vec<StreamId>, StorageError> {
        Ok(self.read(|s| {
            s.streams
                .keys()
                .filter(|id| after.is_none_or(|a| *id > a))
                .take(limit)
                .cloned()
                .collect()
        }))
    }

    async fn lookup_event(&self, event_id: EventId) -> Result<Option<EventLocation>, StorageError> {
        Ok(self.read(|s| s.event_ids.get(&event_id).cloned()))
    }

    async fn recorded_kinds(&self) -> Result<Vec<RecordedKind>, StorageError> {
        Ok(self.read(|s| s.kinds.iter().cloned().collect()))
    }

    async fn receipt(&self, command_id: CommandId) -> Result<Option<CommandReceipt>, StorageError> {
        Ok(self.read(|s| s.receipts.get(&command_id).cloned()))
    }

    async fn processed_at(&self, entry: &InboxEntry) -> Result<Option<Millis>, StorageError> {
        Ok(self.read(|s| s.inbox.get(entry).copied()))
    }

    async fn directive(
        &self,
        directive_id: DirectiveId,
    ) -> Result<Option<OutboxRow>, StorageError> {
        Ok(self.read(|s| s.outbox.get(&directive_id).cloned()))
    }

    async fn directives(
        &self,
        status: OutboxStatus,
        limit: usize,
    ) -> Result<Vec<OutboxRow>, StorageError> {
        Ok(self.read(|s| {
            let mut rows: Vec<_> = s
                .outbox
                .values()
                .filter(|r| r.status == status)
                .cloned()
                .collect();
            rows.sort_by_key(|r| (r.next_attempt_at, r.directive_id));
            rows.truncate(limit);
            rows
        }))
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

/// Makes [`MemoryJournal`]s for the journal contract; reopening hands back the same store.
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
}
