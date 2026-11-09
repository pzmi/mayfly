//! One append, as one `BEGIN IMMEDIATE` transaction (`doc/event-store.md` §4.2).
//!
//! **Lock order, then reporting order.** The statements run in the lock order of ES-I16:
//! stream heads (appended and read streams, by `StreamId`), the receipt row, the inbox key, the
//! event identities (ascending id), then the writes in the same order and the journal,
//! manifest and projection rows. SQLite's write lock is the whole file, taken by `BEGIN
//! IMMEDIATE`, so the order is trivially kept here; it is written down so the statement
//! sequence matches PostgreSQL's (ES-09), where it is what prevents a wait cycle. The reads
//! decide the refusal in the port's **reporting order** (receipt, inbox, event ids, expected
//! revisions, stream epochs), which is not the lock order: a duplicate id is reported ahead of
//! a stale revision even though the heads were read first (JC-09, JC-25).
//!
//! **No correctness from the single writer** (§4.3 rule 2). Every refusal also stands in the
//! data: a head moves only by a compare-and-set on its revision and epoch (0 rows:
//! `WrongExpectedRevision`), a new head, a receipt, an inbox entry and an identity are inserted
//! under their primary keys (a violation, classified by the constraint the statement guards, is
//! the same refusal), and a stale refusal receipt is replaced only where the stored row is still
//! the one judged (JC-29b). So two writers on one file, two writer threads or two processes, are
//! refused by the store, not by a thread having seen both writes (the contract's two-connection
//! and two-handle modes). Pinned by `should_refuse_by_the_head_compare_and_set_when_the_reads_missed_a_writer`,
//! `should_refuse_by_the_identity_key_when_the_reads_missed_a_stored_id` and
//! `should_refuse_by_the_receipt_compare_and_set_when_the_stale_refusal_was_replaced`, which skip
//! the reads and leave the refusal to the data.
//!
//! **Outcomes (ES-I18).** Any failure before `COMMIT` rolls back and is definite. An error from
//! `COMMIT`, or anything a barrier does after it, is `OutcomeUnknown` (pinned by
//! `should_map_each_barrier_action_to_its_outcome_when_a_hook_strikes`).

use std::collections::{BTreeMap, BTreeSet};

use mayfly_control::journal::{
    AppendError, Appended, AppendedStream, CommandReceipt, Decision, ExpectedRevision,
    ReceiptOutcome, StorageError, StreamHead, StreamId, StreamRevision, UnitOfWork,
};
use mayfly_domain::EventId;
use rusqlite::{Connection, params};

use crate::classify::{Constraint, Failure, classify};
use crate::hooks::{HookAction, StoreHooks};
use crate::rows::{self, GENERATION, StoredReceipt, sql_millis, sql_revision};

/// Why an append stopped before an ordinary commit.
#[derive(Debug)]
pub(crate) enum Stop {
    /// Refused or failed before the commit: roll back, report this (definite).
    Refused(AppendError),
    /// The outcome is unknown: report `OutcomeUnknown` with this code (an open transaction is
    /// rolled back first; a committed one has none left).
    Unknown(&'static str),
    /// A barrier crashed the process: drop the connection without another statement.
    Crash,
}

/// Commits `unit` on `conn`, consulting `hooks` at each barrier. On [`Stop::Crash`] the caller
/// drops the connection.
pub(crate) fn append<H: StoreHooks>(
    conn: &Connection,
    hooks: &H,
    unit: &UnitOfWork,
) -> Result<Appended, Stop> {
    if let Err(malformed) = unit.check_shape() {
        return Err(Stop::Refused(AppendError::Malformed(malformed)));
    }
    if let Some(stream) = unit.epoch_above_build() {
        return Err(Stop::Refused(AppendError::EpochAboveBuild {
            stream: stream.clone(),
        }));
    }
    let mut tx = Tx {
        conn,
        hooks,
        statements: 0,
    };
    // A barrier may stop the append right after `BEGIN`: that stop rolls back like any other.
    let outcome = tx
        .begin()
        .and_then(|()| tx.judge(unit))
        .and_then(|judged| tx.apply(unit, judged));
    match outcome {
        Ok(appended) => {
            tx.commit()?;
            Ok(appended)
        }
        Err(stop) => {
            if !matches!(stop, Stop::Crash) {
                tx.rollback();
            }
            Err(stop)
        }
    }
}

fn refused(failure: Failure) -> Stop {
    Stop::Refused(match failure {
        Failure::Busy => AppendError::Busy,
        Failure::Unique => AppendError::Storage(StorageError::new("constraint_failed")),
        Failure::Storage(e) => AppendError::Storage(e),
    })
}

/// What the reads found that the writes need.
struct Judged {
    heads: BTreeMap<StreamId, Option<StreamHead>>,
    /// The stored outcome text of a stale refusal receipt the unit replaces.
    stale_receipt: Option<String>,
}

struct Tx<'c, H> {
    conn: &'c Connection,
    hooks: &'c H,
    statements: u32,
}

impl<H: StoreHooks> Tx<'_, H> {
    fn barrier(action: HookAction, committed: bool) -> Result<(), Stop> {
        match (action, committed) {
            (HookAction::Proceed, _) => Ok(()),
            (HookAction::Crash, _) => Err(Stop::Crash),
            (_, true) => Err(Stop::Unknown("reply_lost")),
            (HookAction::Fail, false) => Err(Stop::Refused(AppendError::Storage(
                StorageError::new("injected_failure"),
            ))),
            (HookAction::Busy, false) => Err(Stop::Refused(AppendError::Busy)),
            (HookAction::LoseReply, false) => Err(Stop::Unknown("reply_lost")),
        }
    }

    /// Counts a statement that ran (whatever it returned) and passes its barrier.
    fn ran(&mut self, committed: bool) -> Result<(), Stop> {
        self.statements += 1;
        Self::barrier(self.hooks.after_statement(self.statements), committed)
    }

    fn begin(&mut self) -> Result<(), Stop> {
        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| refused(classify(&e)))?;
        Self::barrier(self.hooks.after_statement(0), false)
    }

    fn rollback(&self) {
        if !self.conn.is_autocommit() {
            // A failed rollback leaves the transaction open; the next BEGIN then fails as
            // `engine_error` rather than writing inside it.
            let _ = self.conn.execute_batch("ROLLBACK");
        }
    }

    fn commit(&mut self) -> Result<(), Stop> {
        Self::barrier(self.hooks.before_commit(), false).inspect_err(|stop| {
            if !matches!(stop, Stop::Crash) {
                self.rollback();
            }
        })?;
        if self.conn.execute_batch("COMMIT").is_err() {
            self.rollback();
            return Err(Stop::Unknown("commit_failed"));
        }
        self.ran(true)?;
        Self::barrier(self.hooks.after_commit(), true)
    }

    /// Runs one read statement.
    fn read<T>(&mut self, f: impl FnOnce(&Connection) -> Result<T, Failure>) -> Result<T, Stop> {
        let result = f(self.conn);
        self.ran(false)?;
        result.map_err(refused)
    }

    /// Runs one write statement guarded by `guard`: `Ok(None)` when that constraint refused it.
    fn write_guarded(
        &mut self,
        guard: Constraint,
        sql: &str,
        values: impl rusqlite::Params,
    ) -> Result<Option<usize>, Stop> {
        let result = self
            .conn
            .prepare_cached(sql)
            .and_then(|mut s| s.execute(values));
        self.ran(false)?;
        match result {
            Ok(rows) => Ok(Some(rows)),
            Err(e) => match classify(&e) {
                Failure::Unique if guard != Constraint::JournalEvent => Ok(None),
                Failure::Unique => Err(Stop::Refused(AppendError::Storage(StorageError::new(
                    "revision_taken",
                )))),
                other => Err(refused(other)),
            },
        }
    }

    /// Runs one write statement no unique constraint can refuse.
    fn exec(&mut self, sql: &str, values: impl rusqlite::Params) -> Result<usize, Stop> {
        let result = self
            .conn
            .prepare_cached(sql)
            .and_then(|mut s| s.execute(values));
        self.ran(false)?;
        result.map_err(|e| refused(classify(&e)))
    }

    /// The reads, in lock order; the refusal, in reporting order.
    fn judge(&mut self, unit: &UnitOfWork) -> Result<Judged, Stop> {
        let mut named: Vec<&StreamId> = unit
            .appends
            .iter()
            .map(|a| &a.stream)
            .chain(unit.reads.iter().map(|r| &r.stream))
            .collect();
        named.sort();
        let mut heads = BTreeMap::new();
        for stream in named {
            let head = self.read(|c| rows::head(c, stream))?;
            heads.insert(stream.clone(), head);
        }
        let mut stale_receipt = None;
        if let Some(new) = &unit.receipt {
            let stored = self.read(|c| rows::receipt(c, new.command_id))?;
            if let Some(stored) = stored {
                stale_receipt = Some(self.judge_receipt(unit, stored, &heads)?);
            }
        }
        if let Some(key) = &unit.inbox
            && self.read(|c| rows::inbox_contains(c, key))?
        {
            return Err(Stop::Refused(AppendError::DuplicateMessage(key.clone())));
        }
        let ids: BTreeSet<EventId> = unit.event_ids().collect();
        for id in ids {
            if self.read(|c| rows::lookup_event(c, id))?.is_some() {
                return Err(Stop::Refused(AppendError::DuplicateEventId(id)));
            }
        }
        let mut conditions: Vec<(&StreamId, ExpectedRevision)> = unit
            .appends
            .iter()
            .map(|a| (&a.stream, a.expected))
            .chain(unit.reads.iter().map(|r| (&r.stream, r.expected)))
            .collect();
        conditions.sort_by_key(|(stream, _)| *stream);
        for (stream, expected) in conditions {
            let actual = heads.get(stream).cloned().flatten().map(|h| h.revision);
            if !expected.matches(actual) {
                return Err(Stop::Refused(AppendError::WrongExpectedRevision {
                    stream: stream.clone(),
                    expected,
                    actual,
                }));
            }
        }
        for (stream, head) in &heads {
            if let Some(head) = head
                && unit.appends.iter().any(|a| &a.stream == stream)
                && head.epoch > unit.supported_epochs.of(&head.aggregate)
            {
                return Err(Stop::Refused(AppendError::StreamEpochUnsupported {
                    stream: stream.clone(),
                    epoch: head.epoch,
                }));
            }
        }
        Ok(Judged {
            heads,
            stale_receipt,
        })
    }

    /// Judges a stored receipt for the unit's command: the refusal it answers with, or the
    /// stored text of a refusal receipt gone stale, which the unit may replace (E3).
    fn judge_receipt(
        &mut self,
        unit: &UnitOfWork,
        stored: StoredReceipt,
        heads: &BTreeMap<StreamId, Option<StreamHead>>,
    ) -> Result<String, Stop> {
        let answers = |receipt: CommandReceipt| {
            Stop::Refused(AppendError::CommandReplayed(Box::new(receipt)))
        };
        let new_intent = unit.receipt.as_ref().map(|r| r.intent);
        if Some(stored.receipt.intent) != new_intent {
            return Err(Stop::Refused(AppendError::CommandIdReused {
                command_id: stored.receipt.command_id,
            }));
        }
        let bound = match &stored.receipt.outcome {
            ReceiptOutcome::Accepted(_) => return Err(answers(stored.receipt)),
            ReceiptOutcome::Refused { bound, .. } => bound.clone(),
        };
        let at = match heads.get(&bound.stream) {
            Some(head) => head.as_ref().map(|h| h.revision),
            None => self
                .read(|c| rows::head(c, &bound.stream))?
                .map(|h| h.revision),
        };
        if bound.expected.matches(at) {
            return Err(answers(stored.receipt));
        }
        Ok(stored.outcome_text)
    }

    /// The writes, in lock order.
    fn apply(&mut self, unit: &UnitOfWork, judged: Judged) -> Result<Appended, Stop> {
        let appended = planned(unit)?;
        let mut appends: Vec<_> = unit.appends.iter().zip(&appended.streams).collect();
        appends.sort_by(|(a, _), (b, _)| a.stream.cmp(&b.stream));
        for (append, placed) in &appends {
            let old = judged.heads.get(&append.stream).cloned().flatten();
            let epoch = append
                .events
                .iter()
                .map(|e| e.required_epoch)
                .fold(old.as_ref().map_or(0, |h| h.epoch), u32::max);
            let last = sql_revision(placed.last).map_err(refused)?;
            let moved = match (&old, append.expected) {
                (Some(old), ExpectedRevision::Exactly(expected)) => {
                    self.exec(
                        "UPDATE stream_head SET revision = ?2, required_epoch = ?3 \
                     WHERE stream_id = ?1 AND revision = ?4 AND required_epoch = ?5",
                        params![
                            append.stream.as_str(),
                            last,
                            i64::from(epoch),
                            sql_revision(expected).map_err(refused)?,
                            i64::from(old.epoch)
                        ],
                    )? == 1
                }
                _ => self
                    .write_guarded(
                        Constraint::StreamHead,
                        "INSERT INTO stream_head (stream_id, aggregate, revision, required_epoch) \
                         VALUES (?1, ?2, ?3, ?4)",
                        params![
                            append.stream.as_str(),
                            append.aggregate.as_str(),
                            last,
                            i64::from(epoch)
                        ],
                    )?
                    .is_some(),
            };
            if !moved {
                let actual = self
                    .read(|c| rows::head(c, &append.stream))?
                    .map(|h| h.revision);
                return Err(Stop::Refused(AppendError::WrongExpectedRevision {
                    stream: append.stream.clone(),
                    expected: append.expected,
                    actual,
                }));
            }
        }
        if let Some(new) = &unit.receipt {
            let outcome = match &new.decision {
                Decision::Accepted => ReceiptOutcome::Accepted(appended.clone()),
                Decision::Refused { rejection } => ReceiptOutcome::Refused {
                    bound: unit.reads.first().cloned().ok_or(Stop::Refused(
                        AppendError::Storage(StorageError::new("refusal_unbound")),
                    ))?,
                    rejection: rejection.clone(),
                },
            };
            let text = rows::encode_outcome(&outcome);
            let id = new.command_id.to_string();
            let intent = rows::intent_text(&new.intent);
            let stored = match &judged.stale_receipt {
                Some(stale) => {
                    self.exec(
                        "UPDATE command_receipt SET command_type = ?2, outcome = ?3 \
                         WHERE command_id = ?1 AND intent = ?4 AND outcome = ?5",
                        params![id, new.command_type.as_str(), text, intent, stale],
                    )? == 1
                }
                None => self
                    .write_guarded(
                        Constraint::CommandReceipt,
                        "INSERT INTO command_receipt (command_id, command_type, intent, outcome) \
                         VALUES (?1, ?2, ?3, ?4)",
                        params![id, new.command_type.as_str(), intent, text],
                    )?
                    .is_some(),
            };
            if !stored {
                return Err(self.receipt_moved(unit)?);
            }
        }
        if let Some(key) = &unit.inbox {
            let at = sql_millis(unit.recorded_at).map_err(refused)?;
            if self
                .write_guarded(
                    Constraint::Inbox,
                    "INSERT INTO inbox (source, message_id, recorded_at) VALUES (?1, ?2, ?3)",
                    params![key.source.as_str(), key.message_id.as_str(), at],
                )?
                .is_none()
            {
                return Err(Stop::Refused(AppendError::DuplicateMessage(key.clone())));
            }
        }
        let mut placed: Vec<(EventId, &StreamId, StreamRevision)> = appended
            .streams
            .iter()
            .flat_map(|s| {
                s.event_ids
                    .iter()
                    .zip(0..)
                    .map(move |(id, i)| (*id, &s.stream, StreamRevision::new(s.first.get() + i)))
            })
            .collect();
        placed.sort_by_key(|(id, _, _)| *id);
        for (id, stream, revision) in &placed {
            if self
                .write_guarded(
                    Constraint::JournalIdentity,
                    "INSERT INTO journal_identity (event_id, stream_id, revision) VALUES (?1, ?2, ?3)",
                    params![id.to_string(), stream.as_str(), sql_revision(*revision).map_err(refused)?],
                )?
                .is_none()
            {
                return Err(Stop::Refused(AppendError::DuplicateEventId(*id)));
            }
        }
        let at = sql_millis(unit.recorded_at).map_err(refused)?;
        for (append, placed) in &appends {
            for (event, i) in append.events.iter().zip(0..) {
                let revision =
                    sql_revision(StreamRevision::new(placed.first.get() + i)).map_err(refused)?;
                self.write_guarded(
                    Constraint::JournalEvent,
                    "INSERT INTO journal_event (stream_id, revision, event_id, event_type, \
                     schema_version, payload, recorded_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        append.stream.as_str(),
                        revision,
                        event.event_id.to_string(),
                        event.kind.event_type,
                        i64::from(event.kind.schema_version),
                        event.payload.as_str(),
                        at
                    ],
                )?;
            }
        }
        let kinds: BTreeSet<_> = unit
            .appends
            .iter()
            .flat_map(|a| a.events.iter().map(|e| &e.kind))
            .collect();
        for kind in kinds {
            self.exec(
                "INSERT INTO event_kind_manifest (event_type, schema_version, first_recorded_at) \
                 VALUES (?1, ?2, ?3) ON CONFLICT (event_type, schema_version) DO NOTHING",
                params![kind.event_type, i64::from(kind.schema_version), at],
            )?;
        }
        for row in &unit.projections {
            match &row.row {
                Some(payload) => self.exec(
                    "INSERT INTO projection_row (spec, generation, key, stream_id, payload) \
                     VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (spec, generation, key) \
                     DO UPDATE SET stream_id = excluded.stream_id, payload = excluded.payload",
                    params![
                        row.projection.as_str(),
                        GENERATION,
                        row.key,
                        row.stream.as_str(),
                        payload.as_str()
                    ],
                )?,
                None => self.exec(
                    "DELETE FROM projection_row WHERE spec = ?1 AND generation = ?2 AND key = ?3",
                    params![row.projection.as_str(), GENERATION, row.key],
                )?,
            };
        }
        Ok(appended)
    }

    /// The receipt row changed between the read and the write (a racing writer of the same
    /// command on another engine): judged again, it answers as it now stands.
    fn receipt_moved(&mut self, unit: &UnitOfWork) -> Result<Stop, Stop> {
        let Some(new) = &unit.receipt else {
            return Ok(Stop::Refused(AppendError::Storage(StorageError::new(
                "receipt_conflict",
            ))));
        };
        let stored = self.read(|c| rows::receipt(c, new.command_id))?;
        Ok(match stored {
            Some(stored) => match self.judge_receipt(unit, stored, &BTreeMap::new()) {
                Err(stop) => stop,
                Ok(_) => Stop::Refused(AppendError::Storage(StorageError::new("receipt_conflict"))),
            },
            None => Stop::Refused(AppendError::Storage(StorageError::new("receipt_conflict"))),
        })
    }
}

/// Where each append's events go: from the revision after the expected one, consecutively.
fn planned(unit: &UnitOfWork) -> Result<Appended, Stop> {
    let exhausted = || {
        Stop::Refused(AppendError::Storage(StorageError::new(
            "revision_exhausted",
        )))
    };
    let mut appended = Appended::default();
    for append in &unit.appends {
        let first = append.expected.next_revision().ok_or_else(exhausted)?;
        let count = u64::try_from(append.events.len()).map_err(|_| exhausted())?;
        let last = first
            .get()
            .checked_add(count.saturating_sub(1))
            .map(StreamRevision::new)
            .filter(|r| r.is_storable())
            .ok_or_else(exhausted)?;
        appended.streams.push(AppendedStream {
            stream: append.stream.clone(),
            first,
            last,
            event_ids: append.events.iter().map(|e| e.event_id).collect(),
        });
    }
    Ok(appended)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use mayfly_control::journal::{
        AggregateName, CommandType, Decision, IntentDigest, NewEvent, NewReceipt, Payload,
        StoredKind, SupportedEpochs,
    };
    use mayfly_domain::{CommandId, Millis};

    use super::*;
    use crate::hooks::NoHooks;
    use crate::schema::{MIGRATIONS, migrate};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Barrier {
        Statement(u32),
        BeforeCommit,
        AfterCommit,
    }

    /// Hooks that strike once, at one barrier.
    struct Strike {
        at: Barrier,
        action: HookAction,
        done: AtomicBool,
    }

    impl Strike {
        fn new(at: Barrier, action: HookAction) -> Self {
            Self {
                at,
                action,
                done: AtomicBool::new(false),
            }
        }

        fn fire(&self, here: Barrier) -> HookAction {
            if here == self.at && !self.done.swap(true, Ordering::SeqCst) {
                self.action
            } else {
                HookAction::Proceed
            }
        }
    }

    impl StoreHooks for Strike {
        fn after_statement(&self, statement: u32) -> HookAction {
            self.fire(Barrier::Statement(statement))
        }

        fn before_commit(&self) -> HookAction {
            self.fire(Barrier::BeforeCommit)
        }

        fn after_commit(&self) -> HookAction {
            self.fire(Barrier::AfterCommit)
        }
    }

    fn store() -> Connection {
        let conn = Connection::open_in_memory().unwrap_or_else(|e| panic!("{e}"));
        conn.execute_batch("PRAGMA foreign_keys = ON")
            .unwrap_or_else(|e| panic!("{e}"));
        migrate(&conn, MIGRATIONS, None, Millis::new(1)).unwrap_or_else(|e| panic!("{e}"));
        conn
    }

    fn stream(name: &str) -> StreamId {
        StreamId::new(name).unwrap_or_else(|e| panic!("{e}"))
    }

    fn attempt() -> AggregateName {
        AggregateName::new("attempt").unwrap_or_else(|e| panic!("{e}"))
    }

    fn unit() -> UnitOfWork {
        UnitOfWork::new(Millis::new(5), SupportedEpochs::new().with(attempt(), 1))
    }

    fn appending(s: &StreamId, expected: ExpectedRevision, id: u128) -> UnitOfWork {
        unit().with_append(
            s.clone(),
            attempt(),
            expected,
            vec![NewEvent {
                event_id: EventId::from_u128(id),
                kind: StoredKind {
                    event_type: "vm_cloned".to_owned(),
                    schema_version: 1,
                },
                required_epoch: 1,
                payload: Payload::new("{}"),
            }],
        )
    }

    fn receipt(decision: Decision) -> NewReceipt {
        NewReceipt {
            command_id: CommandId::from_u128(7),
            command_type: CommandType::new("start_attempt").unwrap_or_else(|e| panic!("{e}")),
            intent: IntentDigest::new([1; 32]),
            decision,
        }
    }

    fn outcome(result: Result<Appended, Stop>) -> String {
        match result {
            Ok(_) => "committed".to_owned(),
            Err(Stop::Refused(e)) => format!("refused {e:?}"),
            Err(Stop::Unknown(code)) => format!("unknown {code}"),
            Err(Stop::Crash) => "crash".to_owned(),
        }
    }

    #[test]
    fn should_map_each_barrier_action_to_its_outcome_when_a_hook_strikes() {
        // given
        let strikes = [
            (Barrier::Statement(0), HookAction::Fail),
            (Barrier::Statement(2), HookAction::Busy),
            (Barrier::Statement(3), HookAction::LoseReply),
            (Barrier::BeforeCommit, HookAction::Fail),
            (Barrier::BeforeCommit, HookAction::Busy),
            (Barrier::BeforeCommit, HookAction::LoseReply),
            (Barrier::AfterCommit, HookAction::Fail),
            (Barrier::AfterCommit, HookAction::Busy),
            (Barrier::AfterCommit, HookAction::LoseReply),
            (Barrier::AfterCommit, HookAction::Crash),
        ];

        // when
        let seen: Vec<(String, bool, bool)> = strikes
            .iter()
            .zip(1_u128..)
            .map(|((at, action), n)| {
                let conn = store();
                let s = stream("attempt-1");
                let got = outcome(append(
                    &conn,
                    &Strike::new(*at, *action),
                    &appending(&s, ExpectedRevision::NoStream, n),
                ));
                let present = rows::head(&conn, &s).ok().flatten().is_some();
                (got, present, conn.is_autocommit())
            })
            .collect();

        // then
        let injected = "refused Storage(StorageError { code: \"injected_failure\" })";
        assert_eq!(
            seen,
            [
                (injected.to_owned(), false, true),
                ("refused Busy".to_owned(), false, true),
                ("unknown reply_lost".to_owned(), false, true),
                (injected.to_owned(), false, true),
                ("refused Busy".to_owned(), false, true),
                ("unknown reply_lost".to_owned(), false, true),
                ("unknown reply_lost".to_owned(), true, true),
                ("unknown reply_lost".to_owned(), true, true),
                ("unknown reply_lost".to_owned(), true, true),
                ("crash".to_owned(), true, true),
            ]
        );
    }

    #[test]
    fn should_refuse_by_the_head_compare_and_set_when_the_reads_missed_a_writer() {
        // given
        let conn = store();
        let s = stream("attempt-1");
        append(
            &conn,
            &NoHooks,
            &appending(&s, ExpectedRevision::NoStream, 1),
        )
        .unwrap_or_else(|e| panic!("{e:?}"));
        let unit = appending(&s, ExpectedRevision::NoStream, 2);
        let blind = Judged {
            heads: BTreeMap::from([(s.clone(), None)]),
            stale_receipt: None,
        };
        let mut tx = Tx {
            conn: &conn,
            hooks: &NoHooks,
            statements: 0,
        };

        // when
        tx.begin().unwrap_or_else(|e| panic!("{e:?}"));
        let got = outcome(tx.apply(&unit, blind));
        tx.rollback();

        // then
        let want = AppendError::WrongExpectedRevision {
            stream: s,
            expected: ExpectedRevision::NoStream,
            actual: Some(StreamRevision::FIRST),
        };
        assert_eq!(got, format!("refused {want:?}"));
    }

    #[test]
    fn should_refuse_by_the_identity_key_when_the_reads_missed_a_stored_id() {
        // given
        let conn = store();
        let (s1, s2) = (stream("attempt-1"), stream("attempt-2"));
        append(
            &conn,
            &NoHooks,
            &appending(&s1, ExpectedRevision::NoStream, 1),
        )
        .unwrap_or_else(|e| panic!("{e:?}"));
        let unit = appending(&s2, ExpectedRevision::NoStream, 1);
        let blind = Judged {
            heads: BTreeMap::from([(s2.clone(), None)]),
            stale_receipt: None,
        };
        let mut tx = Tx {
            conn: &conn,
            hooks: &NoHooks,
            statements: 0,
        };

        // when
        tx.begin().unwrap_or_else(|e| panic!("{e:?}"));
        let got = outcome(tx.apply(&unit, blind));
        tx.rollback();

        // then
        assert_eq!(
            got,
            format!(
                "refused {:?}",
                AppendError::DuplicateEventId(EventId::from_u128(1))
            )
        );
        assert_eq!(rows::head(&conn, &s2), Ok(None));
    }

    #[test]
    fn should_refuse_by_the_receipt_compare_and_set_when_the_stale_refusal_was_replaced() {
        // given
        let conn = store();
        let (s1, s2) = (stream("attempt-1"), stream("attempt-2"));
        let refused = Decision::Refused {
            rejection: Payload::new("{\"code\":\"pool_draining\"}"),
        };
        append(
            &conn,
            &NoHooks,
            &unit()
                .with_read(s1.clone(), ExpectedRevision::NoStream)
                .with_receipt(receipt(refused)),
        )
        .unwrap_or_else(|e| panic!("{e:?}"));
        append(
            &conn,
            &NoHooks,
            &appending(&s1, ExpectedRevision::NoStream, 1),
        )
        .unwrap_or_else(|e| panic!("{e:?}"));
        let stale = rows::receipt(&conn, CommandId::from_u128(7))
            .ok()
            .flatten()
            .map(|r| r.outcome_text);
        append(
            &conn,
            &NoHooks,
            &appending(&s2, ExpectedRevision::NoStream, 2)
                .with_receipt(receipt(Decision::Accepted)),
        )
        .unwrap_or_else(|e| panic!("{e:?}"));
        let s3 = stream("attempt-3");
        let late =
            appending(&s3, ExpectedRevision::NoStream, 3).with_receipt(receipt(Decision::Accepted));
        let blind = Judged {
            heads: BTreeMap::from([(s3.clone(), None)]),
            stale_receipt: stale,
        };
        let mut tx = Tx {
            conn: &conn,
            hooks: &NoHooks,
            statements: 0,
        };

        // when
        tx.begin().unwrap_or_else(|e| panic!("{e:?}"));
        let got = tx.apply(&late, blind);
        tx.rollback();

        // then
        assert!(
            matches!(&got, Err(Stop::Refused(AppendError::CommandReplayed(r)))
                if matches!(&r.outcome, ReceiptOutcome::Accepted(a) if a.streams[0].stream == s2)),
            "{got:?}"
        );
        assert_eq!(rows::head(&conn, &s3), Ok(None));
    }

    #[test]
    fn should_refuse_as_exhausted_when_a_stream_is_at_the_last_storable_revision() {
        // given
        let conn = store();
        conn.execute_batch(
            "INSERT INTO stream_head (stream_id, aggregate, revision, required_epoch) \
             VALUES ('attempt-1', 'attempt', 9223372036854775807, 1)",
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let s = stream("attempt-1");

        // when
        let got = outcome(append(
            &conn,
            &NoHooks,
            &appending(&s, ExpectedRevision::Exactly(StreamRevision::MAX), 1),
        ));

        // then
        assert_eq!(
            got,
            "refused Storage(StorageError { code: \"revision_exhausted\" })"
        );
        assert!(conn.is_autocommit());
    }
}
