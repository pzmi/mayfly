//! The `Journal` port (plan §5.2): the control plane's only durable state, written one atomic
//! [`UnitOfWork`] at a time.
//!
//! It follows the stream-scoped event store design (ADR 0021): order is per stream, held by a
//! **stream head** compared and advanced by each append (the expected revision); event ids are
//! unique across the journal through an **event-id registry**; **command receipts** make a
//! repeated command answer as its first run did; and there is **no global journal order**: no
//! sequence, no "read everything after position n". Consumers progress per stream, by
//! enumerating [`Journal::streams`] and reading each with [`Journal::read_stream`] after their own
//! checkpoint. No correctness here may rest on a single writer or on a storage row order, so a
//! PostgreSQL adapter can follow the SQLite one (ES-02) with no change to this port.
//!
//! The port is shaped by what `AggregateStore::save` (ES-03), the only caller of
//! [`Journal::append`] (fnd §6.3, `ARCH-single-append`), must commit in one transaction:
//! expected-revision checks and events per stream, the command receipt, the inbox entry that
//! caused the command, outbox rows, and synchronous projection updates. Payloads are already
//! encoded; the journal never parses them, so this crate names no format here.
//!
//! Implementations: `mayfly_testkit::journal::MemoryJournal` and the SQLite journal (ES-02).
//! Both pass one contract suite, `mayfly_testkit::contracts::journal` (ver §5), whose own
//! tests show deliberately broken journals failing named cases.

mod error;
mod outbox;
mod unit;
mod values;

use std::future::Future;

use mayfly_domain::{CommandId, DirectiveId, EventId, Millis};

pub use error::{AppendError, StorageError};
pub use outbox::{NewDirective, OutboxRow, OutboxStatus, OutboxTransition, OutboxWrite};
pub use unit::{Appended, MalformedUnit, StreamAppend, UnitOfWork};
pub use values::{
    CommandReceipt, EventLocation, InboxEntry, InboxSource, JournalValueError, MessageId, NewEvent,
    Payload, ProjectionName, ProjectionWrite, ReceiptStatus, RecordedEvent, RecordedKind, StreamId,
    StreamRevision,
};

/// The event journal with its receipts, inbox, outbox and projection rows.
///
/// A native async trait used through generics (D8); every returned future is `Send`.
///
/// **Atomicity.** [`Journal::append`] commits a whole unit or nothing. Readers never see part
/// of a unit, and a unit that fails leaves every table as it was (contract `all_or_nothing`).
///
/// **Concurrency.** Writers of one stream serialise on its head: of two appends expecting the
/// same revision, exactly one commits and the other is refused with
/// [`AppendError::WrongExpectedRevision`] and no effect (contract `racing_writers_on_one_stream`).
/// Writers of different streams share nothing but the unique keys of the event-id registry, the
/// receipts, the inbox and the outbox: none is refused or rolled back because another stream
/// moved (contract `independent_streams`). A stream with no events has no head: a refused unit,
/// or a unit whose append to a new stream carries no event, leaves it absent.
///
/// **Check order.** A unit is checked in this order and refused with the first failure:
/// 1. its shape ([`UnitOfWork::check_shape`]) → [`AppendError::Malformed`], whatever is stored;
/// 2. its receipt's command id → [`AppendError::DuplicateCommand`], carrying the original
///    receipt, so a repeated command gets its first answer even though the streams have moved;
/// 3. its inbox entry → [`AppendError::DuplicateMessage`], for the same reason on redelivery;
/// 4. each stream's expected revision, in the unit's order → [`AppendError::WrongExpectedRevision`];
/// 5. each event id against the registry → [`AppendError::DuplicateEventId`];
/// 6. each outbox write, in order → `DuplicateDirective`, `UnknownDirective`,
///    `OutboxStatusMismatch`, `IllegalOutboxTransition`.
///
/// Steps 2 and 3 before step 4 are contract cases (`receipt_replay`, `inbox_dedupe`).
///
/// **Cancellation.** `append` is not cancel-safe in the sense of having no effect: once polled,
/// a dropped future leaves the unit wholly committed or wholly absent, never part of it; the
/// caller learns which from the receipt (`MemoryJournal` commits within one poll; ES-02's
/// writer thread must keep the same promise). Every read is cancel-safe (it has no effect).
///
/// **Time.** The journal reads no clock: `recorded_at` comes in the unit (ADR 0020).
pub trait Journal: Send + Sync {
    /// Commits `unit` atomically and returns the events it recorded.
    ///
    /// # Errors
    /// See [`AppendError`]; every variant but `OutcomeUnknown` means nothing was committed.
    fn append(
        &self,
        unit: UnitOfWork,
    ) -> impl Future<Output = Result<Appended, AppendError>> + Send;

    /// The stream head: the stream's current revision ([`StreamRevision::EMPTY`] when it has no
    /// events).
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn stream_revision(
        &self,
        stream: &StreamId,
    ) -> impl Future<Output = Result<StreamRevision, StorageError>> + Send;

    /// Up to `limit` of the stream's events with a revision after `after`, in revision order:
    /// how an aggregate is loaded and how a consumer reads past its per-stream checkpoint.
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn read_stream(
        &self,
        stream: &StreamId,
        after: StreamRevision,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<RecordedEvent>, StorageError>> + Send;

    /// Up to `limit` streams that have events, in ascending stream id order, after `after`
    /// (from the first when `None`): the enumeration a rebuild or a new consumer pages through.
    /// Ordered by id, never by time or position.
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn streams(
        &self,
        after: Option<&StreamId>,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<StreamId>, StorageError>> + Send;

    /// Where `event_id` was recorded, if it was: the event-id registry, through which a retried
    /// append whose ids are stored finds its committed batch.
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn lookup_event(
        &self,
        event_id: EventId,
    ) -> impl Future<Output = Result<Option<EventLocation>, StorageError>> + Send;

    /// Every event kind with at least one recorded event, ascending: the event-kind manifest
    /// the start-up check compares with the kinds this build decodes.
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn recorded_kinds(
        &self,
    ) -> impl Future<Output = Result<Vec<RecordedKind>, StorageError>> + Send;

    /// The receipt of `command_id`, if it ran.
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn receipt(
        &self,
        command_id: CommandId,
    ) -> impl Future<Output = Result<Option<CommandReceipt>, StorageError>> + Send;

    /// When `entry` was processed (its unit's `recorded_at`), if it was.
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn processed_at(
        &self,
        entry: &InboxEntry,
    ) -> impl Future<Output = Result<Option<Millis>, StorageError>> + Send;

    /// The outbox row of `directive_id`, if any.
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn directive(
        &self,
        directive_id: DirectiveId,
    ) -> impl Future<Output = Result<Option<OutboxRow>, StorageError>> + Send;

    /// Up to `limit` outbox rows in `status`, ordered by `next_attempt_at`, then directive id.
    /// The outbox is a queue of effects to send, not a feed of the journal: nothing reads events
    /// through it.
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn directives(
        &self,
        status: OutboxStatus,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<OutboxRow>, StorageError>> + Send;

    /// The projection row `key` of `projection`, if present.
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn projection(
        &self,
        projection: &ProjectionName,
        key: &str,
    ) -> impl Future<Output = Result<Option<Payload>, StorageError>> + Send;
}
