//! Why an append or a read failed.

use std::fmt;

use mayfly_domain::{DirectiveId, EventId};

use crate::journal::outbox::OutboxStatus;
use crate::journal::unit::MalformedUnit;
use crate::journal::values::{CommandReceipt, InboxEntry, StreamId, StreamRevision};

/// The store failed: I/O, corruption, a writer that went away. The reason is for operators and
/// logs; it never carries a payload, a secret or a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageError {
    reason: String,
}

impl StorageError {
    /// A failure described by `reason`.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// The description.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "journal storage failed: {}", self.reason)
    }
}

impl std::error::Error for StorageError {}

/// Why a unit of work was not committed. Every variant but
/// [`AppendError::OutcomeUnknown`] means **nothing** of the unit was committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppendError {
    /// The unit is malformed on its own ([`crate::journal::UnitOfWork::check_shape`]).
    Malformed(MalformedUnit),
    /// The unit's command already has a receipt: the command ran. The original receipt is
    /// returned so a repeat answers exactly as the first run did (fnd §6.4).
    DuplicateCommand(Box<CommandReceipt>),
    /// The unit's inbox entry was already processed: a redelivery.
    DuplicateMessage(InboxEntry),
    /// A stream is not at the revision the writer expected; `with_version_retry` (ES-03)
    /// reloads and decides again.
    WrongExpectedRevision {
        /// The stream.
        stream: StreamId,
        /// What the unit expected.
        expected: StreamRevision,
        /// What the journal holds.
        actual: StreamRevision,
    },
    /// An event id is already recorded.
    DuplicateEventId(EventId),
    /// An outbox insert names a directive that already exists.
    DuplicateDirective(DirectiveId),
    /// An outbox transition names a directive that does not exist.
    UnknownDirective(DirectiveId),
    /// An outbox transition's `from` is not the row's status: someone moved it first.
    OutboxStatusMismatch {
        /// The row.
        directive_id: DirectiveId,
        /// The transition's `from`.
        expected: OutboxStatus,
        /// The row's status.
        actual: OutboxStatus,
    },
    /// An outbox transition the status machine forbids ([`OutboxStatus::allows`]).
    IllegalOutboxTransition {
        /// The row.
        directive_id: DirectiveId,
        /// Its status.
        from: OutboxStatus,
        /// The refused target.
        to: OutboxStatus,
    },
    /// The store failed before committing: nothing was committed.
    Storage(StorageError),
    /// The store failed in a way that leaves the commit's outcome unknown (the reply from the
    /// writer was lost). The unit is either wholly committed or wholly absent; the caller finds
    /// out which by reading the receipt or the inbox entry, never by guessing.
    OutcomeUnknown(StorageError),
}

impl fmt::Display for AppendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(m) => write!(f, "malformed unit of work: {m}"),
            Self::DuplicateCommand(r) => {
                write!(f, "command {} already has a receipt", r.command_id)
            }
            Self::DuplicateMessage(e) => write!(
                f,
                "message {} from {} was already processed",
                e.message_id.as_str(),
                e.source
            ),
            Self::WrongExpectedRevision {
                stream,
                expected,
                actual,
            } => write!(
                f,
                "stream {stream} is at revision {actual}, not the expected {expected}"
            ),
            Self::DuplicateEventId(id) => write!(f, "event {id} is already recorded"),
            Self::DuplicateDirective(id) => write!(f, "directive {id} is already in the outbox"),
            Self::UnknownDirective(id) => write!(f, "directive {id} is not in the outbox"),
            Self::OutboxStatusMismatch {
                directive_id,
                expected,
                actual,
            } => write!(
                f,
                "directive {directive_id} is {actual}, not the expected {expected}"
            ),
            Self::IllegalOutboxTransition {
                directive_id,
                from,
                to,
            } => write!(
                f,
                "directive {directive_id} may not move from {from} to {to}"
            ),
            Self::Storage(e) => write!(f, "{e}; nothing was committed"),
            Self::OutcomeUnknown(e) => write!(f, "{e}; the commit's outcome is unknown"),
        }
    }
}

impl std::error::Error for AppendError {}
