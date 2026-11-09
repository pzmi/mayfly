//! The values a unit of work carries and a read returns. Payloads stay encoded: the journal
//! stores and returns them verbatim and never parses them, so this port names no format
//! (encoding is `AggregateStore`'s job).

use std::fmt;

use mayfly_domain::{CommandId, EventId, Millis};

/// Why a journal name or id did not validate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalValueError {
    /// A stream id is not `[a-z0-9-]{1,128}` starting with a letter or digit.
    InvalidStreamId,
    /// An inbox source is not `[a-z0-9-]{1,128}` starting with a letter or digit.
    InvalidInboxSource,
    /// A message id is empty, longer than 256 bytes or holds a control character.
    InvalidMessageId,
    /// A projection name is not `[a-z][a-z0-9_]{0,63}`.
    InvalidProjectionName,
}

impl fmt::Display for JournalValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidStreamId => "invalid stream id",
            Self::InvalidInboxSource => "invalid inbox source",
            Self::InvalidMessageId => "invalid message id",
            Self::InvalidProjectionName => "invalid projection name",
        })
    }
}

impl std::error::Error for JournalValueError {}

fn valid_label(s: &str) -> bool {
    (1..=128).contains(&s.len())
        && s.bytes().next().is_some_and(|b| b != b'-')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

macro_rules! label_type {
    ($(#[$doc:meta])* $name:ident, $err:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            /// Validates `[a-z0-9-]{1,128}`, not starting with `-`.
            ///
            /// # Errors
            #[doc = concat!("[`JournalValueError::", stringify!($err), "`] for anything else.")]
            pub fn new(value: &str) -> Result<Self, JournalValueError> {
                if valid_label(value) {
                    Ok(Self(value.to_owned()))
                } else {
                    Err(JournalValueError::$err)
                }
            }

            /// The validated text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

label_type!(
    /// A stream, `<aggregate>-<id>` by convention (plan §5.1): the unit of ordering and of the
    /// expected-revision check. Pinned by `should_refuse_stream_id_when_it_is_malformed`.
    StreamId,
    InvalidStreamId
);
label_type!(
    /// Where an inbox message came from: a scale-set session or a data-plane host (plan §3.2,
    /// §7.2). Message ids are unique only within their source.
    InboxSource,
    InvalidInboxSource
);

/// A stream's revision: the number of events in it. [`StreamRevision::EMPTY`] (0) is a stream
/// with no events; the first event is recorded at revision 1.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StreamRevision(u64);

impl StreamRevision {
    /// The revision of a stream with no events.
    pub const EMPTY: Self = Self(0);

    /// Wraps a raw value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The revision the next event takes, or `None` at the numeric limit (never wraps).
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }
}

impl fmt::Display for StreamRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// An encoded document (JSON, by `AggregateStore`'s convention). Opaque here: the journal
/// stores it and returns it unchanged, byte for byte.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Payload(String);

impl Payload {
    /// Wraps an encoded document.
    #[must_use]
    pub fn new(encoded: impl Into<String>) -> Self {
        Self(encoded.into())
    }

    /// The encoded text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Unwraps the encoded text.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

/// An event to append: its caller-minted id, its kind and its encoded payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEvent {
    /// Unique across the journal; a second append of the same id is refused.
    pub event_id: EventId,
    /// `EventKind::event_type`.
    pub event_type: String,
    /// `EventKind::schema_version`.
    pub schema_version: u32,
    /// The event struct's encoded form.
    pub payload: Payload,
}

/// An event as the journal recorded it. It has a place in its stream and nowhere else: the
/// journal keeps no global order (ADR 0021).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedEvent {
    /// Its stream.
    pub stream: StreamId,
    /// Its revision in the stream, from 1.
    pub revision: StreamRevision,
    /// See [`NewEvent::event_id`].
    pub event_id: EventId,
    /// See [`NewEvent::event_type`].
    pub event_type: String,
    /// See [`NewEvent::schema_version`].
    pub schema_version: u32,
    /// See [`NewEvent::payload`].
    pub payload: Payload,
    /// The unit of work's [`crate::journal::UnitOfWork::recorded_at`].
    pub recorded_at: Millis,
}

/// Where a recorded event id lives: the event-id registry's answer (ADR 0021). An id is
/// bound to at most one place, anywhere in the journal.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EventLocation {
    /// The stream holding the event.
    pub stream: StreamId,
    /// Its revision there.
    pub revision: StreamRevision,
}

/// An event kind the journal holds at least one event of: the event-kind manifest, so the
/// unknown-kind check at start-up (safe mode, fnd §6.8) reads a short list, not every event.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RecordedKind {
    /// `EventKind::event_type`.
    pub event_type: String,
    /// `EventKind::schema_version`.
    pub schema_version: u32,
}

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReceiptStatus {
    /// `decide` accepted it and its events were appended in the same unit.
    Accepted,
    /// `decide` refused it; a refusal is deterministic, so its repeat returns the same one.
    Refused,
}

/// The receipt of one command (fnd §6.4): a repeat of `command_id` returns this, unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandReceipt {
    /// The command.
    pub command_id: CommandId,
    /// How it ended.
    pub status: ReceiptStatus,
    /// The encoded result the caller got, returned as is on a repeat.
    pub result: Payload,
}

/// A message id within its source (a scale-set message id, an observation id).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MessageId(String);

impl MessageId {
    /// Validates 1 to 256 bytes with no control character.
    ///
    /// # Errors
    /// [`JournalValueError::InvalidMessageId`] for anything else.
    pub fn new(value: &str) -> Result<Self, JournalValueError> {
        if (1..=256).contains(&value.len()) && !value.chars().any(char::is_control) {
            Ok(Self(value.to_owned()))
        } else {
            Err(JournalValueError::InvalidMessageId)
        }
    }

    /// The validated text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The inbound message a unit of work processed: recorded with the unit, so a redelivery is
/// refused as a duplicate (plan §3.2, §7.2).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InboxEntry {
    /// Where it came from.
    pub source: InboxSource,
    /// Its id within `source`.
    pub message_id: MessageId,
}

/// A projection's name, `[a-z][a-z0-9_]{0,63}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProjectionName(String);

impl ProjectionName {
    /// Validates `[a-z][a-z0-9_]{0,63}`.
    ///
    /// # Errors
    /// [`JournalValueError::InvalidProjectionName`] for anything else.
    pub fn new(value: &str) -> Result<Self, JournalValueError> {
        let mut bytes = value.bytes();
        let valid = value.len() <= 64
            && bytes.next().is_some_and(|b| b.is_ascii_lowercase())
            && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(JournalValueError::InvalidProjectionName)
        }
    }

    /// The validated text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One synchronous projection update, committed with the unit that caused it. A placeholder
/// shape: a keyed document per projection, which ES-04 replaces with projection specs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionWrite {
    /// The projection.
    pub projection: ProjectionName,
    /// The row's key within the projection.
    pub key: String,
    /// The new encoded row, or `None` to remove it.
    pub value: Option<Payload>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_refuse_stream_id_when_it_is_malformed() {
        // given
        let long = "a".repeat(129);
        let bad = [
            "",
            "-attempt",
            "Attempt-1",
            "attempt_1",
            "attempt 1",
            long.as_str(),
        ];

        // when
        let results: Vec<_> = bad.iter().map(|s| StreamId::new(s)).collect();

        // then
        assert!(
            results
                .iter()
                .all(|r| *r == Err(JournalValueError::InvalidStreamId))
        );
        assert_eq!(
            StreamId::new("attempt-00ff").map(|s| s.as_str().to_owned()),
            Ok("attempt-00ff".to_owned())
        );
    }

    #[test]
    fn should_refuse_message_id_when_it_is_empty_long_or_has_control_characters() {
        // given
        let bad = [String::new(), "x".repeat(257), "a\nb".to_owned()];

        // when
        let results: Vec<_> = bad.iter().map(|s| MessageId::new(s)).collect();

        // then
        assert!(
            results
                .iter()
                .all(|r| *r == Err(JournalValueError::InvalidMessageId))
        );
        assert!(MessageId::new("42").is_ok());
    }

    #[test]
    fn should_refuse_projection_name_when_it_is_malformed() {
        // given
        let long = "a".repeat(65);
        let bad = ["", "1attempts", "Attempts", "attempt-list", long.as_str()];

        // when
        let results: Vec<_> = bad.iter().map(|s| ProjectionName::new(s)).collect();

        // then
        assert!(
            results
                .iter()
                .all(|r| *r == Err(JournalValueError::InvalidProjectionName))
        );
        assert!(ProjectionName::new("pending_directives").is_ok());
    }

    #[test]
    fn should_stop_at_limit_when_revision_is_maximal() {
        // given
        let last = StreamRevision::new(u64::MAX);

        // when
        let next = last.next();

        // then
        assert_eq!(next, None);
        assert_eq!(StreamRevision::EMPTY.next(), Some(StreamRevision::new(1)));
    }
}
