//! The journal contract suite (ver §5, `doc/event-store.md` §7.1): one set of cases every
//! `Journal` implementation passes, written once over a [`JournalFactory`].
//!
//! Cases are named by their id in `doc/event-store.md` §7.1 (`JC-01` …). The suite holds the
//! stream-scoped design (ADR 0021, ADR 0034): per-stream order on a stream head, an event-id
//! registry, command receipts with intent digests and refusal receipts bound to the revisions they read,
//! an inbox, stream epochs, and no global order to read (no method returns events of more
//! than one stream; the suite reads the journal by enumerating streams). JC-26 (durability
//! settings at open) and JC-27 (raw `UPDATE`/`DELETE` refused) need an engine: they are the
//! [`EngineCase`]s, run by [`check_engine_contract`] over an [`EngineJournalFactory`] (the
//! SQLite adapter's `tests/journal_contract.rs`; the PostgreSQL adapter, ES-09).
//!
//! **What the concurrency cases prove, and what they do not.** JC-21, JC-22, JC-29..JC-32
//! (JC-29b included)
//! drive several appends (and a reader) at once on one task ([`join_all`], [`join2`]), through
//! two handles where the factory gives a second one. An adapter whose append really waits (a
//! writer thread, a database) interleaves them at its await points, so the cases check the
//! *outcomes* of the interleavings it allows: exactly one of two racing units commits, a
//! disjoint unit is never refused for another stream, a reader never sees part of a unit. They
//! do not prove freedom from a global lock: a store that serialises every unit without refusing
//! any passes them (memory does, by construction). That structural half (no waiter blocked by
//! a unit on another stream) is ES-09's `pg_blocking_pids` check, and the model's
//! `implStepSerial` relation is the admitted restriction for SQLite (`doc/event-store.md`
//! §7.3).
//!
//! Every case starts from [`JournalFactory::fresh`] and reopens where a case says so, so one
//! suite runs a journal fresh (a factory whose reopen hands the same store back) and reopened
//! (a factory that closes and reopens its file). [`check_journal_contract`] returns a report
//! of failed cases instead of panicking: the suite's own tests use it to show deliberately
//! broken journals failing named cases (`tests/journal_contract.rs`, `doc/event-store.md`
//! §7.6). A factory that cannot inject a fault a case needs fails that case: no case is skipped.

use std::fmt::{self, Debug, Display};
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::task::{Context, Poll};

use mayfly_control::journal::{
    AggregateName, AppendError, Appended, AppendedStream, CommandReceipt, CommandType, Decision,
    EventLocation, ExpectedRevision, InboxKey, InboxSource, IntentDigest, Journal, MAX_PAGE,
    MalformedUnit, MessageId, NewEvent, NewReceipt, Payload, ProjectionName, ProjectionWrite,
    ReadCondition, ReceiptOutcome, RecordedEvent, RowWrite, StoredKind, StreamHead, StreamId,
    StreamRevision, SupportedEpochs, UnitOfWork,
};
use mayfly_domain::{CommandId, EventId, Millis};

use crate::block_on::block_on;

/// A fault the contract asks a factory to arm for the next append (JC-23, JC-25).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectedFault {
    /// The store fails before the commit: a definite `Storage` error, nothing written.
    StorageBeforeCommit,
    /// A bounded lock wait runs out: a definite `Busy`, nothing written.
    Busy,
    /// The unit commits, then its reply is lost: `OutcomeUnknown` with the unit present.
    ReplyLostAfterCommit,
    /// The commit's reply is lost and the commit did not apply: `OutcomeUnknown` with the unit
    /// absent.
    ReplyLostBeforeCommit,
    /// The process dies after the given number of the unit's statements (0: before the first);
    /// the store is then reopened with [`JournalFactory::crash`]. A factory answers `false`
    /// past its last statement, which ends JC-23's walk.
    CrashAfterStatement(u32),
}

/// Makes the journals the suite runs against, and the hooks its modes need.
///
/// Every method is required, so a new adapter cannot skip a mode by omission: memory answers
/// each one trivially, the SQLite adapter (ES-02) with a second connection or file handle,
/// `StoreHooks` and a dropped connection.
pub trait JournalFactory {
    /// The implementation under test.
    type Journal: Journal;

    /// A new, empty journal.
    fn fresh(&self) -> impl Future<Output = Self::Journal>;

    /// The same store, closed and opened again; a store with nothing to reopen returns
    /// `journal` itself. Whatever was committed before must read the same after.
    fn reopen(&self, journal: Self::Journal) -> impl Future<Output = Self::Journal>;

    /// Another handle on the same store, as a second process would hold (two-handle mode):
    /// it shares no cache with `journal` (JC-21, JC-29..JC-31).
    fn second_handle(&self, journal: &Self::Journal) -> impl Future<Output = Self::Journal>;

    /// Arms `fault` for the next append on `journal`'s store; `false` when this factory cannot
    /// (the case then fails, it never skips).
    fn inject(&self, journal: &Self::Journal, fault: InjectedFault) -> impl Future<Output = bool>;

    /// The process holding `journal` died; the store, reopened (crash-injected mode, JC-23).
    fn crash(&self, journal: Self::Journal) -> impl Future<Output = Self::Journal>;
}

/// One case of the journal contract, named after its id in `doc/event-store.md` §7.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum JournalCase {
    /// JC-01: an absent stream reads empty and has no head; an empty journal lists nothing.
    AbsentStream,
    /// JC-02: a batch takes consecutive revisions from 0 and reads back as written.
    ConsecutiveRevisions,
    /// JC-03: a later append continues from the head; reads honour `after` and `through`.
    ContinuesFromHead,
    /// JC-04: a stale expected revision is refused and writes nothing in any table.
    StaleExpectedRevision,
    /// JC-05: an expected revision ahead of the head is refused.
    AheadExpectedRevision,
    /// JC-06: `Exactly` on an absent stream and `NoStream` on an existing one are refused.
    NoStreamMismatch,
    /// JC-07: an event id stored on another stream refuses the unit, nothing written.
    DuplicateEventIdAcrossStreams,
    /// JC-08: a malformed unit is refused before anything is checked or written.
    MalformedUnit,
    /// JC-09: the reporting order of `AppendError` for a unit that breaks several rules.
    RefusalOrder,
    /// JC-10: a multi-stream unit whose second stream conflicts writes nothing on the first;
    /// an accepted unit writes every part.
    MultiStreamAllOrNothing,
    /// JC-11: a moved read condition refuses the unit; a `NoStream` read is invalidated by a
    /// create; a held read writes nothing of its own.
    ReadConditions,
    /// JC-12: a replayed command returns its first outcome after its streams moved, writing
    /// nothing; a zero-event acceptance answers like any other.
    ReceiptReplay,
    /// JC-13: a reused command id with another intent is `CommandIdReused`.
    CommandIdReused,
    /// JC-14: a unit the store refuses leaves no receipt and the command may later commit; a
    /// domain refusal leaves a refusal receipt bound to its revision.
    StoreRefusalLeavesNoReceipt,
    /// JC-15: a redelivered inbox key is `DuplicateMessage`; an inbox-only unit commits.
    InboxDedupe,
    /// JC-16: `lookup_event` finds each id at its place and none of a refused batch.
    LookupEvent,
    /// JC-17: the manifest lists every kind ever committed and none of a refused unit.
    KindManifest,
    /// JC-18: epochs rise to the batch maximum and never fall; a stream above the build refuses
    /// appends but serves reads; a batch above the build is refused before any write.
    Epochs,
    /// JC-19: payloads round-trip byte for byte; a NUL is refused before any write; errors carry
    /// no payload text.
    PayloadRoundTrip,
    /// JC-20: `streams` pages through every head once, in byte order of the id.
    StreamsEnumeration,
    /// JC-21: two handles decide against the same revision; exactly one append commits.
    RacingHandles,
    /// JC-22: units on disjoint streams both commit; units naming two streams in opposite
    /// orders both finish.
    IndependentStreams,
    /// JC-23: a crash after any statement of a unit leaves it wholly present or wholly absent.
    CrashAtomicity,
    /// JC-24: heads, receipts, inbox, manifest, epochs and rows survive a reopen and still
    /// answer.
    SurvivesReopen,
    /// JC-25: only a fault at the commit is `OutcomeUnknown`; re-appending the identical unit
    /// resolves it.
    OutcomeUnknown,
    /// JC-28: a refusal receipt answers at its revisions and is re-decided and replaced after
    /// any bound stream moved (a refusal decided over two streams included); one whose stream
    /// moved before commit is refused.
    RefusalReceipts,
    /// JC-29: two units with one command id on different streams: exactly one commits.
    RacingCommandIds,
    /// JC-29b: two units re-deciding one command whose refusal receipt went stale (its stream
    /// moved), on different streams: exactly one commits and replaces the receipt, the other
    /// is answered by the winner's receipt.
    RacingStaleRefusal,
    /// JC-30: two units with one inbox key on different streams: exactly one commits.
    RacingInboxKeys,
    /// JC-31: two units with one event id on different streams: exactly one commits.
    RacingEventIds,
    /// JC-32: a reader running beside an append never sees part of the unit.
    UnitVisibleAtOnce,
    /// JC-33: a receipt answers a repeat only while every stream its answer is for is within
    /// the repeating build's epoch, judged with the receipt; otherwise `StreamEpochUnsupported`
    /// naming the lowest such stream, so an older build is never answered from a newer build's
    /// outcome.
    ReplayWithinEpoch,
    /// JC-34: a refusal receipt carrying an inbox key is `Malformed(RefusalWithInbox)` and
    /// consumes nothing.
    RefusalWithInbox,
}

impl JournalCase {
    /// Every case, in the order the suite runs them.
    pub const ALL: [Self; 33] = [
        Self::AbsentStream,
        Self::ConsecutiveRevisions,
        Self::ContinuesFromHead,
        Self::StaleExpectedRevision,
        Self::AheadExpectedRevision,
        Self::NoStreamMismatch,
        Self::DuplicateEventIdAcrossStreams,
        Self::MalformedUnit,
        Self::RefusalOrder,
        Self::MultiStreamAllOrNothing,
        Self::ReadConditions,
        Self::ReceiptReplay,
        Self::CommandIdReused,
        Self::StoreRefusalLeavesNoReceipt,
        Self::InboxDedupe,
        Self::LookupEvent,
        Self::KindManifest,
        Self::Epochs,
        Self::PayloadRoundTrip,
        Self::StreamsEnumeration,
        Self::RacingHandles,
        Self::IndependentStreams,
        Self::CrashAtomicity,
        Self::SurvivesReopen,
        Self::OutcomeUnknown,
        Self::RefusalReceipts,
        Self::RacingCommandIds,
        Self::RacingStaleRefusal,
        Self::RacingInboxKeys,
        Self::RacingEventIds,
        Self::UnitVisibleAtOnce,
        Self::ReplayWithinEpoch,
        Self::RefusalWithInbox,
    ];

    /// The case's id in `doc/event-store.md` §7.1.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::AbsentStream => "JC-01",
            Self::ConsecutiveRevisions => "JC-02",
            Self::ContinuesFromHead => "JC-03",
            Self::StaleExpectedRevision => "JC-04",
            Self::AheadExpectedRevision => "JC-05",
            Self::NoStreamMismatch => "JC-06",
            Self::DuplicateEventIdAcrossStreams => "JC-07",
            Self::MalformedUnit => "JC-08",
            Self::RefusalOrder => "JC-09",
            Self::MultiStreamAllOrNothing => "JC-10",
            Self::ReadConditions => "JC-11",
            Self::ReceiptReplay => "JC-12",
            Self::CommandIdReused => "JC-13",
            Self::StoreRefusalLeavesNoReceipt => "JC-14",
            Self::InboxDedupe => "JC-15",
            Self::LookupEvent => "JC-16",
            Self::KindManifest => "JC-17",
            Self::Epochs => "JC-18",
            Self::PayloadRoundTrip => "JC-19",
            Self::StreamsEnumeration => "JC-20",
            Self::RacingHandles => "JC-21",
            Self::IndependentStreams => "JC-22",
            Self::CrashAtomicity => "JC-23",
            Self::SurvivesReopen => "JC-24",
            Self::OutcomeUnknown => "JC-25",
            Self::RefusalReceipts => "JC-28",
            Self::RacingCommandIds => "JC-29",
            Self::RacingStaleRefusal => "JC-29b",
            Self::RacingInboxKeys => "JC-30",
            Self::RacingEventIds => "JC-31",
            Self::UnitVisibleAtOnce => "JC-32",
            Self::ReplayWithinEpoch => "JC-33",
            Self::RefusalWithInbox => "JC-34",
        }
    }

    /// The case's `snake_case` name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::AbsentStream => "absent_stream",
            Self::ConsecutiveRevisions => "consecutive_revisions",
            Self::ContinuesFromHead => "continues_from_head",
            Self::StaleExpectedRevision => "stale_expected_revision",
            Self::AheadExpectedRevision => "ahead_expected_revision",
            Self::NoStreamMismatch => "no_stream_mismatch",
            Self::DuplicateEventIdAcrossStreams => "duplicate_event_id_across_streams",
            Self::MalformedUnit => "malformed_unit",
            Self::RefusalOrder => "refusal_order",
            Self::MultiStreamAllOrNothing => "multi_stream_all_or_nothing",
            Self::ReadConditions => "read_conditions",
            Self::ReceiptReplay => "receipt_replay",
            Self::CommandIdReused => "command_id_reused",
            Self::StoreRefusalLeavesNoReceipt => "store_refusal_leaves_no_receipt",
            Self::InboxDedupe => "inbox_dedupe",
            Self::LookupEvent => "lookup_event",
            Self::KindManifest => "kind_manifest",
            Self::Epochs => "epochs",
            Self::PayloadRoundTrip => "payload_round_trip",
            Self::StreamsEnumeration => "streams_enumeration",
            Self::RacingHandles => "racing_handles",
            Self::IndependentStreams => "independent_streams",
            Self::CrashAtomicity => "crash_atomicity",
            Self::SurvivesReopen => "survives_reopen",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::RefusalReceipts => "refusal_receipts",
            Self::RacingCommandIds => "racing_command_ids",
            Self::RacingStaleRefusal => "racing_stale_refusal",
            Self::RacingInboxKeys => "racing_inbox_keys",
            Self::RacingEventIds => "racing_event_ids",
            Self::UnitVisibleAtOnce => "unit_visible_at_once",
            Self::ReplayWithinEpoch => "replay_within_epoch",
            Self::RefusalWithInbox => "refusal_with_inbox",
        }
    }
}

impl Display for JournalCase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.id(), self.name())
    }
}

/// A failed case and the first check in it that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseFailure {
    /// The case.
    pub case: JournalCase,
    /// What was expected and what the journal did.
    pub detail: String,
}

/// The outcome of a contract run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JournalContractReport {
    /// Every failed case, in run order.
    pub failures: Vec<CaseFailure>,
}

impl JournalContractReport {
    /// Whether every case passed.
    #[must_use]
    pub fn is_green(&self) -> bool {
        self.failures.is_empty()
    }

    /// The failed cases, in run order.
    #[must_use]
    pub fn failed_cases(&self) -> Vec<JournalCase> {
        self.failures.iter().map(|f| f.case).collect()
    }
}

impl Display for JournalContractReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_green() {
            return f.write_str("journal contract: every case passed");
        }
        writeln!(
            f,
            "journal contract: {} case(s) failed",
            self.failures.len()
        )?;
        for failure in &self.failures {
            writeln!(f, "- {}: {}", failure.case, failure.detail)?;
        }
        Ok(())
    }
}

/// Runs every case against journals from `factory`.
pub async fn check_journal_contract<F: JournalFactory>(factory: &F) -> JournalContractReport {
    let mut report = JournalContractReport::default();
    for case in JournalCase::ALL {
        if let Err(detail) = check_case(factory, case).await {
            report.failures.push(CaseFailure { case, detail });
        }
    }
    report
}

/// Runs every case and panics with the report unless all pass: the form an implementation's
/// own test calls when its journal needs no runtime.
///
/// # Panics
/// When a case fails; the message lists each failed case and why.
pub fn run_journal_contract<F: JournalFactory>(factory: &F) {
    let report = block_on(check_journal_contract(factory));
    assert!(report.is_green(), "{report}");
}

/// Runs one case.
///
/// # Errors
/// What was expected and what the journal did, at the first check that failed.
pub async fn check_case<F: JournalFactory>(factory: &F, case: JournalCase) -> Result<(), String> {
    match case {
        JournalCase::AbsentStream => absent_stream(factory).await,
        JournalCase::ConsecutiveRevisions => consecutive_revisions(factory).await,
        JournalCase::ContinuesFromHead => continues_from_head(factory).await,
        JournalCase::StaleExpectedRevision => stale_expected_revision(factory).await,
        JournalCase::AheadExpectedRevision => ahead_expected_revision(factory).await,
        JournalCase::NoStreamMismatch => no_stream_mismatch(factory).await,
        JournalCase::DuplicateEventIdAcrossStreams => duplicate_event_id(factory).await,
        JournalCase::MalformedUnit => malformed_unit(factory).await,
        JournalCase::RefusalOrder => refusal_order(factory).await,
        JournalCase::MultiStreamAllOrNothing => multi_stream_all_or_nothing(factory).await,
        JournalCase::ReadConditions => read_conditions(factory).await,
        JournalCase::ReceiptReplay => receipt_replay(factory).await,
        JournalCase::CommandIdReused => command_id_reused(factory).await,
        JournalCase::StoreRefusalLeavesNoReceipt => store_refusal_leaves_no_receipt(factory).await,
        JournalCase::InboxDedupe => inbox_dedupe(factory).await,
        JournalCase::LookupEvent => lookup_event(factory).await,
        JournalCase::KindManifest => kind_manifest(factory).await,
        JournalCase::Epochs => epochs_case(factory).await,
        JournalCase::PayloadRoundTrip => payload_round_trip(factory).await,
        JournalCase::StreamsEnumeration => streams_enumeration(factory).await,
        JournalCase::RacingHandles => racing_handles(factory).await,
        JournalCase::IndependentStreams => independent_streams(factory).await,
        JournalCase::CrashAtomicity => crash_atomicity(factory).await,
        JournalCase::SurvivesReopen => survives_reopen(factory).await,
        JournalCase::OutcomeUnknown => outcome_unknown(factory).await,
        JournalCase::RefusalReceipts => refusal_receipts(factory).await,
        JournalCase::RacingCommandIds => racing_command_ids(factory).await,
        JournalCase::RacingStaleRefusal => racing_stale_refusal(factory).await,
        JournalCase::RacingInboxKeys => racing_inbox_keys(factory).await,
        JournalCase::RacingEventIds => racing_event_ids(factory).await,
        JournalCase::UnitVisibleAtOnce => unit_visible_at_once(factory).await,
        JournalCase::ReplayWithinEpoch => replay_within_epoch(factory).await,
        JournalCase::RefusalWithInbox => refusal_with_inbox(factory).await,
    }
}

// ---------------------------------------------------------------------------------------------
// Checks and fixtures

type Check = Result<(), String>;

fn eq<T: PartialEq + Debug>(what: &str, got: &T, want: &T) -> Check {
    if got == want {
        Ok(())
    } else {
        Err(format!("{what}: got {got:?}, want {want:?}"))
    }
}

fn ensure(what: &str, holds: bool) -> Check {
    if holds { Ok(()) } else { Err(what.to_owned()) }
}

fn io<T, E: Display>(what: &str, result: Result<T, E>) -> Result<T, String> {
    result.map_err(|e| format!("{what}: unexpected error: {e}"))
}

fn refused<T: Debug>(result: Result<T, AppendError>) -> Result<AppendError, String> {
    match result {
        Ok(done) => Err(format!("expected a refusal, the unit committed: {done:?}")),
        Err(error) => Ok(error),
    }
}

fn stream(name: &str) -> StreamId {
    StreamId::new(name).unwrap_or_else(|e| panic!("contract stream id {name}: {e}"))
}

/// The aggregate of a contract stream: its prefix before the first `-`.
fn aggregate_of(stream: &StreamId) -> AggregateName {
    let prefix = stream.as_str().split('-').next().unwrap_or_default();
    AggregateName::new(prefix).unwrap_or_else(|e| panic!("contract aggregate {prefix}: {e}"))
}

fn aggregate(name: &str) -> AggregateName {
    AggregateName::new(name).unwrap_or_else(|e| panic!("contract aggregate {name}: {e}"))
}

fn rev(n: u64) -> StreamRevision {
    StreamRevision::new(n)
}

fn at(n: u64) -> ExpectedRevision {
    ExpectedRevision::Exactly(rev(n))
}

const NEW: ExpectedRevision = ExpectedRevision::NoStream;

/// A build supporting epoch `epoch` for the contract's aggregates (`attempt`, `pool`).
fn build(epoch: u32) -> SupportedEpochs {
    SupportedEpochs::new()
        .with(aggregate("attempt"), epoch)
        .with(aggregate("pool"), epoch)
        .with(aggregate("scaleset"), epoch)
}

fn unit(ms: u64) -> UnitOfWork {
    UnitOfWork::new(Millis::new(ms), build(1))
}

fn kind(event_type: &str, schema_version: u32) -> StoredKind {
    StoredKind {
        event_type: event_type.to_owned(),
        schema_version,
    }
}

fn event(n: u128) -> NewEvent {
    NewEvent {
        event_id: EventId::from_u128(n),
        kind: kind("contract_fact_recorded", 1),
        required_epoch: 1,
        payload: Payload::new(format!("{{\"n\":{n}}}")),
    }
}

fn events(ids: &[u128]) -> Vec<NewEvent> {
    ids.iter().copied().map(event).collect()
}

/// Adds an append of `ids` to `s` at `expected`, its aggregate taken from its prefix.
fn appending(
    unit: UnitOfWork,
    s: &StreamId,
    expected: ExpectedRevision,
    ids: &[u128],
) -> UnitOfWork {
    unit.with_append(s.clone(), aggregate_of(s), expected, events(ids))
}

fn intent(n: u8) -> IntentDigest {
    IntentDigest::new([n; 32])
}

fn command_type() -> CommandType {
    CommandType::new("contract_command").unwrap_or_else(|e| panic!("contract command type: {e}"))
}

fn accepted(command: u128, intent_of: u8) -> NewReceipt {
    NewReceipt {
        command_id: CommandId::from_u128(command),
        command_type: command_type(),
        intent: intent(intent_of),
        decision: Decision::Accepted,
    }
}

fn rejection(code: &str) -> Payload {
    Payload::new(format!("{{\"code\":\"{code}\"}}"))
}

fn refusal(command: u128, intent_of: u8, code: &str) -> NewReceipt {
    NewReceipt {
        command_id: CommandId::from_u128(command),
        command_type: command_type(),
        intent: intent(intent_of),
        decision: Decision::Refused {
            rejection: rejection(code),
        },
    }
}

fn key(source: &str, message: &str) -> InboxKey {
    InboxKey {
        source: InboxSource::new(source)
            .unwrap_or_else(|e| panic!("contract inbox source {source}: {e}")),
        message_id: MessageId::new(message)
            .unwrap_or_else(|e| panic!("contract message id {message}: {e}")),
    }
}

fn projection_name() -> ProjectionName {
    ProjectionName::new("contract_rows").unwrap_or_else(|e| panic!("contract projection: {e}"))
}

/// The stored key of the contract row tagged `tag` of stream `s`: within the stream (ES-I10).
fn row_key(s: &StreamId, tag: &str) -> String {
    format!("{s}/{tag}")
}

fn row(s: &StreamId, tag: &str, value: Option<&str>) -> ProjectionWrite {
    ProjectionWrite {
        projection: projection_name(),
        stream: s.clone(),
        rows: vec![RowWrite {
            key: row_key(s, tag),
            row: value.map(Payload::new),
        }],
    }
}

fn ids(events: &[RecordedEvent]) -> Vec<u128> {
    events.iter().map(|e| e.event_id.as_u128()).collect()
}

fn appended(s: &StreamId, first: u64, ids: &[u128]) -> AppendedStream {
    AppendedStream {
        stream: s.clone(),
        first: rev(first),
        last: rev(first + ids.len() as u64 - 1),
        event_ids: ids.iter().copied().map(EventId::from_u128).collect(),
    }
}

/// What a [`Probe`] reads of a journal: enough to prove a refused unit left no trace, read
/// stream by stream and key by key (no global read exists).
#[derive(Debug, Clone, PartialEq)]
struct Observed {
    heads: Vec<Option<StreamHead>>,
    events: Vec<Vec<RecordedEvent>>,
    receipts: Vec<Option<CommandReceipt>>,
    inbox: Vec<bool>,
    identities: Vec<Option<EventLocation>>,
    kinds: Vec<StoredKind>,
    rows: Vec<Option<Payload>>,
    streams: Vec<StreamHead>,
}

/// The streams, commands, inbox keys, event ids and projection rows a case looks at.
#[derive(Debug, Clone, Default)]
struct Probe {
    streams: Vec<StreamId>,
    commands: Vec<u128>,
    keys: Vec<InboxKey>,
    events: Vec<u128>,
    rows: Vec<String>,
}

async fn observe<J: Journal>(journal: &J, probe: &Probe) -> Result<Observed, String> {
    let mut seen = Observed {
        heads: Vec::new(),
        events: Vec::new(),
        receipts: Vec::new(),
        inbox: Vec::new(),
        identities: Vec::new(),
        kinds: io("stored_kinds", journal.stored_kinds().await)?,
        rows: Vec::new(),
        streams: all_streams(journal).await?,
    };
    for s in &probe.streams {
        seen.heads.push(io("head", journal.head(s).await)?);
        seen.events
            .push(io("read_stream", journal.read_stream(s, None, None).await)?);
    }
    for c in &probe.commands {
        seen.receipts.push(io(
            "receipt",
            journal.receipt(CommandId::from_u128(*c)).await,
        )?);
    }
    for k in &probe.keys {
        seen.inbox
            .push(io("inbox_contains", journal.inbox_contains(k).await)?);
    }
    for e in &probe.events {
        seen.identities.push(io(
            "lookup_event",
            journal.lookup_event(EventId::from_u128(*e)).await,
        )?);
    }
    for r in &probe.rows {
        let mut found = None;
        for s in &probe.streams {
            if found.is_none() {
                found = io(
                    "projection",
                    journal.projection(&projection_name(), &row_key(s, r)).await,
                )?;
            }
        }
        seen.rows.push(found);
    }
    Ok(seen)
}

/// Every stream head, paged by key.
async fn all_streams<J: Journal>(journal: &J) -> Result<Vec<StreamHead>, String> {
    let mut heads: Vec<StreamHead> = Vec::new();
    loop {
        let after = heads.last().map(|h| h.stream.clone());
        let page = io("streams", journal.streams(after.as_ref(), 3).await)?;
        if page.is_empty() {
            return Ok(heads);
        }
        heads.extend(page);
        if heads.len() > 10_000 {
            return Err("streams paged past 10000 heads: the cursor does not advance".to_owned());
        }
    }
}

async fn head_revision<J: Journal>(
    journal: &J,
    s: &StreamId,
) -> Result<Option<StreamRevision>, String> {
    Ok(io("head", journal.head(s).await)?.map(|h| h.revision))
}

async fn must_inject<F: JournalFactory>(
    factory: &F,
    journal: &F::Journal,
    fault: InjectedFault,
) -> Check {
    ensure(
        &format!("the factory cannot inject {fault:?}, so the case cannot run (it never skips)"),
        factory.inject(journal, fault).await,
    )
}

/// Polls every future on the calling task until all are ready: appends run "at once" without
/// a runtime, interleaving wherever an implementation yields.
pub async fn join_all<F: Future>(futures: Vec<F>) -> Vec<F::Output> {
    let mut pending: Vec<Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
    let mut outputs: Vec<Option<F::Output>> = pending.iter().map(|_| None).collect();
    poll_fn(|cx| {
        for (future, output) in pending.iter_mut().zip(outputs.iter_mut()) {
            if output.is_none()
                && let Poll::Ready(value) = future.as_mut().poll(cx)
            {
                *output = Some(value);
            }
        }
        if outputs.iter().all(Option::is_some) {
            Poll::Ready(outputs.iter_mut().filter_map(Option::take).collect())
        } else {
            Poll::Pending
        }
    })
    .await
}

/// Polls two futures of different types on the calling task until both are ready.
pub async fn join2<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let (mut a, mut b) = (Box::pin(a), Box::pin(b));
    let (mut out_a, mut out_b) = (None, None);
    poll_fn(|cx| {
        if out_a.is_none()
            && let Poll::Ready(v) = a.as_mut().poll(cx)
        {
            out_a = Some(v);
        }
        if out_b.is_none()
            && let Poll::Ready(v) = b.as_mut().poll(cx)
        {
            out_b = Some(v);
        }
        match (out_a.take(), out_b.take()) {
            (Some(x), Some(y)) => Poll::Ready((x, y)),
            (x, y) => {
                out_a = x;
                out_b = y;
                Poll::Pending
            }
        }
    })
    .await
}

/// Pending once, waking itself: lets the other futures of a join run.
struct YieldNow(bool);

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

/// A unit numbered `n`: `event_id` on `s` at `expected`, with receipt `n`, inbox key `n` and a
/// projection row `n`, so a loser's leftovers can be looked for with [`traces_of`].
fn tagged(n: u128, s: &StreamId, expected: ExpectedRevision, event_id: u128) -> UnitOfWork {
    appending(unit(5_000), s, expected, &[event_id])
        .with_receipt(accepted(n, 1))
        .with_inbox(key("scaleset-1", &n.to_string()))
        .with_projection(row(s, &n.to_string(), Some("{}")))
}

/// Whether the event, receipt, inbox entry and row of [`tagged`] unit `n` are stored.
async fn traces_of<J: Journal>(
    journal: &J,
    s: &StreamId,
    n: u128,
    event_id: u128,
) -> Result<[bool; 4], String> {
    Ok([
        io(
            "lookup_event",
            journal.lookup_event(EventId::from_u128(event_id)).await,
        )?
        .is_some(),
        io("receipt", journal.receipt(CommandId::from_u128(n)).await)?.is_some(),
        io(
            "inbox_contains",
            journal
                .inbox_contains(&key("scaleset-1", &n.to_string()))
                .await,
        )?,
        io(
            "projection",
            journal
                .projection(&projection_name(), &row_key(s, &n.to_string()))
                .await,
        )?
        .is_some(),
    ])
}

/// Checks that exactly one of two racing outcomes committed and the other is `lost`, then
/// returns `(winner index, loser index)`.
fn one_winner(
    what: &str,
    outcomes: &[Result<Appended, AppendError>],
    lost: impl Fn(usize) -> AppendError,
) -> Result<(usize, usize), String> {
    let (winner, loser) = match outcomes {
        [Ok(_), Err(_)] => (0, 1),
        [Err(_), Ok(_)] => (1, 0),
        other => {
            return Err(format!(
                "{what}: exactly one of two racing units must commit, got {other:?}"
            ));
        }
    };
    let got = outcomes
        .get(loser)
        .cloned()
        .map(|o| o.map(|_| ()))
        .unwrap_or(Ok(()));
    eq(
        &format!("{what}: the loser's answer"),
        &got,
        &Err(lost(loser)),
    )?;
    Ok((winner, loser))
}

// ---------------------------------------------------------------------------------------------
// JC-01 .. JC-06: stream heads and expected revisions

async fn absent_stream<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let s1 = stream("attempt-1");
    eq(
        "head of an absent stream",
        &io("head", journal.head(&s1).await)?,
        &None,
    )?;
    eq(
        "events of an absent stream",
        &io("read_stream", journal.read_stream(&s1, None, None).await)?,
        &Vec::new(),
    )?;
    eq(
        "streams of an empty journal",
        &all_streams(&journal).await?,
        &Vec::new(),
    )?;
    eq(
        "kinds of an empty journal",
        &io("stored_kinds", journal.stored_kinds().await)?,
        &Vec::new(),
    )?;
    eq(
        "an id in an empty journal",
        &io(
            "lookup_event",
            journal.lookup_event(EventId::from_u128(1)).await,
        )?,
        &None,
    )?;
    eq(
        "a receipt in an empty journal",
        &io("receipt", journal.receipt(CommandId::from_u128(1)).await)?,
        &None,
    )?;
    eq(
        "an inbox key in an empty journal",
        &io(
            "inbox_contains",
            journal.inbox_contains(&key("scaleset-1", "1")).await,
        )?,
        &false,
    )?;
    let journal = factory.reopen(journal).await;
    eq(
        "head after reopen",
        &io("head", journal.head(&s1).await)?,
        &None,
    )
}

async fn consecutive_revisions<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let s1 = stream("attempt-1");
    let mut other = event(3);
    other.kind = kind("contract_other_recorded", 3);
    let written = io(
        "first append",
        journal
            .append(unit(1_000).with_append(
                s1.clone(),
                aggregate("attempt"),
                NEW,
                vec![event(1), event(2), other.clone()],
            ))
            .await,
    )?;
    eq(
        "what the append reports",
        &written,
        &Appended {
            streams: vec![appended(&s1, 0, &[1, 2, 3])],
        },
    )?;
    let journal = factory.reopen(journal).await;
    let read = io("read_stream", journal.read_stream(&s1, None, None).await)?;
    eq(
        "revisions from 0",
        &read.iter().map(|e| e.revision).collect::<Vec<_>>(),
        &vec![rev(0), rev(1), rev(2)],
    )?;
    let third = read
        .get(2)
        .ok_or_else(|| "the stream holds fewer than three events".to_owned())?;
    eq(
        "the third event as recorded",
        third,
        &RecordedEvent {
            stream: s1.clone(),
            revision: rev(2),
            event_id: other.event_id,
            kind: other.kind,
            payload: other.payload,
            recorded_at: Millis::new(1_000),
        },
    )?;
    eq(
        "the head",
        &io("head", journal.head(&s1).await)?,
        &Some(StreamHead {
            stream: s1.clone(),
            revision: rev(2),
            aggregate: aggregate("attempt"),
            epoch: 1,
        }),
    )
}

async fn continues_from_head<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let s1 = stream("attempt-1");
    io(
        "seed",
        journal
            .append(appending(unit(1_000), &s1, NEW, &[1, 2]))
            .await,
    )?;
    let journal = factory.reopen(journal).await;
    let second = io(
        "second append",
        journal
            .append(appending(unit(2_000), &s1, at(1), &[3, 4]))
            .await,
    )?;
    eq(
        "the second append",
        &second.streams,
        &vec![appended(&s1, 2, &[3, 4])],
    )?;
    let journal = factory.reopen(journal).await;
    let read = |after: Option<u64>, through: Option<u64>| {
        journal.read_stream(&s1, after.map(rev), through.map(rev))
    };
    eq(
        "the whole stream",
        &ids(&io("read_stream", read(None, None).await)?),
        &vec![1, 2, 3, 4],
    )?;
    eq(
        "after 1",
        &ids(&io("read_stream", read(Some(1), None).await)?),
        &vec![3, 4],
    )?;
    eq(
        "through 1",
        &ids(&io("read_stream", read(None, Some(1)).await)?),
        &vec![1, 2],
    )?;
    eq(
        "after 0 through 2",
        &ids(&io("read_stream", read(Some(0), Some(2)).await)?),
        &vec![2, 3],
    )?;
    eq(
        "after the head",
        &ids(&io("read_stream", read(Some(3), None).await)?),
        &Vec::new(),
    )?;
    eq(
        "through below after",
        &ids(&io("read_stream", read(Some(2), Some(1)).await)?),
        &Vec::new(),
    )?;
    eq(
        "the second unit's recorded_at",
        &io("read_stream", read(Some(1), None).await)?
            .iter()
            .map(|e| e.recorded_at)
            .collect::<Vec<_>>(),
        &vec![Millis::new(2_000); 2],
    )
}

/// The probe of the expected-revision cases: two streams and everything [`tagged`] 9 writes.
fn revision_probe() -> Probe {
    Probe {
        streams: vec![stream("attempt-1"), stream("attempt-2")],
        commands: vec![9],
        keys: vec![key("scaleset-1", "9")],
        events: vec![1, 2, 30],
        rows: vec!["9".to_owned()],
    }
}

/// Refuses each of `attempts` with `WrongExpectedRevision` and checks nothing was written.
async fn refuse_revisions<F: JournalFactory>(
    factory: &F,
    attempts: &[(StreamId, ExpectedRevision, Option<StreamRevision>)],
) -> Check {
    let journal = factory.fresh().await;
    let s1 = stream("attempt-1");
    io(
        "seed",
        journal
            .append(appending(unit(1_000), &s1, NEW, &[1, 2]))
            .await,
    )?;
    let probe = revision_probe();
    let before = observe(&journal, &probe).await?;
    let mut journal = journal;
    for (target, expected, actual) in attempts {
        let mut refused_unit = tagged(9, target, *expected, 30);
        if let Some(first) = refused_unit.appends.first_mut() {
            first.events.push(NewEvent {
                kind: kind("contract_refused_recorded", 1),
                ..event(31)
            });
        }
        eq(
            &format!("an append to {target} expecting {expected}"),
            &journal.append(refused_unit).await.map(|_| ()),
            &Err(AppendError::WrongExpectedRevision {
                stream: target.clone(),
                expected: *expected,
                actual: *actual,
            }),
        )?;
        eq(
            &format!("the journal after refusing {target} at {expected}"),
            &observe(&journal, &probe).await?,
            &before,
        )?;
        journal = factory.reopen(journal).await;
        eq(
            "the journal after reopen",
            &observe(&journal, &probe).await?,
            &before,
        )?;
    }
    io(
        "the unit at the right revision",
        journal.append(tagged(9, &s1, at(1), 30)).await,
    )?;
    eq(
        "the head after the retry",
        &head_revision(&journal, &s1).await?,
        &Some(rev(2)),
    )
}

async fn stale_expected_revision<F: JournalFactory>(factory: &F) -> Check {
    let s1 = stream("attempt-1");
    refuse_revisions(factory, &[(s1.clone(), at(0), Some(rev(1)))]).await
}

async fn ahead_expected_revision<F: JournalFactory>(factory: &F) -> Check {
    let s1 = stream("attempt-1");
    refuse_revisions(
        factory,
        &[
            (s1.clone(), at(2), Some(rev(1))),
            (s1.clone(), at(9), Some(rev(1))),
        ],
    )
    .await
}

async fn no_stream_mismatch<F: JournalFactory>(factory: &F) -> Check {
    let (s1, s2) = (stream("attempt-1"), stream("attempt-2"));
    refuse_revisions(
        factory,
        &[(s1.clone(), NEW, Some(rev(1))), (s2.clone(), at(0), None)],
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// JC-07 .. JC-11: identities, shape, order, atomicity, read sets

async fn duplicate_event_id<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let (s1, s2) = (stream("attempt-1"), stream("attempt-2"));
    io(
        "seed",
        journal.append(appending(unit(1_000), &s1, NEW, &[1])).await,
    )?;
    let journal = factory.reopen(journal).await;
    let probe = Probe {
        streams: vec![s1.clone(), s2.clone()],
        events: vec![1, 2],
        ..Probe::default()
    };
    let before = observe(&journal, &probe).await?;
    eq(
        "a unit carrying an id stored on another stream",
        &journal
            .append(appending(unit(2_000), &s2, NEW, &[2, 1]))
            .await
            .map(|_| ()),
        &Err(AppendError::DuplicateEventId(EventId::from_u128(1))),
    )?;
    eq(
        "the journal after the refusal",
        &observe(&journal, &probe).await?,
        &before,
    )?;
    let journal = factory.reopen(journal).await;
    io(
        "the refused unit's fresh event alone",
        journal.append(appending(unit(3_000), &s2, NEW, &[2])).await,
    )?;
    eq(
        "where the stored id still lives",
        &io(
            "lookup_event",
            journal.lookup_event(EventId::from_u128(1)).await,
        )?,
        &Some(EventLocation {
            stream: s1,
            revision: rev(0),
        }),
    )
}

async fn malformed_unit<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let (s1, s2) = (stream("attempt-1"), stream("attempt-2"));
    io(
        "seed",
        journal.append(appending(unit(1_000), &s1, NEW, &[1])).await,
    )?;
    let probe = Probe {
        streams: vec![s1.clone(), s2.clone()],
        commands: vec![1],
        keys: vec![key("scaleset-1", "1")],
        events: vec![2, 3],
        rows: vec!["a".to_owned()],
    };
    let before = observe(&journal, &probe).await?;
    let extras = |u: UnitOfWork| {
        u.with_receipt(accepted(1, 1))
            .with_inbox(key("scaleset-1", "1"))
    };
    let mut bad_type = event(2);
    bad_type.kind.event_type = "ContractFact".to_owned();
    let cases = [
        (
            unit(2_000).with_read(s1.clone(), at(0)),
            MalformedUnit::Empty,
        ),
        (
            extras(unit(2_000).with_append(s2.clone(), aggregate("attempt"), NEW, Vec::new())),
            MalformedUnit::EmptyAppend,
        ),
        (
            extras(appending(
                appending(unit(2_000), &s1, at(0), &[2]),
                &s1,
                at(0),
                &[3],
            )),
            MalformedUnit::DuplicateStream,
        ),
        (
            extras(appending(unit(2_000), &s2, NEW, &[2]).with_read(s2.clone(), NEW)),
            MalformedUnit::DuplicateStream,
        ),
        (
            extras(appending(unit(2_000), &s1, at(0), &[2, 2])),
            MalformedUnit::DuplicateEventId,
        ),
        (
            extras(appending(
                unit(2_000),
                &s1,
                ExpectedRevision::Exactly(rev(u64::MAX)),
                &[2],
            )),
            MalformedUnit::RevisionOutOfRange,
        ),
        (
            extras(unit(2_000).with_append(s2.clone(), aggregate("attempt"), NEW, vec![bad_type])),
            MalformedUnit::InvalidEventType,
        ),
        (
            unit(2_000)
                .with_receipt(refusal(1, 1, "pool_draining"))
                .with_inbox(key("scaleset-1", "1")),
            MalformedUnit::RefusalNotBound,
        ),
        (
            extras(
                appending(unit(2_000), &s2, NEW, &[2])
                    .with_projection(row(&s2, "a", Some("{}")))
                    .with_projection(row(&s2, "a", None)),
            ),
            MalformedUnit::DuplicateProjectionRow,
        ),
        (
            extras(appending(unit(2_000), &s2, NEW, &[2]).with_projection(row(
                &s1,
                "a",
                Some("{}"),
            ))),
            MalformedUnit::ProjectionOutsideUnit,
        ),
        (
            extras(appending(unit(2_000), &s2, NEW, &[2]).with_projection(row(
                &s2,
                "a\nb",
                Some("{}"),
            ))),
            MalformedUnit::InvalidProjectionKey,
        ),
    ];
    for (bad, malformed) in cases {
        eq(
            &format!("a unit that is {malformed}"),
            &journal.append(bad).await.map(|_| ()),
            &Err(AppendError::Malformed(malformed)),
        )?;
    }
    let journal = factory.reopen(journal).await;
    eq(
        "the journal after malformed units",
        &observe(&journal, &probe).await?,
        &before,
    )
}

/// The JC-09 unit that breaks every rule from `stage` on, in reporting order: 1 shape, 2 epoch
/// above the build, 3 receipt, 4 inbox, 5 event ids, 6 revisions, 7 stream epoch. Stage 8
/// breaks none.
fn order_unit(stage: u8, replay: bool) -> UnitOfWork {
    let (s0, s1, s3, s4) = (
        stream("attempt-0"),
        stream("attempt-1"),
        stream("attempt-3"),
        stream("attempt-4"),
    );
    let breaks = |rule: u8| stage <= rule;
    let s1_ids: &[u128] = if breaks(5) { &[21, 7, 1] } else { &[21] };
    let mut u = appending(
        unit(2_000),
        &s1,
        if breaks(6) { at(5) } else { at(0) },
        s1_ids,
    )
    .with_read(s0.clone(), if breaks(6) { NEW } else { at(0) });
    if breaks(7) {
        u = appending(u, &s3, at(0), &[23]);
    }
    if breaks(2) {
        let mut newer = event(24);
        newer.required_epoch = 2;
        u = u.with_append(s4, aggregate("attempt"), NEW, vec![newer]);
    }
    if breaks(1) {
        u = u.with_read(s1, at(0));
    }
    u = u.with_receipt(if breaks(3) {
        accepted(1, if replay { 1 } else { 2 })
    } else {
        accepted(2, 1)
    });
    u.with_inbox(if breaks(4) {
        key("scaleset-1", "m1")
    } else {
        key("scaleset-1", "m2")
    })
}

async fn refusal_order<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let (s0, s1, s3) = (
        stream("attempt-0"),
        stream("attempt-1"),
        stream("attempt-3"),
    );
    io(
        "seed attempt-0",
        journal.append(appending(unit(1_000), &s0, NEW, &[7])).await,
    )?;
    let first = io(
        "seed attempt-1",
        journal
            .append(
                appending(unit(1_000), &s1, NEW, &[1])
                    .with_receipt(accepted(1, 1))
                    .with_inbox(key("scaleset-1", "m1")),
            )
            .await,
    )?;
    let mut newer = event(3);
    newer.required_epoch = 2;
    io(
        "seed attempt-3 at epoch 2",
        journal
            .append(UnitOfWork::new(Millis::new(1_000), build(2)).with_append(
                s3.clone(),
                aggregate("attempt"),
                NEW,
                vec![newer],
            ))
            .await,
    )?;
    let journal = factory.reopen(journal).await;
    let stored = CommandReceipt {
        command_id: CommandId::from_u128(1),
        command_type: command_type(),
        intent: intent(1),
        outcome: ReceiptOutcome::Accepted(first),
    };
    let expected = [
        AppendError::Malformed(MalformedUnit::DuplicateStream),
        AppendError::EpochAboveBuild {
            stream: stream("attempt-4"),
        },
        AppendError::CommandIdReused {
            command_id: CommandId::from_u128(1),
        },
        AppendError::DuplicateMessage(key("scaleset-1", "m1")),
        AppendError::DuplicateEventId(EventId::from_u128(1)),
        AppendError::WrongExpectedRevision {
            stream: s0,
            expected: NEW,
            actual: Some(rev(0)),
        },
        AppendError::StreamEpochUnsupported {
            stream: s3,
            epoch: 2,
        },
    ];
    for (stage, want) in (1_u8..).zip(expected) {
        eq(
            &format!("a unit breaking rules {stage} to 7"),
            &journal.append(order_unit(stage, false)).await.map(|_| ()),
            &Err(want),
        )?;
    }
    eq(
        "a replay breaking every later rule",
        &journal.append(order_unit(3, true)).await.map(|_| ()),
        &Err(AppendError::CommandReplayed(Box::new(stored))),
    )?;
    io(
        "the unit that breaks none",
        journal.append(order_unit(8, false)).await,
    )
    .map(|_| ())
}

async fn multi_stream_all_or_nothing<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let (s1, s2) = (stream("attempt-1"), stream("attempt-2"));
    io(
        "seed",
        journal.append(appending(unit(1_000), &s2, NEW, &[1])).await,
    )?;
    let journal = factory.reopen(journal).await;
    let probe = Probe {
        streams: vec![s1.clone(), s2.clone()],
        commands: vec![1],
        keys: vec![key("scaleset-1", "m-1")],
        events: vec![10, 11],
        rows: vec!["k1".to_owned(), "k2".to_owned()],
    };
    let before = observe(&journal, &probe).await?;
    let whole = |s2_expected: ExpectedRevision, s2_event: u128| {
        appending(
            appending(unit(2_000), &s1, NEW, &[10]),
            &s2,
            s2_expected,
            &[s2_event],
        )
        .with_receipt(accepted(1, 1))
        .with_inbox(key("scaleset-1", "m-1"))
        .with_projection(row(&s1, "k1", Some("{\"v\":1}")))
        .with_projection(row(&s2, "k2", Some("{\"v\":2}")))
    };
    let refusals = [
        (
            "a unit whose second stream conflicts",
            whole(NEW, 11),
            AppendError::WrongExpectedRevision {
                stream: s2.clone(),
                expected: NEW,
                actual: Some(rev(0)),
            },
        ),
        (
            "a unit whose last event id is stored",
            whole(at(0), 1),
            AppendError::DuplicateEventId(EventId::from_u128(1)),
        ),
    ];
    let mut journal = journal;
    for (what, refused_unit, error) in refusals {
        eq(
            what,
            &journal.append(refused_unit).await.map(|_| ()),
            &Err(error),
        )?;
        eq(
            &format!("the journal after {what}"),
            &observe(&journal, &probe).await?,
            &before,
        )?;
        journal = factory.reopen(journal).await;
        eq(
            &format!("the journal after {what}, reopened"),
            &observe(&journal, &probe).await?,
            &before,
        )?;
    }
    let written = io("the whole unit", journal.append(whole(at(0), 11)).await)?;
    eq(
        "what the whole unit appended",
        &written.streams,
        &vec![appended(&s1, 0, &[10]), appended(&s2, 1, &[11])],
    )?;
    let journal = factory.reopen(journal).await;
    let after = observe(&journal, &probe).await?;
    eq(
        "heads after the whole unit",
        &after
            .heads
            .iter()
            .map(|h| h.as_ref().map(|h| h.revision))
            .collect::<Vec<_>>(),
        &vec![Some(rev(0)), Some(rev(1))],
    )?;
    eq(
        "the receipt of the whole unit",
        &io("receipt", journal.receipt(CommandId::from_u128(1)).await)?.map(|r| r.outcome),
        &Some(ReceiptOutcome::Accepted(written)),
    )?;
    eq(
        "the inbox entry of the whole unit",
        &after.inbox,
        &vec![true],
    )?;
    eq(
        "the rows of the whole unit",
        &after.rows,
        &vec![
            Some(Payload::new("{\"v\":1}")),
            Some(Payload::new("{\"v\":2}")),
        ],
    )
}

async fn read_conditions<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let (pool, a1, a2, a3) = (
        stream("pool-1"),
        stream("attempt-1"),
        stream("attempt-2"),
        stream("attempt-3"),
    );
    io(
        "seed the pool",
        journal
            .append(appending(unit(1_000), &pool, NEW, &[1]))
            .await,
    )?;
    let decided = |s: &StreamId, read: ExpectedRevision, id: u128| {
        appending(unit(2_000), s, NEW, &[id]).with_read(pool.clone(), read)
    };
    io(
        "a decision on the pool as read",
        journal.append(decided(&a1, at(0), 10)).await,
    )?;
    eq(
        "a read writes no event and moves no head",
        &(
            head_revision(&journal, &pool).await?,
            ids(&io(
                "read_stream",
                journal.read_stream(&pool, None, None).await,
            )?),
        ),
        &(Some(rev(0)), vec![1]),
    )?;
    io(
        "the pool moves",
        journal
            .append(appending(unit(3_000), &pool, at(0), &[2]))
            .await,
    )?;
    eq(
        "a decision on the pool before it moved",
        &journal.append(decided(&a2, at(0), 11)).await.map(|_| ()),
        &Err(AppendError::WrongExpectedRevision {
            stream: pool.clone(),
            expected: at(0),
            actual: Some(rev(1)),
        }),
    )?;
    eq(
        "the refused decision's stream",
        &head_revision(&journal, &a2).await?,
        &None,
    )?;
    let absent = stream("pool-2");
    let on_absent = |s: &StreamId, id: u128| {
        appending(unit(4_000), s, NEW, &[id]).with_read(absent.clone(), NEW)
    };
    io(
        "a decision on an absent pool",
        journal.append(on_absent(&a2, 12)).await,
    )?;
    io(
        "the pool is created",
        journal
            .append(appending(unit(5_000), &absent, NEW, &[3]))
            .await,
    )?;
    eq(
        "a decision on the pool before its creation",
        &journal.append(on_absent(&a3, 13)).await.map(|_| ()),
        &Err(AppendError::WrongExpectedRevision {
            stream: absent,
            expected: NEW,
            actual: Some(rev(0)),
        }),
    )?;
    let journal = factory.reopen(journal).await;
    eq(
        "the refused decision's stream after reopen",
        &head_revision(&journal, &a3).await?,
        &None,
    )
}

// ---------------------------------------------------------------------------------------------
// JC-12 .. JC-15, JC-28: receipts and the inbox

async fn receipt_replay<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let s1 = stream("attempt-1");
    let first = io(
        "the command's unit",
        journal
            .append(appending(unit(1_000), &s1, NEW, &[1, 2]).with_receipt(accepted(1, 1)))
            .await,
    )?;
    io(
        "another command moves the stream",
        journal
            .append(appending(unit(2_000), &s1, at(1), &[3]))
            .await,
    )?;
    let journal = factory.reopen(journal).await;
    let stored = CommandReceipt {
        command_id: CommandId::from_u128(1),
        command_type: command_type(),
        intent: intent(1),
        outcome: ReceiptOutcome::Accepted(first),
    };
    eq(
        "the stored receipt",
        &io("receipt", journal.receipt(CommandId::from_u128(1)).await)?,
        &Some(stored.clone()),
    )?;
    let probe = Probe {
        streams: vec![s1.clone()],
        commands: vec![1, 2],
        events: vec![4, 5],
        ..Probe::default()
    };
    let before = observe(&journal, &probe).await?;
    for (what, expected) in [("its first", NEW), ("the current", at(2))] {
        eq(
            &format!("a repeat of the command at {what} revision"),
            &journal
                .append(appending(unit(3_000), &s1, expected, &[4]).with_receipt(accepted(1, 1)))
                .await
                .map(|_| ()),
            &Err(AppendError::CommandReplayed(Box::new(stored.clone()))),
        )?;
    }
    eq(
        "the journal after the repeats",
        &observe(&journal, &probe).await?,
        &before,
    )?;
    io(
        "an accepted command with no event",
        journal
            .append(unit(4_000).with_receipt(accepted(2, 1)))
            .await,
    )?;
    let journal = factory.reopen(journal).await;
    let empty = CommandReceipt {
        command_id: CommandId::from_u128(2),
        command_type: command_type(),
        intent: intent(1),
        outcome: ReceiptOutcome::Accepted(Appended::default()),
    };
    eq(
        "a repeat of the command with no event",
        &journal
            .append(appending(unit(5_000), &s1, at(2), &[5]).with_receipt(accepted(2, 1)))
            .await
            .map(|_| ()),
        &Err(AppendError::CommandReplayed(Box::new(empty))),
    )?;
    eq(
        "the stream after every repeat",
        &head_revision(&journal, &s1).await?,
        &Some(rev(2)),
    )
}

async fn command_id_reused<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let (s1, s2) = (stream("attempt-1"), stream("attempt-2"));
    io(
        "an accepted command",
        journal
            .append(appending(unit(1_000), &s1, NEW, &[1]).with_receipt(accepted(1, 1)))
            .await,
    )?;
    io(
        "a refused command",
        journal
            .append(unit(1_000).with_read(s2.clone(), NEW).with_receipt(refusal(
                2,
                1,
                "pool_draining",
            )))
            .await,
    )?;
    let journal = factory.reopen(journal).await;
    let probe = Probe {
        streams: vec![s1.clone(), s2.clone()],
        commands: vec![1, 2],
        events: vec![3, 4],
        ..Probe::default()
    };
    let before = observe(&journal, &probe).await?;
    let reuses = [
        (
            "the accepted command's id with another intent",
            appending(unit(2_000), &s1, at(0), &[3]).with_receipt(accepted(1, 2)),
            1,
        ),
        (
            "the refused command's id with another intent at its revision",
            appending(unit(2_000), &s2, NEW, &[4]).with_receipt(accepted(2, 2)),
            2,
        ),
    ];
    for (what, reuse, command) in reuses {
        eq(
            what,
            &journal.append(reuse).await.map(|_| ()),
            &Err(AppendError::CommandIdReused {
                command_id: CommandId::from_u128(command),
            }),
        )?;
    }
    io(
        "the refused command's stream moves",
        journal.append(appending(unit(3_000), &s2, NEW, &[5])).await,
    )?;
    eq(
        "the refused command's id with another intent after its stream moved",
        &journal
            .append(appending(unit(4_000), &s2, at(0), &[4]).with_receipt(accepted(2, 2)))
            .await
            .map(|_| ()),
        &Err(AppendError::CommandIdReused {
            command_id: CommandId::from_u128(2),
        }),
    )?;
    eq(
        "the receipts after the reuses",
        &observe(&journal, &probe).await?.receipts,
        &before.receipts,
    )
}

async fn store_refusal_leaves_no_receipt<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let (s1, s2) = (stream("attempt-1"), stream("attempt-2"));
    io(
        "seed",
        journal.append(appending(unit(1_000), &s1, NEW, &[1])).await,
    )?;
    let command = || accepted(5, 1);
    eq(
        "a command refused for its revision",
        &journal
            .append(appending(unit(2_000), &s1, NEW, &[2]).with_receipt(command()))
            .await
            .map(|_| ()),
        &Err(AppendError::WrongExpectedRevision {
            stream: s1.clone(),
            expected: NEW,
            actual: Some(rev(0)),
        }),
    )?;
    eq(
        "a command refused for an identity",
        &journal
            .append(appending(unit(2_000), &s2, NEW, &[1]).with_receipt(command()))
            .await
            .map(|_| ()),
        &Err(AppendError::DuplicateEventId(EventId::from_u128(1))),
    )?;
    must_inject(factory, &journal, InjectedFault::Busy).await?;
    eq(
        "a command refused as busy",
        &journal
            .append(appending(unit(2_000), &s1, at(0), &[2]).with_receipt(command()))
            .await
            .map(|_| ()),
        &Err(AppendError::Busy),
    )?;
    let journal = factory.reopen(journal).await;
    eq(
        "the receipt after store refusals",
        &io("receipt", journal.receipt(CommandId::from_u128(5)).await)?,
        &None,
    )?;
    io(
        "the same command, accepted later",
        journal
            .append(appending(unit(3_000), &s1, at(0), &[2]).with_receipt(command()))
            .await,
    )?;
    io(
        "a domain refusal",
        journal
            .append(
                unit(4_000)
                    .with_read(s1.clone(), at(1))
                    .with_receipt(refusal(6, 1, "pool_draining")),
            )
            .await,
    )?;
    let journal = factory.reopen(journal).await;
    eq(
        "the domain refusal's receipt",
        &io("receipt", journal.receipt(CommandId::from_u128(6)).await)?.map(|r| r.outcome),
        &Some(ReceiptOutcome::Refused {
            bound: vec![ReadCondition {
                stream: s1.clone(),
                expected: at(1),
            }],
            rejection: rejection("pool_draining"),
        }),
    )?;
    eq(
        "the refused command's stream",
        &head_revision(&journal, &s1).await?,
        &Some(rev(1)),
    )
}

async fn inbox_dedupe<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let (s1, s2) = (stream("scaleset-1"), stream("scaleset-2"));
    let message = key("scaleset-1", "42");
    io(
        "the message's unit",
        journal
            .append(appending(unit(1_000), &s1, NEW, &[1]).with_inbox(message.clone()))
            .await,
    )?;
    let journal = factory.reopen(journal).await;
    eq(
        "the consumed key",
        &io("inbox_contains", journal.inbox_contains(&message).await)?,
        &true,
    )?;
    let probe = Probe {
        streams: vec![s1.clone(), s2.clone()],
        keys: vec![message.clone()],
        events: vec![2],
        ..Probe::default()
    };
    let before = observe(&journal, &probe).await?;
    for (what, s, expected) in [
        ("at the current revision", &s1, at(0)),
        ("at a stale revision", &s1, NEW),
        ("on another stream", &s2, NEW),
    ] {
        eq(
            &format!("a redelivery {what}"),
            &journal
                .append(appending(unit(2_000), s, expected, &[2]).with_inbox(message.clone()))
                .await
                .map(|_| ()),
            &Err(AppendError::DuplicateMessage(message.clone())),
        )?;
    }
    eq(
        "the journal after redeliveries",
        &observe(&journal, &probe).await?,
        &before,
    )?;
    let other_source = key("dp-host1", "42");
    eq(
        "the same id from another source, before it is consumed",
        &io(
            "inbox_contains",
            journal.inbox_contains(&other_source).await,
        )?,
        &false,
    )?;
    io(
        "a unit that only consumes a message",
        journal
            .append(unit(3_000).with_inbox(other_source.clone()))
            .await,
    )?;
    let journal = factory.reopen(journal).await;
    eq(
        "the inbox-only unit's key",
        &io(
            "inbox_contains",
            journal.inbox_contains(&other_source).await,
        )?,
        &true,
    )?;
    eq(
        "streams after the inbox-only unit",
        &all_streams(&journal).await?.len(),
        &1,
    )?;
    eq(
        "a redelivery of the inbox-only message",
        &journal
            .append(unit(4_000).with_inbox(other_source.clone()))
            .await
            .map(|_| ()),
        &Err(AppendError::DuplicateMessage(other_source)),
    )
}

async fn refusal_receipts<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let (s1, s2) = (stream("attempt-1"), stream("attempt-2"));
    io(
        "seed",
        journal.append(appending(unit(1_000), &s1, NEW, &[1])).await,
    )?;
    let refused_at = |expected: ExpectedRevision| {
        unit(2_000)
            .with_read(s1.clone(), expected)
            .with_receipt(refusal(1, 1, "pool_draining"))
    };
    io(
        "a refusal at revision 0",
        journal.append(refused_at(at(0))).await,
    )?;
    let journal = factory.reopen(journal).await;
    let recorded = CommandReceipt {
        command_id: CommandId::from_u128(1),
        command_type: command_type(),
        intent: intent(1),
        outcome: ReceiptOutcome::Refused {
            bound: vec![ReadCondition {
                stream: s1.clone(),
                expected: at(0),
            }],
            rejection: rejection("pool_draining"),
        },
    };
    for (what, repeat) in [
        ("the same refusal", refused_at(at(0))),
        (
            "an acceptance",
            appending(unit(3_000), &s1, at(0), &[2]).with_receipt(accepted(1, 1)),
        ),
    ] {
        eq(
            &format!("a repeat as {what} while the stream is at revision 0"),
            &journal.append(repeat).await.map(|_| ()),
            &Err(AppendError::CommandReplayed(Box::new(recorded.clone()))),
        )?;
    }
    eq(
        "the stream after the replays",
        &head_revision(&journal, &s1).await?,
        &Some(rev(0)),
    )?;
    io(
        "another command moves the stream",
        journal
            .append(appending(unit(4_000), &s1, at(0), &[3]))
            .await,
    )?;
    eq(
        "a refusal decided before the stream moved, committed after",
        &journal
            .append(
                unit(5_000)
                    .with_read(s1.clone(), at(0))
                    .with_receipt(refusal(2, 1, "pool_draining")),
            )
            .await
            .map(|_| ()),
        &Err(AppendError::WrongExpectedRevision {
            stream: s1.clone(),
            expected: at(0),
            actual: Some(rev(1)),
        }),
    )?;
    eq(
        "the receipt of the refusal that lost its stream",
        &io("receipt", journal.receipt(CommandId::from_u128(2)).await)?,
        &None,
    )?;
    io(
        "the repeat re-decided as a refusal at revision 1",
        journal.append(refused_at(at(1))).await,
    )?;
    eq(
        "the refusal receipt, now bound to revision 1",
        &io("receipt", journal.receipt(CommandId::from_u128(1)).await)?.map(|r| r.outcome),
        &Some(ReceiptOutcome::Refused {
            bound: vec![ReadCondition {
                stream: s1.clone(),
                expected: at(1),
            }],
            rejection: rejection("pool_draining"),
        }),
    )?;
    io(
        "the stream moves again",
        journal
            .append(appending(unit(6_000), &s1, at(1), &[4]))
            .await,
    )?;
    let accepted_now = io(
        "the repeat re-decided as an acceptance",
        journal
            .append(appending(unit(7_000), &s1, at(2), &[5]).with_receipt(accepted(1, 1)))
            .await,
    )?;
    let journal = factory.reopen(journal).await;
    let replaced = CommandReceipt {
        outcome: ReceiptOutcome::Accepted(accepted_now),
        ..recorded
    };
    eq(
        "the receipt the acceptance replaced the refusal with",
        &io("receipt", journal.receipt(CommandId::from_u128(1)).await)?,
        &Some(replaced.clone()),
    )?;
    eq(
        "a later repeat as a refusal at the current revision",
        &journal.append(refused_at(at(3))).await.map(|_| ()),
        &Err(AppendError::CommandReplayed(Box::new(replaced))),
    )?;
    io(
        "a refusal on an absent stream",
        journal
            .append(
                unit(8_000)
                    .with_read(s2.clone(), NEW)
                    .with_receipt(refusal(3, 1, "no_pool")),
            )
            .await,
    )?;
    io(
        "the absent stream is created",
        journal.append(appending(unit(9_000), &s2, NEW, &[6])).await,
    )?;
    io(
        "the repeat after the creation, re-decided",
        journal
            .append(appending(unit(10_000), &s2, at(0), &[7]).with_receipt(accepted(3, 1)))
            .await,
    )?;
    refusal_over_two_reads(factory, journal).await
}

/// JC-28's multi-read half (ADR 0041): a decision that read two streams binds its refusal to
/// both, named in any order and recorded in `StreamId` order; it answers only while both are
/// where it read them, and once either moved the repeat is decided afresh.
async fn refusal_over_two_reads<F: JournalFactory>(factory: &F, journal: F::Journal) -> Check {
    let (target, pool) = (stream("attempt-3"), stream("pool-1"));
    io(
        "the pool the decision reads",
        journal
            .append(appending(unit(11_000), &pool, NEW, &[8]))
            .await,
    )?;
    let refused_over = |pool_at: ExpectedRevision| {
        unit(12_000)
            .with_read(pool.clone(), pool_at)
            .with_read(target.clone(), NEW)
            .with_receipt(refusal(4, 1, "pool_draining"))
    };
    io(
        "a refusal decided over the target and the pool",
        journal.append(refused_over(at(0))).await,
    )?;
    let journal = factory.reopen(journal).await;
    let bound_at = |pool_at: ExpectedRevision| {
        vec![
            ReadCondition {
                stream: target.clone(),
                expected: NEW,
            },
            ReadCondition {
                stream: pool.clone(),
                expected: pool_at,
            },
        ]
    };
    let recorded = CommandReceipt {
        command_id: CommandId::from_u128(4),
        command_type: command_type(),
        intent: intent(1),
        outcome: ReceiptOutcome::Refused {
            bound: bound_at(at(0)),
            rejection: rejection("pool_draining"),
        },
    };
    eq(
        "a repeat while both read streams are unchanged",
        &journal.append(refused_over(at(0))).await.map(|_| ()),
        &Err(AppendError::CommandReplayed(Box::new(recorded))),
    )?;
    io(
        "the pool moves; the target does not",
        journal
            .append(appending(unit(13_000), &pool, at(0), &[9]))
            .await,
    )?;
    io(
        "the repeat after only the pool moved, re-decided",
        journal.append(refused_over(at(1))).await,
    )?;
    eq(
        "the refusal receipt, now bound to the pool's new revision",
        &io("receipt", journal.receipt(CommandId::from_u128(4)).await)?.map(|r| r.outcome),
        &Some(ReceiptOutcome::Refused {
            bound: bound_at(at(1)),
            rejection: rejection("pool_draining"),
        }),
    )
}

// ---------------------------------------------------------------------------------------------
// JC-16 .. JC-20: the registry, the manifest, epochs, payloads, enumeration

async fn lookup_event<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let (s1, s2) = (stream("attempt-1"), stream("attempt-2"));
    io(
        "a two-stream unit",
        journal
            .append(appending(
                appending(unit(1_000), &s1, NEW, &[1, 2]),
                &s2,
                NEW,
                &[3],
            ))
            .await,
    )?;
    eq(
        "a refused batch",
        &journal
            .append(appending(
                appending(unit(2_000), &s1, at(1), &[4, 5]),
                &s2,
                NEW,
                &[6],
            ))
            .await
            .map(|_| ()),
        &Err(AppendError::WrongExpectedRevision {
            stream: s2.clone(),
            expected: NEW,
            actual: Some(rev(0)),
        }),
    )?;
    let journal = factory.reopen(journal).await;
    let mut found = Vec::new();
    for id in 1..=6 {
        found.push(io(
            "lookup_event",
            journal.lookup_event(EventId::from_u128(id)).await,
        )?);
    }
    let place = |s: &StreamId, r: u64| {
        Some(EventLocation {
            stream: s.clone(),
            revision: rev(r),
        })
    };
    eq(
        "every id's place",
        &found,
        &vec![
            place(&s1, 0),
            place(&s1, 1),
            place(&s2, 0),
            None,
            None,
            None,
        ],
    )
}

async fn kind_manifest<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let (s1, s2) = (stream("attempt-1"), stream("attempt-2"));
    let with_kind = |id: u128, event_type: &str, version: u32| NewEvent {
        kind: kind(event_type, version),
        ..event(id)
    };
    io(
        "two kinds",
        journal
            .append(unit(1_000).with_append(
                s1.clone(),
                aggregate("attempt"),
                NEW,
                vec![
                    with_kind(1, "contract_b_recorded", 2),
                    with_kind(2, "contract_a_recorded", 1),
                ],
            ))
            .await,
    )?;
    io(
        "a kind again and its next version",
        journal
            .append(unit(2_000).with_append(
                s1.clone(),
                aggregate("attempt"),
                at(1),
                vec![
                    with_kind(3, "contract_a_recorded", 1),
                    with_kind(4, "contract_a_recorded", 2),
                ],
            ))
            .await,
    )?;
    eq(
        "a refused unit with a new kind",
        &journal
            .append(unit(3_000).with_append(
                s2.clone(),
                aggregate("attempt"),
                at(0),
                vec![with_kind(5, "contract_c_recorded", 1)],
            ))
            .await
            .map(|_| ()),
        &Err(AppendError::WrongExpectedRevision {
            stream: s2,
            expected: at(0),
            actual: None,
        }),
    )?;
    let journal = factory.reopen(journal).await;
    eq(
        "the manifest",
        &io("stored_kinds", journal.stored_kinds().await)?,
        &vec![
            kind("contract_a_recorded", 1),
            kind("contract_a_recorded", 2),
            kind("contract_b_recorded", 2),
        ],
    )
}

fn at_epoch(id: u128, epoch: u32) -> NewEvent {
    NewEvent {
        required_epoch: epoch,
        ..event(id)
    }
}

async fn epochs_case<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let (s1, s2, s9) = (
        stream("attempt-1"),
        stream("attempt-2"),
        stream("attempt-9"),
    );
    let by = |epoch: u32| UnitOfWork::new(Millis::new(1_000), build(epoch));
    let add = |u: UnitOfWork, s: &StreamId, expected: ExpectedRevision, events: Vec<NewEvent>| {
        u.with_append(s.clone(), aggregate("attempt"), expected, events)
    };
    io(
        "an event needing epoch 2, by a build supporting 2",
        journal
            .append(add(by(2), &s1, NEW, vec![at_epoch(1, 2)]))
            .await,
    )?;
    io(
        "an event needing epoch 1 after it",
        journal
            .append(add(by(2), &s1, at(0), vec![at_epoch(2, 1)]))
            .await,
    )?;
    let journal = factory.reopen(journal).await;
    let head = io("head", journal.head(&s1).await)?;
    eq(
        "the stream's epoch, never lowered",
        &head.as_ref().map(|h| (h.revision, h.epoch)),
        &Some((rev(1), 2)),
    )?;
    eq(
        "an append by a build supporting 1",
        &journal
            .append(add(by(1), &s1, at(1), vec![at_epoch(3, 1)]))
            .await
            .map(|_| ()),
        &Err(AppendError::StreamEpochUnsupported {
            stream: s1.clone(),
            epoch: 2,
        }),
    )?;
    eq(
        "the stream's head after it",
        &io("head", journal.head(&s1).await)?,
        &head,
    )?;
    eq(
        "the stream's events, still read",
        &ids(&io(
            "read_stream",
            journal.read_stream(&s1, None, None).await,
        )?),
        &vec![1, 2],
    )?;
    eq(
        "the stream, still enumerated",
        &all_streams(&journal).await?.first().map(|h| h.epoch),
        &Some(2),
    )?;
    io(
        "a build supporting 1 reading the stream to decide another",
        journal
            .append(add(by(1), &s9, NEW, vec![at_epoch(9, 1)]).with_read(s1.clone(), at(1)))
            .await,
    )?;
    eq(
        "a batch above the build, with a stale revision too",
        &journal
            .append(add(
                add(by(2), &s1, at(7), vec![at_epoch(4, 1)]),
                &s2,
                NEW,
                vec![at_epoch(5, 3)],
            ))
            .await
            .map(|_| ()),
        &Err(AppendError::EpochAboveBuild { stream: s2.clone() }),
    )?;
    eq(
        "an aggregate the build does not support",
        &journal
            .append(by(9).with_append(
                stream("host-1"),
                aggregate("host"),
                NEW,
                vec![at_epoch(6, 1)],
            ))
            .await
            .map(|_| ()),
        &Err(AppendError::EpochAboveBuild {
            stream: stream("host-1"),
        }),
    )?;
    eq(
        "the refused batches' streams",
        &(
            head_revision(&journal, &s2).await?,
            head_revision(&journal, &stream("host-1")).await?,
        ),
        &(None, None),
    )?;
    io(
        "a build supporting 2 appends on",
        journal
            .append(add(by(2), &s1, at(1), vec![at_epoch(7, 1)]))
            .await,
    )?;
    eq(
        "the stream's epoch after it",
        &io("head", journal.head(&s1).await)?.map(|h| h.epoch),
        &Some(2),
    )
}

async fn payload_round_trip<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let s1 = stream("attempt-1");
    let big = format!("{{\"blob\":\"{}\"}}", "x".repeat(1024 * 1024));
    let payloads = [
        "{\"note\":\"zażółć gęślą jaźń ✓ 🦋\"}".to_owned(),
        "{}".to_owned(),
        "{\"escaped\":\"\\\\u0000\"}".to_owned(),
        big,
    ];
    let written: Vec<NewEvent> = payloads
        .iter()
        .zip(1_u128..)
        .map(|(p, n)| NewEvent {
            payload: Payload::new(p.clone()),
            ..event(n)
        })
        .collect();
    io(
        "payloads",
        journal
            .append(unit(1_000).with_append(s1.clone(), aggregate("attempt"), NEW, written.clone()))
            .await,
    )?;
    let journal = factory.reopen(journal).await;
    eq(
        "payloads read back byte for byte",
        &io("read_stream", journal.read_stream(&s1, None, None).await)?
            .into_iter()
            .map(|e| e.payload)
            .collect::<Vec<_>>(),
        &written.into_iter().map(|e| e.payload).collect::<Vec<_>>(),
    )?;
    let probe = Probe {
        streams: vec![s1.clone(), stream("attempt-2")],
        events: vec![10],
        ..Probe::default()
    };
    let before = observe(&journal, &probe).await?;
    for nul in ["{\"n\":\"a\u{0}b\"}", "{\"n\":\"\\u0000\"}"] {
        let with_nul = NewEvent {
            payload: Payload::new(nul),
            ..event(10)
        };
        eq(
            &format!("a payload holding a NUL ({nul:?})"),
            &journal
                .append(unit(2_000).with_append(
                    stream("attempt-2"),
                    aggregate("attempt"),
                    NEW,
                    vec![with_nul],
                ))
                .await
                .map(|_| ()),
            &Err(AppendError::Malformed(MalformedUnit::NulInPayload)),
        )?;
    }
    eq(
        "the journal after NUL payloads",
        &observe(&journal, &probe).await?,
        &before,
    )?;
    let marker = "contract-secret-marker";
    let marked = |id: u128| NewEvent {
        payload: Payload::new(format!("{{\"m\":\"{marker}\"}}")),
        ..event(id)
    };
    io(
        "a marked command",
        journal
            .append(
                unit(3_000)
                    .with_append(
                        stream("attempt-3"),
                        aggregate("attempt"),
                        NEW,
                        vec![marked(20)],
                    )
                    .with_receipt(accepted(1, 1))
                    .with_inbox(key("scaleset-1", "1")),
            )
            .await,
    )?;
    let errors = [
        journal
            .append(unit(4_000).with_append(
                s1.clone(),
                aggregate("attempt"),
                NEW,
                vec![marked(21)],
            ))
            .await,
        journal
            .append(unit(4_000).with_append(
                stream("attempt-4"),
                aggregate("attempt"),
                NEW,
                vec![marked(20)],
            ))
            .await,
        journal
            .append(
                unit(4_000)
                    .with_append(
                        stream("attempt-4"),
                        aggregate("attempt"),
                        NEW,
                        vec![marked(22)],
                    )
                    .with_receipt(accepted(1, 1)),
            )
            .await,
        journal
            .append(
                unit(4_000)
                    .with_append(
                        stream("attempt-4"),
                        aggregate("attempt"),
                        NEW,
                        vec![marked(22)],
                    )
                    .with_inbox(key("scaleset-1", "1")),
            )
            .await,
    ];
    for error in errors {
        let error = refused(error)?;
        ensure(
            &format!("an error carries no payload text: {error:?}"),
            !format!("{error} {error:?}").contains(marker),
        )?;
    }
    Ok(())
}

async fn streams_enumeration<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let names = [
        "poola",
        "pool-b",
        "attempt-9",
        "attempt-10",
        "attempt-1x",
        "pool-a",
    ];
    for (name, n) in names.iter().zip(1_u128..) {
        let s = stream(name);
        let aggregate_name = if name.starts_with("pool") {
            "pool"
        } else {
            "attempt"
        };
        io(
            &format!("a stream {name}"),
            journal
                .append(unit(1_000).with_append(s, aggregate(aggregate_name), NEW, events(&[n])))
                .await,
        )?;
    }
    eq(
        "a refused create",
        &journal
            .append(appending(unit(2_000), &stream("attempt-5"), at(0), &[99]))
            .await
            .map(|_| ()),
        &Err(AppendError::WrongExpectedRevision {
            stream: stream("attempt-5"),
            expected: at(0),
            actual: None,
        }),
    )?;
    let journal = factory.reopen(journal).await;
    let byte_order = vec![
        "attempt-10",
        "attempt-1x",
        "attempt-9",
        "pool-a",
        "pool-b",
        "poola",
    ];
    let paged = all_streams(&journal).await?;
    eq(
        "streams paged by three, in byte order",
        &paged.iter().map(|h| h.stream.as_str()).collect::<Vec<_>>(),
        &byte_order,
    )?;
    let whole = io("streams", journal.streams(None, u32::MAX).await)?;
    eq("one page with the largest limit", &whole, &paged)?;
    eq(
        "an empty page",
        &io("streams", journal.streams(None, 0).await)?,
        &Vec::new(),
    )?;
    eq(
        "a page after a key that is no stream",
        &io(
            "streams",
            journal.streams(Some(&stream("attempt-2")), 2).await,
        )?
        .into_iter()
        .map(|h| h.stream.as_str().to_owned())
        .collect::<Vec<_>>(),
        &vec!["attempt-9".to_owned(), "pool-a".to_owned()],
    )?;
    eq(
        "a page after the last stream",
        &io(
            "streams",
            journal.streams(Some(&stream("poola")), MAX_PAGE).await,
        )?,
        &Vec::new(),
    )?;
    for listed in &paged {
        eq(
            &format!("the listed head of {}", listed.stream),
            &io("head", journal.head(&listed.stream).await)?.as_ref(),
            &Some(listed),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// JC-21 .. JC-25, JC-29 .. JC-32: concurrency, crashes and unknown outcomes

async fn racing_handles<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let s1 = stream("attempt-1");
    io(
        "seed",
        journal.append(appending(unit(1_000), &s1, NEW, &[1])).await,
    )?;
    let other = factory.second_handle(&journal).await;
    for round in 0..6_u64 {
        let base = 10 + u128::from(round) * 2;
        let decided = io("head", journal.head(&s1).await)?.map(|h| h.revision);
        let decided_too = io("head", other.head(&s1).await)?.map(|h| h.revision);
        eq(
            &format!("round {round}: both handles read one head"),
            &decided_too,
            &decided,
        )?;
        let expected = ExpectedRevision::of(decided);
        let (first, second) = (
            tagged(base, &s1, expected, base),
            tagged(base + 1, &s1, expected, base + 1),
        );
        let outcomes = if round % 3 == 2 {
            join_all(vec![journal.append(first), other.append(second)]).await
        } else if round % 3 == 1 {
            let b = other.append(second).await;
            let a = journal.append(first).await;
            vec![a, b]
        } else {
            let a = journal.append(first).await;
            let b = other.append(second).await;
            vec![a, b]
        };
        let now = expected.next_revision();
        let (winner, loser) = one_winner(&format!("round {round}"), &outcomes, |_| {
            AppendError::WrongExpectedRevision {
                stream: s1.clone(),
                expected,
                actual: now,
            }
        })?;
        let (winner, loser) = (base + winner as u128, base + loser as u128);
        eq(
            &format!("round {round}: the head"),
            &head_revision(&other, &s1).await?,
            &now,
        )?;
        eq(
            &format!("round {round}: the winner's unit"),
            &traces_of(&journal, &s1, winner, winner).await?,
            &[true; 4],
        )?;
        eq(
            &format!("round {round}: the loser's unit"),
            &traces_of(&other, &s1, loser, loser).await?,
            &[false; 4],
        )?;
    }
    Ok(())
}

async fn independent_streams<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let other = factory.second_handle(&journal).await;
    let streams: Vec<StreamId> = (1..=8).map(|i| stream(&format!("attempt-{i}"))).collect();
    let outcomes = join_all(
        streams
            .iter()
            .zip(1_u128..)
            .map(|(s, n)| {
                let handle = if n % 2 == 0 { &other } else { &journal };
                handle.append(tagged(n, s, NEW, n))
            })
            .collect(),
    )
    .await;
    for (outcome, s) in outcomes.iter().zip(&streams) {
        eq(
            &format!("a new stream {s} written beside seven others"),
            &outcome.clone().map(|_| ()),
            &Ok(()),
        )?;
    }
    let (s1, s2, s3, s4) = (&streams[0], &streams[1], &streams[2], &streams[3]);
    let mixed = join_all(vec![
        journal.append(tagged(20, s1, NEW, 20)),
        other.append(tagged(21, s2, at(0), 21)),
    ])
    .await;
    eq(
        "a stale writer of one stream",
        &mixed.first().cloned().map(|o| o.map(|_| ())),
        &Some(Err(AppendError::WrongExpectedRevision {
            stream: s1.clone(),
            expected: NEW,
            actual: Some(rev(0)),
        })),
    )?;
    eq(
        "a current writer of another stream beside it",
        &mixed.get(1).cloned().map(|o| o.map(|_| ())),
        &Some(Ok(())),
    )?;
    io(
        "a write to a third stream",
        journal.append(tagged(22, s3, at(0), 22)).await,
    )?;
    io(
        "a writer of a fourth stream that decided before the third moved",
        other.append(tagged(23, s4, at(0), 23)).await,
    )?;
    let (s5, s6) = (&streams[4], &streams[5]);
    let opposite = join_all(vec![
        journal.append(appending(
            appending(unit(6_000), s5, at(0), &[30]),
            s6,
            at(0),
            &[31],
        )),
        other.append(appending(
            appending(unit(6_000), s6, at(0), &[32]),
            s5,
            at(0),
            &[33],
        )),
    ])
    .await;
    one_winner(
        "units naming two streams in opposite orders",
        &opposite,
        |_| AppendError::WrongExpectedRevision {
            stream: s5.clone(),
            expected: at(0),
            actual: Some(rev(1)),
        },
    )?;
    let journal = factory.reopen(journal).await;
    let revisions = [0, 1, 1, 1, 1, 1, 0, 0];
    for (s, want) in streams.iter().zip(revisions) {
        eq(
            &format!("the head of {s}"),
            &head_revision(&journal, s).await?,
            &Some(rev(want)),
        )?;
    }
    Ok(())
}

/// The JC-23 / JC-25 unit: a new stream, a second stream at revision 0, a receipt, an inbox
/// key and a row on each stream.
fn whole_unit(command: u128, s1: &StreamId, s2: &StreamId, ids: [u128; 2]) -> UnitOfWork {
    appending(
        appending(unit(2_000), s1, NEW, &[ids[0]]),
        s2,
        at(0),
        &[ids[1]],
    )
    .with_receipt(accepted(command, 1))
    .with_inbox(key("scaleset-1", &command.to_string()))
    .with_projection(row(s1, "r1", Some("{}")))
    .with_projection(row(s2, "r2", Some("{}")))
}

async fn crash_atomicity<F: JournalFactory>(factory: &F) -> Check {
    let (s1, s2) = (stream("attempt-1"), stream("attempt-2"));
    let probe = Probe {
        streams: vec![s1.clone(), s2.clone()],
        commands: vec![1],
        keys: vec![key("scaleset-1", "1")],
        events: vec![10, 11],
        rows: vec!["r1".to_owned(), "r2".to_owned()],
    };
    let mut points = 0;
    for point in 0..64_u32 {
        let journal = factory.fresh().await;
        io(
            "seed",
            journal.append(appending(unit(1_000), &s2, NEW, &[1])).await,
        )?;
        let before = observe(&journal, &probe).await?;
        if !factory
            .inject(&journal, InjectedFault::CrashAfterStatement(point))
            .await
        {
            break;
        }
        points += 1;
        let outcome = journal.append(whole_unit(1, &s1, &s2, [10, 11])).await;
        let journal = factory.crash(journal).await;
        let after = observe(&journal, &probe).await?;
        if after == before {
            ensure(
                &format!("a crash after statement {point}: an absent unit was reported committed"),
                outcome.is_err(),
            )?;
            continue;
        }
        let whole = after.heads.iter().all(Option::is_some)
            && after.receipts.iter().all(Option::is_some)
            && after.inbox.iter().all(|b| *b)
            && after.identities.iter().all(Option::is_some)
            && after.rows.iter().all(Option::is_some);
        ensure(
            &format!("a crash after statement {point} left part of the unit: {after:?}"),
            whole,
        )?;
        ensure(
            &format!(
                "a crash after statement {point}: a present unit was reported as definitely failed"
            ),
            !matches!(outcome, Err(ref e) if e.is_definite()),
        )?;
    }
    ensure(
        "the factory offers no crash point, so the case cannot run",
        points > 0,
    )
}

async fn survives_reopen<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let (s1, s2) = (stream("attempt-1"), stream("attempt-2"));
    let message = key("scaleset-1", "7");
    let first = io(
        "an accepted command at epoch 2",
        journal
            .append(
                UnitOfWork::new(Millis::new(1_000), build(2))
                    .with_append(s1.clone(), aggregate("attempt"), NEW, vec![at_epoch(1, 2)])
                    .with_receipt(accepted(1, 1))
                    .with_inbox(message.clone())
                    .with_projection(row(&s1, "r", Some("{\"v\":1}"))),
            )
            .await,
    )?;
    io(
        "a refused command",
        journal
            .append(unit(2_000).with_read(s2.clone(), NEW).with_receipt(refusal(
                2,
                1,
                "pool_draining",
            )))
            .await,
    )?;
    let probe = Probe {
        streams: vec![s1.clone(), s2.clone()],
        commands: vec![1, 2],
        keys: vec![message.clone()],
        events: vec![1],
        rows: vec!["r".to_owned()],
    };
    let before = observe(&journal, &probe).await?;
    let journal = factory.reopen(journal).await;
    eq(
        "everything after reopen",
        &observe(&journal, &probe).await?,
        &before,
    )?;
    let replay = CommandReceipt {
        command_id: CommandId::from_u128(1),
        command_type: command_type(),
        intent: intent(1),
        outcome: ReceiptOutcome::Accepted(first),
    };
    eq(
        "a repeat of the accepted command after reopen",
        &journal
            .append(
                UnitOfWork::new(Millis::new(3_000), build(2))
                    .with_append(s1.clone(), aggregate("attempt"), NEW, vec![at_epoch(1, 2)])
                    .with_receipt(accepted(1, 1)),
            )
            .await
            .map(|_| ()),
        &Err(AppendError::CommandReplayed(Box::new(replay))),
    )?;
    eq(
        "a repeat of the refused command after reopen",
        &journal
            .append(unit(3_000).with_read(s2.clone(), NEW).with_receipt(refusal(
                2,
                1,
                "pool_draining",
            )))
            .await
            .map(|e| e.streams.len()),
        &Err(AppendError::CommandReplayed(Box::new(CommandReceipt {
            command_id: CommandId::from_u128(2),
            command_type: command_type(),
            intent: intent(1),
            outcome: ReceiptOutcome::Refused {
                bound: vec![ReadCondition {
                    stream: s2.clone(),
                    expected: NEW,
                }],
                rejection: rejection("pool_draining"),
            },
        }))),
    )?;
    eq(
        "a redelivery after reopen",
        &journal
            .append(appending(unit(3_000), &s2, NEW, &[3]).with_inbox(message.clone()))
            .await
            .map(|_| ()),
        &Err(AppendError::DuplicateMessage(message)),
    )?;
    eq(
        "an append by a build supporting 1 after reopen",
        &journal
            .append(appending(unit(3_000), &s1, at(0), &[4]))
            .await
            .map(|_| ()),
        &Err(AppendError::StreamEpochUnsupported {
            stream: s1,
            epoch: 2,
        }),
    )
}

async fn outcome_unknown<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let (s1, s2) = (stream("attempt-1"), stream("attempt-2"));
    io(
        "seed",
        journal.append(appending(unit(1_000), &s2, NEW, &[1])).await,
    )?;
    let probe = Probe {
        streams: vec![s1.clone(), s2.clone()],
        commands: vec![1],
        keys: vec![key("scaleset-1", "1")],
        events: vec![10, 11],
        rows: vec!["r1".to_owned(), "r2".to_owned()],
    };
    let before = observe(&journal, &probe).await?;
    for (fault, want) in [
        (
            InjectedFault::StorageBeforeCommit,
            "a definite storage failure",
        ),
        (InjectedFault::Busy, "a definite busy refusal"),
    ] {
        must_inject(factory, &journal, fault).await?;
        let error = refused(journal.append(whole_unit(1, &s1, &s2, [10, 11])).await)?;
        ensure(
            &format!("{fault:?} must be {want}, got {error:?}"),
            error.is_definite()
                && matches!(
                    (fault, &error),
                    (InjectedFault::StorageBeforeCommit, AppendError::Storage(_))
                        | (InjectedFault::Busy, AppendError::Busy)
                ),
        )?;
        eq(
            &format!("the journal after {fault:?}"),
            &observe(&journal, &probe).await?,
            &before,
        )?;
    }

    must_inject(factory, &journal, InjectedFault::ReplyLostBeforeCommit).await?;
    let error = refused(journal.append(whole_unit(1, &s1, &s2, [10, 11])).await)?;
    ensure(
        &format!("a reply lost before the commit is OutcomeUnknown, got {error:?}"),
        matches!(error, AppendError::OutcomeUnknown(_)) && !error.is_definite(),
    )?;
    let committed = io(
        "the identical unit re-appended after an outcome that did not commit",
        journal.append(whole_unit(1, &s1, &s2, [10, 11])).await,
    )?;
    eq(
        "what the re-append appended",
        &committed.streams,
        &vec![appended(&s1, 0, &[10]), appended(&s2, 1, &[11])],
    )?;

    must_inject(factory, &journal, InjectedFault::ReplyLostAfterCommit).await?;
    let command = appending(unit(3_000), &s1, at(0), &[20, 21]).with_receipt(accepted(2, 1));
    let error = refused(journal.append(command.clone()).await)?;
    ensure(
        &format!("a reply lost after the commit is OutcomeUnknown, got {error:?}"),
        matches!(error, AppendError::OutcomeUnknown(_)),
    )?;
    eq(
        "the identical command re-appended: its receipt answers",
        &journal.append(command).await.map(|_| ()),
        &Err(AppendError::CommandReplayed(Box::new(CommandReceipt {
            command_id: CommandId::from_u128(2),
            command_type: command_type(),
            intent: intent(1),
            outcome: ReceiptOutcome::Accepted(Appended {
                streams: vec![appended(&s1, 1, &[20, 21])],
            }),
        }))),
    )?;

    must_inject(factory, &journal, InjectedFault::ReplyLostAfterCommit).await?;
    let receiptless = appending(unit(4_000), &s1, at(2), &[31, 30]);
    let error = refused(journal.append(receiptless.clone()).await)?;
    ensure(
        &format!("a reply lost after the commit is OutcomeUnknown, got {error:?}"),
        matches!(error, AppendError::OutcomeUnknown(_)),
    )?;
    eq(
        "the identical receipt-less unit re-appended: its lowest id answers",
        &journal.append(receiptless).await.map(|_| ()),
        &Err(AppendError::DuplicateEventId(EventId::from_u128(30))),
    )?;
    eq(
        "where the answering id lives: the place the unit wrote it",
        &io(
            "lookup_event",
            journal.lookup_event(EventId::from_u128(30)).await,
        )?,
        &Some(EventLocation {
            stream: s1.clone(),
            revision: rev(4),
        }),
    )?;
    lost_replies_without_events(factory, &journal, &s1).await
}

/// JC-25 for the units that append nothing: a refusal receipt and an inbox-only unit. A
/// re-append after a reply lost before the commit commits now; after one lost after the commit
/// it is answered by what the first commit stored (the refusal receipt, the inbox entry).
async fn lost_replies_without_events<F: JournalFactory>(
    factory: &F,
    journal: &F::Journal,
    s1: &StreamId,
) -> Check {
    let refusal_unit = || {
        unit(5_000)
            .with_read(s1.clone(), at(4))
            .with_receipt(refusal(3, 1, "pool_draining"))
    };
    must_inject(factory, journal, InjectedFault::ReplyLostBeforeCommit).await?;
    let error = refused(journal.append(refusal_unit()).await)?;
    ensure(
        &format!("a refusal receipt's reply lost before the commit, got {error:?}"),
        matches!(error, AppendError::OutcomeUnknown(_)),
    )?;
    eq(
        "the refusal receipt after a reply lost before the commit",
        &io("receipt", journal.receipt(CommandId::from_u128(3)).await)?,
        &None,
    )?;
    must_inject(factory, journal, InjectedFault::ReplyLostAfterCommit).await?;
    let error = refused(journal.append(refusal_unit()).await)?;
    ensure(
        &format!("a refusal receipt's reply lost after the commit, got {error:?}"),
        matches!(error, AppendError::OutcomeUnknown(_)),
    )?;
    eq(
        "the identical refusal re-appended: the stored refusal answers",
        &journal.append(refusal_unit()).await.map(|_| ()),
        &Err(AppendError::CommandReplayed(Box::new(CommandReceipt {
            command_id: CommandId::from_u128(3),
            command_type: command_type(),
            intent: intent(1),
            outcome: ReceiptOutcome::Refused {
                bound: vec![ReadCondition {
                    stream: s1.clone(),
                    expected: at(4),
                }],
                rejection: rejection("pool_draining"),
            },
        }))),
    )?;

    let message = key("scaleset-1", "lost");
    let inbox_only = || unit(6_000).with_inbox(message.clone());
    must_inject(factory, journal, InjectedFault::ReplyLostBeforeCommit).await?;
    let error = refused(journal.append(inbox_only()).await)?;
    ensure(
        &format!("an inbox-only unit's reply lost before the commit, got {error:?}"),
        matches!(error, AppendError::OutcomeUnknown(_)),
    )?;
    eq(
        "the message after a reply lost before the commit",
        &io("inbox_contains", journal.inbox_contains(&message).await)?,
        &false,
    )?;
    must_inject(factory, journal, InjectedFault::ReplyLostAfterCommit).await?;
    let error = refused(journal.append(inbox_only()).await)?;
    ensure(
        &format!("an inbox-only unit's reply lost after the commit, got {error:?}"),
        matches!(error, AppendError::OutcomeUnknown(_)),
    )?;
    eq(
        "the identical inbox-only unit re-appended: the stored entry answers",
        &journal.append(inbox_only()).await.map(|_| ()),
        &Err(AppendError::DuplicateMessage(message)),
    )
}

async fn racing_unique_key<F: JournalFactory>(
    factory: &F,
    what: &str,
    units: [UnitOfWork; 2],
    lost: impl Fn(usize) -> AppendError,
) -> Check {
    let journal = factory.fresh().await;
    let other = factory.second_handle(&journal).await;
    let streams: Vec<StreamId> = units
        .iter()
        .filter_map(|u| u.appends.first().map(|a| a.stream.clone()))
        .collect();
    let [a, b] = units;
    let outcomes = join_all(vec![journal.append(a), other.append(b)]).await;
    let (winner, loser) = one_winner(what, &outcomes, lost)?;
    let journal = factory.reopen(journal).await;
    let heads = [
        head_revision(&journal, &streams[winner]).await?,
        head_revision(&journal, &streams[loser]).await?,
    ];
    eq(
        &format!("{what}: the winner's and the loser's streams"),
        &heads,
        &[Some(rev(0)), None],
    )
}

async fn racing_command_ids<F: JournalFactory>(factory: &F) -> Check {
    let (s1, s2) = (stream("attempt-1"), stream("attempt-2"));
    let unit_on =
        |s: &StreamId, id: u128| appending(unit(1_000), s, NEW, &[id]).with_receipt(accepted(7, 1));
    let first = Appended {
        streams: vec![appended(&s1, 0, &[1])],
    };
    let second = Appended {
        streams: vec![appended(&s2, 0, &[2])],
    };
    racing_unique_key(
        factory,
        "two units of one command on two streams",
        [unit_on(&s1, 1), unit_on(&s2, 2)],
        |loser| {
            AppendError::CommandReplayed(Box::new(CommandReceipt {
                command_id: CommandId::from_u128(7),
                command_type: command_type(),
                intent: intent(1),
                outcome: ReceiptOutcome::Accepted(if loser == 0 {
                    second.clone()
                } else {
                    first.clone()
                }),
            }))
        },
    )
    .await
}

/// JC-29b. Two writers each re-decide command 7, whose refusal receipt is stale (its stream
/// moved after the refusal), and append on their own new stream. Each read the stale refusal,
/// so a journal that replaces the receipt without comparing it with the one it judged (a blind
/// upsert) commits both. Exactly one may commit; the other is answered by the winner's
/// receipt. The memory journal passes by judging one unit at a time; an engine passes because
/// it replaces the refusal only where the stored row is still the stale one.
async fn racing_stale_refusal<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let (s1, s2, s3) = (
        stream("attempt-1"),
        stream("attempt-2"),
        stream("attempt-3"),
    );
    io(
        "a refusal of command 7 bound to a new stream",
        journal
            .append(unit(1_000).with_read(s1.clone(), NEW).with_receipt(refusal(
                7,
                1,
                "pool_draining",
            )))
            .await,
    )?;
    io(
        "the bound stream moves",
        journal.append(appending(unit(1_500), &s1, NEW, &[1])).await,
    )?;
    let other = factory.second_handle(&journal).await;
    let redecided =
        |s: &StreamId, id: u128| appending(unit(2_000), s, NEW, &[id]).with_receipt(accepted(7, 1));
    let outcomes = join_all(vec![
        journal.append(redecided(&s2, 2)),
        other.append(redecided(&s3, 3)),
    ])
    .await;
    let won = |winner: usize| Appended {
        streams: vec![if winner == 0 {
            appended(&s2, 0, &[2])
        } else {
            appended(&s3, 0, &[3])
        }],
    };
    let receipt_of = |winner: usize| CommandReceipt {
        command_id: CommandId::from_u128(7),
        command_type: command_type(),
        intent: intent(1),
        outcome: ReceiptOutcome::Accepted(won(winner)),
    };
    let (winner, loser) = one_winner(
        "two re-decisions of one command with a stale refusal receipt",
        &outcomes,
        |loser| AppendError::CommandReplayed(Box::new(receipt_of(1 - loser))),
    )?;
    let journal = factory.reopen(journal).await;
    let streams = [&s2, &s3];
    eq(
        "the winner's and the loser's streams",
        &[
            head_revision(&journal, streams[winner]).await?,
            head_revision(&journal, streams[loser]).await?,
        ],
        &[Some(rev(0)), None],
    )?;
    eq(
        "the receipt that replaced the refusal",
        &io("receipt", journal.receipt(CommandId::from_u128(7)).await)?,
        &Some(receipt_of(winner)),
    )
}

async fn racing_inbox_keys<F: JournalFactory>(factory: &F) -> Check {
    let (s1, s2) = (stream("scaleset-1"), stream("scaleset-2"));
    let message = key("scaleset-1", "9");
    let unit_on =
        |s: &StreamId, id: u128| appending(unit(1_000), s, NEW, &[id]).with_inbox(message.clone());
    racing_unique_key(
        factory,
        "two units consuming one message on two streams",
        [unit_on(&s1, 1), unit_on(&s2, 2)],
        |_| AppendError::DuplicateMessage(message.clone()),
    )
    .await
}

async fn racing_event_ids<F: JournalFactory>(factory: &F) -> Check {
    let (s1, s2) = (stream("attempt-1"), stream("attempt-2"));
    racing_unique_key(
        factory,
        "two units carrying one event id on two streams",
        [
            appending(unit(1_000), &s1, NEW, &[5]),
            appending(unit(1_000), &s2, NEW, &[5]),
        ],
        |_| AppendError::DuplicateEventId(EventId::from_u128(5)),
    )
    .await
}

/// One part of [`whole_unit`], present or not: `0` the first stream's head, `1` its events,
/// `2` the second stream's event identity, `3` the second stream's head, `4` the receipt, `5`
/// the inbox entry, `6` the second stream's row.
async fn read_part<J: Journal>(
    journal: &J,
    s1: &StreamId,
    s2: &StreamId,
    part: usize,
) -> Result<bool, String> {
    Ok(match part {
        0 => head_revision(journal, s1).await?.is_some(),
        1 => !io("read_stream", journal.read_stream(s1, None, None).await)?.is_empty(),
        2 => io(
            "lookup_event",
            journal.lookup_event(EventId::from_u128(11)).await,
        )?
        .is_some(),
        3 => head_revision(journal, s2).await? == Some(rev(1)),
        4 => io("receipt", journal.receipt(CommandId::from_u128(1)).await)?.is_some(),
        5 => io(
            "inbox_contains",
            journal.inbox_contains(&key("scaleset-1", "1")).await,
        )?,
        _ => io(
            "projection",
            journal
                .projection(&projection_name(), &row_key(s2, "r2"))
                .await,
        )?
        .is_some(),
    })
}

/// The number of parts [`read_part`] reads.
const PARTS: usize = 7;

/// One reader pass over the parts of [`whole_unit`], each present or not, in the order read:
/// first stream first, or (`reverse`) last stream first, so a journal that exposes either end
/// of a unit before the other is caught by one of the two orders.
async fn read_parts<J: Journal>(
    journal: &J,
    s1: &StreamId,
    s2: &StreamId,
    reverse: bool,
) -> Result<Vec<bool>, String> {
    let mut order: Vec<usize> = (0..PARTS).collect();
    if reverse {
        order.reverse();
    }
    let mut parts = Vec::new();
    for part in order {
        parts.push(read_part(journal, s1, s2, part).await?);
    }
    Ok(parts)
}

async fn unit_visible_at_once<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let reader = factory.second_handle(&journal).await;
    let (s1, s2) = (stream("attempt-1"), stream("attempt-2"));
    io(
        "seed",
        journal.append(appending(unit(1_000), &s2, NEW, &[1])).await,
    )?;
    let observer = async {
        let mut passes = Vec::new();
        for n in 0..16 {
            passes.push(read_parts(&reader, &s1, &s2, n % 2 == 0).await);
            YieldNow(false).await;
        }
        passes
    };
    let (outcome, passes) =
        join2(journal.append(whole_unit(1, &s1, &s2, [10, 11])), observer).await;
    io("the unit", outcome)?;
    for (n, pass) in passes.into_iter().enumerate() {
        let pass = pass?;
        // A commit between two reads of one pass shows earlier parts absent and later parts
        // present; a part present before one absent means the reader saw half a unit.
        let torn = pass.windows(2).any(|w| matches!(w, [true, false]));
        ensure(
            &format!("reader pass {n} saw part of a unit: {pass:?}"),
            !torn,
        )?;
    }
    eq(
        "every part after the commit",
        &read_parts(&reader, &s1, &s2, false).await?,
        &vec![true; PARTS],
    )
}

// ---------------------------------------------------------------------------------------------
// JC-33, JC-34: receipts across builds, refusals and the inbox (ES-06F)

/// JC-33: a newer build (epoch 2) raises streams and records receipts there; an older build
/// (epoch 1) repeating the same commands is refused `StreamEpochUnsupported` naming the lowest
/// stream the answer is for, never answered from the receipt, whatever the receipt: an
/// acceptance (its appended stream), a refusal bound to two raised streams (the lowest named),
/// an acceptance with no event (the repeat's own stream stands in). The newer build is still
/// answered, and a receipt whose streams are within the older build answers it too.
async fn replay_within_epoch<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let (s1, s2, s3, s4, s5) = (
        stream("attempt-1"),
        stream("attempt-2"),
        stream("attempt-3"),
        stream("attempt-4"),
        stream("attempt-5"),
    );
    let by = |epoch: u32| UnitOfWork::new(Millis::new(1_000), build(epoch));
    let add = |u: UnitOfWork, s: &StreamId, expected: ExpectedRevision, id: u128, epoch: u32| {
        u.with_append(
            s.clone(),
            aggregate("attempt"),
            expected,
            vec![at_epoch(id, epoch)],
        )
    };
    // The newer build: command 1 accepted on s1 at epoch 2; s3, s2 and s4 raised; command 2
    // refused over s3 and s2; command 3 accepted on s4 with no event; command 4 accepted on s5
    // at epoch 1.
    io(
        "command 1 by the newer build",
        journal
            .append(add(by(2), &s1, NEW, 1, 2).with_receipt(accepted(1, 1)))
            .await,
    )?;
    for (s, id) in [(&s3, 3), (&s2, 2), (&s4, 4)] {
        io(
            "a stream raised by the newer build",
            journal.append(add(by(2), s, NEW, id, 2)).await,
        )?;
    }
    io(
        "command 2 refused by the newer build",
        journal
            .append(
                by(2)
                    .with_read(s3.clone(), at(0))
                    .with_read(s2.clone(), at(0))
                    .with_receipt(refusal(2, 1, "pool_draining")),
            )
            .await,
    )?;
    io(
        "command 3 accepted with no event by the newer build",
        journal
            .append(
                by(2)
                    .with_read(s4.clone(), at(0))
                    .with_receipt(accepted(3, 1)),
            )
            .await,
    )?;
    let fourth = io(
        "command 4 by the newer build at epoch 1",
        journal
            .append(add(by(2), &s5, NEW, 5, 1).with_receipt(accepted(4, 1)))
            .await,
    )?;
    let journal = factory.reopen(journal).await;
    let probe = Probe {
        streams: vec![s1.clone(), s2.clone(), s3.clone(), s4.clone(), s5.clone()],
        commands: vec![1, 2, 3, 4],
        events: vec![1, 2, 3, 4, 5, 11, 15],
        ..Probe::default()
    };
    let before = observe(&journal, &probe).await?;
    let older = [
        (
            "command 1 repeated by the older build",
            add(by(1), &s1, NEW, 11, 1).with_receipt(accepted(1, 1)),
            &s1,
        ),
        (
            "command 2 repeated by the older build",
            by(1)
                .with_read(s2.clone(), at(0))
                .with_read(s3.clone(), at(0))
                .with_receipt(refusal(2, 1, "pool_draining")),
            &s2,
        ),
        (
            "command 3 repeated by the older build",
            by(1)
                .with_read(s4.clone(), at(0))
                .with_receipt(accepted(3, 1)),
            &s4,
        ),
    ];
    for (what, repeat, named) in older {
        eq(
            what,
            &journal.append(repeat).await.map(|_| ()),
            &Err(AppendError::StreamEpochUnsupported {
                stream: named.clone(),
                epoch: 2,
            }),
        )?;
    }
    eq(
        "the journal after the older build's repeats",
        &observe(&journal, &probe).await?,
        &before,
    )?;
    let replayed = |r: Result<Appended, AppendError>| match r {
        Err(AppendError::CommandReplayed(receipt)) => Some(receipt.command_id.as_u128()),
        _ => None,
    };
    eq(
        "command 1 repeated by the newer build",
        &replayed(
            journal
                .append(add(by(2), &s1, NEW, 11, 1).with_receipt(accepted(1, 1)))
                .await,
        ),
        &Some(1),
    )?;
    eq(
        "command 4 repeated by the older build",
        &journal
            .append(add(by(1), &s5, NEW, 15, 1).with_receipt(accepted(4, 1)))
            .await
            .map(|_| ()),
        &Err(AppendError::CommandReplayed(Box::new(CommandReceipt {
            command_id: CommandId::from_u128(4),
            command_type: command_type(),
            intent: intent(1),
            outcome: ReceiptOutcome::Accepted(fourth),
        }))),
    )
}

/// JC-34: a refusal receipt carrying an inbox key is malformed (ADR 0041 rule 6: a refusal
/// consumes no message); nothing is written, and the key is still free afterwards.
async fn refusal_with_inbox<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let s1 = stream("attempt-1");
    let message = key("scaleset-1", "m1");
    let probe = Probe {
        streams: vec![s1.clone()],
        commands: vec![1],
        keys: vec![message.clone()],
        ..Probe::default()
    };
    let before = observe(&journal, &probe).await?;
    eq(
        "a refusal carrying an inbox key",
        &journal
            .append(
                unit(1_000)
                    .with_read(s1.clone(), NEW)
                    .with_receipt(refusal(1, 1, "pool_draining"))
                    .with_inbox(message.clone()),
            )
            .await
            .map(|_| ()),
        &Err(AppendError::Malformed(MalformedUnit::RefusalWithInbox)),
    )?;
    let journal = factory.reopen(journal).await;
    eq(
        "the journal after it",
        &observe(&journal, &probe).await?,
        &before,
    )?;
    io(
        "the same refusal without the key",
        journal
            .append(unit(1_000).with_read(s1.clone(), NEW).with_receipt(refusal(
                1,
                1,
                "pool_draining",
            )))
            .await,
    )?;
    io(
        "the key, consumed by a unit that only records it",
        journal.append(unit(1_000).with_inbox(message)).await,
    )
    .map(|_| ())
}

// ---------------------------------------------------------------------------------------------
// JC-26, JC-27: the cases only a storage engine has (adapters only, `doc/event-store.md` §7.1)

/// A raw statement that would rewrite history, run on the store beside the adapter (as an
/// operator's SQL shell or a buggy migration would), never through the port (JC-27).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Tamper {
    /// Change a stored event's payload.
    UpdateEvent,
    /// Delete a stored event.
    DeleteEvent,
    /// Move a stored event identity to another revision.
    UpdateIdentity,
    /// Delete a stored event identity.
    DeleteIdentity,
    /// Lower a stream head's revision.
    LowerHeadRevision,
    /// Lower a stream head's epoch.
    LowerHeadEpoch,
    /// Delete a stream head.
    DeleteHead,
}

impl Tamper {
    /// Every tampering JC-27 tries.
    pub const ALL: [Self; 7] = [
        Self::UpdateEvent,
        Self::DeleteEvent,
        Self::UpdateIdentity,
        Self::DeleteIdentity,
        Self::LowerHeadRevision,
        Self::LowerHeadEpoch,
        Self::DeleteHead,
    ];
}

/// The hooks of a factory whose journal stores into an engine (SQLite, PostgreSQL): what the
/// engine-only cases need beside [`JournalFactory`]. The memory journal has no engine and no
/// such factory.
pub trait EngineJournalFactory: JournalFactory {
    /// Opens a new store whose engine lacks the durability settings the adapter requires
    /// (journal mode, synchronous commit), under the production profile (`allow_relaxed` false)
    /// or the test profile that tolerates it (`allow_relaxed` true). `None`: the adapter refused
    /// to open it (JC-26).
    fn open_without_durability(
        &self,
        allow_relaxed: bool,
    ) -> impl Future<Output = Option<Self::Journal>>;

    /// Runs `tamper` on `stream`'s first event (and its head) through a raw engine connection
    /// beside `journal`; `true` when the engine accepted the statement (JC-27).
    fn tamper(
        &self,
        journal: &Self::Journal,
        stream: &StreamId,
        tamper: Tamper,
    ) -> impl Future<Output = bool>;
}

/// One case only an engine has, named after its id in `doc/event-store.md` §7.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EngineCase {
    /// JC-26: a store without its durability settings is refused at open unless the test
    /// profile allows it.
    DurabilityAtOpen,
    /// JC-27: raw `UPDATE`/`DELETE` of journal rows, and any lowering of a head, is refused by
    /// the store itself, and the journal reads the same afterwards.
    HistoryRefusesRewrite,
}

impl EngineCase {
    /// Every engine case, in run order.
    pub const ALL: [Self; 2] = [Self::DurabilityAtOpen, Self::HistoryRefusesRewrite];

    /// The case's id in `doc/event-store.md` §7.1.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::DurabilityAtOpen => "JC-26",
            Self::HistoryRefusesRewrite => "JC-27",
        }
    }

    /// The case's `snake_case` name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::DurabilityAtOpen => "durability_at_open",
            Self::HistoryRefusesRewrite => "history_refuses_rewrite",
        }
    }
}

impl Display for EngineCase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.id(), self.name())
    }
}

/// Runs the engine cases against `factory`; returns each failed case with why.
pub async fn check_engine_contract<F: EngineJournalFactory>(
    factory: &F,
) -> Vec<(EngineCase, String)> {
    let mut failures = Vec::new();
    for case in EngineCase::ALL {
        let outcome = match case {
            EngineCase::DurabilityAtOpen => durability_at_open(factory).await,
            EngineCase::HistoryRefusesRewrite => history_refuses_rewrite(factory).await,
        };
        if let Err(detail) = outcome {
            failures.push((case, detail));
        }
    }
    failures
}

async fn durability_at_open<F: EngineJournalFactory>(factory: &F) -> Check {
    ensure(
        "a store without its durability settings opened under the production profile",
        factory.open_without_durability(false).await.is_none(),
    )?;
    ensure(
        "a store without its durability settings refused under the test profile",
        factory.open_without_durability(true).await.is_some(),
    )
}

async fn history_refuses_rewrite<F: EngineJournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let s1 = stream("attempt-1");
    io(
        "an event at epoch 2",
        journal
            .append(UnitOfWork::new(Millis::new(1_000), build(2)).with_append(
                s1.clone(),
                aggregate("attempt"),
                NEW,
                vec![at_epoch(1, 2)],
            ))
            .await,
    )?;
    io(
        "a second event",
        journal
            .append(UnitOfWork::new(Millis::new(2_000), build(2)).with_append(
                s1.clone(),
                aggregate("attempt"),
                at(0),
                vec![at_epoch(2, 2)],
            ))
            .await,
    )?;
    let probe = Probe {
        streams: vec![s1.clone()],
        events: vec![1, 2],
        ..Probe::default()
    };
    let before = observe(&journal, &probe).await?;
    for tamper in Tamper::ALL {
        ensure(
            &format!("the store accepted {tamper:?}"),
            !factory.tamper(&journal, &s1, tamper).await,
        )?;
        eq(
            &format!("the journal after a refused {tamper:?}"),
            &observe(&journal, &probe).await?,
            &before,
        )?;
    }
    let journal = factory.reopen(journal).await;
    eq(
        "the journal after the refusals and a reopen",
        &observe(&journal, &probe).await?,
        &before,
    )
}
