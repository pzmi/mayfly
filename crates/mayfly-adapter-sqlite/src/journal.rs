//! `SqliteJournal`: the `Journal` port over one SQLite file (`doc/event-store.md` §4).

use std::fmt;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use mayfly_control::journal::{
    AppendError, Appended, CommandReceipt, EventLocation, InboxKey, Journal, MAX_PAGE, Payload,
    ProjectionName, RecordedEvent, StorageError, StoredKind, StreamHead, StreamId, StreamRevision,
    UnitOfWork,
};
use mayfly_domain::{CommandId, EventId, Millis};
use rusqlite::Connection;
use rusqlite::config::DbConfig;

use crate::append::{self, Stop};
use crate::classify::{Failure, read_error};
use crate::hooks::{NoHooks, StoreHooks};
use crate::rows;
use crate::schema::{self, MIGRATIONS};
use crate::writer::{NotQueued, Worker};

/// What the adapter requires of the engine's durability settings at open (ES-I19, JC-26).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Durability {
    /// Production: `journal_mode` reads back `wal` and `synchronous` reads back `2` (`FULL`)
    /// on every connection, or the store refuses to open.
    #[default]
    Full,
    /// The test profile: the same settings are asked for, and a store where they did not take
    /// (an in-memory store, a file system without shared memory) still opens. Never used by
    /// the daemon.
    Relaxed,
}

/// How a journal is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqliteOptions {
    /// What is required of the durability settings.
    pub durability: Durability,
    /// Writer connections, each on its own thread. One in production; the contract's
    /// two-connection mode opens two, which proves no refusal depends on there being one
    /// (`doc/event-store.md` §4.3 rule 2).
    pub writers: NonZeroUsize,
    /// How long a writer waits for the file's write lock before the append is `Busy`.
    pub busy_timeout: Duration,
    /// When the store is opened, from the control plane's `Clock` (ADR 0020): what a
    /// migration records as `applied_at`. The journal reads no clock.
    pub opened_at: Millis,
}

impl SqliteOptions {
    /// Production options: full durability, one writer, a 5 s lock wait.
    #[must_use]
    pub const fn new(opened_at: Millis) -> Self {
        Self {
            durability: Durability::Full,
            writers: NonZeroUsize::MIN,
            busy_timeout: Duration::from_secs(5),
            opened_at,
        }
    }
}

/// Why a journal did not open.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OpenError {
    /// The engine failed to open or configure the file.
    Engine(StorageError),
    /// A durability setting did not read back as required (ES-I19): the store would not survive
    /// a crash as the port promises.
    Durability {
        /// The setting.
        setting: &'static str,
        /// What it read back as.
        found: String,
    },
    /// The store was migrated by a newer build whose schema this build cannot run against
    /// (`doc/event-store.md` §6.3 rule 3).
    SchemaAboveBuild {
        /// The store's schema version.
        store: u32,
        /// The lowest version a build must know to run against it.
        compat_floor: u32,
        /// The newest version this build knows.
        build: u32,
    },
    /// A migration failed and was rolled back.
    Migration {
        /// The migration.
        version: u32,
        /// Why.
        error: StorageError,
    },
    /// The copy taken before a migration failed; the migration did not run.
    Backup {
        /// The migration it was for.
        version: u32,
        /// Why.
        error: StorageError,
    },
    /// A connection thread could not be started.
    Thread,
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Engine(e) => write!(f, "cannot open the journal: {e}"),
            Self::Durability { setting, found } => write!(
                f,
                "the journal's {setting} is {found}, not what crash durability requires"
            ),
            Self::SchemaAboveBuild {
                store,
                compat_floor,
                build,
            } => write!(
                f,
                "the journal's schema is version {store} and needs a build that knows version \
                 {compat_floor}; this build knows up to {build}"
            ),
            Self::Migration { version, error } => {
                write!(f, "migration {version} failed and was rolled back: {error}")
            }
            Self::Backup { version, error } => write!(
                f,
                "the copy before migration {version} failed, so it did not run: {error}"
            ),
            Self::Thread => f.write_str("cannot start a journal connection thread"),
        }
    }
}

impl std::error::Error for OpenError {}

fn open_error(error: &rusqlite::Error) -> OpenError {
    OpenError::Engine(read_error(error))
}

/// The durability problem with what a connection read back, if any (pinned by
/// `should_name_the_setting_when_durability_does_not_read_back`).
fn durability_problem(journal_mode: &str, synchronous: i64) -> Option<(&'static str, String)> {
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Some(("journal_mode", journal_mode.to_owned()));
    }
    if synchronous != 2 {
        return Some(("synchronous", synchronous.to_string()));
    }
    None
}

/// Opens and configures one connection: lock wait, WAL, `synchronous = FULL` and foreign keys,
/// each read back.
fn connect(path: &Path, options: &SqliteOptions) -> Result<Connection, OpenError> {
    let conn = Connection::open(path).map_err(|e| open_error(&e))?;
    conn.busy_timeout(options.busy_timeout)
        .map_err(|e| open_error(&e))?;
    let journal_mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .map_err(|e| open_error(&e))?;
    conn.execute_batch("PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;")
        .map_err(|e| open_error(&e))?;
    let synchronous: i64 = conn
        .query_row("PRAGMA synchronous", [], |r| r.get(0))
        .map_err(|e| open_error(&e))?;
    let foreign_keys: i64 = conn
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .map_err(|e| open_error(&e))?;
    if foreign_keys != 1 {
        return Err(OpenError::Durability {
            setting: "foreign_keys",
            found: foreign_keys.to_string(),
        });
    }
    if let Some((setting, found)) = durability_problem(&journal_mode, synchronous)
        && options.durability == Durability::Full
    {
        return Err(OpenError::Durability { setting, found });
    }
    Ok(conn)
}

/// Closes `conn` cleanly, or (when `hooks` say a crash is being played) leaving the WAL for the
/// next open to recover.
fn close<H: StoreHooks>(hooks: &H, conn: Connection) {
    if !hooks.checkpoint_on_close() {
        // Best effort: an engine that refuses the option checkpoints, which a reopen reads
        // the same.
        let _ = conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true);
    }
    drop(conn);
}

struct Inner<H> {
    writers: Vec<Worker>,
    reader: Worker,
    next: AtomicUsize,
    hooks: Arc<H>,
    path: PathBuf,
}

/// The SQLite journal: one file, one or more writer connections, one reader connection, each on
/// its own thread (ADR 0008, amended by ADR 0021). Clones share the connections; a second
/// [`SqliteJournal::open`] of the same file is a second handle, as another process would hold.
///
/// It passes the journal contract in five modes, fresh, reopened, two-connection, two-handle and
/// crash-injected, plus the engine cases JC-26 and JC-27 (`tests/journal_contract.rs`:
/// `should_pass_journal_contract_when_*`). It caches nothing across transactions: no head,
/// receipt or epoch is remembered between calls (`doc/event-store.md` §4.3 rule 3).
pub struct SqliteJournal<H: StoreHooks = NoHooks> {
    inner: Arc<Inner<H>>,
}

impl<H: StoreHooks> Clone for SqliteJournal<H> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<H: StoreHooks> fmt::Debug for SqliteJournal<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqliteJournal")
            .field("writers", &self.inner.writers.len())
            .finish_non_exhaustive()
    }
}

impl SqliteJournal<NoHooks> {
    /// Opens (creating and migrating if needed) the journal at `path` with production hooks.
    ///
    /// # Errors
    /// [`OpenError`] when the file cannot be opened, its durability settings do not read back
    /// as `options` require, its schema is above this build, or a migration fails.
    pub fn open(path: &Path, options: SqliteOptions) -> Result<Self, OpenError> {
        Self::open_with_hooks(path, options, NoHooks)
    }
}

impl<H: StoreHooks> SqliteJournal<H> {
    /// Opens the journal at `path` with `hooks` at its commit barriers (adapter tests and the
    /// store lab only, `ARCH-store-hooks`).
    ///
    /// # Errors
    /// As [`SqliteJournal::open`].
    pub fn open_with_hooks(
        path: &Path,
        options: SqliteOptions,
        hooks: H,
    ) -> Result<Self, OpenError> {
        let hooks = Arc::new(hooks);
        let first = connect(path, &options)?;
        let backup = Some(path).filter(|p| !p.to_string_lossy().starts_with("file:"));
        schema::migrate(&first, MIGRATIONS, backup, options.opened_at)?;
        let mut writers = Vec::with_capacity(options.writers.get());
        let mut first = Some(first);
        for n in 0..options.writers.get() {
            let conn = match first.take() {
                Some(conn) => conn,
                None => connect(path, &options)?,
            };
            let closing = Arc::clone(&hooks);
            writers.push(
                Worker::spawn(conn, &format!("mayfly-sqlite-writer-{n}"), move |c| {
                    close(closing.as_ref(), c);
                })
                .map_err(|_| OpenError::Thread)?,
            );
        }
        let reader = connect(path, &options)?;
        reader
            .execute_batch("PRAGMA query_only = ON")
            .map_err(|e| open_error(&e))?;
        let closing = Arc::clone(&hooks);
        let reader = Worker::spawn(reader, "mayfly-sqlite-reader", move |c| {
            close(closing.as_ref(), c);
        })
        .map_err(|_| OpenError::Thread)?;
        Ok(Self {
            inner: Arc::new(Inner {
                writers,
                reader,
                next: AtomicUsize::new(0),
                hooks,
                path: path.to_owned(),
            }),
        })
    }

    /// The file this journal stores into.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Runs `f` on a writer connection, the next in turn: how the delivery ledger runs its
    /// transactions on the journal's writers (`crate::ledger`).
    pub(crate) async fn write<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, Failure> + Send + 'static,
    ) -> Result<T, StorageError> {
        let n = self.inner.next.fetch_add(1, Ordering::Relaxed) % self.inner.writers.len();
        let worker = self
            .inner
            .writers
            .get(n)
            .ok_or(StorageError::new("writer_gone"))?;
        let pending = worker
            .run(move |slot| match slot.conn.as_ref() {
                Some(conn) => f(conn).map_err(Failure::storage_error),
                None => Err(StorageError::new("connection_lost")),
            })
            .map_err(|refused| match refused {
                NotQueued::Full => StorageError::new("busy"),
                NotQueued::Gone => StorageError::new("writer_gone"),
            })?;
        pending
            .await
            .unwrap_or(Err(StorageError::new("writer_gone")))
    }

    pub(crate) async fn read<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, Failure> + Send + 'static,
    ) -> Result<T, StorageError> {
        let pending = self
            .inner
            .reader
            .run(move |slot| match slot.conn.as_ref() {
                Some(conn) => f(conn).map_err(Failure::storage_error),
                None => Err(StorageError::new("connection_lost")),
            })
            .map_err(|refused| match refused {
                NotQueued::Full => StorageError::new("busy"),
                NotQueued::Gone => StorageError::new("reader_gone"),
            })?;
        pending
            .await
            .unwrap_or(Err(StorageError::new("reader_gone")))
    }
}

impl<H: StoreHooks> Journal for SqliteJournal<H> {
    async fn append(&self, unit: UnitOfWork) -> Result<Appended, AppendError> {
        let n = self.inner.next.fetch_add(1, Ordering::Relaxed) % self.inner.writers.len();
        let hooks = Arc::clone(&self.inner.hooks);
        let worker = self
            .inner
            .writers
            .get(n)
            .ok_or(AppendError::Storage(StorageError::new("writer_gone")))?;
        let pending = worker
            .run(move |slot| {
                let Some(conn) = slot.conn.as_ref() else {
                    return Err(AppendError::Storage(StorageError::new("connection_lost")));
                };
                match append::append(conn, hooks.as_ref(), &unit) {
                    Ok(appended) => Ok(appended),
                    Err(Stop::Refused(error)) => Err(error),
                    Err(Stop::Unknown(code)) => {
                        Err(AppendError::OutcomeUnknown(StorageError::new(code)))
                    }
                    Err(Stop::Crash) => {
                        if let Some(conn) = slot.conn.take() {
                            close(hooks.as_ref(), conn);
                        }
                        Err(AppendError::OutcomeUnknown(StorageError::new(
                            "connection_lost",
                        )))
                    }
                }
            })
            .map_err(|refused| match refused {
                NotQueued::Full => AppendError::Busy,
                NotQueued::Gone => AppendError::Storage(StorageError::new("writer_gone")),
            })?;
        pending
            .await
            .unwrap_or(Err(AppendError::OutcomeUnknown(StorageError::new(
                "writer_gone",
            ))))
    }

    async fn head(&self, stream: &StreamId) -> Result<Option<StreamHead>, StorageError> {
        let stream = stream.clone();
        self.read(move |c| rows::head(c, &stream)).await
    }

    async fn read_stream(
        &self,
        stream: &StreamId,
        after: Option<StreamRevision>,
        through: Option<StreamRevision>,
    ) -> Result<Vec<RecordedEvent>, StorageError> {
        let stream = stream.clone();
        self.read(move |c| rows::read_stream(c, &stream, after, through))
            .await
    }

    async fn lookup_event(&self, event_id: EventId) -> Result<Option<EventLocation>, StorageError> {
        self.read(move |c| rows::lookup_event(c, event_id)).await
    }

    async fn receipt(&self, command_id: CommandId) -> Result<Option<CommandReceipt>, StorageError> {
        self.read(move |c| Ok(rows::receipt(c, command_id)?.map(|stored| stored.receipt)))
            .await
    }

    async fn inbox_contains(&self, key: &InboxKey) -> Result<bool, StorageError> {
        let key = key.clone();
        self.read(move |c| rows::inbox_contains(c, &key)).await
    }

    async fn streams(
        &self,
        after: Option<&StreamId>,
        limit: u32,
    ) -> Result<Vec<StreamHead>, StorageError> {
        let after = after.cloned();
        let limit = limit.min(MAX_PAGE);
        self.read(move |c| rows::streams(c, after.as_ref(), limit))
            .await
    }

    async fn stored_kinds(&self) -> Result<Vec<StoredKind>, StorageError> {
        self.read(rows::stored_kinds).await
    }

    async fn projection(
        &self,
        projection: &ProjectionName,
        key: &str,
    ) -> Result<Option<Payload>, StorageError> {
        let (projection, key) = (projection.clone(), key.to_owned());
        self.read(move |c| rows::projection(c, &projection, &key))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_name_the_setting_when_durability_does_not_read_back() {
        // given
        let readings = [
            ("wal", 2),
            ("WAL", 2),
            ("memory", 2),
            ("delete", 2),
            ("wal", 1),
            ("wal", 0),
        ];

        // when
        let problems: Vec<_> = readings
            .iter()
            .map(|(mode, sync)| durability_problem(mode, *sync))
            .collect();

        // then
        assert_eq!(
            problems,
            [
                None,
                None,
                Some(("journal_mode", "memory".to_owned())),
                Some(("journal_mode", "delete".to_owned())),
                Some(("synchronous", "1".to_owned())),
                Some(("synchronous", "0".to_owned())),
            ]
        );
    }

    #[test]
    fn should_say_what_failed_when_an_open_error_is_displayed() {
        // given
        let errors = [
            OpenError::Engine(StorageError::new("cannot_open")),
            OpenError::Durability {
                setting: "journal_mode",
                found: "memory".to_owned(),
            },
            OpenError::SchemaAboveBuild {
                store: 3,
                compat_floor: 2,
                build: 1,
            },
            OpenError::Migration {
                version: 1,
                error: StorageError::new("disk_full"),
            },
            OpenError::Backup {
                version: 2,
                error: StorageError::new("io_error"),
            },
            OpenError::Thread,
        ];

        // when
        let texts: Vec<String> = errors.iter().map(ToString::to_string).collect();

        // then
        assert_eq!(
            texts,
            [
                "cannot open the journal: journal storage failed: cannot_open",
                "the journal's journal_mode is memory, not what crash durability requires",
                "the journal's schema is version 3 and needs a build that knows version 2; this build knows up to 1",
                "migration 1 failed and was rolled back: journal storage failed: disk_full",
                "the copy before migration 2 failed, so it did not run: journal storage failed: io_error",
                "cannot start a journal connection thread",
            ]
        );
    }
}
