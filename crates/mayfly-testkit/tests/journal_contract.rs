//! The journal contract's own tests (ver §5): `MemoryJournal` passes it, and each deliberately
//! broken journal of `doc/event-store.md` §7.6 fails the cases named for its fault. Each test
//! pins the exact set of failed cases, which must include the ones §7.6 names: a change that
//! lets a broken journal through, or makes it fail for an unrelated reason, shows here.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};
use std::thread;

use mayfly_control::journal::{
    AggregateName, AppendError, Appended, CommandReceipt, Decision, EventLocation,
    ExpectedRevision, InboxKey, Journal, MalformedUnit, NewEvent, Payload, ProjectionName,
    ProjectionWrite, ReceiptOutcome, RecordedEvent, StorageError, StoredKind, StreamHead, StreamId,
    StreamRevision, SupportedEpochs, UnitOfWork,
};
use mayfly_domain::{CommandId, EventId, Millis};
use mayfly_testkit::block_on;
use mayfly_testkit::contracts::journal::{
    InjectedFault, JournalCase, JournalFactory, check_journal_contract, run_journal_contract,
};
use mayfly_testkit::journal::{MemoryJournal, MemoryJournalFactory};

/// The fault a broken journal carries; each is a mistake a real journal could make, named as
/// in `doc/event-store.md` §7.6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// `LooseRevisionJournal`: ignores expected revisions and appends at the current head.
    LooseRevision,
    /// `PartialCommitJournal`: when a multi-stream unit fails, its first stream's events stay
    /// written.
    PartialCommit,
    /// `ForgetfulReceiptsJournal`: receipts written before a reopen are lost by it.
    ForgetfulReceipts,
    /// `IntentBlindReceipts`: a receipt answers a repeat whatever its intent.
    IntentBlindReceipts,
    /// `ForgetfulInbox`: a redelivered message is consumed again.
    ForgetfulInbox,
    /// `CachedHeadJournal`: each handle trusts the heads it cached over the store's, so a
    /// unit the store refuses as stale commits when this handle's cache agrees with it.
    CachedHead,
    /// `GlobalLockJournal`: refuses a unit (busy) while a unit on another stream is open.
    GlobalLock,
    /// `LateIdCheckJournal`: checks expected revisions before event ids, so a duplicate id is
    /// reported as a stale revision.
    LateIdCheck,
    /// `TornUnitJournal`: makes a unit's last stream visible after the rest of it.
    TornUnit,
    /// A torn unit the other way round: its last stream, with the receipt, the inbox entry
    /// and that stream's rows, visible before the rest. Only a reader that reads the last
    /// stream first sees the tear (JC-32's reverse passes).
    TornUnitLastFirst,
    /// `BlindReceiptUpsertJournal`: judges a unit, lets another writer run, then writes it and
    /// replaces its command's receipt without comparing it with the one it judged, so two
    /// re-decisions of a command with a stale refusal receipt both commit (JC-29b).
    BlindReceiptUpsert,
    /// `OneReadRefusalJournal`: binds a refusal receipt to the lowest of the streams the
    /// decision read and forgets the others, so a refusal decided over two streams is replayed
    /// after the other one moved (ADR 0041).
    OneReadRefusal,
    /// `ReplayBeforeEpochJournal`: answers a repeat from its receipt before judging the epochs
    /// of the streams the answer is for, so an older build is answered from a newer build's
    /// outcome (the order ES-06F's review found, JC-33).
    ReplayBeforeEpoch,
    /// `RefusalInboxJournal`: commits a refusal receipt that carries an inbox key, consuming
    /// the message, so a redelivery after the bound streams moved would be dropped (JC-34).
    RefusalConsumesInbox,
}

/// Pending once, waking itself: where a broken journal lets another future run.
struct YieldOnce(bool);

impl Future for YieldOnce {
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

#[derive(Debug, Clone)]
struct BrokenJournal {
    inner: MemoryJournal,
    fault: Fault,
    receipts_before_reopen: Arc<Mutex<BTreeSet<CommandId>>>,
    forgotten: BTreeSet<CommandId>,
    open: Arc<Mutex<BTreeSet<StreamId>>>,
    cached_heads: Arc<Mutex<BTreeMap<StreamId, Option<StreamRevision>>>>,
}

impl BrokenJournal {
    fn new(fault: Fault, inner: MemoryJournal) -> Self {
        Self {
            inner,
            fault,
            receipts_before_reopen: Arc::default(),
            forgotten: BTreeSet::new(),
            open: Arc::default(),
            cached_heads: Arc::default(),
        }
    }

    /// The head this handle cached for `stream`, if it read or wrote one.
    fn cached(&self, stream: &StreamId) -> Option<Option<StreamRevision>> {
        self.cached_heads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(stream)
            .copied()
    }

    fn forgets(&self, command_id: CommandId) -> bool {
        self.fault == Fault::ForgetfulReceipts && self.forgotten.contains(&command_id)
    }

    async fn current(&self, stream: &StreamId) -> Result<Option<StreamRevision>, AppendError> {
        Ok(self
            .inner
            .head(stream)
            .await
            .map_err(AppendError::Storage)?
            .map(|h| h.revision))
    }

    /// Rewrites every expected revision of `unit` to the stream's current head.
    async fn loosened(&self, mut unit: UnitOfWork) -> Result<UnitOfWork, AppendError> {
        for append in &mut unit.appends {
            append.expected = ExpectedRevision::of(self.current(&append.stream).await?);
        }
        for read in &mut unit.reads {
            read.expected = ExpectedRevision::of(self.current(&read.stream).await?);
        }
        Ok(unit)
    }

    async fn append_broken(&self, unit: UnitOfWork) -> Result<Appended, AppendError> {
        if self.fault == Fault::RefusalConsumesInbox
            && unit.check_shape() == Err(MalformedUnit::RefusalWithInbox)
        {
            let mut refusal = unit.clone();
            refusal.inbox = None;
            let appended = self.inner.append(refusal).await?;
            let mut note = UnitOfWork::new(unit.recorded_at, unit.supported_epochs.clone());
            note.inbox = unit.inbox;
            self.inner.append(note).await?;
            return Ok(appended);
        }
        if unit.check_shape().is_err() {
            return self.inner.append(unit).await;
        }
        match self.fault {
            Fault::RefusalConsumesInbox => self.inner.append(unit).await,
            Fault::ReplayBeforeEpoch => match self.inner.append(unit.clone()).await {
                Err(AppendError::StreamEpochUnsupported { stream, epoch }) => {
                    let stored = match &unit.receipt {
                        Some(new) => self
                            .inner
                            .receipt(new.command_id)
                            .await
                            .map_err(AppendError::Storage)?
                            .filter(|stored| stored.intent == new.intent),
                        None => None,
                    };
                    Err(stored.map_or(
                        AppendError::StreamEpochUnsupported { stream, epoch },
                        |stored| AppendError::CommandReplayed(Box::new(stored)),
                    ))
                }
                other => other,
            },
            Fault::LooseRevision => {
                let unit = self.loosened(unit).await?;
                self.inner.append(unit).await
            }
            Fault::PartialCommit => match self.inner.append(unit.clone()).await {
                Err(error) if unit.appends.len() > 1 => {
                    if let Some(first) = unit.appends.first() {
                        let half = UnitOfWork::new(unit.recorded_at, unit.supported_epochs.clone())
                            .with_append(
                                first.stream.clone(),
                                first.aggregate.clone(),
                                first.expected,
                                first.events.clone(),
                            );
                        let _ = self.inner.append(half).await;
                    }
                    Err(error)
                }
                other => other,
            },
            Fault::ForgetfulReceipts => {
                let mut unit = unit;
                if unit
                    .receipt
                    .as_ref()
                    .is_some_and(|r| self.forgets(r.command_id))
                {
                    unit.receipt = None;
                }
                let command = unit.receipt.as_ref().map(|r| r.command_id);
                let appended = self.inner.append(unit).await?;
                if let Some(command) = command {
                    self.receipts_before_reopen
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(command);
                }
                Ok(appended)
            }
            Fault::IntentBlindReceipts => match self.inner.append(unit).await {
                Err(AppendError::CommandIdReused { command_id }) => {
                    match self.inner.receipt(command_id).await {
                        Ok(Some(stored)) => Err(AppendError::CommandReplayed(Box::new(stored))),
                        _ => Err(AppendError::CommandIdReused { command_id }),
                    }
                }
                other => other,
            },
            Fault::ForgetfulInbox => match self.inner.append(unit.clone()).await {
                Err(AppendError::DuplicateMessage(_)) => {
                    let mut unit = unit;
                    unit.inbox = None;
                    self.inner.append(unit).await
                }
                other => other,
            },
            Fault::CachedHead => {
                let outcome = match self.inner.append(unit.clone()).await {
                    Err(AppendError::WrongExpectedRevision {
                        stream, expected, ..
                    }) if self.cached(&stream).is_some_and(|c| expected.matches(c)) => {
                        let unit = self.loosened(unit).await?;
                        self.inner.append(unit).await
                    }
                    other => other,
                };
                if let Ok(appended) = &outcome {
                    let mut cache = self
                        .cached_heads
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    for written in &appended.streams {
                        cache.insert(written.stream.clone(), Some(written.last));
                    }
                }
                outcome
            }
            Fault::GlobalLock => {
                let mine: BTreeSet<StreamId> = unit
                    .appends
                    .iter()
                    .map(|a| a.stream.clone())
                    .chain(unit.reads.iter().map(|r| r.stream.clone()))
                    .collect();
                {
                    let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
                    if open.iter().any(|s| !mine.contains(s)) {
                        return Err(AppendError::Busy);
                    }
                    open.extend(mine.iter().cloned());
                }
                YieldOnce(false).await;
                let outcome = self.inner.append(unit).await;
                self.open
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .retain(|s| !mine.contains(s));
                outcome
            }
            Fault::LateIdCheck => match self.inner.append(unit.clone()).await {
                Err(AppendError::DuplicateEventId(id)) => {
                    let mut conditions: Vec<(StreamId, ExpectedRevision)> = unit
                        .appends
                        .iter()
                        .map(|a| (a.stream.clone(), a.expected))
                        .chain(unit.reads.iter().map(|r| (r.stream.clone(), r.expected)))
                        .collect();
                    conditions.sort();
                    for (stream, expected) in conditions {
                        let actual = self.current(&stream).await?;
                        if !expected.matches(actual) {
                            return Err(AppendError::WrongExpectedRevision {
                                stream,
                                expected,
                                actual,
                            });
                        }
                    }
                    Err(AppendError::DuplicateEventId(id))
                }
                other => other,
            },
            Fault::OneReadRefusal => {
                let mut unit = unit;
                let refusal = unit
                    .receipt
                    .as_ref()
                    .is_some_and(|r| matches!(r.decision, Decision::Refused { .. }));
                if refusal {
                    unit.reads.sort_by(|a, b| a.stream.cmp(&b.stream));
                    unit.reads.truncate(1);
                }
                self.inner.append(unit).await
            }
            Fault::TornUnit => {
                if unit.appends.len() < 2 {
                    return self.inner.append(unit).await;
                }
                if let Some(refusal) = self.inner.judge(&unit) {
                    return Err(refusal);
                }
                let mut rest = unit.clone();
                let last = rest.appends.pop();
                rest.projections
                    .retain(|p| last.as_ref().is_none_or(|l| l.stream != p.stream));
                let first = self.inner.append(rest).await?;
                YieldOnce(false).await;
                let mut tail = UnitOfWork::new(unit.recorded_at, unit.supported_epochs.clone());
                tail.appends.extend(last.clone());
                tail.projections = unit
                    .projections
                    .into_iter()
                    .filter(|p| last.as_ref().is_some_and(|l| l.stream == p.stream))
                    .collect();
                let second = self.inner.append(tail).await?;
                let mut all = first;
                all.streams.extend(second.streams);
                Ok(all)
            }
            Fault::TornUnitLastFirst => {
                if unit.appends.len() < 2 {
                    return self.inner.append(unit).await;
                }
                if let Some(refusal) = self.inner.judge(&unit) {
                    return Err(refusal);
                }
                let mut rest = unit.clone();
                let last = rest.appends.pop();
                let on_last =
                    |p: &ProjectionWrite| last.as_ref().is_some_and(|l| l.stream == p.stream);
                rest.projections.retain(|p| !on_last(p));
                rest.receipt = None;
                rest.inbox = None;
                let mut tail = UnitOfWork::new(unit.recorded_at, unit.supported_epochs.clone());
                tail.appends.extend(last.clone());
                tail.receipt = unit.receipt.clone();
                tail.inbox = unit.inbox.clone();
                tail.projections = unit
                    .projections
                    .iter()
                    .filter(|p| on_last(p))
                    .cloned()
                    .collect();
                let second = self.inner.append(tail).await?;
                YieldOnce(false).await;
                let mut all = self.inner.append(rest).await?;
                all.streams.extend(second.streams);
                Ok(all)
            }
            Fault::BlindReceiptUpsert => {
                let Some(receipt) = unit.receipt.clone() else {
                    return self.inner.append(unit).await;
                };
                if let Some(refusal) = self.inner.judge(&unit) {
                    return Err(refusal);
                }
                YieldOnce(false).await;
                let mut bound = unit.reads.clone();
                bound.sort_by(|a, b| a.stream.cmp(&b.stream));
                let mut bare = unit;
                bare.receipt = None;
                let appended = if bare.appends.is_empty() && bare.inbox.is_none() {
                    Appended::default()
                } else {
                    self.inner.append(bare).await?
                };
                let outcome = match receipt.decision {
                    Decision::Refused { rejection } if !bound.is_empty() => {
                        ReceiptOutcome::Refused { bound, rejection }
                    }
                    _ => ReceiptOutcome::Accepted(appended.clone()),
                };
                self.inner.overwrite_receipt(CommandReceipt {
                    command_id: receipt.command_id,
                    command_type: receipt.command_type,
                    intent: receipt.intent,
                    outcome,
                });
                Ok(appended)
            }
        }
    }
}

impl Journal for BrokenJournal {
    async fn append(&self, unit: UnitOfWork) -> Result<Appended, AppendError> {
        self.append_broken(unit).await
    }

    async fn head(&self, stream: &StreamId) -> Result<Option<StreamHead>, StorageError> {
        let head = self.inner.head(stream).await?;
        if self.fault == Fault::CachedHead {
            self.cached_heads
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(stream.clone(), head.as_ref().map(|h| h.revision));
        }
        Ok(head)
    }

    async fn read_stream(
        &self,
        stream: &StreamId,
        after: Option<StreamRevision>,
        through: Option<StreamRevision>,
    ) -> Result<Vec<RecordedEvent>, StorageError> {
        self.inner.read_stream(stream, after, through).await
    }

    async fn lookup_event(&self, event_id: EventId) -> Result<Option<EventLocation>, StorageError> {
        self.inner.lookup_event(event_id).await
    }

    async fn receipt(&self, command_id: CommandId) -> Result<Option<CommandReceipt>, StorageError> {
        if self.forgets(command_id) {
            return Ok(None);
        }
        self.inner.receipt(command_id).await
    }

    async fn inbox_contains(&self, key: &InboxKey) -> Result<bool, StorageError> {
        self.inner.inbox_contains(key).await
    }

    async fn streams(
        &self,
        after: Option<&StreamId>,
        limit: u32,
    ) -> Result<Vec<StreamHead>, StorageError> {
        self.inner.streams(after, limit).await
    }

    async fn stored_kinds(&self) -> Result<Vec<StoredKind>, StorageError> {
        self.inner.stored_kinds().await
    }

    async fn projection(
        &self,
        projection: &ProjectionName,
        key: &str,
    ) -> Result<Option<Payload>, StorageError> {
        self.inner.projection(projection, key).await
    }
}

struct BrokenFactory(Fault);

impl JournalFactory for BrokenFactory {
    type Journal = BrokenJournal;

    async fn fresh(&self) -> BrokenJournal {
        BrokenJournal::new(self.0, MemoryJournal::new())
    }

    async fn reopen(&self, journal: BrokenJournal) -> BrokenJournal {
        let forgotten = journal
            .receipts_before_reopen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        BrokenJournal {
            forgotten,
            cached_heads: Arc::default(),
            ..journal
        }
    }

    async fn second_handle(&self, journal: &BrokenJournal) -> BrokenJournal {
        BrokenJournal {
            cached_heads: Arc::default(),
            ..journal.clone()
        }
    }

    async fn inject(&self, journal: &BrokenJournal, fault: InjectedFault) -> bool {
        journal.inner.inject(fault)
    }

    async fn crash(&self, journal: BrokenJournal) -> BrokenJournal {
        self.reopen(journal).await
    }
}

fn failed_cases(fault: Fault) -> Vec<JournalCase> {
    block_on(check_journal_contract(&BrokenFactory(fault))).failed_cases()
}

/// Checks the §7.6 cases are among `failed` before the exact set is compared.
fn assert_fails_named(failed: &[JournalCase], named: &[JournalCase]) {
    for case in named {
        assert!(failed.contains(case), "{case} did not fail: {failed:?}");
    }
}

#[test]
fn should_pass_journal_contract_when_journal_is_in_memory() {
    // given
    let factory = MemoryJournalFactory;

    // when
    let report = block_on(check_journal_contract(&factory));

    // then
    assert!(report.is_green(), "{report}");
    run_journal_contract(&factory);
}

#[test]
fn should_fail_revision_cases_when_journal_ignores_expected_revision() {
    // given
    let fault = Fault::LooseRevision;

    // when
    let failed = failed_cases(fault);

    // then
    assert_fails_named(
        &failed,
        &[
            JournalCase::StaleExpectedRevision,
            JournalCase::RacingHandles,
        ],
    );
    assert_eq!(
        failed,
        [
            JournalCase::StaleExpectedRevision,
            JournalCase::AheadExpectedRevision,
            JournalCase::NoStreamMismatch,
            JournalCase::RefusalOrder,
            JournalCase::MultiStreamAllOrNothing,
            JournalCase::ReadConditions,
            JournalCase::StoreRefusalLeavesNoReceipt,
            JournalCase::LookupEvent,
            JournalCase::KindManifest,
            JournalCase::PayloadRoundTrip,
            JournalCase::StreamsEnumeration,
            JournalCase::RacingHandles,
            JournalCase::IndependentStreams,
            JournalCase::RefusalReceipts
        ]
    );
}

#[test]
fn should_fail_atomicity_cases_when_journal_commits_half_a_unit() {
    // given
    let fault = Fault::PartialCommit;

    // when
    let failed = failed_cases(fault);

    // then
    assert_fails_named(
        &failed,
        &[
            JournalCase::MultiStreamAllOrNothing,
            JournalCase::CrashAtomicity,
        ],
    );
    assert_eq!(
        failed,
        [
            JournalCase::RefusalOrder,
            JournalCase::MultiStreamAllOrNothing,
            JournalCase::LookupEvent,
            JournalCase::CrashAtomicity,
            JournalCase::OutcomeUnknown
        ]
    );
}

#[test]
fn should_fail_survives_reopen_when_journal_loses_receipts_on_reopen() {
    // given
    let fault = Fault::ForgetfulReceipts;

    // when
    let failed = failed_cases(fault);

    // then
    assert_fails_named(&failed, &[JournalCase::SurvivesReopen]);
    assert_eq!(
        failed,
        [
            JournalCase::RefusalOrder,
            JournalCase::MultiStreamAllOrNothing,
            JournalCase::ReceiptReplay,
            JournalCase::CommandIdReused,
            JournalCase::StoreRefusalLeavesNoReceipt,
            JournalCase::SurvivesReopen,
            JournalCase::RefusalReceipts,
            JournalCase::RacingStaleRefusal,
            JournalCase::ReplayWithinEpoch
        ]
    );
}

#[test]
fn should_fail_command_id_reused_when_receipts_ignore_the_intent() {
    // given
    let fault = Fault::IntentBlindReceipts;

    // when
    let failed = failed_cases(fault);

    // then
    assert_fails_named(&failed, &[JournalCase::CommandIdReused]);
    assert_eq!(
        failed,
        [JournalCase::RefusalOrder, JournalCase::CommandIdReused]
    );
}

#[test]
fn should_fail_inbox_dedupe_when_journal_consumes_a_redelivery() {
    // given
    let fault = Fault::ForgetfulInbox;

    // when
    let failed = failed_cases(fault);

    // then
    assert_fails_named(&failed, &[JournalCase::InboxDedupe]);
    assert_eq!(
        failed,
        [
            JournalCase::RefusalOrder,
            JournalCase::InboxDedupe,
            JournalCase::PayloadRoundTrip,
            JournalCase::SurvivesReopen,
            JournalCase::OutcomeUnknown,
            JournalCase::RacingInboxKeys
        ]
    );
}

#[test]
fn should_fail_racing_handles_when_journal_caches_heads_per_handle() {
    // given
    let fault = Fault::CachedHead;

    // when
    let failed = failed_cases(fault);

    // then
    assert_fails_named(&failed, &[JournalCase::RacingHandles]);
    assert_eq!(failed, [JournalCase::RacingHandles]);
}

#[test]
fn should_fail_independent_streams_when_journal_holds_one_global_lock() {
    // given
    let fault = Fault::GlobalLock;

    // when
    let failed = failed_cases(fault);

    // then
    assert_fails_named(&failed, &[JournalCase::IndependentStreams]);
    assert_eq!(
        failed,
        [
            JournalCase::IndependentStreams,
            JournalCase::RacingCommandIds,
            JournalCase::RacingStaleRefusal,
            JournalCase::RacingInboxKeys,
            JournalCase::RacingEventIds
        ]
    );
}

#[test]
fn should_fail_refusal_order_when_journal_checks_ids_after_revisions() {
    // given
    let fault = Fault::LateIdCheck;

    // when
    let failed = failed_cases(fault);

    // then
    assert_fails_named(&failed, &[JournalCase::RefusalOrder]);
    assert_eq!(
        failed,
        [JournalCase::RefusalOrder, JournalCase::OutcomeUnknown]
    );
}

#[test]
fn should_fail_unit_visible_at_once_when_journal_shows_a_unit_in_parts() {
    // given
    let fault = Fault::TornUnit;

    // when
    let failed = failed_cases(fault);

    // then
    assert_fails_named(&failed, &[JournalCase::UnitVisibleAtOnce]);
    assert_eq!(
        failed,
        [
            JournalCase::MultiStreamAllOrNothing,
            JournalCase::CrashAtomicity,
            JournalCase::UnitVisibleAtOnce
        ]
    );
}

#[test]
fn should_fail_refusal_receipts_when_journal_binds_a_refusal_to_one_read() {
    // given
    let fault = Fault::OneReadRefusal;

    // when
    let failed = failed_cases(fault);

    // then
    assert_eq!(failed, [JournalCase::RefusalReceipts]);
}

#[test]
fn should_fail_replay_within_epoch_when_journal_replays_before_the_epoch() {
    // given
    let fault = Fault::ReplayBeforeEpoch;

    // when
    let failed = failed_cases(fault);

    // then
    assert_eq!(failed, [JournalCase::ReplayWithinEpoch]);
}

#[test]
fn should_fail_refusal_with_inbox_when_journal_lets_a_refusal_consume_a_message() {
    // given
    let fault = Fault::RefusalConsumesInbox;

    // when
    let failed = failed_cases(fault);

    // then
    assert_eq!(failed, [JournalCase::RefusalWithInbox]);
}

#[test]
fn should_report_refusal_before_injected_fault_when_memory_judges_a_unit() {
    // given
    let journal = MemoryJournal::new();
    let s1 = StreamId::new("attempt-1").unwrap_or_else(|e| panic!("{e}"));
    let attempt = AggregateName::new("attempt").unwrap_or_else(|e| panic!("{e}"));
    let unit = |id: u128| {
        UnitOfWork::new(
            Millis::new(1),
            SupportedEpochs::new().with(attempt.clone(), 1),
        )
        .with_append(
            s1.clone(),
            attempt.clone(),
            ExpectedRevision::NoStream,
            vec![NewEvent {
                event_id: EventId::from_u128(id),
                kind: StoredKind {
                    event_type: "contract_fact".to_owned(),
                    schema_version: 1,
                },
                required_epoch: 1,
                payload: Payload::new("{}"),
            }],
        )
    };
    let mut malformed = unit(1);
    malformed.appends[0].events.clear();

    // when
    let armed = journal.inject(InjectedFault::Busy);
    let refused = block_on(journal.append(malformed));
    let next = block_on(journal.append(unit(1)));

    // then
    assert!(armed);
    assert_eq!(
        refused,
        Err(AppendError::Malformed(MalformedUnit::EmptyAppend))
    );
    assert!(
        next.is_ok(),
        "the fault was consumed by the refused unit: {next:?}"
    );
}

#[test]
#[should_panic(expected = "JC-24 survives_reopen")]
fn should_panic_naming_the_case_when_run_form_meets_a_broken_journal() {
    // given
    let factory = BrokenFactory(Fault::ForgetfulReceipts);

    // when
    run_journal_contract(&factory);

    // then
    unreachable!("run_journal_contract returned for a broken journal");
}

/// A factory that cannot inject faults: the cases that need one fail, never skip.
struct NoFaultsFactory;

impl JournalFactory for NoFaultsFactory {
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

    async fn inject(&self, _journal: &MemoryJournal, _fault: InjectedFault) -> bool {
        false
    }

    async fn crash(&self, journal: MemoryJournal) -> MemoryJournal {
        journal
    }
}

#[test]
fn should_fail_fault_cases_when_factory_cannot_inject_faults() {
    // given
    let factory = NoFaultsFactory;

    // when
    let failed = block_on(check_journal_contract(&factory)).failed_cases();

    // then
    assert_eq!(
        failed,
        [
            JournalCase::StoreRefusalLeavesNoReceipt,
            JournalCase::CrashAtomicity,
            JournalCase::OutcomeUnknown
        ]
    );
}

#[test]
fn should_name_every_case_by_its_event_store_id_when_listed() {
    // given
    let cases = JournalCase::ALL;

    // when
    let ids: Vec<&str> = cases.iter().map(|c| c.id()).collect();

    // then
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted, ids, "case ids are unique and listed in order");
    assert!(
        ids.iter()
            .all(|id| id.starts_with("JC-") && (id.len() == 5 || id == &"JC-29b"))
    );
    assert!(!ids.contains(&"JC-26") && !ids.contains(&"JC-27"));
}

/// Pending until another thread sets `ready` and wakes the stored waker.
struct Gate(Arc<Mutex<(bool, Option<Waker>)>>);

impl Future for Gate {
    type Output = &'static str;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<&'static str> {
        let mut gate = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if gate.0 {
            Poll::Ready("opened")
        } else {
            gate.1 = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

#[test]
fn should_complete_future_when_it_is_woken_from_another_thread() {
    // given
    let shared = Arc::new(Mutex::new((false, None::<Waker>)));
    let opener = Arc::clone(&shared);
    let (polled_tx, polled_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let handle = thread::spawn(move || {
        polled_rx.recv().ok();
        let mut gate = opener.lock().unwrap_or_else(PoisonError::into_inner);
        gate.0 = true;
        if let Some(waker) = gate.1.take() {
            waker.wake();
        }
    });
    let first_poll = std::future::poll_fn(move |_| {
        polled_tx.send(()).ok();
        Poll::Ready(())
    });

    // when
    let outcome = block_on(async {
        first_poll.await;
        Gate(shared).await
    });

    // then
    assert_eq!(outcome, "opened");
    handle.join().unwrap();
}

#[test]
fn should_fail_unit_visible_at_once_when_journal_shows_the_last_stream_first() {
    // given
    let fault = Fault::TornUnitLastFirst;

    // when
    let failed = failed_cases(fault);

    // then
    assert_fails_named(&failed, &[JournalCase::UnitVisibleAtOnce]);
    assert_eq!(
        failed,
        [
            JournalCase::MultiStreamAllOrNothing,
            JournalCase::IndependentStreams,
            JournalCase::CrashAtomicity,
            JournalCase::UnitVisibleAtOnce
        ]
    );
}

#[test]
fn should_fail_racing_stale_refusal_when_journal_upserts_receipts_blindly() {
    // given
    let fault = Fault::BlindReceiptUpsert;

    // when
    let failed = failed_cases(fault);

    // then
    assert_fails_named(&failed, &[JournalCase::RacingStaleRefusal]);
    assert_eq!(
        failed,
        [
            JournalCase::CrashAtomicity,
            JournalCase::OutcomeUnknown,
            JournalCase::RacingCommandIds,
            JournalCase::RacingStaleRefusal
        ]
    );
}
