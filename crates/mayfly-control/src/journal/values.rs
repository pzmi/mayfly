//! The values a unit of work carries and a read returns (`doc/event-store.md` §3.1). Payloads
//! stay encoded: the journal stores and returns them verbatim and never parses them, so this
//! port names no format (encoding is `AggregateStore`'s job).

use std::collections::BTreeMap;
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
    /// An aggregate name is not `[a-z][a-z0-9_]{0,63}`.
    InvalidAggregateName,
    /// A command type is not `[a-z][a-z0-9_]{0,63}`.
    InvalidCommandType,
}

impl fmt::Display for JournalValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidStreamId => "invalid stream id",
            Self::InvalidInboxSource => "invalid inbox source",
            Self::InvalidMessageId => "invalid message id",
            Self::InvalidProjectionName => "invalid projection name",
            Self::InvalidAggregateName => "invalid aggregate name",
            Self::InvalidCommandType => "invalid command type",
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

/// `[a-z][a-z0-9_]{0,63}`: the shape of every `snake_case` name the journal stores (event types,
/// aggregates, command types, projections), so both engines store it as plain ASCII text.
pub(crate) fn valid_snake_name(s: &str) -> bool {
    let mut bytes = s.bytes();
    s.len() <= 64
        && bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

macro_rules! text_type {
    ($(#[$doc:meta])* $name:ident, $valid:ident, $rule:literal, $err:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            #[doc = concat!("Validates `", $rule, "`.")]
            ///
            /// # Errors
            #[doc = concat!("[`JournalValueError::", stringify!($err), "`] for anything else.")]
            pub fn new(value: &str) -> Result<Self, JournalValueError> {
                if $valid(value) {
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

text_type!(
    /// A stream, `<aggregate>-<id>` by convention (plan §5.1): the unit of ordering and of the
    /// expected-revision check. Its `Ord` is byte order, which is the order
    /// [`crate::journal::Journal::streams`] enumerates in (`COLLATE "C"` on PostgreSQL, never a
    /// locale). Pinned by `should_refuse_stream_id_when_it_is_malformed`.
    StreamId,
    valid_label,
    "[a-z0-9-]{1,128}, not starting with -",
    InvalidStreamId
);
text_type!(
    /// Where an inbox message came from: a scale-set session or a data-plane host (plan §3.2,
    /// §7.2). Message ids are unique only within their source.
    InboxSource,
    valid_label,
    "[a-z0-9-]{1,128}, not starting with -",
    InvalidInboxSource
);
text_type!(
    /// A projection's name.
    ProjectionName,
    valid_snake_name,
    "[a-z][a-z0-9_]{0,63}",
    InvalidProjectionName
);
text_type!(
    /// An aggregate's name (`Aggregate::NAME`): what a stream head records and what a build's
    /// supported epoch is keyed by.
    AggregateName,
    valid_snake_name,
    "[a-z][a-z0-9_]{0,63}",
    InvalidAggregateName
);
text_type!(
    /// A command's type, recorded in its receipt.
    CommandType,
    valid_snake_name,
    "[a-z][a-z0-9_]{0,63}",
    InvalidCommandType
);

/// The revision of one event in its stream: the first event is revision 0, the next 1, and so
/// on with no hole (ES-I2). A stream with no events has no revision and no head: it is
/// [`ExpectedRevision::NoStream`], never "revision 0 with nothing in it".
///
/// Both engines store revisions as `i64`, so [`StreamRevision::MAX`] is `i64::MAX`; a unit
/// expecting more is malformed and a stream at the maximum takes no further event (pinned by
/// `should_stop_at_limit_when_revision_is_maximal`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StreamRevision(u64);

impl StreamRevision {
    /// The revision of a stream's first event.
    pub const FIRST: Self = Self(0);
    /// The highest revision either engine can store.
    pub const MAX: Self = Self(i64::MAX.unsigned_abs());

    /// Wraps a raw value; [`StreamRevision::is_storable`] says whether a store can hold it.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Whether the value is at most [`StreamRevision::MAX`].
    #[must_use]
    pub const fn is_storable(self) -> bool {
        self.0 <= Self::MAX.0
    }

    /// The revision after this one, or `None` past [`StreamRevision::MAX`] (never wraps).
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        if self.0 < Self::MAX.0 {
            Some(Self(self.0 + 1))
        } else {
            None
        }
    }
}

impl fmt::Display for StreamRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// What a writer (or a validated read) believes a stream's head is. There is no "any": every
/// write is a decision against a read state (`doc/event-store.md` §1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ExpectedRevision {
    /// The stream has no event yet.
    NoStream,
    /// The stream's last event is at this revision.
    Exactly(StreamRevision),
}

impl ExpectedRevision {
    /// What a head at `head` (or no head) satisfies.
    #[must_use]
    pub const fn of(head: Option<StreamRevision>) -> Self {
        match head {
            Some(revision) => Self::Exactly(revision),
            None => Self::NoStream,
        }
    }

    /// Whether a stream whose last revision is `head` (`None`: absent) is where this expects.
    #[must_use]
    pub fn matches(self, head: Option<StreamRevision>) -> bool {
        self == Self::of(head)
    }

    /// The revision the next appended event takes, or `None` past the maximum.
    #[must_use]
    pub const fn next_revision(self) -> Option<StreamRevision> {
        match self {
            Self::NoStream => Some(StreamRevision::FIRST),
            Self::Exactly(revision) => revision.next(),
        }
    }
}

impl fmt::Display for ExpectedRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoStream => f.write_str("no stream"),
            Self::Exactly(revision) => write!(f, "revision {revision}"),
        }
    }
}

/// A stream head: the one row a writer of the stream contends on (`doc/event-store.md` §2.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StreamHead {
    /// The stream.
    pub stream: StreamId,
    /// The revision of its last event.
    pub revision: StreamRevision,
    /// The aggregate that created it; its supported epoch judges later appends.
    pub aggregate: AggregateName,
    /// The highest `required_epoch` of any event it holds (1 when none asked for more). Never
    /// decreases (ES-I12).
    pub epoch: u32,
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

    /// Whether the text holds a NUL: a raw `U+0000`, or the JSON escape `\u0000` (an escaped
    /// backslash before `u0000` is not one). PostgreSQL's `jsonb` and `text` refuse both, so a
    /// unit carrying one is malformed on every store (JC-19; pinned by
    /// `should_find_nul_when_payload_holds_a_raw_or_escaped_one`).
    #[must_use]
    pub fn holds_nul(&self) -> bool {
        let mut bytes = self.0.bytes();
        while let Some(byte) = bytes.next() {
            match byte {
                0 => return true,
                b'\\' => {
                    let escaped = bytes.next();
                    let hex: Vec<u8> = bytes.clone().take(4).collect();
                    if escaped == Some(b'u') && hex == b"0000" {
                        return true;
                    }
                }
                _ => {}
            }
        }
        false
    }
}

/// An event's kind as the journal stores it: the codec's `EventKind`, as text, so the journal
/// can hold (and report) kinds this build does not know (`doc/event-store.md` §6.3).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StoredKind {
    /// `EventKind::event_type`, `[a-z][a-z0-9_]{0,63}`.
    pub event_type: String,
    /// `EventKind::schema_version`.
    pub schema_version: u32,
}

/// An event to append: its caller-derived id, its kind, the epoch it needs, and its payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEvent {
    /// Unique across the journal; a second append of the same id is refused (ES-I4).
    pub event_id: EventId,
    /// Its kind.
    pub kind: StoredKind,
    /// The lowest supported epoch a build needs to decide its stream once it is stored
    /// (`EventSet::required_epoch`, 1 by default); raises the stream's epoch (ES-I12).
    pub required_epoch: u32,
    /// The event struct's encoded form.
    pub payload: Payload,
}

/// An event as the journal recorded it. It has a place in its stream and nowhere else: the
/// journal keeps no global order (ADR 0021).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedEvent {
    /// Its stream.
    pub stream: StreamId,
    /// Its revision in the stream, from 0.
    pub revision: StreamRevision,
    /// See [`NewEvent::event_id`].
    pub event_id: EventId,
    /// See [`NewEvent::kind`].
    pub kind: StoredKind,
    /// See [`NewEvent::payload`].
    pub payload: Payload,
    /// The unit of work's [`crate::journal::UnitOfWork::recorded_at`]; informational only,
    /// nothing orders or decides by it (ADR 0020).
    pub recorded_at: Millis,
}

/// Where a recorded event id lives: the event-id registry's answer (ES-I4). An id is bound to
/// at most one place, anywhere in the journal.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EventLocation {
    /// The stream holding the event.
    pub stream: StreamId,
    /// Its revision there.
    pub revision: StreamRevision,
}

/// The canonical digest of a command's body (`doc/event-store.md` §3.1): what tells a repeat of
/// a command id from a reuse of it with another intent (ES-I5). Computed by `AggregateStore`
/// (ES-03); the journal only compares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IntentDigest([u8; 32]);

impl IntentDigest {
    /// Wraps a digest.
    #[must_use]
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The digest's bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for IntentDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

/// A message id within its source (a scale-set message id, an observation id).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MessageId(String);

impl MessageId {
    /// Validates 1 to 256 bytes with no control character (so no NUL either).
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

/// The inbound message a unit of work consumed: recorded with the unit, so a redelivery is
/// refused as a duplicate (ES-I6, plan §3.2, §7.2).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InboxKey {
    /// Where it came from.
    pub source: InboxSource,
    /// Its id within `source`.
    pub message_id: MessageId,
}

/// A stream a decision read without writing, with the revision it read: the unit is refused
/// if the stream moved (a `NoStream` read is invalidated by a concurrent create).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReadCondition {
    /// The stream read.
    pub stream: StreamId,
    /// What it was at.
    pub expected: ExpectedRevision,
}

/// This build's supported epoch per aggregate (`Aggregate::SUPPORTED_EPOCH`, ES-03): the
/// journal refuses a batch that needs more and a stream that already holds more (ES-I12). An
/// aggregate this build does not list supports epoch 0, so every append to it is refused.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SupportedEpochs(BTreeMap<AggregateName, u32>);

impl SupportedEpochs {
    /// No aggregate supported.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds (or replaces) `aggregate`'s supported epoch.
    #[must_use]
    pub fn with(mut self, aggregate: AggregateName, epoch: u32) -> Self {
        self.0.insert(aggregate, epoch);
        self
    }

    /// `aggregate`'s supported epoch, 0 when this build does not know it.
    #[must_use]
    pub fn of(&self, aggregate: &AggregateName) -> u32 {
        self.0.get(aggregate).copied().unwrap_or(0)
    }
}

/// How the command a receipt records was decided, as the writer supplies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// `decide` accepted it: the unit's appends (possibly none) are its events.
    Accepted,
    /// `decide` refused it. The unit carries no append and exactly one read, the stream and
    /// revision it was decided at, which the refusal receipt is bound to (E3, ADR 0034).
    Refused {
        /// The encoded refusal (codes and ids, never free text): what a repeat at the same
        /// revision is answered with.
        rejection: Payload,
    },
}

/// The receipt a unit records for the command that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewReceipt {
    /// The command.
    pub command_id: CommandId,
    /// Its type.
    pub command_type: CommandType,
    /// Its canonical intent digest.
    pub intent: IntentDigest,
    /// How it was decided.
    pub decision: Decision,
}

/// What a stored receipt answers a repeat with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptOutcome {
    /// Accepted, with what its unit appended; answers every repeat for ever.
    Accepted(crate::journal::Appended),
    /// Refused while `bound` held; answers a repeat only while the stream is still there. Once
    /// it moved, the command is decided afresh and its outcome replaces this receipt (E3).
    Refused {
        /// The stream and revision the refusal was decided at.
        bound: ReadCondition,
        /// The encoded refusal.
        rejection: Payload,
    },
}

/// The receipt of one command (fnd §6.4, ES-I5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandReceipt {
    /// The command.
    pub command_id: CommandId,
    /// Its type.
    pub command_type: CommandType,
    /// Its canonical intent digest.
    pub intent: IntentDigest,
    /// Its recorded outcome.
    pub outcome: ReceiptOutcome,
}

/// One inline projection row write, committed with the unit that caused it (ES-I9). A
/// placeholder shape until ES-04's projection specs and generations: a keyed document per
/// projection, owned by one stream the unit appends to (ES-I10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionWrite {
    /// The projection.
    pub projection: ProjectionName,
    /// The stream the row belongs to; it must be one the unit appends to.
    pub stream: StreamId,
    /// The row's key within the projection: 1 to 256 bytes, no control character.
    pub key: String,
    /// The new encoded row, or `None` to remove it.
    pub row: Option<Payload>,
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
            "attempt\u{0}1",
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
        let bad = [
            String::new(),
            "x".repeat(257),
            "a\nb".to_owned(),
            "a\u{0}b".to_owned(),
        ];

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
    fn should_refuse_snake_names_when_they_are_malformed() {
        // given
        let long = "a".repeat(65);
        let bad = ["", "1attempts", "Attempts", "attempt-list", long.as_str()];

        // when
        let projections: Vec<_> = bad.iter().map(|s| ProjectionName::new(s)).collect();
        let aggregates: Vec<_> = bad.iter().map(|s| AggregateName::new(s)).collect();
        let commands: Vec<_> = bad.iter().map(|s| CommandType::new(s)).collect();

        // then
        assert!(
            projections
                .iter()
                .all(|r| *r == Err(JournalValueError::InvalidProjectionName))
        );
        assert!(
            aggregates
                .iter()
                .all(|r| *r == Err(JournalValueError::InvalidAggregateName))
        );
        assert!(
            commands
                .iter()
                .all(|r| *r == Err(JournalValueError::InvalidCommandType))
        );
        assert!(ProjectionName::new("pending_effects").is_ok());
        assert!(AggregateName::new("attempt").is_ok());
        assert!(CommandType::new("start_attempt").is_ok());
    }

    #[test]
    fn should_stop_at_limit_when_revision_is_maximal() {
        // given
        let last = StreamRevision::MAX;

        // when
        let next = last.next();

        // then
        assert_eq!(next, None);
        assert_eq!(last.get(), 9_223_372_036_854_775_807);
        assert!(last.is_storable());
        assert!(!StreamRevision::new(last.get() + 1).is_storable());
        assert_eq!(
            ExpectedRevision::NoStream.next_revision(),
            Some(StreamRevision::FIRST)
        );
        assert_eq!(
            ExpectedRevision::Exactly(StreamRevision::new(4)).next_revision(),
            Some(StreamRevision::new(5))
        );
        assert_eq!(ExpectedRevision::Exactly(last).next_revision(), None);
    }

    #[test]
    fn should_match_expected_revision_when_head_is_where_it_says() {
        // given
        let at_two = ExpectedRevision::Exactly(StreamRevision::new(2));

        // when
        let matches = [
            at_two.matches(Some(StreamRevision::new(2))),
            at_two.matches(Some(StreamRevision::new(3))),
            at_two.matches(None),
            ExpectedRevision::NoStream.matches(None),
            ExpectedRevision::NoStream.matches(Some(StreamRevision::FIRST)),
        ];

        // then
        assert_eq!(matches, [true, false, false, true, false]);
    }

    #[test]
    fn should_find_nul_when_payload_holds_a_raw_or_escaped_one() {
        // given
        let with_nul = ["a\u{0}b", "{\"n\":\"\\u0000\"}", "\\u0000"];
        let without = [
            "{\"n\":\"\\\\u0000\"}",
            "{\"n\":\"\\u0001\"}",
            "{\"n\":\"\\u000\"}",
            "zażółć ✓",
            "",
            "\\",
        ];

        // when
        let found: Vec<bool> = with_nul
            .iter()
            .map(|s| Payload::new(*s).holds_nul())
            .collect();
        let clean: Vec<bool> = without
            .iter()
            .map(|s| Payload::new(*s).holds_nul())
            .collect();

        // then
        assert!(found.iter().all(|f| *f), "{found:?}");
        assert!(clean.iter().all(|f| !*f), "{clean:?}");
    }

    #[test]
    fn should_report_supported_epoch_zero_when_build_does_not_know_the_aggregate() {
        // given
        let attempt = AggregateName::new("attempt").unwrap_or_else(|e| panic!("{e}"));
        let host = AggregateName::new("host").unwrap_or_else(|e| panic!("{e}"));
        let epochs = SupportedEpochs::new().with(attempt.clone(), 2);

        // when
        let known = epochs.of(&attempt);
        let unknown = epochs.of(&host);

        // then
        assert_eq!((known, unknown), (2, 0));
    }

    #[test]
    fn should_print_intent_digest_as_hex_when_displayed() {
        // given
        let mut bytes = [0_u8; 32];
        bytes[0] = 0xab;
        bytes[31] = 0x01;

        // when
        let text = IntentDigest::new(bytes).to_string();

        // then
        assert_eq!(text.len(), 64);
        assert!(text.starts_with("ab00"));
        assert!(text.ends_with("0001"));
    }
}
