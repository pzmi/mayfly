//! Reading and encoding the journal's rows: one statement per read (`doc/event-store.md` §3.7),
//! values checked on the way out so a corrupt row is a fixed error, never a panic or a guess.
//!
//! Ids are 32-hex `TEXT`, revisions and epochs `INTEGER` (`i64`, `doc/event-store.md` §4.3
//! rule 6), payloads `TEXT` returned byte for byte. A receipt's outcome is a small JSON document
//! this module alone writes and reads (§4.1 `command_receipt.outcome`):
//! `{"accepted":[{"stream":…,"first":…,"last":…,"events":[…]}]}` or
//! `{"refused":{"bound":[{"stream":…,"expected":null|n},…],"rejection":…}}` (the bound reads
//! in `StreamId` order, ADR 0041). The engine never parses it: no
//! JSON function of either engine is on a correctness path (§4.3 rule 4).

use mayfly_control::journal::{
    AggregateName, Appended, AppendedStream, CommandReceipt, CommandType, EventLocation,
    ExpectedRevision, InboxKey, IntentDigest, Payload, ProjectionName, ReadCondition,
    ReceiptOutcome, RecordedEvent, StorageError, StoredKind, StreamHead, StreamId, StreamRevision,
};
use mayfly_domain::{CommandId, EventId, Millis};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::classify::{Failure, classify};

/// The generation every inline projection row lives in until ES-04 brings generations.
pub(crate) const GENERATION: i64 = 0;

/// A row that does not decode: a fixed code, never the value.
pub(crate) const fn unreadable() -> Failure {
    Failure::Storage(StorageError::new("row_unreadable"))
}

fn engine(error: &rusqlite::Error) -> Failure {
    classify(error)
}

/// A revision as both engines store it.
pub(crate) fn sql_revision(revision: StreamRevision) -> Result<i64, Failure> {
    i64::try_from(revision.get())
        .map_err(|_| Failure::Storage(StorageError::new("revision_exhausted")))
}

pub(crate) fn revision_of(value: i64) -> Result<StreamRevision, Failure> {
    u64::try_from(value)
        .map(StreamRevision::new)
        .map_err(|_| unreadable())
}

fn u32_of(value: i64) -> Result<u32, Failure> {
    u32::try_from(value).map_err(|_| unreadable())
}

/// Milliseconds as both engines store them; above `i64::MAX` is not a reading any clock gives.
pub(crate) fn sql_millis(at: Millis) -> Result<i64, Failure> {
    i64::try_from(at.get()).map_err(|_| Failure::Storage(StorageError::new("instant_out_of_range")))
}

pub(crate) fn millis_of(value: i64) -> Result<Millis, Failure> {
    u64::try_from(value)
        .map(Millis::new)
        .map_err(|_| unreadable())
}

pub(crate) fn stream_of(text: &str) -> Result<StreamId, Failure> {
    StreamId::new(text).map_err(|_| unreadable())
}

fn event_id_of(text: &str) -> Result<EventId, Failure> {
    text.parse().map_err(|_| unreadable())
}

/// The digest as 64 lowercase hex digits.
pub(crate) fn intent_text(intent: &IntentDigest) -> String {
    intent.to_string()
}

fn intent_of(text: &str) -> Result<IntentDigest, Failure> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return Err(unreadable());
    }
    let mut digest = [0_u8; 32];
    for (out, pair) in digest.iter_mut().zip(bytes.chunks(2)) {
        let pair = std::str::from_utf8(pair).map_err(|_| unreadable())?;
        *out = u8::from_str_radix(pair, 16).map_err(|_| unreadable())?;
    }
    Ok(IntentDigest::new(digest))
}

/// A stream head row: `None` when the stream has no event.
pub(crate) fn head(conn: &Connection, stream: &StreamId) -> Result<Option<StreamHead>, Failure> {
    let row = conn
        .prepare_cached(
            "SELECT revision, aggregate, required_epoch FROM stream_head WHERE stream_id = ?1",
        )
        .and_then(|mut s| {
            s.query_row(params![stream.as_str()], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .optional()
        })
        .map_err(|e| engine(&e))?;
    row.map(|(revision, aggregate, epoch)| {
        Ok(StreamHead {
            stream: stream.clone(),
            revision: revision_of(revision)?,
            aggregate: AggregateName::new(&aggregate).map_err(|_| unreadable())?,
            epoch: u32_of(epoch)?,
        })
    })
    .transpose()
}

/// The events of `stream` after `after` and through `through`, in revision order (one stream:
/// ordering by its revision is per stream, never across streams; ES-I17).
pub(crate) fn read_stream(
    conn: &Connection,
    stream: &StreamId,
    after: Option<StreamRevision>,
    through: Option<StreamRevision>,
) -> Result<Vec<RecordedEvent>, Failure> {
    let after = after.map_or(Ok(-1), sql_revision).unwrap_or(i64::MAX);
    let through = through
        .map_or(Ok(i64::MAX), sql_revision)
        .unwrap_or(i64::MAX);
    let rows = conn
        .prepare_cached(
            "SELECT revision, event_id, event_type, schema_version, payload, recorded_at \
             FROM journal_event WHERE stream_id = ?1 AND revision > ?2 AND revision <= ?3 \
             ORDER BY revision",
        )
        .and_then(|mut s| {
            s.query_map(params![stream.as_str(), after, through], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|e| engine(&e))?;
    rows.into_iter()
        .map(|(revision, id, event_type, version, payload, at)| {
            Ok(RecordedEvent {
                stream: stream.clone(),
                revision: revision_of(revision)?,
                event_id: event_id_of(&id)?,
                kind: StoredKind {
                    event_type,
                    schema_version: u32_of(version)?,
                },
                payload: Payload::new(payload),
                recorded_at: millis_of(at)?,
            })
        })
        .collect()
}

/// Where `event_id` lives, if it is recorded.
pub(crate) fn lookup_event(
    conn: &Connection,
    event_id: EventId,
) -> Result<Option<EventLocation>, Failure> {
    let row = conn
        .prepare_cached("SELECT stream_id, revision FROM journal_identity WHERE event_id = ?1")
        .and_then(|mut s| {
            s.query_row(params![event_id.to_string()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .optional()
        })
        .map_err(|e| engine(&e))?;
    row.map(|(stream, revision)| {
        Ok(EventLocation {
            stream: stream_of(&stream)?,
            revision: revision_of(revision)?,
        })
    })
    .transpose()
}

/// A stored receipt with its outcome's text exactly as stored (what a compare-and-set of a
/// stale refusal compares against).
pub(crate) struct StoredReceipt {
    pub(crate) receipt: CommandReceipt,
    pub(crate) outcome_text: String,
}

/// The receipt of `command_id`, if one is stored.
pub(crate) fn receipt(
    conn: &Connection,
    command_id: CommandId,
) -> Result<Option<StoredReceipt>, Failure> {
    let row = conn
        .prepare_cached(
            "SELECT command_type, intent, outcome FROM command_receipt WHERE command_id = ?1",
        )
        .and_then(|mut s| {
            s.query_row(params![command_id.to_string()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .optional()
        })
        .map_err(|e| engine(&e))?;
    row.map(|(command_type, intent, outcome_text)| {
        Ok(StoredReceipt {
            receipt: CommandReceipt {
                command_id,
                command_type: CommandType::new(&command_type).map_err(|_| unreadable())?,
                intent: intent_of(&intent)?,
                outcome: decode_outcome(&outcome_text).ok_or_else(unreadable)?,
            },
            outcome_text,
        })
    })
    .transpose()
}

/// Whether a committed unit consumed `key`.
pub(crate) fn inbox_contains(conn: &Connection, key: &InboxKey) -> Result<bool, Failure> {
    conn.prepare_cached("SELECT 1 FROM inbox WHERE source = ?1 AND message_id = ?2")
        .and_then(|mut s| s.exists(params![key.source.as_str(), key.message_id.as_str()]))
        .map_err(|e| engine(&e))
}

/// Up to `limit` heads with an id above `after`, by key in byte order (`BINARY` collation is
/// `memcmp`), never by time or position.
pub(crate) fn streams(
    conn: &Connection,
    after: Option<&StreamId>,
    limit: u32,
) -> Result<Vec<StreamHead>, Failure> {
    let rows = conn
        .prepare_cached(
            "SELECT stream_id, revision, aggregate, required_epoch FROM stream_head \
             WHERE ?1 IS NULL OR stream_id > ?1 ORDER BY stream_id LIMIT ?2",
        )
        .and_then(|mut s| {
            s.query_map(
                params![after.map(StreamId::as_str), i64::from(limit)],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|e| engine(&e))?;
    rows.into_iter()
        .map(|(stream, revision, aggregate, epoch)| {
            Ok(StreamHead {
                stream: stream_of(&stream)?,
                revision: revision_of(revision)?,
                aggregate: AggregateName::new(&aggregate).map_err(|_| unreadable())?,
                epoch: u32_of(epoch)?,
            })
        })
        .collect()
}

/// Every kind with a committed event, ascending (type, then schema version).
pub(crate) fn stored_kinds(conn: &Connection) -> Result<Vec<StoredKind>, Failure> {
    let rows = conn
        .prepare_cached(
            "SELECT event_type, schema_version FROM event_kind_manifest \
             ORDER BY event_type, schema_version",
        )
        .and_then(|mut s| {
            s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|e| engine(&e))?;
    rows.into_iter()
        .map(|(event_type, version)| {
            Ok(StoredKind {
                event_type,
                schema_version: u32_of(version)?,
            })
        })
        .collect()
}

/// The projection row `key` of `projection`.
pub(crate) fn projection(
    conn: &Connection,
    projection: &ProjectionName,
    key: &str,
) -> Result<Option<Payload>, Failure> {
    conn.prepare_cached(
        "SELECT payload FROM projection_row WHERE spec = ?1 AND generation = ?2 AND key = ?3",
    )
    .and_then(|mut s| {
        s.query_row(params![projection.as_str(), GENERATION, key], |r| {
            r.get::<_, String>(0)
        })
        .optional()
    })
    .map(|row| row.map(Payload::new))
    .map_err(|e| engine(&e))
}

/// The stored form of a receipt's outcome (module docs).
pub(crate) fn encode_outcome(outcome: &ReceiptOutcome) -> String {
    match outcome {
        ReceiptOutcome::Accepted(appended) => json!({
            "accepted": appended.streams.iter().map(|s| json!({
                "stream": s.stream.as_str(),
                "first": s.first.get(),
                "last": s.last.get(),
                "events": s.event_ids.iter().map(ToString::to_string).collect::<Vec<_>>(),
            })).collect::<Vec<_>>()
        }),
        ReceiptOutcome::Refused { bound, rejection } => json!({
            "refused": {
                "bound": bound.iter().map(|read| json!({
                    "stream": read.stream.as_str(),
                    "expected": match read.expected {
                        ExpectedRevision::NoStream => Value::Null,
                        ExpectedRevision::Exactly(r) => json!(r.get()),
                    },
                })).collect::<Vec<_>>(),
                "rejection": rejection.as_str(),
            }
        }),
    }
    .to_string()
}

/// The outcome `text` holds, or `None` when it is not one [`encode_outcome`] writes.
pub(crate) fn decode_outcome(text: &str) -> Option<ReceiptOutcome> {
    let value: Value = serde_json::from_str(text).ok()?;
    if let Some(streams) = value.get("accepted") {
        let streams = streams
            .as_array()?
            .iter()
            .map(|s| {
                Some(AppendedStream {
                    stream: StreamId::new(s.get("stream")?.as_str()?).ok()?,
                    first: StreamRevision::new(s.get("first")?.as_u64()?),
                    last: StreamRevision::new(s.get("last")?.as_u64()?),
                    event_ids: s
                        .get("events")?
                        .as_array()?
                        .iter()
                        .map(|id| id.as_str()?.parse().ok())
                        .collect::<Option<Vec<_>>>()?,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        return Some(ReceiptOutcome::Accepted(Appended { streams }));
    }
    let refused = value.get("refused")?;
    let bound = refused
        .get("bound")?
        .as_array()?
        .iter()
        .map(|read| {
            let expected = match read.get("expected")? {
                Value::Null => ExpectedRevision::NoStream,
                n => ExpectedRevision::Exactly(StreamRevision::new(n.as_u64()?)),
            };
            Some(ReadCondition {
                stream: StreamId::new(read.get("stream")?.as_str()?).ok()?,
                expected,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    if bound.is_empty() {
        return None;
    }
    Some(ReceiptOutcome::Refused {
        bound,
        rejection: Payload::new(refused.get("rejection")?.as_str()?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(name: &str) -> StreamId {
        StreamId::new(name).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn should_round_trip_receipt_outcomes_when_encoded_and_decoded() {
        // given
        let outcomes = [
            ReceiptOutcome::Accepted(Appended::default()),
            ReceiptOutcome::Accepted(Appended {
                streams: vec![AppendedStream {
                    stream: stream("attempt-1"),
                    first: StreamRevision::new(3),
                    last: StreamRevision::new(4),
                    event_ids: vec![EventId::from_u128(1), EventId::from_u128(u128::MAX)],
                }],
            }),
            ReceiptOutcome::Refused {
                bound: vec![ReadCondition {
                    stream: stream("pool-1"),
                    expected: ExpectedRevision::NoStream,
                }],
                rejection: Payload::new("{\"code\":\"zażółć\\\"\"}"),
            },
            ReceiptOutcome::Refused {
                bound: vec![
                    ReadCondition {
                        stream: stream("attempt-1"),
                        expected: ExpectedRevision::Exactly(StreamRevision::new(2)),
                    },
                    ReadCondition {
                        stream: stream("pool-1"),
                        expected: ExpectedRevision::Exactly(StreamRevision::MAX),
                    },
                ],
                rejection: Payload::new(""),
            },
        ];

        // when
        let decoded: Vec<Option<ReceiptOutcome>> = outcomes
            .iter()
            .map(|o| decode_outcome(&encode_outcome(o)))
            .collect();

        // then
        let want: Vec<Option<ReceiptOutcome>> = outcomes.into_iter().map(Some).collect();
        assert_eq!(decoded, want);
    }

    #[test]
    fn should_refuse_to_decode_when_an_outcome_is_not_one_this_module_writes() {
        // given
        let texts = [
            "",
            "[]",
            "{}",
            "{\"accepted\":{}}",
            "{\"accepted\":[{\"stream\":\"Bad\",\"first\":0,\"last\":0,\"events\":[]}]}",
            "{\"accepted\":[{\"stream\":\"a-1\",\"first\":0,\"last\":0,\"events\":[\"x\"]}]}",
            "{\"refused\":{\"bound\":[{\"stream\":\"a-1\",\"expected\":-1}],\"rejection\":\"\"}}",
            "{\"refused\":{\"bound\":[{\"stream\":\"a-1\",\"expected\":null}]}}",
            "{\"refused\":{\"bound\":[],\"rejection\":\"\"}}",
            "{\"refused\":{\"stream\":\"a-1\",\"expected\":null,\"rejection\":\"\"}}",
        ];

        // when
        let decoded: Vec<Option<ReceiptOutcome>> =
            texts.iter().map(|t| decode_outcome(t)).collect();

        // then
        assert!(decoded.iter().all(Option::is_none), "{decoded:?}");
    }

    #[test]
    fn should_round_trip_intent_digest_when_stored_as_hex() {
        // given
        let mut bytes = [0_u8; 32];
        bytes[0] = 0xab;
        bytes[31] = 0x0f;
        let digest = IntentDigest::new(bytes);

        // when
        let back = intent_of(&intent_text(&digest));

        // then
        assert_eq!(back, Ok(digest));
        assert_eq!(intent_of("ab"), Err(unreadable()));
        assert_eq!(intent_of(&"zz".repeat(32)), Err(unreadable()));
    }

    #[test]
    fn should_refuse_values_when_they_do_not_fit_the_port() {
        // given
        let negative = -1_i64;

        // when
        let results = (
            revision_of(negative),
            u32_of(i64::from(u32::MAX) + 1),
            millis_of(negative),
            sql_revision(StreamRevision::new(u64::MAX)),
            sql_millis(Millis::new(u64::MAX)),
        );

        // then
        assert_eq!(results.0, Err(unreadable()));
        assert_eq!(results.1, Err(unreadable()));
        assert_eq!(results.2, Err(unreadable()));
        assert_eq!(
            results.3,
            Err(Failure::Storage(StorageError::new("revision_exhausted")))
        );
        assert_eq!(
            results.4,
            Err(Failure::Storage(StorageError::new("instant_out_of_range")))
        );
    }
}
