//! The unit of work: everything one command commits, atomically (plan §5.2, fnd §6.3).

use std::collections::BTreeSet;
use std::fmt;

use mayfly_domain::Millis;

use crate::journal::outbox::OutboxWrite;
use crate::journal::values::{
    CommandReceipt, InboxEntry, NewEvent, ProjectionWrite, RecordedEvent, StreamId, StreamRevision,
};

/// Events for one stream, appended only if the stream is still at `expected`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamAppend {
    /// The stream.
    pub stream: StreamId,
    /// The revision the writer loaded; any other current revision refuses the whole unit.
    pub expected: StreamRevision,
    /// The events, in order. May be empty: the append is then a pure revision guard.
    pub events: Vec<NewEvent>,
}

/// Everything one command commits in one transaction: the expected-revision checks and events
/// per stream, the command receipt, the inbox entry that caused it, outbox rows, and
/// synchronous projection updates. A journal applies all of it or none of it.
///
/// `recorded_at` is supplied by the writer, read from the control plane's `Clock` port
/// (Unix-epoch wall-clock milliseconds, ADR 0020); the journal reads no clock. It becomes every
/// event's `recorded_at` and the inbox entry's `processed_at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitOfWork {
    /// When the unit was recorded.
    pub recorded_at: Millis,
    /// Per-stream appends, each stream at most once.
    pub streams: Vec<StreamAppend>,
    /// The command's receipt.
    pub receipt: Option<CommandReceipt>,
    /// The inbound message the unit processed.
    pub inbox: Option<InboxEntry>,
    /// Outbox inserts and transitions, each directive at most once.
    pub outbox: Vec<OutboxWrite>,
    /// Projection updates, each `(projection, key)` at most once.
    pub projections: Vec<ProjectionWrite>,
}

impl UnitOfWork {
    /// An empty unit recorded at `recorded_at`; add parts with the `with_*` methods.
    #[must_use]
    pub const fn new(recorded_at: Millis) -> Self {
        Self {
            recorded_at,
            streams: Vec::new(),
            receipt: None,
            inbox: None,
            outbox: Vec::new(),
            projections: Vec::new(),
        }
    }

    /// Adds `events` for `stream`, expected at `expected`.
    #[must_use]
    pub fn with_stream(
        mut self,
        stream: StreamId,
        expected: StreamRevision,
        events: Vec<NewEvent>,
    ) -> Self {
        self.streams.push(StreamAppend {
            stream,
            expected,
            events,
        });
        self
    }

    /// Sets the command receipt.
    #[must_use]
    pub fn with_receipt(mut self, receipt: CommandReceipt) -> Self {
        self.receipt = Some(receipt);
        self
    }

    /// Sets the inbox entry.
    #[must_use]
    pub fn with_inbox(mut self, entry: InboxEntry) -> Self {
        self.inbox = Some(entry);
        self
    }

    /// Adds an outbox write.
    #[must_use]
    pub fn with_outbox(mut self, write: OutboxWrite) -> Self {
        self.outbox.push(write);
        self
    }

    /// Adds a projection write.
    #[must_use]
    pub fn with_projection(mut self, write: ProjectionWrite) -> Self {
        self.projections.push(write);
        self
    }

    /// Whether the unit is well formed on its own, before any journal state is consulted.
    /// Every `Journal::append` checks this first (the journal contract's `malformed_unit`
    /// case; `should_refuse_unit_when_it_names_a_stream_twice` and its siblings here).
    ///
    /// # Errors
    /// The first [`MalformedUnit`] found, in the order the variants are declared.
    pub fn check_shape(&self) -> Result<(), MalformedUnit> {
        if self.streams.is_empty()
            && self.receipt.is_none()
            && self.inbox.is_none()
            && self.outbox.is_empty()
            && self.projections.is_empty()
        {
            return Err(MalformedUnit::Empty);
        }
        let mut streams = BTreeSet::new();
        if !self.streams.iter().all(|s| streams.insert(&s.stream)) {
            return Err(MalformedUnit::DuplicateStream);
        }
        let mut events = BTreeSet::new();
        if !self
            .streams
            .iter()
            .flat_map(|s| &s.events)
            .all(|e| events.insert(e.event_id))
        {
            return Err(MalformedUnit::DuplicateEventId);
        }
        let mut directives = BTreeSet::new();
        if !self
            .outbox
            .iter()
            .all(|w| directives.insert(w.directive_id()))
        {
            return Err(MalformedUnit::DuplicateDirective);
        }
        let mut rows = BTreeSet::new();
        if !self
            .projections
            .iter()
            .all(|p| rows.insert((&p.projection, &p.key)))
        {
            return Err(MalformedUnit::DuplicateProjectionRow);
        }
        Ok(())
    }
}

/// Why a unit of work is malformed regardless of journal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MalformedUnit {
    /// It changes nothing.
    Empty,
    /// It names a stream twice.
    DuplicateStream,
    /// Two of its events share an id.
    DuplicateEventId,
    /// It touches a directive twice.
    DuplicateDirective,
    /// It writes a projection row twice.
    DuplicateProjectionRow,
}

impl fmt::Display for MalformedUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "the unit of work changes nothing",
            Self::DuplicateStream => "the unit of work names a stream twice",
            Self::DuplicateEventId => "two events of the unit of work share an id",
            Self::DuplicateDirective => "the unit of work touches a directive twice",
            Self::DuplicateProjectionRow => "the unit of work writes a projection row twice",
        })
    }
}

/// What a committed unit recorded: its events, in the unit's order, with their streams and
/// revisions (what reactors are notified with after the commit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Appended {
    /// The recorded events.
    pub events: Vec<RecordedEvent>,
}

#[cfg(test)]
mod tests {
    use mayfly_domain::{AttemptId, CommandId, DirectiveId, EventId, Generation};

    use super::*;
    use crate::journal::outbox::{NewDirective, OutboxStatus, OutboxTransition};
    use crate::journal::values::{Payload, ProjectionName, ReceiptStatus};

    fn stream(name: &str) -> StreamId {
        StreamId::new(name).unwrap_or_else(|e| panic!("{e}"))
    }

    fn event(id: u128) -> NewEvent {
        NewEvent {
            event_id: EventId::from_u128(id),
            event_type: "vm_cloned".to_owned(),
            schema_version: 1,
            payload: Payload::new("{}"),
        }
    }

    fn projection(key: &str) -> ProjectionWrite {
        ProjectionWrite {
            projection: ProjectionName::new("attempts").unwrap_or_else(|e| panic!("{e}")),
            key: key.to_owned(),
            value: None,
        }
    }

    #[test]
    fn should_accept_unit_when_it_only_records_a_receipt() {
        // given
        let unit = UnitOfWork::new(Millis::new(1)).with_receipt(CommandReceipt {
            command_id: CommandId::from_u128(1),
            status: ReceiptStatus::Refused,
            result: Payload::new("{}"),
        });

        // when
        let shape = unit.check_shape();

        // then
        assert_eq!(shape, Ok(()));
    }

    #[test]
    fn should_refuse_unit_when_it_changes_nothing() {
        // given
        let unit = UnitOfWork::new(Millis::new(1));

        // when
        let shape = unit.check_shape();

        // then
        assert_eq!(shape, Err(MalformedUnit::Empty));
    }

    #[test]
    fn should_refuse_unit_when_it_names_a_stream_twice() {
        // given
        let unit = UnitOfWork::new(Millis::new(1))
            .with_stream(stream("a-1"), StreamRevision::EMPTY, vec![event(1)])
            .with_stream(stream("a-1"), StreamRevision::EMPTY, vec![event(2)]);

        // when
        let shape = unit.check_shape();

        // then
        assert_eq!(shape, Err(MalformedUnit::DuplicateStream));
    }

    #[test]
    fn should_refuse_unit_when_two_events_share_an_id_across_streams() {
        // given
        let unit = UnitOfWork::new(Millis::new(1))
            .with_stream(stream("a-1"), StreamRevision::EMPTY, vec![event(1)])
            .with_stream(stream("a-2"), StreamRevision::EMPTY, vec![event(1)]);

        // when
        let shape = unit.check_shape();

        // then
        assert_eq!(shape, Err(MalformedUnit::DuplicateEventId));
    }

    #[test]
    fn should_refuse_unit_when_it_touches_a_directive_twice() {
        // given
        let id = DirectiveId::from_u128(9);
        let unit = UnitOfWork::new(Millis::new(1))
            .with_outbox(OutboxWrite::Insert(NewDirective {
                directive_id: id,
                attempt_id: AttemptId::from_u128(1),
                generation: Generation::FIRST,
                kind: "clone_vm".to_owned(),
                request: Payload::new("{}"),
                next_attempt_at: Millis::new(1),
            }))
            .with_outbox(OutboxWrite::Transition(OutboxTransition {
                directive_id: id,
                from: OutboxStatus::Pending,
                to: OutboxStatus::Sent,
                next_attempt_at: Millis::new(1),
            }));

        // when
        let shape = unit.check_shape();

        // then
        assert_eq!(shape, Err(MalformedUnit::DuplicateDirective));
    }

    #[test]
    fn should_refuse_unit_when_it_writes_a_projection_row_twice() {
        // given
        let unit = UnitOfWork::new(Millis::new(1))
            .with_projection(projection("k"))
            .with_projection(projection("k"));

        // when
        let shape = unit.check_shape();

        // then
        assert_eq!(shape, Err(MalformedUnit::DuplicateProjectionRow));
    }
}
