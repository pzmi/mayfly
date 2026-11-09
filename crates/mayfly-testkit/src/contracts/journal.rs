//! The journal contract suite (ver §5): one set of cases every `Journal` implementation
//! passes, written once over a [`JournalFactory`].
//!
//! Every case starts from [`JournalFactory::fresh`] and calls [`JournalFactory::reopen`]
//! between writing and checking, so one suite runs a journal fresh (a factory whose reopen
//! hands the same handle back) and reopened (a factory that closes and reopens its file).
//! [`check_journal_contract`] returns a report of failed cases instead of panicking: the
//! suite's own tests use it to show deliberately broken journals failing named cases
//! (`tests/journal_contract.rs`).
//!
//! The cases hold the stream-scoped event store design (ADR 0021): per-stream order on a
//! stream head, an event-id registry, command receipts, and no global order to read. The
//! concurrency cases drive several appends at once on one task ([`join_all`]), so an adapter
//! whose append really waits (a writer thread, a database) interleaves them; none of them
//! needs a runtime.

use std::fmt::{self, Debug, Display};
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::task::Poll;

use mayfly_control::journal::{
    AppendError, CommandReceipt, EventLocation, InboxEntry, InboxSource, Journal, MalformedUnit,
    MessageId, NewDirective, NewEvent, OutboxRow, OutboxStatus, OutboxTransition, OutboxWrite,
    Payload, ProjectionName, ProjectionWrite, ReceiptStatus, RecordedEvent, RecordedKind, StreamId,
    StreamRevision, UnitOfWork,
};
use mayfly_domain::{AttemptId, CommandId, DirectiveId, EventId, Generation, Millis};

use crate::block_on::block_on;

/// Makes the journals the suite runs against.
pub trait JournalFactory {
    /// The implementation under test.
    type Journal: Journal;

    /// A new, empty journal.
    fn fresh(&self) -> impl Future<Output = Self::Journal>;

    /// The same store, closed and opened again; a store with nothing to reopen returns
    /// `journal` itself. Whatever was committed before must read the same after.
    fn reopen(&self, journal: Self::Journal) -> impl Future<Output = Self::Journal>;
}

/// One case of the journal contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum JournalCase {
    /// Appended events read back per stream with their revisions, sequences, kinds,
    /// payloads and `recorded_at`.
    AppendsAndReadsStreams,
    /// A wrong expected revision (stale, ahead, or on a missing stream) is refused with the
    /// actual revision and changes nothing.
    ExpectedRevisionConflict,
    /// An event id already recorded is refused and changes nothing.
    DuplicateEventId,
    /// A unit refused at any step leaves every table as it was; an accepted one writes all
    /// of its parts.
    AllOrNothing,
    /// A receipt survives a reopen; a repeat of its command returns the original receipt,
    /// even when the streams have moved, and changes nothing.
    ReceiptReplay,
    /// A processed inbox entry refuses its redelivery, even when the streams have moved;
    /// the same message id from another source is a different message.
    InboxDedupe,
    /// Outbox rows move exactly as `OutboxStatus::allows` says, refuse a stale `from`, count
    /// attempts and list by status.
    OutboxStatusTransitions,
    /// Two appends racing at one expected revision of one stream: exactly one commits, the
    /// other is refused as a lost race and leaves nothing of its unit behind (the stream head's
    /// compare-and-set).
    RacingWritersOnOneStream,
    /// Appends to different streams commit at once, each judged by its own stream alone; only
    /// an event id two of them share decides between them (the event-id registry).
    IndependentStreams,
    /// Per-stream reads honour their bounds; streams enumerate by id; the event-id registry
    /// and the event-kind manifest answer; a refused or eventless append creates no stream.
    StreamReads,
    /// Projection rows are set, replaced and removed with their unit.
    ProjectionWrites,
    /// A malformed unit is refused before anything is checked or written.
    MalformedUnit,
}

impl JournalCase {
    /// Every case, in the order the suite runs them.
    pub const ALL: [Self; 12] = [
        Self::AppendsAndReadsStreams,
        Self::ExpectedRevisionConflict,
        Self::DuplicateEventId,
        Self::AllOrNothing,
        Self::ReceiptReplay,
        Self::InboxDedupe,
        Self::OutboxStatusTransitions,
        Self::RacingWritersOnOneStream,
        Self::IndependentStreams,
        Self::StreamReads,
        Self::ProjectionWrites,
        Self::MalformedUnit,
    ];

    /// The case's `snake_case` name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::AppendsAndReadsStreams => "appends_and_reads_streams",
            Self::ExpectedRevisionConflict => "expected_revision_conflict",
            Self::DuplicateEventId => "duplicate_event_id",
            Self::AllOrNothing => "all_or_nothing",
            Self::ReceiptReplay => "receipt_replay",
            Self::InboxDedupe => "inbox_dedupe",
            Self::OutboxStatusTransitions => "outbox_status_transitions",
            Self::RacingWritersOnOneStream => "racing_writers_on_one_stream",
            Self::IndependentStreams => "independent_streams",
            Self::StreamReads => "stream_reads",
            Self::ProjectionWrites => "projection_writes",
            Self::MalformedUnit => "malformed_unit",
        }
    }
}

impl Display for JournalCase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
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
        JournalCase::AppendsAndReadsStreams => appends_and_reads_streams(factory).await,
        JournalCase::ExpectedRevisionConflict => expected_revision_conflict(factory).await,
        JournalCase::DuplicateEventId => duplicate_event_id(factory).await,
        JournalCase::AllOrNothing => all_or_nothing(factory).await,
        JournalCase::ReceiptReplay => receipt_replay(factory).await,
        JournalCase::InboxDedupe => inbox_dedupe(factory).await,
        JournalCase::OutboxStatusTransitions => outbox_status_transitions(factory).await,
        JournalCase::RacingWritersOnOneStream => racing_writers_on_one_stream(factory).await,
        JournalCase::IndependentStreams => independent_streams(factory).await,
        JournalCase::StreamReads => stream_reads(factory).await,
        JournalCase::ProjectionWrites => projection_writes(factory).await,
        JournalCase::MalformedUnit => malformed_unit(factory).await,
    }
}

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

fn stream(name: &str) -> StreamId {
    StreamId::new(name).unwrap_or_else(|e| panic!("contract stream id {name}: {e}"))
}

fn rev(n: u64) -> StreamRevision {
    StreamRevision::new(n)
}

fn at(ms: u64) -> Millis {
    Millis::new(ms)
}

fn event(n: u128) -> NewEvent {
    NewEvent {
        event_id: EventId::from_u128(n),
        event_type: "contract_fact_recorded".to_owned(),
        schema_version: 1,
        payload: Payload::new(format!("{{\"n\":{n}}}")),
    }
}

fn ids(events: &[RecordedEvent]) -> Vec<u128> {
    events.iter().map(|e| e.event_id.as_u128()).collect()
}

fn receipt(command: u128, status: ReceiptStatus, result: &str) -> CommandReceipt {
    CommandReceipt {
        command_id: CommandId::from_u128(command),
        status,
        result: Payload::new(result),
    }
}

fn inbox(source: &str, message: &str) -> InboxEntry {
    InboxEntry {
        source: InboxSource::new(source)
            .unwrap_or_else(|e| panic!("contract inbox source {source}: {e}")),
        message_id: MessageId::new(message)
            .unwrap_or_else(|e| panic!("contract message id {message}: {e}")),
    }
}

fn directive(id: u128, next_attempt_at: u64) -> OutboxWrite {
    OutboxWrite::Insert(NewDirective {
        directive_id: DirectiveId::from_u128(id),
        attempt_id: AttemptId::from_u128(id + 1_000),
        generation: Generation::FIRST,
        kind: "clone_vm".to_owned(),
        request: Payload::new(format!("{{\"directive\":{id}}}")),
        next_attempt_at: at(next_attempt_at),
    })
}

fn transition(id: u128, from: OutboxStatus, to: OutboxStatus, next: u64) -> OutboxWrite {
    OutboxWrite::Transition(OutboxTransition {
        directive_id: DirectiveId::from_u128(id),
        from,
        to,
        next_attempt_at: at(next),
    })
}

fn projection_name() -> ProjectionName {
    ProjectionName::new("contract_rows").unwrap_or_else(|e| panic!("contract projection: {e}"))
}

fn row(key: &str, value: Option<&str>) -> ProjectionWrite {
    ProjectionWrite {
        projection: projection_name(),
        key: key.to_owned(),
        value: value.map(Payload::new),
    }
}

/// Every recorded event, stream by stream in id order, each in revision order: what the
/// journal holds, read without any global order.
async fn history<J: Journal>(journal: &J) -> Result<Vec<RecordedEvent>, String> {
    let streams = io("streams", journal.streams(None, usize::MAX).await)?;
    let mut events = Vec::new();
    for stream in &streams {
        events.extend(io(
            "read_stream",
            journal
                .read_stream(stream, StreamRevision::EMPTY, usize::MAX)
                .await,
        )?);
    }
    Ok(events)
}

/// Polls every future on the calling task until all are ready, in order: appends run "at
/// once" without a runtime, interleaving wherever an implementation yields.
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

async fn appends_and_reads_streams<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let s1 = stream("attempt-1");
    eq(
        "revision of a new stream",
        &io("stream_revision", journal.stream_revision(&s1).await)?,
        &StreamRevision::EMPTY,
    )?;
    ensure(
        "an empty journal reads no events",
        history(&journal).await?.is_empty(),
    )?;

    let mut accented = event(2);
    accented.payload = Payload::new("{\"note\":\"zażółć \\u0000 ✓\"}");
    accented.schema_version = 3;
    let first = io(
        "first append",
        journal
            .append(UnitOfWork::new(at(1_000)).with_stream(
                s1.clone(),
                StreamRevision::EMPTY,
                vec![event(1), accented.clone()],
            ))
            .await,
    )?;
    eq(
        "revisions of the first append",
        &first.events.iter().map(|e| e.revision).collect::<Vec<_>>(),
        &vec![rev(1), rev(2)],
    )?;
    let recorded = first
        .events
        .get(1)
        .ok_or_else(|| "the first append recorded fewer than two events".to_owned())?;
    eq("recorded event id", &recorded.event_id, &accented.event_id)?;
    eq("recorded type", &recorded.event_type, &accented.event_type)?;
    eq("recorded version", &recorded.schema_version, &3)?;
    eq("recorded payload", &recorded.payload, &accented.payload)?;
    eq("recorded stream", &recorded.stream, &s1)?;
    eq("recorded_at", &recorded.recorded_at, &at(1_000))?;

    let journal = factory.reopen(journal).await;
    eq(
        "stream read after reopen",
        &io(
            "read_stream",
            journal
                .read_stream(&s1, StreamRevision::EMPTY, usize::MAX)
                .await,
        )?,
        &first.events,
    )?;
    eq(
        "revision after two events",
        &io("stream_revision", journal.stream_revision(&s1).await)?,
        &rev(2),
    )?;

    let second = io(
        "second append",
        journal
            .append(UnitOfWork::new(at(2_000)).with_stream(s1.clone(), rev(2), vec![event(3)]))
            .await,
    )?;
    eq(
        "the third event",
        &second
            .events
            .iter()
            .map(|e| (e.revision, e.recorded_at))
            .collect::<Vec<_>>(),
        &vec![(rev(3), at(2_000))],
    )?;
    eq(
        "the whole stream",
        &ids(&history(&journal).await?),
        &vec![1, 2, 3],
    )
}

async fn expected_revision_conflict<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let s1 = stream("attempt-1");
    let s2 = stream("attempt-2");
    io(
        "seed append",
        journal
            .append(UnitOfWork::new(at(1_000)).with_stream(
                s1.clone(),
                StreamRevision::EMPTY,
                vec![event(1), event(2)],
            ))
            .await,
    )?;
    let before = history(&journal).await?;

    let attempts = [
        (s1.clone(), StreamRevision::EMPTY, rev(2)),
        (s1.clone(), rev(1), rev(2)),
        (s1.clone(), rev(3), rev(2)),
        (s2.clone(), rev(1), StreamRevision::EMPTY),
    ];
    for (target, expected, actual) in attempts {
        let refused = journal
            .append(
                UnitOfWork::new(at(2_000))
                    .with_stream(target.clone(), expected, vec![event(3)])
                    .with_receipt(receipt(1, ReceiptStatus::Accepted, "{}")),
            )
            .await;
        eq(
            &format!("append to {target} expecting {expected}"),
            &refused.map(|_| ()),
            &Err(AppendError::WrongExpectedRevision {
                stream: target,
                expected,
                actual,
            }),
        )?;
        eq(
            "events after a conflict",
            &history(&journal).await?,
            &before,
        )?;
        eq(
            "receipt after a conflict",
            &io("receipt", journal.receipt(CommandId::from_u128(1)).await)?,
            &None,
        )?;
    }

    let journal = factory.reopen(journal).await;
    eq("events after reopen", &history(&journal).await?, &before)?;
    eq(
        "revision of the conflicted stream",
        &io("stream_revision", journal.stream_revision(&s1).await)?,
        &rev(2),
    )?;
    eq(
        "revision of the missing stream",
        &io("stream_revision", journal.stream_revision(&s2).await)?,
        &StreamRevision::EMPTY,
    )?;
    io(
        "the refused unit at the right revision",
        journal
            .append(
                UnitOfWork::new(at(3_000))
                    .with_stream(s1.clone(), rev(2), vec![event(3)])
                    .with_receipt(receipt(1, ReceiptStatus::Accepted, "{}")),
            )
            .await,
    )?;
    eq(
        "revision after the retry",
        &io("stream_revision", journal.stream_revision(&s1).await)?,
        &rev(3),
    )
}

async fn duplicate_event_id<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let s1 = stream("attempt-1");
    let s2 = stream("attempt-2");
    io(
        "seed append",
        journal
            .append(UnitOfWork::new(at(1_000)).with_stream(
                s1,
                StreamRevision::EMPTY,
                vec![event(1)],
            ))
            .await,
    )?;
    let journal = factory.reopen(journal).await;
    let before = history(&journal).await?;

    let refused = journal
        .append(UnitOfWork::new(at(2_000)).with_stream(
            s2.clone(),
            StreamRevision::EMPTY,
            vec![event(2), event(1)],
        ))
        .await;
    eq(
        "append of a recorded event id",
        &refused.map(|_| ()),
        &Err(AppendError::DuplicateEventId(EventId::from_u128(1))),
    )?;
    eq(
        "events after the refusal",
        &history(&journal).await?,
        &before,
    )?;
    eq(
        "revision of the refused stream",
        &io("stream_revision", journal.stream_revision(&s2).await)?,
        &StreamRevision::EMPTY,
    )?;

    let journal = factory.reopen(journal).await;
    io(
        "the refused unit's fresh event alone",
        journal
            .append(UnitOfWork::new(at(3_000)).with_stream(
                s2,
                StreamRevision::EMPTY,
                vec![event(2)],
            ))
            .await,
    )
    .map(|_| ())
}

/// What `all_or_nothing` reads to prove a refused unit left no trace.
#[derive(Debug, PartialEq)]
struct Trace {
    events: Vec<RecordedEvent>,
    s1: StreamRevision,
    s2: StreamRevision,
    receipt: Option<CommandReceipt>,
    processed: Option<Millis>,
    seeded: Option<OutboxRow>,
    inserted: Option<OutboxRow>,
    row: Option<Payload>,
}

async fn trace<J: Journal>(journal: &J) -> Result<Trace, String> {
    Ok(Trace {
        events: history(journal).await?,
        s1: io(
            "stream_revision",
            journal.stream_revision(&stream("attempt-1")).await,
        )?,
        s2: io(
            "stream_revision",
            journal.stream_revision(&stream("attempt-2")).await,
        )?,
        receipt: io("receipt", journal.receipt(CommandId::from_u128(1)).await)?,
        processed: io(
            "processed_at",
            journal.processed_at(&inbox("scaleset-1", "m-1")).await,
        )?,
        seeded: io(
            "directive",
            journal.directive(DirectiveId::from_u128(1)).await,
        )?,
        inserted: io(
            "directive",
            journal.directive(DirectiveId::from_u128(2)).await,
        )?,
        row: io(
            "projection",
            journal.projection(&projection_name(), "k").await,
        )?,
    })
}

async fn all_or_nothing<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let s1 = stream("attempt-1");
    let s2 = stream("attempt-2");
    io(
        "seed append",
        journal
            .append(
                UnitOfWork::new(at(1_000))
                    .with_stream(s2.clone(), StreamRevision::EMPTY, vec![event(1)])
                    .with_outbox(directive(1, 1_000)),
            )
            .await,
    )?;
    let journal = factory.reopen(journal).await;
    let before = trace(&journal).await?;

    let whole = |s2_expected: u64, s2_event: u128, d1_from: OutboxStatus| {
        UnitOfWork::new(at(2_000))
            .with_stream(s1.clone(), StreamRevision::EMPTY, vec![event(10)])
            .with_stream(s2.clone(), rev(s2_expected), vec![event(s2_event)])
            .with_receipt(receipt(1, ReceiptStatus::Accepted, "{\"ok\":true}"))
            .with_inbox(inbox("scaleset-1", "m-1"))
            .with_outbox(directive(2, 2_000))
            .with_outbox(transition(1, d1_from, OutboxStatus::Sent, 2_000))
            .with_projection(row("k", Some("{\"v\":1}")))
    };
    let refusals = [
        (
            "a unit whose second stream conflicts",
            whole(0, 11, OutboxStatus::Pending),
            AppendError::WrongExpectedRevision {
                stream: s2.clone(),
                expected: StreamRevision::EMPTY,
                actual: rev(1),
            },
        ),
        (
            "a unit whose last event id is recorded",
            whole(1, 1, OutboxStatus::Pending),
            AppendError::DuplicateEventId(EventId::from_u128(1)),
        ),
        (
            "a unit whose last outbox write is stale",
            whole(1, 11, OutboxStatus::Sent),
            AppendError::OutboxStatusMismatch {
                directive_id: DirectiveId::from_u128(1),
                expected: OutboxStatus::Sent,
                actual: OutboxStatus::Pending,
            },
        ),
    ];
    let mut journal = journal;
    for (what, unit, error) in refusals {
        eq(what, &journal.append(unit).await.map(|_| ()), &Err(error))?;
        eq(
            &format!("state after {what}"),
            &trace(&journal).await?,
            &before,
        )?;
        journal = factory.reopen(journal).await;
        eq(
            &format!("state after {what}, reopened"),
            &trace(&journal).await?,
            &before,
        )?;
    }

    io(
        "the whole unit",
        journal.append(whole(1, 11, OutboxStatus::Pending)).await,
    )?;
    let journal = factory.reopen(journal).await;
    let after = trace(&journal).await?;
    eq(
        "events of the whole unit",
        &ids(&after.events),
        &vec![10, 1, 11],
    )?;
    eq("first stream after the whole unit", &after.s1, &rev(1))?;
    eq("second stream after the whole unit", &after.s2, &rev(2))?;
    eq(
        "receipt of the whole unit",
        &after.receipt,
        &Some(receipt(1, ReceiptStatus::Accepted, "{\"ok\":true}")),
    )?;
    eq(
        "inbox entry of the whole unit",
        &after.processed,
        &Some(at(2_000)),
    )?;
    eq(
        "moved directive",
        &after.seeded.map(|r| r.status),
        &Some(OutboxStatus::Sent),
    )?;
    eq(
        "inserted directive",
        &after.inserted.map(|r| r.status),
        &Some(OutboxStatus::Pending),
    )?;
    eq(
        "projection row",
        &after.row,
        &Some(Payload::new("{\"v\":1}")),
    )
}

async fn receipt_replay<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let s1 = stream("attempt-1");
    let original = receipt(1, ReceiptStatus::Accepted, "{\"events\":1}");
    io(
        "the command's unit",
        journal
            .append(
                UnitOfWork::new(at(1_000))
                    .with_stream(s1.clone(), StreamRevision::EMPTY, vec![event(1)])
                    .with_receipt(original.clone()),
            )
            .await,
    )?;
    let journal = factory.reopen(journal).await;
    eq(
        "receipt after reopen",
        &io("receipt", journal.receipt(CommandId::from_u128(1)).await)?,
        &Some(original.clone()),
    )?;

    let replay = journal
        .append(
            UnitOfWork::new(at(2_000))
                .with_stream(s1.clone(), StreamRevision::EMPTY, vec![event(2)])
                .with_receipt(receipt(1, ReceiptStatus::Accepted, "{\"events\":2}")),
        )
        .await;
    eq(
        "repeat of the command with a stale revision",
        &replay.map(|_| ()),
        &Err(AppendError::DuplicateCommand(Box::new(original.clone()))),
    )?;
    eq(
        "stream after the repeat",
        &ids(&history(&journal).await?),
        &vec![1],
    )?;

    let refusal = receipt(2, ReceiptStatus::Refused, "{\"code\":\"pool_draining\"}");
    io(
        "a refused command's receipt alone",
        journal
            .append(UnitOfWork::new(at(3_000)).with_receipt(refusal.clone()))
            .await,
    )?;
    let journal = factory.reopen(journal).await;
    eq(
        "first receipt after the second reopen",
        &io("receipt", journal.receipt(CommandId::from_u128(1)).await)?,
        &Some(original),
    )?;
    eq(
        "refusal receipt after reopen",
        &io("receipt", journal.receipt(CommandId::from_u128(2)).await)?,
        &Some(refusal.clone()),
    )?;
    eq(
        "repeat of the refused command",
        &journal
            .append(UnitOfWork::new(at(4_000)).with_receipt(receipt(
                2,
                ReceiptStatus::Accepted,
                "{}",
            )))
            .await
            .map(|_| ()),
        &Err(AppendError::DuplicateCommand(Box::new(refusal))),
    )?;
    eq(
        "receipt of a command that never ran",
        &io("receipt", journal.receipt(CommandId::from_u128(3)).await)?,
        &None,
    )
}

async fn inbox_dedupe<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let s1 = stream("scaleset-1");
    let message = inbox("scaleset-1", "42");
    io(
        "the message's unit",
        journal
            .append(
                UnitOfWork::new(at(1_000))
                    .with_stream(s1.clone(), StreamRevision::EMPTY, vec![event(1)])
                    .with_inbox(message.clone()),
            )
            .await,
    )?;
    let journal = factory.reopen(journal).await;
    eq(
        "processed_at after reopen",
        &io("processed_at", journal.processed_at(&message).await)?,
        &Some(at(1_000)),
    )?;

    for (what, expected) in [("current", rev(1)), ("stale", StreamRevision::EMPTY)] {
        let redelivery = journal
            .append(
                UnitOfWork::new(at(2_000))
                    .with_stream(s1.clone(), expected, vec![event(2)])
                    .with_inbox(message.clone()),
            )
            .await;
        eq(
            &format!("redelivery at the {what} revision"),
            &redelivery.map(|_| ()),
            &Err(AppendError::DuplicateMessage(message.clone())),
        )?;
        eq(
            "events after a redelivery",
            &ids(&history(&journal).await?),
            &vec![1],
        )?;
    }
    eq(
        "processed_at after redeliveries",
        &io("processed_at", journal.processed_at(&message).await)?,
        &Some(at(1_000)),
    )?;

    let other_source = inbox("dp-host1", "42");
    eq(
        "the same id from another source before it is processed",
        &io("processed_at", journal.processed_at(&other_source).await)?,
        &None,
    )?;
    io(
        "the same id from another source",
        journal
            .append(
                UnitOfWork::new(at(3_000))
                    .with_stream(s1, rev(1), vec![event(2)])
                    .with_inbox(other_source.clone()),
            )
            .await,
    )?;
    let journal = factory.reopen(journal).await;
    eq(
        "processed_at of the other source",
        &io("processed_at", journal.processed_at(&other_source).await)?,
        &Some(at(3_000)),
    )
}

/// The legal moves from `Pending` that reach `status`.
fn path_to(status: OutboxStatus) -> &'static [OutboxStatus] {
    match status {
        OutboxStatus::Pending => &[],
        OutboxStatus::Sent => &[OutboxStatus::Sent],
        OutboxStatus::Unknown => &[OutboxStatus::Sent, OutboxStatus::Unknown],
        OutboxStatus::Done => &[OutboxStatus::Sent, OutboxStatus::Done],
        OutboxStatus::Cancelled => &[OutboxStatus::Cancelled],
    }
}

async fn move_one<J: Journal>(journal: &J, write: OutboxWrite) -> Result<(), AppendError> {
    journal
        .append(UnitOfWork::new(at(5_000)).with_outbox(write))
        .await
        .map(|_| ())
}

async fn outbox_status_transitions<F: JournalFactory>(factory: &F) -> Check {
    use OutboxStatus::{Done, Pending, Sent, Unknown};

    let journal = factory.fresh().await;
    let d1 = DirectiveId::from_u128(1);
    io("insert", move_one(&journal, directive(1, 1_000)).await)?;
    let journal = factory.reopen(journal).await;
    let stored = io("directive", journal.directive(d1).await)?
        .ok_or_else(|| "an inserted directive is missing".to_owned())?;
    eq("inserted status", &stored.status, &Pending)?;
    eq("inserted attempts", &stored.attempts, &0)?;
    eq(
        "inserted attempt id",
        &stored.attempt_id,
        &AttemptId::from_u128(1_001),
    )?;
    eq(
        "inserted generation",
        &stored.generation,
        &Generation::FIRST,
    )?;
    eq("inserted kind", &stored.kind.as_str(), &"clone_vm")?;
    eq(
        "inserted request",
        &stored.request,
        &Payload::new("{\"directive\":1}"),
    )?;
    eq(
        "inserted next_attempt_at",
        &stored.next_attempt_at,
        &at(1_000),
    )?;

    eq(
        "a second insert of the directive",
        &move_one(&journal, directive(1, 9_000)).await,
        &Err(AppendError::DuplicateDirective(d1)),
    )?;
    eq(
        "a transition of an unknown directive",
        &move_one(&journal, transition(9, Pending, Sent, 1)).await,
        &Err(AppendError::UnknownDirective(DirectiveId::from_u128(9))),
    )?;

    let steps = [
        (Pending, Sent, 2_000, 1),
        (Sent, Pending, 6_000, 1),
        (Pending, Sent, 7_000, 2),
    ];
    for (from, to, next, attempts) in steps {
        io(
            "a legal move",
            move_one(&journal, transition(1, from, to, next)).await,
        )?;
        let moved = io("directive", journal.directive(d1).await)?;
        eq(
            &format!("row after {from} -> {to}"),
            &moved.map(|r| (r.status, r.attempts, r.next_attempt_at)),
            &Some((to, attempts, at(next))),
        )?;
    }
    eq(
        "a move from a stale status",
        &move_one(&journal, transition(1, Pending, Sent, 1)).await,
        &Err(AppendError::OutboxStatusMismatch {
            directive_id: d1,
            expected: Pending,
            actual: Sent,
        }),
    )?;
    io(
        "sent to unknown",
        move_one(&journal, transition(1, Sent, Unknown, 8_000)).await,
    )?;
    io(
        "unknown to done",
        move_one(&journal, transition(1, Unknown, Done, 9_000)).await,
    )?;

    let mut id = 100;
    for from in OutboxStatus::ALL {
        for to in OutboxStatus::ALL {
            id += 1;
            io("insert", move_one(&journal, directive(id, 1_000)).await)?;
            let mut current = Pending;
            for step in path_to(from) {
                io(
                    "a legal move on the way",
                    move_one(&journal, transition(id, current, *step, 1_000)).await,
                )?;
                current = *step;
            }
            let outcome = move_one(&journal, transition(id, from, to, 1_000)).await;
            let row = io(
                "directive",
                journal.directive(DirectiveId::from_u128(id)).await,
            )?
            .map(|r| r.status);
            if from.allows(to) {
                eq(&format!("legal move {from} -> {to}"), &outcome, &Ok(()))?;
                eq(&format!("status after {from} -> {to}"), &row, &Some(to))?;
            } else {
                eq(
                    &format!("illegal move {from} -> {to}"),
                    &outcome,
                    &Err(AppendError::IllegalOutboxTransition {
                        directive_id: DirectiveId::from_u128(id),
                        from,
                        to,
                    }),
                )?;
                eq(
                    &format!("status after refused {from} -> {to}"),
                    &row,
                    &Some(from),
                )?;
            }
        }
    }

    let journal = factory.reopen(journal).await;
    eq(
        "first directive after reopen",
        &io("directive", journal.directive(d1).await)?.map(|r| (r.status, r.attempts)),
        &Some((Done, 2)),
    )?;
    io("insert", move_one(&journal, directive(2, 3_000)).await)?;
    io("insert", move_one(&journal, directive(3, 2_000)).await)?;
    let pending: Vec<_> = io("directives", journal.directives(Pending, 100).await)?
        .into_iter()
        .map(|r| (r.next_attempt_at.get(), r.directive_id.as_u128()))
        .collect();
    let mut expected = vec![(2_000, 3), (3_000, 2)];
    expected.extend((0..25_usize).filter_map(|k| {
        let (from, to) = (OutboxStatus::ALL[k / 5], OutboxStatus::ALL[k % 5]);
        let last = if from.allows(to) { to } else { from };
        (last == Pending).then_some((1_000, 101 + k as u128))
    }));
    expected.sort_unstable();
    eq("pending rows by next_attempt_at", &pending, &expected)?;
    eq(
        "pending rows with a limit",
        &io("directives", journal.directives(Pending, 2).await)?
            .into_iter()
            .map(|r| r.directive_id.as_u128())
            .collect::<Vec<_>>(),
        &expected
            .iter()
            .take(2)
            .map(|(_, id)| *id)
            .collect::<Vec<_>>(),
    )
}

/// A unit that appends `event_id` to `stream` at `expected`, with a receipt, an inbox entry, a
/// directive and a projection row all numbered `n`, so a loser's leftovers can be looked for.
fn tagged_unit(n: u128, stream: &StreamId, expected: StreamRevision, event_id: u128) -> UnitOfWork {
    UnitOfWork::new(at(5_000))
        .with_stream(stream.clone(), expected, vec![event(event_id)])
        .with_receipt(receipt(n, ReceiptStatus::Accepted, "{}"))
        .with_inbox(inbox("scaleset-1", &n.to_string()))
        .with_outbox(directive(n, 5_000))
        .with_projection(row(&n.to_string(), Some("{}")))
}

/// Whether anything of [`tagged_unit`] `n` (event `event_id`) is in the journal, part by part.
async fn traces_of<J: Journal>(journal: &J, n: u128, event_id: u128) -> Result<[bool; 5], String> {
    Ok([
        io(
            "lookup_event",
            journal.lookup_event(EventId::from_u128(event_id)).await,
        )?
        .is_some(),
        io("receipt", journal.receipt(CommandId::from_u128(n)).await)?.is_some(),
        io(
            "processed_at",
            journal
                .processed_at(&inbox("scaleset-1", &n.to_string()))
                .await,
        )?
        .is_some(),
        io(
            "directive",
            journal.directive(DirectiveId::from_u128(n)).await,
        )?
        .is_some(),
        io(
            "projection",
            journal.projection(&projection_name(), &n.to_string()).await,
        )?
        .is_some(),
    ])
}

async fn racing_writers_on_one_stream<F: JournalFactory>(factory: &F) -> Check {
    let mut journal = factory.fresh().await;
    let s1 = stream("attempt-1");
    io(
        "seed append",
        journal
            .append(UnitOfWork::new(at(1_000)).with_stream(
                s1.clone(),
                StreamRevision::EMPTY,
                vec![event(1)],
            ))
            .await,
    )?;
    for round in 0..8_u64 {
        let expected = rev(round + 1);
        let (a, b) = (u128::from(round) * 2 + 10, u128::from(round) * 2 + 11);
        let outcomes = join_all(vec![
            journal.append(tagged_unit(a, &s1, expected, a)),
            journal.append(tagged_unit(b, &s1, expected, b)),
        ])
        .await;
        let lost: Result<(), AppendError> = Err(AppendError::WrongExpectedRevision {
            stream: s1.clone(),
            expected,
            actual: rev(round + 2),
        });
        let (winner, loser) = match (&outcomes[0], &outcomes[1]) {
            (Ok(_), refused) => (a, (b, refused)),
            (refused, Ok(_)) => (b, (a, refused)),
            (first, second) => {
                return Err(format!(
                    "round {round}: no writer committed: {first:?} and {second:?}"
                ));
            }
        };
        eq(
            &format!("round {round}: the losing writer's answer"),
            &loser.1.clone().map(|_| ()),
            &lost,
        )?;
        journal = factory.reopen(journal).await;
        eq(
            &format!("round {round}: the stream head"),
            &io("stream_revision", journal.stream_revision(&s1).await)?,
            &rev(round + 2),
        )?;
        eq(
            &format!("round {round}: the winner's unit"),
            &traces_of(&journal, winner, winner).await?,
            &[true; 5],
        )?;
        eq(
            &format!("round {round}: the loser's unit"),
            &traces_of(&journal, loser.0, loser.0).await?,
            &[false; 5],
        )?;
    }
    Ok(())
}

async fn independent_streams<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let streams: Vec<StreamId> = (1..=8).map(|i| stream(&format!("attempt-{i}"))).collect();
    let outcomes = join_all(
        streams
            .iter()
            .zip(1_u128..)
            .map(|(s, n)| journal.append(tagged_unit(n, s, StreamRevision::EMPTY, n)))
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

    let mixed = join_all(vec![
        journal.append(tagged_unit(20, &streams[0], StreamRevision::EMPTY, 20)),
        journal.append(tagged_unit(21, &streams[1], rev(1), 21)),
    ])
    .await;
    eq(
        "a stale writer of the first stream",
        &mixed[0].clone().map(|_| ()),
        &Err(AppendError::WrongExpectedRevision {
            stream: streams[0].clone(),
            expected: StreamRevision::EMPTY,
            actual: rev(1),
        }),
    )?;
    eq(
        "a current writer of the second stream beside it",
        &mixed[1].clone().map(|_| ()),
        &Ok(()),
    )?;

    io(
        "a write to the third stream",
        journal
            .append(tagged_unit(22, &streams[2], rev(1), 22))
            .await,
    )?;
    io(
        "a writer of the fourth stream after another stream moved",
        journal
            .append(tagged_unit(23, &streams[3], rev(1), 23))
            .await,
    )?;

    let shared_id = join_all(vec![
        journal.append(tagged_unit(30, &streams[4], rev(1), 99)),
        journal.append(tagged_unit(31, &streams[5], rev(1), 99)),
    ])
    .await;
    let committed = shared_id.iter().filter(|o| o.is_ok()).count();
    eq(
        "writers of one event id on two streams that commit",
        &committed,
        &1,
    )?;
    ensure(
        "the other writer of the shared id is refused as a duplicate",
        shared_id
            .iter()
            .any(|o| *o == Err(AppendError::DuplicateEventId(EventId::from_u128(99)))),
    )?;

    let journal = factory.reopen(journal).await;
    let revisions = [rev(1), rev(2), rev(2), rev(2)];
    for (s, want) in streams.iter().zip(revisions) {
        eq(
            &format!("revision of {s}"),
            &io("stream_revision", journal.stream_revision(s).await)?,
            &want,
        )?;
    }
    let fifth = io(
        "stream_revision",
        journal.stream_revision(&streams[4]).await,
    )?;
    let sixth = io(
        "stream_revision",
        journal.stream_revision(&streams[5]).await,
    )?;
    eq(
        "the shared id's streams together",
        &(fifth.get() + sixth.get()),
        &3,
    )?;
    eq(
        "where the shared id lives",
        &io(
            "lookup_event",
            journal.lookup_event(EventId::from_u128(99)).await,
        )?
        .map(|l| l.revision),
        &Some(rev(2)),
    )
}

async fn stream_reads<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let s1 = stream("attempt-1");
    let s2 = stream("attempt-2");
    let s3 = stream("attempt-3");
    let mut other_kind = event(5);
    other_kind.event_type = "contract_other_recorded".to_owned();
    other_kind.schema_version = 2;
    let units = [
        UnitOfWork::new(at(1_000)).with_stream(s2.clone(), rev(0), vec![event(1), event(2)]),
        UnitOfWork::new(at(2_000)).with_stream(s1.clone(), rev(0), vec![event(3)]),
        UnitOfWork::new(at(3_000))
            .with_stream(s1.clone(), rev(1), vec![event(4)])
            .with_stream(s2.clone(), rev(2), vec![other_kind]),
    ];
    for unit in units {
        io("append", journal.append(unit).await)?;
    }
    eq(
        "an append to a new stream with no event",
        &journal
            .append(UnitOfWork::new(at(4_000)).with_stream(s3.clone(), rev(0), Vec::new()))
            .await
            .map(|a| a.events.len()),
        &Ok(0),
    )?;
    eq(
        "a refused append to a new stream",
        &journal
            .append(UnitOfWork::new(at(4_000)).with_stream(s3.clone(), rev(1), vec![event(6)]))
            .await
            .map(|_| ()),
        &Err(AppendError::WrongExpectedRevision {
            stream: s3.clone(),
            expected: rev(1),
            actual: StreamRevision::EMPTY,
        }),
    )?;

    let journal = factory.reopen(journal).await;
    eq(
        "streams in id order, none without events",
        &io("streams", journal.streams(None, usize::MAX).await)?,
        &vec![s1.clone(), s2.clone()],
    )?;
    eq(
        "streams after the first",
        &io("streams", journal.streams(Some(&s1), 10).await)?,
        &vec![s2.clone()],
    )?;
    eq(
        "streams with a limit",
        &io("streams", journal.streams(None, 1).await)?,
        &vec![s1.clone()],
    )?;
    eq(
        "a stream in revision order",
        &ids(&io(
            "read_stream",
            journal.read_stream(&s2, rev(0), usize::MAX).await,
        )?),
        &vec![1, 2, 5],
    )?;
    eq(
        "a stream after revision 1, two at most",
        &ids(&io(
            "read_stream",
            journal.read_stream(&s2, rev(1), 2).await,
        )?),
        &vec![2, 5],
    )?;
    eq(
        "a stream after its head",
        &io("read_stream", journal.read_stream(&s2, rev(3), 10).await)?,
        &Vec::new(),
    )?;
    eq(
        "a stream read with a zero limit",
        &io("read_stream", journal.read_stream(&s1, rev(0), 0).await)?,
        &Vec::new(),
    )?;
    eq(
        "an absent stream",
        &(
            io("stream_revision", journal.stream_revision(&s3).await)?,
            io("read_stream", journal.read_stream(&s3, rev(0), 10).await)?,
        ),
        &(StreamRevision::EMPTY, Vec::new()),
    )?;
    eq(
        "where an event id lives",
        &io(
            "lookup_event",
            journal.lookup_event(EventId::from_u128(4)).await,
        )?,
        &Some(EventLocation {
            stream: s1.clone(),
            revision: rev(2),
        }),
    )?;
    eq(
        "an event id never recorded",
        &io(
            "lookup_event",
            journal.lookup_event(EventId::from_u128(6)).await,
        )?,
        &None,
    )?;
    eq(
        "the event-kind manifest",
        &io("recorded_kinds", journal.recorded_kinds().await)?,
        &vec![
            RecordedKind {
                event_type: "contract_fact_recorded".to_owned(),
                schema_version: 1,
            },
            RecordedKind {
                event_type: "contract_other_recorded".to_owned(),
                schema_version: 2,
            },
        ],
    )
}

async fn projection_writes<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let name = projection_name();
    io(
        "set two rows with an event",
        journal
            .append(
                UnitOfWork::new(at(1_000))
                    .with_stream(stream("attempt-1"), StreamRevision::EMPTY, vec![event(1)])
                    .with_projection(row("a", Some("{\"v\":1}")))
                    .with_projection(row("b", Some("{\"v\":2}"))),
            )
            .await,
    )?;
    io(
        "replace one row",
        journal
            .append(UnitOfWork::new(at(2_000)).with_projection(row("a", Some("{\"v\":3}"))))
            .await,
    )?;
    let journal = factory.reopen(journal).await;
    eq(
        "replaced row",
        &io("projection", journal.projection(&name, "a").await)?,
        &Some(Payload::new("{\"v\":3}")),
    )?;
    eq(
        "untouched row",
        &io("projection", journal.projection(&name, "b").await)?,
        &Some(Payload::new("{\"v\":2}")),
    )?;
    io(
        "remove a row",
        journal
            .append(UnitOfWork::new(at(3_000)).with_projection(row("b", None)))
            .await,
    )?;
    let journal = factory.reopen(journal).await;
    eq(
        "removed row",
        &io("projection", journal.projection(&name, "b").await)?,
        &None,
    )?;
    eq(
        "a row of another projection",
        &io(
            "projection",
            journal
                .projection(
                    &ProjectionName::new("other_rows")
                        .unwrap_or_else(|e| panic!("contract projection: {e}")),
                    "a",
                )
                .await,
        )?,
        &None,
    )
}

async fn malformed_unit<F: JournalFactory>(factory: &F) -> Check {
    let journal = factory.fresh().await;
    let s1 = stream("attempt-1");
    io(
        "seed append",
        journal
            .append(UnitOfWork::new(at(1_000)).with_stream(
                s1.clone(),
                StreamRevision::EMPTY,
                vec![event(1)],
            ))
            .await,
    )?;
    let before = history(&journal).await?;
    let with_receipt =
        |unit: UnitOfWork| unit.with_receipt(receipt(1, ReceiptStatus::Accepted, "{}"));
    let cases = [
        (UnitOfWork::new(at(2_000)), MalformedUnit::Empty),
        (
            with_receipt(
                UnitOfWork::new(at(2_000))
                    .with_stream(s1.clone(), rev(1), vec![event(2)])
                    .with_stream(s1.clone(), rev(1), vec![event(3)]),
            ),
            MalformedUnit::DuplicateStream,
        ),
        (
            with_receipt(UnitOfWork::new(at(2_000)).with_stream(
                s1.clone(),
                rev(1),
                vec![event(2), event(2)],
            )),
            MalformedUnit::DuplicateEventId,
        ),
        (
            with_receipt(
                UnitOfWork::new(at(2_000))
                    .with_outbox(directive(1, 1))
                    .with_outbox(directive(1, 2)),
            ),
            MalformedUnit::DuplicateDirective,
        ),
        (
            with_receipt(
                UnitOfWork::new(at(2_000))
                    .with_projection(row("a", Some("{}")))
                    .with_projection(row("a", None)),
            ),
            MalformedUnit::DuplicateProjectionRow,
        ),
    ];
    for (unit, malformed) in cases {
        eq(
            &format!("a unit that is {malformed}"),
            &journal.append(unit).await.map(|_| ()),
            &Err(AppendError::Malformed(malformed)),
        )?;
    }
    let journal = factory.reopen(journal).await;
    eq(
        "events after malformed units",
        &history(&journal).await?,
        &before,
    )?;
    eq(
        "receipt after malformed units",
        &io("receipt", journal.receipt(CommandId::from_u128(1)).await)?,
        &None,
    )?;
    eq(
        "directive after malformed units",
        &io(
            "directive",
            journal.directive(DirectiveId::from_u128(1)).await,
        )?,
        &None,
    )?;
    eq(
        "projection after malformed units",
        &io(
            "projection",
            journal.projection(&projection_name(), "a").await,
        )?,
        &None,
    )
}
