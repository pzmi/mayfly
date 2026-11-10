//! Integration level: the journal contract against a real SQLite file in a tempdir, in the five
//! modes of `doc/event-store.md` §7.1, plus the engine cases JC-26 and JC-27, real lock
//! contention, and the schema a store of the previous version holds. A lower level cannot
//! fail for these reasons: they are about what the engine and the file do.
//!
//! Faults reach the adapter through its commit barriers ([`StoreHooks`]): the factory arms one
//! `InjectedFault` at a time, and the barrier where it applies fires it once.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use mayfly_adapter_sqlite::{
    Durability, HookAction, OpenError, SqliteJournal, SqliteOptions, StoreHooks,
};
use mayfly_control::journal::{
    AggregateName, AppendError, ExpectedRevision, Journal, NewEvent, Payload, StoredKind, StreamId,
    StreamRevision, SupportedEpochs, UnitOfWork,
};
use mayfly_domain::{EventId, Millis};
use mayfly_testkit::block_on;
use mayfly_testkit::contracts::journal::{
    EngineCase, EngineJournalFactory, InjectedFault, JournalContractReport, JournalFactory, Tamper,
    check_engine_contract, check_journal_contract,
};
use rusqlite::Connection;
use tempfile::TempDir;

/// The faults armed for the next append, shared by every handle a factory opens.
#[derive(Debug, Default)]
struct Faults {
    armed: Mutex<Option<InjectedFault>>,
    crashing: AtomicBool,
    crashes: AtomicUsize,
}

/// The test hooks: fire the armed fault at its barrier, once.
#[derive(Debug, Clone, Default)]
struct Hooks(Arc<Faults>);

impl Hooks {
    fn take_if(&self, fires: impl Fn(InjectedFault) -> bool) -> Option<InjectedFault> {
        let mut armed = self.0.armed.lock().unwrap_or_else(PoisonError::into_inner);
        armed.filter(|f| fires(*f)).and_then(|_| armed.take())
    }
}

impl StoreHooks for Hooks {
    fn after_statement(&self, statement: u32) -> HookAction {
        match self.take_if(|f| f == InjectedFault::CrashAfterStatement(statement)) {
            Some(_) => {
                self.0.crashes.fetch_add(1, Ordering::SeqCst);
                HookAction::Crash
            }
            None => HookAction::Proceed,
        }
    }

    fn before_commit(&self) -> HookAction {
        let fault = self.take_if(|f| {
            matches!(
                f,
                InjectedFault::StorageBeforeCommit
                    | InjectedFault::Busy
                    | InjectedFault::ReplyLostBeforeCommit
            )
        });
        match fault {
            Some(InjectedFault::StorageBeforeCommit) => HookAction::Fail,
            Some(InjectedFault::Busy) => HookAction::Busy,
            Some(_) => HookAction::LoseReply,
            None => HookAction::Proceed,
        }
    }

    fn after_commit(&self) -> HookAction {
        match self.take_if(|f| f == InjectedFault::ReplyLostAfterCommit) {
            Some(_) => HookAction::LoseReply,
            None => HookAction::Proceed,
        }
    }

    fn checkpoint_on_close(&self) -> bool {
        !self.0.crashing.load(Ordering::SeqCst)
    }
}

/// What `reopen` does in a mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reopen {
    /// Hands the same journal back (the store was never closed).
    Keep,
    /// Closes every connection cleanly and opens the file again.
    Close,
    /// Closes as a crashed process would, leaving the WAL to recover, and opens again.
    Crash,
}

/// What `second_handle` gives in a mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Second {
    /// A clone sharing the connections.
    Share,
    /// A second journal opened on the file, as another process would hold.
    Open,
}

/// One mode of `doc/event-store.md` §7.1.
#[derive(Debug, Clone, Copy)]
struct Mode {
    writers: usize,
    reopen: Reopen,
    second: Second,
}

const FRESH: Mode = Mode {
    writers: 1,
    reopen: Reopen::Keep,
    second: Second::Share,
};
const REOPENED: Mode = Mode {
    writers: 1,
    reopen: Reopen::Close,
    second: Second::Share,
};
const TWO_CONNECTIONS: Mode = Mode {
    writers: 2,
    reopen: Reopen::Keep,
    second: Second::Share,
};
const TWO_HANDLES: Mode = Mode {
    writers: 1,
    reopen: Reopen::Close,
    second: Second::Open,
};
const CRASH_INJECTED: Mode = Mode {
    writers: 1,
    reopen: Reopen::Crash,
    second: Second::Open,
};

/// Breaks the store the way a negative control needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Broken {
    /// A correct store.
    No,
    /// The journal's triggers dropped after open (JC-27 must fail).
    WithoutTriggers,
    /// Durability waived under the production profile too (JC-26 must fail).
    DurabilityNotEnforced,
}

struct SqliteFactory {
    dir: TempDir,
    files: AtomicUsize,
    hooks: Hooks,
    mode: Mode,
    broken: Broken,
}

impl SqliteFactory {
    fn new(mode: Mode) -> Self {
        Self::broken(mode, Broken::No)
    }

    fn broken(mode: Mode, broken: Broken) -> Self {
        Self {
            dir: tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}")),
            files: AtomicUsize::new(0),
            hooks: Hooks::default(),
            mode,
            broken,
        }
    }

    fn options(&self) -> SqliteOptions {
        SqliteOptions {
            writers: std::num::NonZeroUsize::new(self.mode.writers)
                .unwrap_or(std::num::NonZeroUsize::MIN),
            ..SqliteOptions::new(Millis::new(1))
        }
    }

    fn next_file(&self) -> PathBuf {
        let n = self.files.fetch_add(1, Ordering::SeqCst);
        self.dir.path().join(format!("journal-{n}.sqlite"))
    }

    fn open(&self, path: &Path) -> SqliteJournal<Hooks> {
        SqliteJournal::open_with_hooks(path, self.options(), self.hooks.clone())
            .unwrap_or_else(|e| panic!("open {}: {e}", path.display()))
    }

    fn crash_reopen(&self, journal: SqliteJournal<Hooks>) -> SqliteJournal<Hooks> {
        let path = journal.path().to_owned();
        self.hooks.0.crashing.store(true, Ordering::SeqCst);
        drop(journal);
        self.hooks.0.crashing.store(false, Ordering::SeqCst);
        self.open(&path)
    }
}

fn raw(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap_or_else(|e| panic!("raw open: {e}"));
    conn.busy_timeout(Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("busy timeout: {e}"));
    conn
}

impl JournalFactory for SqliteFactory {
    type Journal = SqliteJournal<Hooks>;

    async fn fresh(&self) -> SqliteJournal<Hooks> {
        let path = self.next_file();
        let journal = self.open(&path);
        if self.broken == Broken::WithoutTriggers {
            raw(&path)
                .execute_batch(
                    "DROP TRIGGER journal_event_no_update; DROP TRIGGER journal_event_no_delete; \
                     DROP TRIGGER journal_identity_no_update; DROP TRIGGER journal_identity_no_delete; \
                     DROP TRIGGER stream_head_no_lowering; DROP TRIGGER stream_head_no_delete;",
                )
                .unwrap_or_else(|e| panic!("drop triggers: {e}"));
        }
        journal
    }

    async fn reopen(&self, journal: SqliteJournal<Hooks>) -> SqliteJournal<Hooks> {
        match self.mode.reopen {
            Reopen::Keep => journal,
            Reopen::Close => {
                let path = journal.path().to_owned();
                drop(journal);
                self.open(&path)
            }
            Reopen::Crash => self.crash_reopen(journal),
        }
    }

    async fn second_handle(&self, journal: &SqliteJournal<Hooks>) -> SqliteJournal<Hooks> {
        match self.mode.second {
            Second::Share => journal.clone(),
            Second::Open => self.open(journal.path()),
        }
    }

    async fn inject(&self, _journal: &SqliteJournal<Hooks>, fault: InjectedFault) -> bool {
        let mut armed = self
            .hooks
            .0
            .armed
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if matches!(*armed, Some(InjectedFault::CrashAfterStatement(_))) {
            // The previous crash point was past the unit's last statement: it never fired, so
            // there is no further point to walk to.
            *armed = None;
            return false;
        }
        *armed = Some(fault);
        true
    }

    async fn crash(&self, journal: SqliteJournal<Hooks>) -> SqliteJournal<Hooks> {
        self.crash_reopen(journal)
    }
}

impl EngineJournalFactory for SqliteFactory {
    async fn open_without_durability(&self, allow_relaxed: bool) -> Option<SqliteJournal<Hooks>> {
        let n = self.files.fetch_add(1, Ordering::SeqCst);
        // Named after this factory's tempdir: shared-cache memory stores are process-wide, and
        // tests run in parallel.
        let unique = self
            .dir
            .path()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let path = PathBuf::from(format!("file:mayfly-{unique}-{n}?mode=memory&cache=shared"));
        let relaxed = allow_relaxed || self.broken == Broken::DurabilityNotEnforced;
        let options = SqliteOptions {
            durability: if relaxed {
                Durability::Relaxed
            } else {
                Durability::Full
            },
            ..self.options()
        };
        SqliteJournal::open_with_hooks(&path, options, self.hooks.clone()).ok()
    }

    async fn tamper(
        &self,
        journal: &SqliteJournal<Hooks>,
        stream: &StreamId,
        tamper: Tamper,
    ) -> bool {
        let sql = match tamper {
            Tamper::UpdateEvent => {
                "UPDATE journal_event SET payload = '{}' WHERE stream_id = ?1 AND revision = 0"
            }
            Tamper::DeleteEvent => {
                "DELETE FROM journal_event WHERE stream_id = ?1 AND revision = 0"
            }
            Tamper::UpdateIdentity => {
                "UPDATE journal_identity SET revision = 100 WHERE stream_id = ?1 AND revision = 0"
            }
            Tamper::DeleteIdentity => {
                "DELETE FROM journal_identity WHERE stream_id = ?1 AND revision = 0"
            }
            Tamper::LowerHeadRevision => "UPDATE stream_head SET revision = 0 WHERE stream_id = ?1",
            Tamper::LowerHeadEpoch => {
                "UPDATE stream_head SET required_epoch = 1 WHERE stream_id = ?1"
            }
            Tamper::DeleteHead => "DELETE FROM stream_head WHERE stream_id = ?1",
        };
        let conn = raw(journal.path());
        conn.execute_batch("PRAGMA foreign_keys = ON")
            .unwrap_or_else(|e| panic!("foreign keys: {e}"));
        conn.execute(sql, [stream.as_str()])
            .is_ok_and(|rows| rows > 0)
    }
}

fn contract(factory: &SqliteFactory) -> (JournalContractReport, Vec<(EngineCase, String)>) {
    (
        block_on(check_journal_contract(factory)),
        block_on(check_engine_contract(factory)),
    )
}

#[test]
fn should_pass_journal_contract_when_sqlite_runs_fresh() {
    // given
    let factory = SqliteFactory::new(FRESH);

    // when
    let (journal, engine) = contract(&factory);

    // then
    assert!(journal.is_green(), "{journal}");
    assert!(engine.is_empty(), "{engine:?}");
}

#[test]
fn should_pass_journal_contract_when_sqlite_is_reopened_between_steps() {
    // given
    let factory = SqliteFactory::new(REOPENED);

    // when
    let (journal, engine) = contract(&factory);

    // then
    assert!(journal.is_green(), "{journal}");
    assert!(engine.is_empty(), "{engine:?}");
}

#[test]
fn should_pass_journal_contract_when_sqlite_has_two_writer_connections() {
    // given
    let factory = SqliteFactory::new(TWO_CONNECTIONS);

    // when
    let (journal, engine) = contract(&factory);

    // then
    assert!(journal.is_green(), "{journal}");
    assert!(engine.is_empty(), "{engine:?}");
}

#[test]
fn should_pass_journal_contract_when_sqlite_has_two_handles_on_one_file() {
    // given
    let factory = SqliteFactory::new(TWO_HANDLES);

    // when
    let (journal, engine) = contract(&factory);

    // then
    assert!(journal.is_green(), "{journal}");
    assert!(engine.is_empty(), "{engine:?}");
}

#[test]
fn should_pass_journal_contract_when_sqlite_crashes_after_each_statement() {
    // given
    let factory = SqliteFactory::new(CRASH_INJECTED);

    // when
    let (journal, engine) = contract(&factory);

    // then
    assert!(journal.is_green(), "{journal}");
    assert!(engine.is_empty(), "{engine:?}");
    // JC-23's unit has 19 statements and the COMMIT (reads: 2 heads, the receipt, the inbox
    // key, 2 ids; writes: 2 heads, the receipt, the inbox entry, 2 identities, 2 events, 1
    // manifest kind, 2 rows, 2 dispatch marks): with the point after BEGIN, 21 crash points,
    // each struck once.
    assert_eq!(factory.hooks.0.crashes.load(Ordering::SeqCst), 21);
}

#[test]
fn should_fail_history_refuses_rewrite_when_the_triggers_are_missing() {
    // given
    let factory = SqliteFactory::broken(FRESH, Broken::WithoutTriggers);

    // when
    let failed: Vec<&str> = block_on(check_engine_contract(&factory))
        .iter()
        .map(|(case, _)| case.id())
        .collect();

    // then
    assert_eq!(failed, ["JC-27"]);
}

#[test]
fn should_fail_durability_at_open_when_the_production_profile_waives_it() {
    // given
    let factory = SqliteFactory::broken(FRESH, Broken::DurabilityNotEnforced);

    // when
    let failed: Vec<&str> = block_on(check_engine_contract(&factory))
        .iter()
        .map(|(case, _)| case.id())
        .collect();

    // then
    assert_eq!(failed, ["JC-26"]);
}

fn attempt() -> AggregateName {
    AggregateName::new("attempt").unwrap_or_else(|e| panic!("{e}"))
}

fn one_event(stream: &StreamId, expected: ExpectedRevision, id: u128) -> UnitOfWork {
    UnitOfWork::new(Millis::new(5), SupportedEpochs::new().with(attempt(), 1)).with_append(
        stream.clone(),
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

#[test]
fn should_report_busy_and_write_nothing_when_another_connection_holds_the_write_lock() {
    // given
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let path = dir.path().join("journal.sqlite");
    let options = SqliteOptions {
        busy_timeout: Duration::from_millis(50),
        ..SqliteOptions::new(Millis::new(1))
    };
    let journal = SqliteJournal::open(&path, options).unwrap_or_else(|e| panic!("{e}"));
    let holder = raw(&path);
    holder
        .execute_batch("BEGIN IMMEDIATE")
        .unwrap_or_else(|e| panic!("{e}"));
    let s1 = StreamId::new("attempt-1").unwrap_or_else(|e| panic!("{e}"));

    // when
    let refused = block_on(journal.append(one_event(&s1, ExpectedRevision::NoStream, 1)));
    holder
        .execute_batch("ROLLBACK")
        .unwrap_or_else(|e| panic!("{e}"));
    let head_after = block_on(journal.head(&s1));
    let later = block_on(journal.append(one_event(&s1, ExpectedRevision::NoStream, 1)));

    // then
    assert_eq!(refused.map(|_| ()), Err(AppendError::Busy));
    assert_eq!(head_after, Ok(None));
    assert!(later.is_ok(), "{later:?}");
}

#[test]
fn should_record_the_first_migration_when_a_store_is_created() {
    // given
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let path = dir.path().join("journal.sqlite");

    // when
    let journal = SqliteJournal::open(&path, SqliteOptions::new(Millis::new(42)));

    // then
    assert!(journal.is_ok(), "{journal:?}");
    let meta: (i64, i64, i64) = raw(&path)
        .query_row(
            "SELECT version, applied_at, compat_floor FROM schema_meta",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(meta, (1, 42, 1));
}

#[test]
fn should_refuse_to_open_when_a_newer_build_raised_the_compatibility_floor() {
    // given
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let path = dir.path().join("journal.sqlite");
    drop(SqliteJournal::open(
        &path,
        SqliteOptions::new(Millis::new(1)),
    ));
    raw(&path)
        .execute_batch(
            "INSERT INTO schema_meta (version, applied_at, compat_floor) VALUES (9, 2, 9)",
        )
        .unwrap_or_else(|e| panic!("{e}"));

    // when
    let reopened = SqliteJournal::open(&path, SqliteOptions::new(Millis::new(3)));

    // then
    assert_eq!(
        reopened.map(|_| ()),
        Err(OpenError::SchemaAboveBuild {
            store: 9,
            compat_floor: 9,
            build: 2
        })
    );
}

fn schema_of(path: &Path) -> Vec<(String, String, String)> {
    let conn = raw(path);
    let mut stmt = conn
        .prepare(
            "SELECT type, name, coalesce(sql, '') FROM sqlite_schema \
             WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
        )
        .unwrap_or_else(|e| panic!("{e}"));
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .and_then(Iterator::collect)
        .unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn should_read_and_extend_a_store_when_it_holds_the_previous_schema() {
    // given
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let previous = dir.path().join("previous.sqlite");
    raw(&previous)
        .execute_batch(include_str!("fixtures/schema-0001.sql"))
        .unwrap_or_else(|e| panic!("fixture: {e}"));
    let fresh = dir.path().join("fresh.sqlite");
    drop(SqliteJournal::open(
        &fresh,
        SqliteOptions::new(Millis::new(1)),
    ));
    let s1 = StreamId::new("attempt-1").unwrap_or_else(|e| panic!("{e}"));

    // when
    let journal = SqliteJournal::open(&previous, SqliteOptions::new(Millis::new(2)))
        .unwrap_or_else(|e| panic!("{e}"));
    let events = block_on(journal.read_stream(&s1, None, None));
    let receipt = block_on(journal.receipt(mayfly_domain::CommandId::from_u128(7)));
    let next = block_on(journal.append(one_event(
        &s1,
        ExpectedRevision::Exactly(StreamRevision::new(0)),
        2,
    )));

    // then
    let events = events.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].payload.as_str(), "{\"n\":1}");
    assert!(matches!(receipt, Ok(Some(_))), "{receipt:?}");
    assert!(next.is_ok(), "{next:?}");
    drop(journal);
    assert_eq!(schema_of(&previous), schema_of(&fresh));
}
