//! The pure functions `AggregateStore` derives its stored values with: the stream of an
//! aggregate instance, the canonical intent digest of a command, the event id of a decided
//! event, and the encodings of events and refusals (`doc/event-store.md` §3.7 step 4).
//!
//! The digest and the ids are what make a retry idempotent across processes and builds: a
//! command re-sent after a lost reply derives the same intent and the same event ids, so the
//! journal answers it from its receipt or finds its events where they were written. Both are
//! golden-tested (`tests/store/codec.rs`), because changing either would make a retry after an
//! upgrade look like a new command.

use mayfly_control_domain::{Aggregate, CommandBody, DecodeError, EventSet, Rejection};
use mayfly_domain::{CommandId, EventId};
use sha2::{Digest, Sha256};

use crate::journal::{
    AggregateName, CommandType, IntentDigest, JournalValueError, NewEvent, Payload, RecordedEvent,
    StoredKind, StreamId,
};

const INTENT_DOMAIN: &[u8] = b"mayfly-intent-v1\n";
const EVENT_ID_DOMAIN: &[u8] = b"mayfly-event-id-v1\n";

/// The stream of instance `instance` of aggregate `A`: `<prefix>-<instance>`, where the prefix
/// is `A::NAME` with `_` written `-` (stream ids are `[a-z0-9-]`). Two aggregates whose prefixes
/// overlap (`host` and `host_run`) could name one stream, so `AggregateStore` also checks the
/// aggregate recorded in the head (`SaveError::AggregateMismatch`).
///
/// # Errors
/// [`JournalValueError::InvalidStreamId`] when the result is not a valid stream id.
pub fn stream_of<A: Aggregate>(instance: &str) -> Result<StreamId, JournalValueError> {
    StreamId::new(&format!("{}-{instance}", A::NAME.replace('_', "-")))
}

/// The canonical intent digest of a command (ES-I5): SHA-256 over a domain tag, the aggregate,
/// the command type, the target stream and the body's serde form as compact JSON, each part
/// newline-separated (the names cannot hold a newline and the body comes last). The command
/// id, `issued_at`, the run epoch and the actor are not part of the intent: a retry re-reads
/// the clock, and the receipt is keyed by the command id already.
///
/// The body's JSON is canonical for the types commands use: struct fields in declaration
/// order, enums externally tagged, maps only as ordered `BTreeMap`s (pure crates may not hold a
/// `HashMap`), no floats. A body whose shape changes changes its digest, so a command retried
/// across such an upgrade is refused `CommandIdReused` rather than replayed: a shape change of a
/// command is a new command type, as for events. Pinned by
/// `tests/store/codec.rs::should_match_the_intents_golden_when_registry_cases_are_digested`.
///
/// # Errors
/// The serializer's error when the body does not serialise.
pub fn intent_digest<B: CommandBody>(
    aggregate: &AggregateName,
    command_type: &CommandType,
    stream: &StreamId,
    body: &B,
) -> Result<IntentDigest, serde_json::Error> {
    let mut hasher = Sha256::new();
    hasher.update(INTENT_DOMAIN);
    for part in [aggregate.as_str(), command_type.as_str(), stream.as_str()] {
        hasher.update(part.as_bytes());
        hasher.update(b"\n");
    }
    hasher.update(serde_json::to_vec(body)?);
    Ok(IntentDigest::new(hasher.finalize().into()))
}

/// The id of the `ordinal`-th event (from 0) a command decided: the first 128 bits of SHA-256
/// over a domain tag, the command id and the ordinal, big-endian. A retried command derives the
/// same ids, so its re-append finds them where the first run wrote them (`lookup_event`), and
/// two commands never share one unless their ids collide (pinned by
/// `tests/store/codec.rs::should_derive_the_golden_event_ids_when_a_command_id_is_given`).
#[must_use]
pub fn derive_event_id(command_id: CommandId, ordinal: u32) -> EventId {
    let mut hasher = Sha256::new();
    hasher.update(EVENT_ID_DOMAIN);
    hasher.update(command_id.as_u128().to_be_bytes());
    hasher.update(ordinal.to_be_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    let mut head = [0_u8; 16];
    head.copy_from_slice(&digest[..16]);
    EventId::from_u128(u128::from_be_bytes(head))
}

/// `event` as the journal stores it: its derived id, its kind, the epoch it needs and its
/// payload as compact JSON.
///
/// # Errors
/// The serializer's error when the event does not serialise.
pub fn encode_event<E: EventSet>(
    event: &E,
    event_id: EventId,
) -> Result<NewEvent, serde_json::Error> {
    let mut out = Vec::new();
    event.encode(&mut serde_json::Serializer::new(&mut out))?;
    let kind = event.kind();
    Ok(NewEvent {
        event_id,
        kind: StoredKind {
            event_type: kind.event_type.to_owned(),
            schema_version: kind.schema_version,
        },
        required_epoch: event.required_epoch(),
        payload: Payload::new(String::from_utf8_lossy(&out).into_owned()),
    })
}

/// Why a recorded event did not decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventDecodeError {
    /// This build does not know the kind: safe mode (`doc/event-store.md` §6.3).
    UnknownKind,
    /// The kind is known but the payload does not have its shape.
    Malformed,
}

/// Decodes a recorded event through its aggregate's codec, upcasting a legacy kind; the whole
/// payload must be consumed.
///
/// # Errors
/// [`EventDecodeError`].
pub fn decode_event<E: EventSet>(recorded: &RecordedEvent) -> Result<E, EventDecodeError> {
    let mut deserializer = serde_json::Deserializer::from_str(recorded.payload.as_str());
    let event = E::decode(
        &recorded.kind.event_type,
        recorded.kind.schema_version,
        &mut deserializer,
    )
    .map_err(|e| match e {
        DecodeError::UnknownKind { .. } => EventDecodeError::UnknownKind,
        DecodeError::Malformed(_) => EventDecodeError::Malformed,
    })?;
    deserializer
        .end()
        .map_err(|_| EventDecodeError::Malformed)?;
    Ok(event)
}

/// A refusal as a refusal receipt stores it: its serde form as compact JSON (for a fieldless
/// rejection, its code).
///
/// # Errors
/// The serializer's error when the refusal does not serialise.
pub fn encode_rejection<R: Rejection>(rejection: &R) -> Result<Payload, serde_json::Error> {
    serde_json::to_string(rejection).map(Payload::new)
}

/// A refusal read back from its receipt.
///
/// # Errors
/// The deserializer's error when the payload is not a refusal of this type (a refusal a newer
/// build recorded with a code this one does not know).
pub fn decode_rejection<R: Rejection>(payload: &Payload) -> Result<R, serde_json::Error> {
    serde_json::from_str(payload.as_str())
}
