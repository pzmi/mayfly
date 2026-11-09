//! What a save answers: the command's outcome, or why the store could not give one.

use std::fmt;

use mayfly_domain::{CommandId, EventId};

use crate::journal::{
    AggregateName, Appended, EventLocation, ExpectedRevision, InboxKey, JournalValueError,
    MalformedUnit, StorageError, StoredKind, StreamId, StreamRevision,
};

/// How a command was decided, first or from its receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome<R> {
    /// Accepted: what its unit appended (possibly nothing, a zero-event acceptance).
    Accepted {
        /// The appended streams, revisions and event ids.
        appended: Appended,
        /// Answered from a receipt another run wrote: this call wrote nothing.
        replayed: bool,
    },
    /// Refused by `decide`; a refusal receipt bound to what the decision read was recorded.
    Refused {
        /// The aggregate's refusal.
        rejection: R,
        /// Answered from a refusal receipt still bound to unchanged streams: this call wrote
        /// nothing.
        replayed: bool,
    },
}

/// Why `AggregateStore` gave no outcome. Every variant but [`SaveError::OutcomeUnknown`] means
/// this call committed nothing; only [`SaveError::Conflict`] is retried by
/// `with_revision_retry` ([`SaveError::is_retryable`]).
///
/// `#[non_exhaustive]`: the online rebuild (ES-12) adds the generation conflict, which is
/// retried like [`SaveError::Conflict`]; callers keep a fallback arm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SaveError {
    /// The aggregate's name, the instance's stream id or the command type is not a valid
    /// journal name: a programming error, refused before any read.
    InvalidName(JournalValueError),
    /// The stream holds events of a higher epoch than this build supports for its aggregate:
    /// it may be read, never written by this build (ES-I12), and this is checked **before** the
    /// receipt is consulted, so a rolled-back build never answers for a newer build's outcome
    /// (`doc/event-store.md` §3.7 step 1).
    StreamEpochUnsupported {
        /// The stream.
        stream: StreamId,
        /// Its epoch.
        epoch: u32,
    },
    /// The stream's head records another aggregate than the one asked to decide it (two
    /// aggregates' stream prefixes overlap): nothing is decided over another aggregate's events.
    AggregateMismatch {
        /// The stream.
        stream: StreamId,
        /// The aggregate its head records.
        recorded: AggregateName,
    },
    /// The command id has a receipt for another intent (ES-I5). Reveals nothing else.
    CommandIdReused {
        /// The reused id.
        command_id: CommandId,
    },
    /// A stored event of the stream has a kind this build does not decode: the command is
    /// refused and safe mode is raised (`doc/event-store.md` §6.3 rule 4).
    UnknownKind {
        /// The stream.
        stream: StreamId,
        /// The kind.
        kind: StoredKind,
    },
    /// A payload did not encode or decode (an event, a refusal, a body): `what` names which,
    /// a fixed `snake_case` code; no payload text is carried (ES-I13).
    Codec {
        /// The stream.
        stream: StreamId,
        /// What failed (`event_payload`, `rejection_payload`, `command_body`).
        what: &'static str,
    },
    /// A stream the decision read moved before the unit committed: decide again
    /// (`with_revision_retry`).
    Conflict {
        /// The stream.
        stream: StreamId,
        /// Where the decision read it.
        expected: ExpectedRevision,
        /// Where it is.
        actual: Option<StreamRevision>,
    },
    /// The command's inbox key was consumed by a committed unit: a redelivery (ES-I6).
    DuplicateMessage(InboxKey),
    /// An event id this command derived is recorded somewhere this command's unit did not
    /// write it: a genuine collision, never resolved as success (pinned by
    /// `tests/store/save.rs::should_report_a_collision_when_a_derived_id_lives_elsewhere`).
    EventIdCollision {
        /// The id.
        event_id: EventId,
        /// Where it is recorded, when the registry says.
        location: Option<EventLocation>,
    },
    /// The store refused the unit as malformed: a programming error.
    Malformed(MalformedUnit),
    /// `AggregateStore::execute` was given reads of other streams: a programming error, refused
    /// before any read. `execute` retries the request as it was built, so a read taken before
    /// the first attempt would be checked at its old revision again and conflict on every
    /// attempt; a decision that reads other streams builds them inside a `with_revision_retry`
    /// closure, fresh on each attempt (pinned by
    /// `tests/store/save.rs::should_refuse_reads_in_execute_when_a_retry_could_not_rebuild_them`
    /// and `should_rebuild_reads_in_the_closure_when_a_read_stream_moves`).
    ReadsOutsideRetry,
    /// A decided event needs a higher epoch than this build supports for the aggregate.
    EpochAboveBuild {
        /// The stream.
        stream: StreamId,
    },
    /// A bounded lock wait ran out: definite, nothing committed, not retried here.
    Busy,
    /// The store failed: definite, nothing committed.
    Storage(StorageError),
    /// The commit's reply was lost and re-appending the identical unit did not settle it
    /// within the bound: the unit is wholly committed or wholly absent, and the caller must
    /// not assume either (ES-I18); retrying the same command later answers from its receipt.
    OutcomeUnknown(StorageError),
}

impl SaveError {
    /// Whether `with_revision_retry` decides again after it: only a moved stream.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::Conflict { .. })
    }
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(e) => write!(f, "cannot address the command: {e}"),
            Self::StreamEpochUnsupported { stream, epoch } => write!(
                f,
                "stream {stream} is at epoch {epoch}, above what this build supports"
            ),
            Self::AggregateMismatch { stream, recorded } => write!(
                f,
                "stream {stream} belongs to aggregate {recorded}, not the one asked"
            ),
            Self::CommandIdReused { command_id } => {
                write!(f, "command id {command_id} was used for another intent")
            }
            Self::UnknownKind { stream, kind } => write!(
                f,
                "stream {stream} holds unknown event kind {}/v{}; safe mode",
                kind.event_type, kind.schema_version
            ),
            Self::Codec { stream, what } => {
                write!(f, "a payload of stream {stream} did not convert: {what}")
            }
            Self::Conflict {
                stream,
                expected,
                actual,
            } => match actual {
                Some(actual) => write!(
                    f,
                    "stream {stream} moved to revision {actual} from the decided {expected}"
                ),
                None => write!(
                    f,
                    "stream {stream} has no event, not the decided {expected}"
                ),
            },
            Self::DuplicateMessage(key) => write!(
                f,
                "message {} from {} was already consumed",
                key.message_id.as_str(),
                key.source
            ),
            Self::EventIdCollision { event_id, .. } => write!(
                f,
                "derived event id {event_id} is recorded where this command did not write it"
            ),
            Self::Malformed(m) => write!(f, "the store refused the unit: {m}"),
            Self::ReadsOutsideRetry => f.write_str(
                "execute retries a request as built: rebuild other streams' reads in a with_revision_retry closure",
            ),
            Self::EpochAboveBuild { stream } => write!(
                f,
                "an event for stream {stream} needs a higher epoch than this build supports"
            ),
            Self::Busy => f.write_str("the journal was busy; nothing was committed"),
            Self::Storage(e) => write!(f, "{e}; nothing was committed"),
            Self::OutcomeUnknown(e) => write!(f, "{e}; the commit's outcome is unknown"),
        }
    }
}

impl std::error::Error for SaveError {}
