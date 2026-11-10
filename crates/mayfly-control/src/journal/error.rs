//! Why an append or a read failed (`doc/event-store.md` §3.3, ES-I18).

use std::fmt;

use mayfly_domain::{CommandId, EventId};

use crate::journal::unit::MalformedUnit;
use crate::journal::values::{
    CommandReceipt, ExpectedRevision, InboxKey, StreamId, StreamRevision,
};

/// The store failed: I/O, corruption, a writer that went away. It carries a fixed `snake_case`
/// code and nothing else, so no payload, secret, path or engine message can reach a log or a
/// caller through it (ES-I13; contract case JC-19 `payload_round_trip`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageError {
    code: &'static str,
}

impl StorageError {
    /// A failure named by `code`, a fixed `snake_case` name (an adapter maps an engine's
    /// extended code or SQLSTATE to one; it never formats one from data).
    #[must_use]
    pub const fn new(code: &'static str) -> Self {
        Self { code }
    }

    /// The code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "journal storage failed: {}", self.code)
    }
}

impl std::error::Error for StorageError {}

/// Why a unit of work was not committed, in **reporting order**: a unit that breaks several
/// rules is refused with the first variant below that applies (contract case JC-09
/// `refusal_order`). Every variant but [`AppendError::OutcomeUnknown`] is **definite**: nothing
/// of the unit was committed ([`AppendError::is_definite`]).
///
/// The reporting order is not the lock order (`doc/event-store.md` §4.2): an adapter takes
/// its locks heads-first in `StreamId` order and may learn of a duplicate event id after it
/// holds every head, yet must still report the duplicate id ahead of a stale revision. That is
/// what lets a writer that lost a commit's reply re-append the identical unit and be answered
/// by its receipt or by `DuplicateEventId` at the place it wrote, rather than by the
/// `WrongExpectedRevision` its own commit caused.
///
/// `#[non_exhaustive]`: the online rebuild (ES-12) adds `ProjectionGenerationMoved`; callers
/// keep a fallback arm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AppendError {
    /// The unit is malformed on its own ([`crate::journal::UnitOfWork::check_shape`]),
    /// whatever is stored.
    Malformed(MalformedUnit),
    /// An event of the unit needs a higher epoch than this build supports for its aggregate
    /// ([`crate::journal::UnitOfWork::epoch_above_build`]): this build could not decide the
    /// stream afterwards (ES-I12).
    EpochAboveBuild {
        /// The lowest such stream in `StreamId` order.
        stream: StreamId,
    },
    /// The unit's command already has a receipt with the same intent that still answers: an
    /// accepted receipt (for ever), or a refusal receipt whose stream is still at the revision
    /// it was decided at (E3). Carries that receipt, so a repeat answers exactly as the first
    /// run did; an `Err` so no caller mistakes it for a fresh commit. Only while every stream
    /// the answer is for ([`CommandReceipt::replay_streams`]) is within this build's supported
    /// epoch, judged in the same transaction as the receipt: otherwise the receipt may be a
    /// newer build's outcome, and the unit is refused [`AppendError::StreamEpochUnsupported`]
    /// instead (contract case JC-33 `replay_within_epoch`).
    CommandReplayed(Box<CommandReceipt>),
    /// The unit's command id has a receipt for another intent. Reveals nothing else.
    CommandIdReused {
        /// The reused id.
        command_id: CommandId,
    },
    /// The unit's inbox key was consumed by a committed unit: a redelivery (ES-I6).
    DuplicateMessage(InboxKey),
    /// An event id of the unit is already recorded; the lowest such id is named. Resolve with
    /// [`crate::journal::Journal::lookup_event`].
    DuplicateEventId(EventId),
    /// A stream the unit appends to or reads is not where the unit expected; the lowest such
    /// stream in `StreamId` order is named. `with_revision_retry` (ES-03) reloads and decides
    /// again.
    WrongExpectedRevision {
        /// The stream.
        stream: StreamId,
        /// What the unit expected.
        expected: ExpectedRevision,
        /// The revision of its last event, `None` when it has none.
        actual: Option<StreamRevision>,
    },
    /// A stream the unit appends to holds events of a higher epoch than this build supports
    /// for its aggregate: this build may read it but never write it (ES-I12, `doc/event-store.md`
    /// §6.3); the lowest such stream in `StreamId` order is named. Also, ahead of
    /// `CommandReplayed`, a stream a stored receipt's answer is for: this build never answers
    /// from it (§3.3).
    StreamEpochUnsupported {
        /// The stream.
        stream: StreamId,
        /// Its epoch.
        epoch: u32,
    },
    /// A bounded lock wait ran out (`busy_timeout`, `lock_timeout`): definite, nothing
    /// committed. The caller reports it; nothing retries it blindly.
    Busy,
    /// The store failed before committing: nothing was committed.
    Storage(StorageError),
    /// The commit's reply was lost: the unit is wholly committed or wholly absent, and only a
    /// read can tell which. Re-append the identical unit: its receipt answers
    /// (`CommandReplayed`), or `DuplicateEventId` names an id that
    /// [`crate::journal::Journal::lookup_event`] finds at the place the unit wrote it; any other
    /// answer is the outcome of a unit that never committed (contract case JC-25
    /// `outcome_unknown`).
    OutcomeUnknown(StorageError),
}

impl AppendError {
    /// Whether the error says for certain that nothing was committed: every variant but
    /// [`AppendError::OutcomeUnknown`] (ES-I18).
    #[must_use]
    pub const fn is_definite(&self) -> bool {
        !matches!(self, Self::OutcomeUnknown(_))
    }
}

impl fmt::Display for AppendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(m) => write!(f, "malformed unit of work: {m}"),
            Self::EpochAboveBuild { stream } => write!(
                f,
                "an event for stream {stream} needs a higher epoch than this build supports"
            ),
            Self::CommandReplayed(r) => {
                write!(f, "command {} already has a receipt", r.command_id)
            }
            Self::CommandIdReused { command_id } => {
                write!(f, "command id {command_id} was used for another intent")
            }
            Self::DuplicateMessage(key) => write!(
                f,
                "message {} from {} was already consumed",
                key.message_id.as_str(),
                key.source
            ),
            Self::DuplicateEventId(id) => write!(f, "event {id} is already recorded"),
            Self::WrongExpectedRevision {
                stream,
                expected,
                actual,
            } => match actual {
                Some(actual) => write!(
                    f,
                    "stream {stream} is at revision {actual}, not the expected {expected}"
                ),
                None => write!(
                    f,
                    "stream {stream} has no event, not the expected {expected}"
                ),
            },
            Self::StreamEpochUnsupported { stream, epoch } => write!(
                f,
                "stream {stream} is at epoch {epoch}, above what this build supports"
            ),
            Self::Busy => f.write_str("the journal was busy; nothing was committed"),
            Self::Storage(e) => write!(f, "{e}; nothing was committed"),
            Self::OutcomeUnknown(e) => write!(f, "{e}; the commit's outcome is unknown"),
        }
    }
}

impl std::error::Error for AppendError {}
