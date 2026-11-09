//! The journal contract's own tests (ver §5): `MemoryJournal` passes it, and each deliberately
//! broken journal fails the cases named for its fault.

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};
use std::thread;

use mayfly_control::journal::{
    AppendError, Appended, CommandReceipt, EventLocation, InboxEntry, Journal, OutboxRow,
    OutboxStatus, Payload, ProjectionName, RecordedEvent, RecordedKind, StorageError, StreamId,
    StreamRevision, UnitOfWork,
};
use mayfly_domain::{CommandId, DirectiveId, EventId, Millis};
use mayfly_testkit::block_on;
use mayfly_testkit::contracts::journal::{
    JournalCase, JournalFactory, check_journal_contract, run_journal_contract,
};
use mayfly_testkit::journal::{MemoryJournal, MemoryJournalFactory};

/// The fault a broken journal carries; each is a mistake a real journal could make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// `LooseRevisionJournal`: ignores the expected revision and appends at the current one.
    LooseRevision,
    /// `PartialCommitJournal`: when a unit fails, its first stream's events stay written.
    PartialCommit,
    /// `ForgetfulReceiptsJournal`: receipts written before a reopen are lost by it.
    ForgetfulReceipts,
    /// `LeakyInboxJournal`: a redelivered message is processed again.
    LeakyInbox,
    /// `RacyHeadJournal`: checks the expected revision, yields, then appends at whatever the
    /// head is by then: the check and the advance of the stream head are not one step.
    RacyHead,
    /// `GlobalWriterJournal`: one writer at a time across every stream; a second append while
    /// one is in flight is turned away as busy.
    GlobalWriter,
}

/// Pending once, waking itself: where a broken journal lets another append run.
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
    in_flight: Arc<AtomicBool>,
}

impl BrokenJournal {
    fn forgets(&self, command_id: CommandId) -> bool {
        self.fault == Fault::ForgetfulReceipts && self.forgotten.contains(&command_id)
    }

    async fn append_broken(&self, unit: UnitOfWork) -> Result<Appended, AppendError> {
        match self.fault {
            Fault::LooseRevision => {
                let mut unit = unit;
                for append in &mut unit.streams {
                    append.expected = self
                        .inner
                        .stream_revision(&append.stream)
                        .await
                        .map_err(AppendError::Storage)?;
                }
                self.inner.append(unit).await
            }
            Fault::PartialCommit => match self.inner.append(unit.clone()).await {
                Err(error) if !matches!(error, AppendError::Malformed(_)) => {
                    if let Some(first) = unit.streams.first() {
                        let half = UnitOfWork::new(unit.recorded_at).with_stream(
                            first.stream.clone(),
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
            Fault::LeakyInbox => match self.inner.append(unit.clone()).await {
                Err(AppendError::DuplicateMessage(_)) => {
                    let mut unit = unit;
                    unit.inbox = None;
                    self.inner.append(unit).await
                }
                other => other,
            },
            Fault::RacyHead => {
                for append in &unit.streams {
                    let actual = self
                        .inner
                        .stream_revision(&append.stream)
                        .await
                        .map_err(AppendError::Storage)?;
                    if actual != append.expected {
                        return Err(AppendError::WrongExpectedRevision {
                            stream: append.stream.clone(),
                            expected: append.expected,
                            actual,
                        });
                    }
                }
                YieldOnce(false).await;
                let mut unit = unit;
                for append in &mut unit.streams {
                    append.expected = self
                        .inner
                        .stream_revision(&append.stream)
                        .await
                        .map_err(AppendError::Storage)?;
                }
                self.inner.append(unit).await
            }
            Fault::GlobalWriter => {
                if self.in_flight.swap(true, Ordering::SeqCst) {
                    return Err(AppendError::Storage(StorageError::new("journal busy")));
                }
                YieldOnce(false).await;
                let outcome = self.inner.append(unit).await;
                self.in_flight.store(false, Ordering::SeqCst);
                outcome
            }
        }
    }
}

impl Journal for BrokenJournal {
    async fn append(&self, unit: UnitOfWork) -> Result<Appended, AppendError> {
        self.append_broken(unit).await
    }

    async fn stream_revision(&self, stream: &StreamId) -> Result<StreamRevision, StorageError> {
        self.inner.stream_revision(stream).await
    }

    async fn read_stream(
        &self,
        stream: &StreamId,
        after: StreamRevision,
        limit: usize,
    ) -> Result<Vec<RecordedEvent>, StorageError> {
        self.inner.read_stream(stream, after, limit).await
    }

    async fn streams(
        &self,
        after: Option<&StreamId>,
        limit: usize,
    ) -> Result<Vec<StreamId>, StorageError> {
        self.inner.streams(after, limit).await
    }

    async fn lookup_event(&self, event_id: EventId) -> Result<Option<EventLocation>, StorageError> {
        self.inner.lookup_event(event_id).await
    }

    async fn recorded_kinds(&self) -> Result<Vec<RecordedKind>, StorageError> {
        self.inner.recorded_kinds().await
    }

    async fn receipt(&self, command_id: CommandId) -> Result<Option<CommandReceipt>, StorageError> {
        if self.forgets(command_id) {
            return Ok(None);
        }
        self.inner.receipt(command_id).await
    }

    async fn processed_at(&self, entry: &InboxEntry) -> Result<Option<Millis>, StorageError> {
        self.inner.processed_at(entry).await
    }

    async fn directive(
        &self,
        directive_id: DirectiveId,
    ) -> Result<Option<OutboxRow>, StorageError> {
        self.inner.directive(directive_id).await
    }

    async fn directives(
        &self,
        status: OutboxStatus,
        limit: usize,
    ) -> Result<Vec<OutboxRow>, StorageError> {
        self.inner.directives(status, limit).await
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
        BrokenJournal {
            inner: MemoryJournal::new(),
            fault: self.0,
            receipts_before_reopen: Arc::default(),
            forgotten: BTreeSet::new(),
            in_flight: Arc::default(),
        }
    }

    async fn reopen(&self, journal: BrokenJournal) -> BrokenJournal {
        let forgotten = journal
            .receipts_before_reopen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        BrokenJournal {
            forgotten,
            ..journal
        }
    }
}

fn failed_cases(fault: Fault) -> Vec<JournalCase> {
    block_on(check_journal_contract(&BrokenFactory(fault))).failed_cases()
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
fn should_fail_every_revision_case_when_journal_ignores_expected_revision() {
    // given
    let fault = Fault::LooseRevision;

    // when
    let failed = failed_cases(fault);

    // then
    assert_eq!(
        failed,
        [
            JournalCase::ExpectedRevisionConflict,
            JournalCase::AllOrNothing,
            JournalCase::RacingWritersOnOneStream,
            JournalCase::IndependentStreams,
            JournalCase::StreamReads
        ]
    );
}

#[test]
fn should_fail_all_or_nothing_when_journal_commits_half_a_unit() {
    // given
    let fault = Fault::PartialCommit;

    // when
    let failed = failed_cases(fault);

    // then
    assert_eq!(
        failed,
        [JournalCase::AllOrNothing, JournalCase::InboxDedupe]
    );
}

#[test]
fn should_fail_receipt_replay_when_journal_loses_receipts_on_reopen() {
    // given
    let fault = Fault::ForgetfulReceipts;

    // when
    let failed = failed_cases(fault);

    // then
    assert_eq!(
        failed,
        [
            JournalCase::AllOrNothing,
            JournalCase::ReceiptReplay,
            JournalCase::RacingWritersOnOneStream
        ]
    );
}

#[test]
fn should_fail_inbox_dedupe_when_journal_processes_a_redelivery() {
    // given
    let fault = Fault::LeakyInbox;

    // when
    let failed = failed_cases(fault);

    // then
    assert_eq!(failed, [JournalCase::InboxDedupe]);
}

#[test]
fn should_fail_racing_writers_when_journal_checks_the_head_apart_from_advancing_it() {
    // given
    let fault = Fault::RacyHead;

    // when
    let failed = failed_cases(fault);

    // then
    assert_eq!(
        failed,
        [
            JournalCase::ReceiptReplay,
            JournalCase::InboxDedupe,
            JournalCase::RacingWritersOnOneStream
        ]
    );
}

#[test]
fn should_fail_independent_streams_when_journal_admits_one_writer_at_a_time() {
    // given
    let fault = Fault::GlobalWriter;

    // when
    let failed = failed_cases(fault);

    // then
    assert_eq!(
        failed,
        [
            JournalCase::RacingWritersOnOneStream,
            JournalCase::IndependentStreams
        ]
    );
}

#[test]
#[should_panic(expected = "receipt_replay")]
fn should_panic_naming_the_case_when_run_form_meets_a_broken_journal() {
    // given
    let factory = BrokenFactory(Fault::ForgetfulReceipts);

    // when
    run_journal_contract(&factory);

    // then
    unreachable!("run_journal_contract returned for a broken journal");
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
