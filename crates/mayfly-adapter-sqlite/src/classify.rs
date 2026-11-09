//! How an engine error becomes a port error (ES-I18, `doc/event-store.md` §4.3 rule 8): by
//! SQLite's **extended result code** and the **constraint the failing statement guards**, never
//! by the engine's message text, which is neither stable nor safe to pass on (it can quote a
//! value; ES-I13).
//!
//! SQLite names the violated index only in its message, so the constraint is known from the
//! statement instead: every write statement of an append can violate exactly one unique
//! constraint, named in [`Constraint`] after its `CONSTRAINT` clause in the schema (the same
//! names as the PostgreSQL schema, §4.1). A uniqueness failure of that statement is that
//! constraint's refusal; any other failure is a fixed [`StorageError`] code.

use mayfly_control::journal::StorageError;
use rusqlite::ffi;

/// The unique constraint a write statement guards, by its schema name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Constraint {
    /// `stream_head_pkey`: a second creator of a stream (`WrongExpectedRevision`).
    StreamHead,
    /// `command_receipt_pkey`: a second receipt for a command (`CommandReplayed`/`CommandIdReused`).
    CommandReceipt,
    /// `inbox_pkey`: a second consumer of a message (`DuplicateMessage`).
    Inbox,
    /// `journal_identity_pkey`: a second place for an event id (`DuplicateEventId`).
    JournalIdentity,
    /// `journal_event_pkey`: a second event at one revision; the head's compare-and-set makes
    /// this unreachable, so it is a storage fault, never a refusal.
    JournalEvent,
}

impl Constraint {
    /// The constraint's name in the schema.
    #[cfg(test)]
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::StreamHead => "stream_head_pkey",
            Self::CommandReceipt => "command_receipt_pkey",
            Self::Inbox => "inbox_pkey",
            Self::JournalIdentity => "journal_identity_pkey",
            Self::JournalEvent => "journal_event_pkey",
        }
    }
}

/// What an engine error means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failure {
    /// A bounded lock wait ran out, or the file is locked: definite, nothing written.
    Busy,
    /// The statement's unique constraint was violated.
    Unique,
    /// Anything else, as a fixed code.
    Storage(StorageError),
}

/// Classifies `error` by its extended result code (pinned by
/// `should_classify_engine_errors_by_extended_code_when_mapped`).
pub(crate) fn classify(error: &rusqlite::Error) -> Failure {
    match error {
        rusqlite::Error::SqliteFailure(failure, _) => by_code(failure.extended_code),
        rusqlite::Error::FromSqlConversionFailure(..)
        | rusqlite::Error::IntegralValueOutOfRange(..)
        | rusqlite::Error::InvalidColumnType(..)
        | rusqlite::Error::Utf8Error(..) => Failure::Storage(StorageError::new("row_unreadable")),
        _ => Failure::Storage(StorageError::new("engine_error")),
    }
}

fn by_code(extended: i32) -> Failure {
    match extended {
        ffi::SQLITE_CONSTRAINT_PRIMARYKEY | ffi::SQLITE_CONSTRAINT_UNIQUE => Failure::Unique,
        ffi::SQLITE_CONSTRAINT_TRIGGER => storage("history_rewrite_refused"),
        ffi::SQLITE_CONSTRAINT_FOREIGNKEY => storage("identity_mismatch"),
        ffi::SQLITE_CONSTRAINT_CHECK
        | ffi::SQLITE_CONSTRAINT_NOTNULL
        | ffi::SQLITE_CONSTRAINT_DATATYPE => storage("row_rejected"),
        _ => match extended & 0xff {
            ffi::SQLITE_BUSY | ffi::SQLITE_LOCKED => Failure::Busy,
            ffi::SQLITE_CONSTRAINT => storage("constraint_failed"),
            ffi::SQLITE_FULL => storage("disk_full"),
            ffi::SQLITE_IOERR => storage("io_error"),
            ffi::SQLITE_CORRUPT => storage("store_corrupt"),
            ffi::SQLITE_NOTADB => storage("not_a_database"),
            ffi::SQLITE_READONLY => storage("read_only"),
            ffi::SQLITE_CANTOPEN => storage("cannot_open"),
            ffi::SQLITE_NOMEM => storage("out_of_memory"),
            ffi::SQLITE_PERM | ffi::SQLITE_AUTH => storage("permission_denied"),
            _ => storage("engine_error"),
        },
    }
}

const fn storage(code: &'static str) -> Failure {
    Failure::Storage(StorageError::new(code))
}

impl Failure {
    /// The [`StorageError`] a read or an open reports for it: a lock that never came is
    /// `busy`, a uniqueness failure no statement expected is `constraint_failed`.
    pub(crate) const fn storage_error(self) -> StorageError {
        match self {
            Self::Busy => StorageError::new("busy"),
            Self::Unique => StorageError::new("constraint_failed"),
            Self::Storage(e) => e,
        }
    }
}

/// The [`StorageError`] a read or an open reports for `error`.
pub(crate) fn read_error(error: &rusqlite::Error) -> StorageError {
    classify(error).storage_error()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(code: i32) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(
            ffi::Error::new(code),
            Some("UNIQUE constraint failed: journal_identity.event_id".to_owned()),
        )
    }

    #[test]
    fn should_classify_engine_errors_by_extended_code_when_mapped() {
        // given
        let cases = [
            (ffi::SQLITE_CONSTRAINT_PRIMARYKEY, Failure::Unique),
            (ffi::SQLITE_CONSTRAINT_UNIQUE, Failure::Unique),
            (ffi::SQLITE_BUSY, Failure::Busy),
            (ffi::SQLITE_BUSY_SNAPSHOT, Failure::Busy),
            (ffi::SQLITE_LOCKED, Failure::Busy),
            (
                ffi::SQLITE_CONSTRAINT_TRIGGER,
                storage("history_rewrite_refused"),
            ),
            (
                ffi::SQLITE_CONSTRAINT_FOREIGNKEY,
                storage("identity_mismatch"),
            ),
            (ffi::SQLITE_CONSTRAINT_CHECK, storage("row_rejected")),
            (ffi::SQLITE_CONSTRAINT_NOTNULL, storage("row_rejected")),
            (ffi::SQLITE_CONSTRAINT_DATATYPE, storage("row_rejected")),
            (ffi::SQLITE_CONSTRAINT, storage("constraint_failed")),
            (ffi::SQLITE_FULL, storage("disk_full")),
            (ffi::SQLITE_IOERR_FSYNC, storage("io_error")),
            (ffi::SQLITE_CORRUPT, storage("store_corrupt")),
            (ffi::SQLITE_NOTADB, storage("not_a_database")),
            (ffi::SQLITE_READONLY, storage("read_only")),
            (ffi::SQLITE_CANTOPEN, storage("cannot_open")),
            (ffi::SQLITE_NOMEM, storage("out_of_memory")),
            (ffi::SQLITE_PERM, storage("permission_denied")),
            (ffi::SQLITE_AUTH, storage("permission_denied")),
            (ffi::SQLITE_MISUSE, storage("engine_error")),
        ];

        // when
        let got: Vec<Failure> = cases
            .iter()
            .map(|(code, _)| classify(&engine(*code)))
            .collect();

        // then
        let want: Vec<Failure> = cases.iter().map(|(_, failure)| *failure).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn should_report_fixed_codes_when_a_read_or_a_decode_fails() {
        // given
        let decode = rusqlite::Error::InvalidColumnType(
            0,
            "revision".to_owned(),
            rusqlite::types::Type::Text,
        );
        let other = rusqlite::Error::InvalidQuery;

        // when
        let codes = [
            read_error(&engine(ffi::SQLITE_BUSY)).code(),
            read_error(&engine(ffi::SQLITE_CONSTRAINT_UNIQUE)).code(),
            read_error(&engine(ffi::SQLITE_IOERR)).code(),
            read_error(&decode).code(),
            read_error(&other).code(),
        ];

        // then
        assert_eq!(
            codes,
            [
                "busy",
                "constraint_failed",
                "io_error",
                "row_unreadable",
                "engine_error"
            ]
        );
    }

    #[test]
    fn should_name_each_guarded_constraint_as_the_schema_does_when_listed() {
        // given
        let schema = include_str!("migrations/0001_journal.sql");
        let constraints = [
            Constraint::StreamHead,
            Constraint::CommandReceipt,
            Constraint::Inbox,
            Constraint::JournalIdentity,
            Constraint::JournalEvent,
        ];

        // when
        let missing: Vec<&str> = constraints
            .iter()
            .map(|c| c.name())
            .filter(|name| !schema.contains(&format!("CONSTRAINT {name} PRIMARY KEY")))
            .collect();

        // then
        assert_eq!(missing, Vec::<&str>::new());
    }
}
