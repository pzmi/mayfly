//! The `Journal` port (plan §5.2, `doc/event-store.md` §3.3): the control plane's durable
//! state, written one atomic [`UnitOfWork`] at a time.
//!
//! It follows the stream-scoped event store design (ADR 0021, amended by ADR 0034): order is per
//! stream, held by a **stream head** compared and advanced by each append (the expected
//! revision); event ids are unique across the journal through an **event-id registry**;
//! **command receipts** carry a canonical intent digest and make a repeated command answer as
//! its first run did, with refusal receipts bound to the revision they were decided at (E3);
//! **stream epochs** keep a build from writing a stream it cannot decide (E6); and there is
//! **no global journal order** (ES-I17): no sequence, no read-all, no head of the journal, and no
//! method here returns events of more than one stream. Readers enumerate [`Journal::streams`]
//! by key and read each with [`Journal::read_stream`] after their own checkpoint. No
//! correctness here rests on a single writer or a storage row order, so the SQLite adapter
//! (ES-02) and the PostgreSQL one (ES-09) implement this port unchanged.
//!
//! **Directives are events** (E2, ADR 0034), with no table or queue of their own: a directive is a
//! `DirectiveIssued` event appended by the unit that decided it, delivered at least once by the
//! `directive_relay` durable consumer. The seam for that consumer is inside
//! [`Journal::append`]: ES-05 adds the dispatch marks the store writes, in the same
//! transaction, for every registered consumer of every appended stream (`doc/event-store.md`
//! §5.1); nothing in the unit names them.
//!
//! The only caller of [`Journal::append`] is `AggregateStore::save` (ES-03; fnd §6.3,
//! `ARCH-single-append`). Payloads are already encoded; the journal never parses them, so this
//! crate names no format here.
//!
//! Implementations: `mayfly_testkit::journal::MemoryJournal`, the SQLite journal (ES-02) and
//! the PostgreSQL journal (ES-09). All pass one contract suite,
//! `mayfly_testkit::contracts::journal` (ver §5), whose own tests show deliberately broken
//! journals failing named cases (`doc/event-store.md` §7.6).

mod error;
mod unit;
mod values;

use std::future::Future;

use mayfly_domain::{CommandId, EventId};

pub use error::{AppendError, StorageError};
pub use unit::{Appended, AppendedStream, MalformedUnit, StreamAppend, UnitOfWork};
pub(crate) use values::valid_snake_name;
pub use values::{
    AggregateName, CommandReceipt, CommandType, Decision, EventLocation, ExpectedRevision,
    InboxKey, InboxSource, IntentDigest, JournalValueError, MessageId, NewEvent, NewReceipt,
    Payload, ProjectionName, ProjectionWrite, ReadCondition, ReceiptOutcome, RecordedEvent,
    StoredKind, StreamHead, StreamId, StreamRevision, SupportedEpochs,
};

/// The largest page [`Journal::streams`] returns; a larger `limit` is clamped to it, so a
/// caller asking for `u32::MAX` gets a page, never an engine error (contract case JC-20).
pub const MAX_PAGE: u32 = 1_000;

/// The event journal with its stream heads, event-id registry, receipts, inbox, event-kind
/// manifest and inline projection rows.
///
/// A native async trait used through generics (D8); every returned future is `Send`.
///
/// **Atomicity (ES-I1).** [`Journal::append`] commits a whole unit or nothing: the events and
/// heads of every stream, the identities, the manifest, the receipt, the inbox entry and the
/// projection rows. A reader never sees part of a unit (contract cases JC-10, JC-32).
///
/// **Reporting order.** A unit is refused with the first applicable [`AppendError`] in the
/// order its variants are declared: shape, epoch above the build, receipt (replay or reuse),
/// inbox, event ids, expected revisions (appends and reads together, lowest `StreamId` first),
/// stream epochs, then the store's own `Busy` and `Storage`. JC-09 pins this order on every
/// store, so two stores refusing one unit for different reasons cannot both pass.
///
/// **Lock order (ES-I16), which is not the reporting order.** An adapter that locks takes, in
/// one transaction: the heads of appended and read streams in `StreamId` order; the receipt
/// key; the inbox key; the event identities in ascending id; then journal rows, manifest rows,
/// projection rows and dispatch marks (`doc/event-store.md` §4.2). Nothing awaits network I/O
/// while holding any of them. Because the order is fixed, no two units wait on each other in a
/// cycle; every wait is bounded and ends in [`AppendError::Busy`].
///
/// **Isolation.** No operation needs `SERIALIZABLE` or relies on it: every refusal comes from
/// data, a compare-and-set of a head or a unique key. PostgreSQL (ES-09) runs `append` at
/// `READ COMMITTED` with `SELECT … FOR NO KEY UPDATE` on written heads and `FOR SHARE` on read
/// heads, in `StreamId` order; SQLite (ES-02) runs it in one `BEGIN IMMEDIATE` transaction.
/// Reads are one statement each.
///
/// **Concurrency.** Writers of one stream serialise on its head: of two appends expecting the
/// same revision, at most one commits and the other is refused `WrongExpectedRevision` with no
/// effect (ES-I2; JC-21). Writers of different streams share only unique keys (event ids,
/// command ids, inbox keys): none is refused because another stream moved, and of two units
/// sharing a key on different streams exactly one commits (ES-I3, ES-I4, ES-I5, ES-I6; JC-22,
/// JC-29, JC-30, JC-31). The contract drives such units at once on one task, so it checks the
/// *outcomes* of the interleavings an implementation's await points allow; it does not prove
/// that two writers of disjoint streams never wait on each other (a global lock that
/// serialises without refusing passes it). The structural half, no waiter blocked by a unit on
/// another stream, is checked on PostgreSQL with `pg_blocking_pids` (ES-09).
///
/// **Unknown outcomes (ES-I18).** Every error but [`AppendError::OutcomeUnknown`] means
/// nothing was committed. On `OutcomeUnknown` the caller re-appends the identical unit: its
/// receipt answers (`CommandReplayed`), or `DuplicateEventId` names an id that
/// [`Journal::lookup_event`] finds where the unit put it, or the unit commits now because the
/// first one never did (JC-25). It never guesses.
///
/// **Cancellation.** Dropping an `append` future once polled leaves the unit wholly committed
/// or wholly absent, never part of it; the caller learns which as for `OutcomeUnknown`. Every
/// read is cancel-safe (it has no effect).
///
/// **Time.** The journal reads no clock: `recorded_at` comes in the unit (ADR 0020) and
/// nothing orders or decides by it.
///
/// Pinned by: `crates/mayfly-testkit/src/contracts/journal.rs`, one function per case, run on
/// memory by `crates/mayfly-testkit/tests/journal_contract.rs::should_pass_journal_contract_when_journal_is_in_memory`
/// and on SQLite by `crates/mayfly-adapter-sqlite/tests/journal_contract.rs`: JC-01
/// `absent_stream`, JC-02 `consecutive_revisions`, JC-03 `continues_from_head`, JC-04
/// `stale_expected_revision`, JC-05 `ahead_expected_revision`, JC-06 `no_stream_mismatch`, JC-07
/// `duplicate_event_id`, JC-08 `malformed_unit`, JC-09 `refusal_order`, JC-10
/// `multi_stream_all_or_nothing`, JC-11 `read_conditions`, JC-12 `receipt_replay`, JC-13
/// `command_id_reused`, JC-14 `store_refusal_leaves_no_receipt`, JC-15 `inbox_dedupe`, JC-16
/// `lookup_event`, JC-17 `kind_manifest`, JC-18 `epochs_case`, JC-19 `payload_round_trip`, JC-20
/// `streams_enumeration`, JC-21 `racing_handles`, JC-22 `independent_streams`, JC-23
/// `crash_atomicity`, JC-24 `survives_reopen`, JC-25 `outcome_unknown`, JC-28
/// `refusal_receipts` (with `refusal_over_two_reads`, ADR 0041), JC-29 `racing_command_ids`,
/// JC-29b `racing_stale_refusal`, JC-30 `racing_inbox_keys`, JC-31 `racing_event_ids`, JC-32
/// `unit_visible_at_once`.
pub trait Journal: Send + Sync {
    /// Commits `unit` atomically and returns what it appended.
    ///
    /// # Errors
    /// See [`AppendError`]; every variant but `OutcomeUnknown` means nothing was committed.
    fn append(
        &self,
        unit: UnitOfWork,
    ) -> impl Future<Output = Result<Appended, AppendError>> + Send;

    /// The stream's head, or `None` when it has no event (contract case JC-01).
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn head(
        &self,
        stream: &StreamId,
    ) -> impl Future<Output = Result<Option<StreamHead>, StorageError>> + Send;

    /// The stream's events with a revision after `after` (from its first when `None`) and at
    /// most `through` (to its head when `None`), in revision order: how an aggregate is loaded
    /// and how a consumer reads past its checkpoint. Never refused for an epoch (ES-I12).
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn read_stream(
        &self,
        stream: &StreamId,
        after: Option<StreamRevision>,
        through: Option<StreamRevision>,
    ) -> impl Future<Output = Result<Vec<RecordedEvent>, StorageError>> + Send;

    /// Where `event_id` was recorded, if it was: the event-id registry (ES-I4), through which a
    /// writer that lost a commit's reply finds its batch.
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn lookup_event(
        &self,
        event_id: EventId,
    ) -> impl Future<Output = Result<Option<EventLocation>, StorageError>> + Send;

    /// The receipt of `command_id`, if one is stored.
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn receipt(
        &self,
        command_id: CommandId,
    ) -> impl Future<Output = Result<Option<CommandReceipt>, StorageError>> + Send;

    /// Whether a committed unit consumed `key`.
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn inbox_contains(
        &self,
        key: &InboxKey,
    ) -> impl Future<Output = Result<bool, StorageError>> + Send;

    /// Up to `limit` (clamped to [`MAX_PAGE`]) stream heads with an id above `after` (from the
    /// first when `None`), in byte order of the id: enumeration by key, never by time or
    /// position (`COLLATE "C"` on PostgreSQL). How a rebuild or a new consumer pages through
    /// the journal (contract case JC-20).
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn streams(
        &self,
        after: Option<&StreamId>,
        limit: u32,
    ) -> impl Future<Output = Result<Vec<StreamHead>, StorageError>> + Send;

    /// Every event kind with at least one committed event, ascending: the event-kind manifest
    /// the start-up check compares with the kinds this build decodes (`doc/event-store.md`
    /// §6.3; contract case JC-17).
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn stored_kinds(&self) -> impl Future<Output = Result<Vec<StoredKind>, StorageError>> + Send;

    /// The projection row `key` of `projection`, if present. An interim read until ES-04's
    /// `ProjectionStore` takes projection rows, with their generations, off this port.
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn projection(
        &self,
        projection: &ProjectionName,
        key: &str,
    ) -> impl Future<Output = Result<Option<Payload>, StorageError>> + Send;
}
