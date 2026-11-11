//! The unit of work: everything one command commits, atomically (`doc/event-store.md` §3.2,
//! fnd §6.3, ES-I1).

use std::collections::BTreeSet;
use std::fmt;

use mayfly_domain::{EventId, Millis};

use crate::journal::values::{
    AggregateName, Decision, ExpectedRevision, InboxKey, MAX_ROW_KEY, NewEvent, NewReceipt,
    ProjectionWrite, ReadCondition, StreamId, StreamRevision, SupportedEpochs, row_key_in_stream,
    valid_snake_name,
};

/// Events for one stream, appended only if the stream is still at `expected`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamAppend {
    /// The stream.
    pub stream: StreamId,
    /// The aggregate the stream belongs to (`Aggregate::NAME`). Its supported epoch judges this
    /// append's events (`EpochAboveBuild`); it is recorded in the head when the append creates
    /// the stream, and from then on the recorded aggregate judges the stream's epoch
    /// (`StreamEpochUnsupported`). The journal does not compare the two: `AggregateStore`
    /// (ES-03) derives both the stream id and this name from one aggregate type.
    pub aggregate: AggregateName,
    /// The head the writer decided against; any other head refuses the whole unit.
    pub expected: ExpectedRevision,
    /// The events, in order; at least one (a guard without events is a read, not an append).
    pub events: Vec<NewEvent>,
}

/// Everything one command commits in one store transaction (ES-I1): the appends with their
/// expected revisions, the validated reads, the command receipt, the inbox entry that caused
/// it, and the inline projection rows. A journal applies all of it or none of it.
///
/// What is **not** here: a directive is a `DirectiveIssued` event in `appends` (E2, ADR 0034;
/// no table of its own), and the dispatch marks that hand it to the `directive_relay` consumer
/// are written by the store itself for every registered consumer of every appended stream
/// (ES-05), so a build marks consumers it does not run. The snapshot request joins with ES-08.
///
/// `recorded_at` is supplied by the writer, read from the control plane's `Clock` port
/// (Unix-epoch wall-clock milliseconds, ADR 0020); the journal reads no clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitOfWork {
    /// Per-stream appends; a stream at most once across `appends` and `reads`.
    pub appends: Vec<StreamAppend>,
    /// Streams the decision read without writing, each refused if it moved.
    pub reads: Vec<ReadCondition>,
    /// The command's receipt.
    pub receipt: Option<NewReceipt>,
    /// The inbound message the unit consumed.
    pub inbox: Option<InboxKey>,
    /// Inline projection writes, each `(projection, stream)` at most once, each for a stream
    /// the unit appends to and keyed within it (ES-I10).
    pub projections: Vec<ProjectionWrite>,
    /// This build's supported epoch per aggregate (ES-I12).
    pub supported_epochs: SupportedEpochs,
    /// When the unit was recorded.
    pub recorded_at: Millis,
}

impl UnitOfWork {
    /// An empty unit recorded at `recorded_at` by a build supporting `supported_epochs`; add
    /// parts with the `with_*` methods.
    #[must_use]
    pub fn new(recorded_at: Millis, supported_epochs: SupportedEpochs) -> Self {
        Self {
            appends: Vec::new(),
            reads: Vec::new(),
            receipt: None,
            inbox: None,
            projections: Vec::new(),
            supported_epochs,
            recorded_at,
        }
    }

    /// Adds `events` for `stream` of `aggregate`, expected at `expected`.
    #[must_use]
    pub fn with_append(
        mut self,
        stream: StreamId,
        aggregate: AggregateName,
        expected: ExpectedRevision,
        events: Vec<NewEvent>,
    ) -> Self {
        self.appends.push(StreamAppend {
            stream,
            aggregate,
            expected,
            events,
        });
        self
    }

    /// Adds a validated read of `stream` at `expected`.
    #[must_use]
    pub fn with_read(mut self, stream: StreamId, expected: ExpectedRevision) -> Self {
        self.reads.push(ReadCondition { stream, expected });
        self
    }

    /// Sets the command receipt.
    #[must_use]
    pub fn with_receipt(mut self, receipt: NewReceipt) -> Self {
        self.receipt = Some(receipt);
        self
    }

    /// Sets the inbox entry.
    #[must_use]
    pub fn with_inbox(mut self, key: InboxKey) -> Self {
        self.inbox = Some(key);
        self
    }

    /// Adds one projection's writes for one stream.
    #[must_use]
    pub fn with_projection(mut self, write: ProjectionWrite) -> Self {
        self.projections.push(write);
        self
    }

    /// Every event id of the unit, in unit order.
    pub fn event_ids(&self) -> impl Iterator<Item = EventId> + '_ {
        self.appends
            .iter()
            .flat_map(|a| a.events.iter().map(|e| e.event_id))
    }

    /// Whether the unit is well formed on its own, before any journal state is consulted.
    /// Every `Journal::append` checks this first (contract case JC-08 `malformed_unit` and
    /// JC-19 `payload_round_trip`; the `should_refuse_unit_when_*` tests here).
    ///
    /// # Errors
    /// The first [`MalformedUnit`] found, in the order the variants are declared.
    pub fn check_shape(&self) -> Result<(), MalformedUnit> {
        if self.appends.is_empty() && self.receipt.is_none() && self.inbox.is_none() {
            return Err(MalformedUnit::Empty);
        }
        if self.appends.iter().any(|a| a.events.is_empty()) {
            return Err(MalformedUnit::EmptyAppend);
        }
        let mut streams = BTreeSet::new();
        let named = self
            .appends
            .iter()
            .map(|a| &a.stream)
            .chain(self.reads.iter().map(|r| &r.stream));
        if !named.into_iter().all(|s| streams.insert(s)) {
            return Err(MalformedUnit::DuplicateStream);
        }
        let mut ids = BTreeSet::new();
        if !self.event_ids().all(|id| ids.insert(id)) {
            return Err(MalformedUnit::DuplicateEventId);
        }
        let storable = |e: ExpectedRevision| match e {
            ExpectedRevision::NoStream => true,
            ExpectedRevision::Exactly(r) => r.is_storable(),
        };
        if !self
            .appends
            .iter()
            .map(|a| a.expected)
            .chain(self.reads.iter().map(|r| r.expected))
            .all(storable)
        {
            return Err(MalformedUnit::RevisionOutOfRange);
        }
        let events = || self.appends.iter().flat_map(|a| &a.events);
        if !events().all(|e| valid_snake_name(&e.kind.event_type)) {
            return Err(MalformedUnit::InvalidEventType);
        }
        let rejection = match &self.receipt {
            Some(NewReceipt {
                decision: Decision::Refused { rejection },
                ..
            }) => Some(rejection),
            _ => None,
        };
        let holds_nul = events().any(|e| e.payload.holds_nul())
            || self
                .projections
                .iter()
                .flat_map(|p| &p.rows)
                .any(|r| r.row.holds_nul())
            || rejection.is_some_and(|r| r.holds_nul());
        if holds_nul {
            return Err(MalformedUnit::NulInPayload);
        }
        if rejection.is_some() && (!self.appends.is_empty() || self.reads.is_empty()) {
            return Err(MalformedUnit::RefusalNotBound);
        }
        if rejection.is_some() && self.inbox.is_some() {
            return Err(MalformedUnit::RefusalWithInbox);
        }
        let mut writes = BTreeSet::new();
        let mut rows = BTreeSet::new();
        let unique = self.projections.iter().all(|p| {
            writes.insert((&p.projection, &p.stream))
                && p.rows.iter().all(|r| rows.insert((&p.projection, &r.key)))
        });
        if !unique {
            return Err(MalformedUnit::DuplicateProjectionRow);
        }
        let local = self.projections.iter().all(|p| {
            self.appends.iter().any(|a| a.stream == p.stream)
                && p.rows.iter().all(|r| row_key_in_stream(&p.stream, &r.key))
        });
        if !local {
            return Err(MalformedUnit::ProjectionOutsideUnit);
        }
        if !self.projections.iter().flat_map(|p| &p.rows).all(|r| {
            (1..=MAX_ROW_KEY).contains(&r.key.len()) && !r.key.chars().any(char::is_control)
        }) {
            return Err(MalformedUnit::InvalidProjectionKey);
        }
        Ok(())
    }

    /// The lowest appended stream, in `StreamId` order like every other refusal that names a
    /// stream (`doc/event-store.md` §3.3), with an event needing more than this build supports:
    /// the [`crate::journal::AppendError::EpochAboveBuild`] check, which reads no stored state.
    /// Pinned by `should_name_the_stream_when_an_event_needs_more_than_the_build_supports`
    /// (two offenders, the higher one first in the unit).
    #[must_use]
    pub fn epoch_above_build(&self) -> Option<&StreamId> {
        self.appends
            .iter()
            .filter(|a| {
                let supported = self.supported_epochs.of(&a.aggregate);
                a.events.iter().any(|e| e.required_epoch > supported)
            })
            .map(|a| &a.stream)
            .min()
    }

    /// Every stream the unit names, appended or read.
    pub fn named_streams(&self) -> impl Iterator<Item = &StreamId> {
        self.appends
            .iter()
            .map(|a| &a.stream)
            .chain(self.reads.iter().map(|r| &r.stream))
    }
}

/// Why a unit of work is malformed regardless of journal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MalformedUnit {
    /// It has no append, no receipt and no inbox entry: it records nothing.
    Empty,
    /// One of its appends has no event.
    EmptyAppend,
    /// It names a stream twice (in `appends`, in `reads`, or in both).
    DuplicateStream,
    /// Two of its events share an id.
    DuplicateEventId,
    /// An expected revision is above what a store can hold.
    RevisionOutOfRange,
    /// An event type is not `[a-z][a-z0-9_]{0,63}`.
    InvalidEventType,
    /// A payload, row or refusal holds a NUL, which PostgreSQL refuses (JC-19).
    NulInPayload,
    /// A refusal receipt with appends, or with no read to bind it to (E3, ADR 0041).
    RefusalNotBound,
    /// A refusal receipt with an inbox entry: a refusal consumes no message, so a redelivery
    /// after the bound streams moved is decided afresh, never dropped as a duplicate (ADR 0041
    /// rule 6; contract case JC-34 `refusal_with_inbox`).
    RefusalWithInbox,
    /// It writes a projection row twice, or one projection's rows of one stream in two writes.
    DuplicateProjectionRow,
    /// A projection write names a stream the unit does not append to, or a row key outside its
    /// stream (ES-I10, contract case PC-05).
    ProjectionOutsideUnit,
    /// A projection key is empty, longer than [`MAX_ROW_KEY`] bytes or holds a control character.
    InvalidProjectionKey,
}

impl fmt::Display for MalformedUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "the unit of work records nothing",
            Self::EmptyAppend => "an append of the unit of work has no event",
            Self::DuplicateStream => "the unit of work names a stream twice",
            Self::DuplicateEventId => "two events of the unit of work share an id",
            Self::RevisionOutOfRange => "an expected revision is above the storable maximum",
            Self::InvalidEventType => "an event type is not a snake_case name",
            Self::NulInPayload => "a payload of the unit of work holds a NUL",
            Self::RefusalNotBound => "a refusal receipt has appends or no read to bind it to",
            Self::RefusalWithInbox => "a refusal receipt consumes an inbox entry",
            Self::DuplicateProjectionRow => "the unit of work writes a projection row twice",
            Self::ProjectionOutsideUnit => "a projection row belongs to a stream not appended",
            Self::InvalidProjectionKey => "a projection key is empty, long or not printable",
        })
    }
}

/// What one appended stream received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendedStream {
    /// The stream.
    pub stream: StreamId,
    /// The revision of its first new event.
    pub first: StreamRevision,
    /// The revision of its last new event: its new head.
    pub last: StreamRevision,
    /// The new events' ids, in order.
    pub event_ids: Vec<EventId>,
}

/// What a committed unit recorded, per stream in the unit's order: the outcome an accepted
/// receipt keeps, and what the after-commit kick names. It carries no payload.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Appended {
    /// The appended streams.
    pub streams: Vec<AppendedStream>,
}

#[cfg(test)]
mod tests {
    use mayfly_domain::CommandId;

    use super::*;
    use crate::journal::values::{
        CommandType, InboxSource, IntentDigest, MessageId, Payload, ProjectionName, RowWrite,
        StoredKind,
    };

    fn stream(name: &str) -> StreamId {
        StreamId::new(name).unwrap_or_else(|e| panic!("{e}"))
    }

    fn attempt() -> AggregateName {
        AggregateName::new("attempt").unwrap_or_else(|e| panic!("{e}"))
    }

    fn event(id: u128) -> NewEvent {
        NewEvent {
            event_id: EventId::from_u128(id),
            kind: StoredKind {
                event_type: "vm_cloned".to_owned(),
                schema_version: 1,
            },
            required_epoch: 1,
            payload: Payload::new("{}"),
        }
    }

    fn unit() -> UnitOfWork {
        UnitOfWork::new(Millis::new(1), SupportedEpochs::new().with(attempt(), 1))
    }

    fn appending(events: Vec<NewEvent>) -> UnitOfWork {
        unit().with_append(stream("a-1"), attempt(), ExpectedRevision::NoStream, events)
    }

    fn receipt(decision: Decision) -> NewReceipt {
        NewReceipt {
            command_id: CommandId::from_u128(1),
            command_type: CommandType::new("start_attempt").unwrap_or_else(|e| panic!("{e}")),
            intent: IntentDigest::new([1; 32]),
            decision,
        }
    }

    fn row(stream_name: &str, key: &str, value: &str) -> ProjectionWrite {
        ProjectionWrite {
            projection: ProjectionName::new("attempts").unwrap_or_else(|e| panic!("{e}")),
            stream: stream(stream_name),
            rows: vec![RowWrite {
                key: key.to_owned(),
                row: Payload::new(value),
            }],
        }
    }

    fn refused() -> Decision {
        Decision::Refused {
            rejection: Payload::new("{\"code\":\"pool_draining\"}"),
        }
    }

    #[test]
    fn should_accept_unit_when_it_only_records_a_bound_refusal_receipt() {
        // given
        let one = unit()
            .with_read(stream("a-1"), ExpectedRevision::NoStream)
            .with_receipt(receipt(refused()));
        let several = unit()
            .with_read(stream("a-1"), ExpectedRevision::NoStream)
            .with_read(
                stream("p-1"),
                ExpectedRevision::Exactly(StreamRevision::new(3)),
            )
            .with_receipt(receipt(refused()));

        // when
        let shapes = [one.check_shape(), several.check_shape()];

        // then
        assert_eq!(shapes, [Ok(()), Ok(())]);
    }

    #[test]
    fn should_accept_unit_when_it_only_records_an_inbox_entry_or_an_accepted_receipt() {
        // given
        let key = InboxKey {
            source: InboxSource::new("scaleset-1").unwrap_or_else(|e| panic!("{e}")),
            message_id: MessageId::new("7").unwrap_or_else(|e| panic!("{e}")),
        };
        let units = [
            unit().with_inbox(key),
            unit().with_receipt(receipt(Decision::Accepted)),
        ];

        // when
        let shapes: Vec<_> = units.iter().map(UnitOfWork::check_shape).collect();

        // then
        assert_eq!(shapes, [Ok(()), Ok(())]);
    }

    #[test]
    fn should_refuse_unit_when_it_records_nothing() {
        // given
        let unit = unit().with_read(stream("a-1"), ExpectedRevision::NoStream);

        // when
        let shape = unit.check_shape();

        // then
        assert_eq!(shape, Err(MalformedUnit::Empty));
    }

    #[test]
    fn should_refuse_unit_when_an_append_has_no_event() {
        // given
        let unit = appending(Vec::new());

        // when
        let shape = unit.check_shape();

        // then
        assert_eq!(shape, Err(MalformedUnit::EmptyAppend));
    }

    #[test]
    fn should_refuse_unit_when_it_names_a_stream_twice() {
        // given
        let twice = appending(vec![event(1)]).with_append(
            stream("a-1"),
            attempt(),
            ExpectedRevision::NoStream,
            vec![event(2)],
        );
        let written_and_read =
            appending(vec![event(1)]).with_read(stream("a-1"), ExpectedRevision::NoStream);

        // when
        let shapes = [twice.check_shape(), written_and_read.check_shape()];

        // then
        assert_eq!(shapes, [Err(MalformedUnit::DuplicateStream); 2]);
    }

    #[test]
    fn should_refuse_unit_when_two_events_share_an_id_across_streams() {
        // given
        let unit = appending(vec![event(1)]).with_append(
            stream("a-2"),
            attempt(),
            ExpectedRevision::NoStream,
            vec![event(1)],
        );

        // when
        let shape = unit.check_shape();

        // then
        assert_eq!(shape, Err(MalformedUnit::DuplicateEventId));
    }

    #[test]
    fn should_refuse_unit_when_an_expected_revision_is_not_storable() {
        // given
        let too_high = ExpectedRevision::Exactly(StreamRevision::new(u64::MAX));
        let unit = appending(vec![event(1)]).with_read(stream("a-2"), too_high);

        // when
        let shape = unit.check_shape();

        // then
        assert_eq!(shape, Err(MalformedUnit::RevisionOutOfRange));
    }

    #[test]
    fn should_refuse_unit_when_an_event_type_is_not_a_snake_name() {
        // given
        let mut bad = event(1);
        bad.kind.event_type = "VmCloned".to_owned();

        // when
        let shape = appending(vec![bad]).check_shape();

        // then
        assert_eq!(shape, Err(MalformedUnit::InvalidEventType));
    }

    #[test]
    fn should_refuse_unit_when_an_event_a_row_or_a_refusal_holds_a_nul() {
        // given
        let mut nul_event = event(1);
        nul_event.payload = Payload::new("{\"n\":\"\\u0000\"}");
        let units = [
            appending(vec![nul_event]),
            appending(vec![event(1)]).with_projection(row("a-1", "a-1", "\u{0}")),
            unit()
                .with_read(stream("a-1"), ExpectedRevision::NoStream)
                .with_receipt(receipt(Decision::Refused {
                    rejection: Payload::new("\u{0}"),
                })),
        ];

        // when
        let shapes: Vec<_> = units.iter().map(UnitOfWork::check_shape).collect();

        // then
        assert_eq!(shapes, [Err(MalformedUnit::NulInPayload); 3]);
    }

    #[test]
    fn should_refuse_unit_when_a_refusal_receipt_has_no_read_or_an_append() {
        // given
        let units = [
            unit().with_receipt(receipt(refused())),
            appending(vec![event(1)]).with_receipt(receipt(refused())),
            appending(vec![event(1)])
                .with_read(stream("a-2"), ExpectedRevision::NoStream)
                .with_receipt(receipt(refused())),
        ];

        // when
        let shapes: Vec<_> = units.iter().map(UnitOfWork::check_shape).collect();

        // then
        assert_eq!(shapes, [Err(MalformedUnit::RefusalNotBound); 3]);
    }

    #[test]
    fn should_refuse_unit_when_a_refusal_receipt_carries_an_inbox_entry() {
        // given
        let key = InboxKey {
            source: InboxSource::new("scaleset-1").unwrap_or_else(|e| panic!("{e}")),
            message_id: MessageId::new("7").unwrap_or_else(|e| panic!("{e}")),
        };
        let unit = unit()
            .with_read(stream("a-1"), ExpectedRevision::NoStream)
            .with_receipt(receipt(refused()))
            .with_inbox(key);

        // when
        let shape = unit.check_shape();

        // then
        assert_eq!(shape, Err(MalformedUnit::RefusalWithInbox));
    }

    #[test]
    fn should_refuse_unit_when_it_writes_a_projection_row_twice() {
        // given
        let mut twice_in_one = row("a-1", "a-1", "{}");
        twice_in_one.rows.extend(row("a-1", "a-1", "{}").rows);
        let units = [
            appending(vec![event(1)])
                .with_projection(row("a-1", "a-1", "{}"))
                .with_projection(row("a-1", "a-1/x", "{}")),
            appending(vec![event(1)]).with_projection(twice_in_one),
        ];

        // when
        let shapes: Vec<_> = units.iter().map(UnitOfWork::check_shape).collect();

        // then
        assert_eq!(shapes, [Err(MalformedUnit::DuplicateProjectionRow); 2]);
    }

    #[test]
    fn should_refuse_unit_when_a_projection_row_belongs_to_another_stream() {
        // given
        let units = [
            appending(vec![event(1)]).with_projection(row("a-2", "a-2", "{}")),
            appending(vec![event(1)]).with_projection(row("a-1", "a-2", "{}")),
            appending(vec![event(1)]).with_projection(row("a-1", "a-2/1", "{}")),
            appending(vec![event(1)]).with_projection(row("a-1", "k", "{}")),
        ];

        // when
        let shapes: Vec<_> = units.iter().map(UnitOfWork::check_shape).collect();

        // then
        assert_eq!(shapes, [Err(MalformedUnit::ProjectionOutsideUnit); 4]);
    }

    #[test]
    fn should_refuse_unit_when_a_projection_key_is_long_or_not_printable() {
        // given
        let long = format!("a-1/{}", "x".repeat(253));
        let units = [
            appending(vec![event(1)]).with_projection(row("a-1", &long, "{}")),
            appending(vec![event(1)]).with_projection(row("a-1", "a-1/a\nb", "{}")),
        ];

        // when
        let shapes: Vec<_> = units.iter().map(UnitOfWork::check_shape).collect();

        // then
        assert_eq!(shapes, [Err(MalformedUnit::InvalidProjectionKey); 2]);
    }

    #[test]
    fn should_name_the_stream_when_an_event_needs_more_than_the_build_supports() {
        // given
        let newer = |id: u128| {
            let mut e = event(id);
            e.required_epoch = 2;
            e
        };
        let unit = appending(vec![event(1)]).with_append(
            stream("a-2"),
            attempt(),
            ExpectedRevision::NoStream,
            vec![newer(2)],
        );
        let two_offenders = appending(vec![event(3)])
            .with_append(
                stream("a-9"),
                attempt(),
                ExpectedRevision::NoStream,
                vec![newer(4)],
            )
            .with_append(
                stream("a-3"),
                attempt(),
                ExpectedRevision::NoStream,
                vec![newer(5)],
            );
        let unknown_aggregate =
            super::UnitOfWork::new(Millis::new(1), SupportedEpochs::new().with(attempt(), 1))
                .with_append(
                    stream("h-1"),
                    AggregateName::new("host").unwrap_or_else(|e| panic!("{e}")),
                    ExpectedRevision::NoStream,
                    vec![event(3)],
                );

        // when
        let above = unit.epoch_above_build();
        let fine = appending(vec![event(1)]).epoch_above_build().cloned();

        // then
        assert_eq!(above, Some(&stream("a-2")));
        assert_eq!(two_offenders.epoch_above_build(), Some(&stream("a-3")));
        assert_eq!(fine, None);
        assert_eq!(unknown_aggregate.epoch_above_build(), Some(&stream("h-1")));
    }
}
