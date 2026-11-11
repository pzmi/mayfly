//! Folding a stream through a projection, and the inline writes of a unit (`doc/event-store.md`
//! §3.6, §3.7 step 4).

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;

use mayfly_control_domain::{Aggregate, EventKind, EventSet};
use sha2::{Digest, Sha256};

use crate::journal::{
    AggregateName, JournalValueError, MAX_ROW_KEY, Payload, ProjectionName, ProjectionWrite,
    RecordedEvent, RowWrite, StreamId,
};
use crate::projection::spec::{EventMeta, ProjectionSpec, Rows};
use crate::store::{EventDecodeError, decode_event};

/// One stream's encoded rows of a projection, by full key (`<stream>` or `<stream>/<suffix>`).
pub type EncodedRows = BTreeMap<String, Payload>;

/// Why a stream could not be folded through a projection.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProjectionError {
    /// A spec's name is not a projection name, or two specs share one.
    InvalidSpec {
        /// The spec's name as written.
        name: &'static str,
    },
    /// An event of the stream does not decode: an unknown kind (safe mode, §6.3) or a payload
    /// of the wrong shape. Nothing is folded past it.
    Decode {
        /// The stream.
        stream: StreamId,
        /// What failed: `unknown_kind` or `malformed`.
        what: &'static str,
    },
    /// A row did not encode, or a suffix holds a control character.
    Row {
        /// The projection.
        projection: &'static str,
        /// The stream.
        stream: StreamId,
    },
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSpec { name } => write!(f, "projection spec {name:?} is not usable"),
            Self::Decode { stream, what } => {
                write!(f, "stream {stream} does not fold: {what} event")
            }
            Self::Row { projection, stream } => {
                write!(f, "projection {projection} wrote a bad row for {stream}")
            }
        }
    }
}

impl std::error::Error for ProjectionError {}

/// A [`ProjectionSpec`] with its types erased, so specs of different aggregates sit in one
/// [`Projections`] list (a function pointer per spec; no trait object, D8).
#[derive(Clone)]
pub struct Projection {
    name: ProjectionName,
    spec: &'static str,
    version: u32,
    aggregate: AggregateName,
    subscribes: &'static [EventKind],
    fold: fn(&'static str, &StreamId, &[RecordedEvent]) -> Result<EncodedRows, ProjectionError>,
}

impl fmt::Debug for Projection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Projection")
            .field("name", &self.name)
            .field("version", &self.version)
            .field("aggregate", &self.aggregate)
            .finish_non_exhaustive()
    }
}

impl Projection {
    /// The spec `S`.
    ///
    /// # Errors
    /// [`ProjectionError::InvalidSpec`] when its name or its aggregate's name is not a journal
    /// name, or its version is 0.
    pub fn of<S: ProjectionSpec>() -> Result<Self, ProjectionError> {
        let invalid = |_: JournalValueError| ProjectionError::InvalidSpec { name: S::NAME };
        if S::VERSION == 0 {
            return Err(ProjectionError::InvalidSpec { name: S::NAME });
        }
        Ok(Self {
            name: ProjectionName::new(S::NAME).map_err(invalid)?,
            spec: S::NAME,
            version: S::VERSION,
            aggregate: AggregateName::new(<S::Source as Aggregate>::NAME).map_err(invalid)?,
            subscribes: S::SUBSCRIBES,
            fold: fold_spec::<S>,
        })
    }

    /// Its name.
    #[must_use]
    pub const fn name(&self) -> &ProjectionName {
        &self.name
    }

    /// Its version.
    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    /// The aggregate whose streams it reads.
    #[must_use]
    pub const fn aggregate(&self) -> &AggregateName {
        &self.aggregate
    }

    /// The event kinds it applies.
    #[must_use]
    pub const fn subscribes(&self) -> &'static [EventKind] {
        self.subscribes
    }

    /// `stream`'s rows after folding `events` (the stream from revision 0) through it: what a
    /// rebuild writes for the stream.
    ///
    /// # Errors
    /// [`ProjectionError::Decode`] or [`ProjectionError::Row`].
    pub fn fold(
        &self,
        stream: &StreamId,
        events: &[RecordedEvent],
    ) -> Result<EncodedRows, ProjectionError> {
        (self.fold)(self.spec, stream, events)
    }

    /// The rows a unit writes for `stream`: **every** row of the fold of `history` (the stream
    /// at the unit's expected revision) then `batch` (the events the unit appends), which the
    /// journal puts in place of the stream's stored rows (ES-04F, [`ProjectionWrite`]).
    ///
    /// The fold starts at revision 0 from the events the decision was made on, so the rows the
    /// unit writes are exactly those at its expected revision plus its batch, whatever another
    /// unit wrote meanwhile; that unit moved the head, so this one is refused
    /// `WrongExpectedRevision` and decided again (the model's `rowAfter`; a fold from the rows
    /// read *now* was the bug ES-06's first Apalache run found in the model). The whole set,
    /// not the difference from the fold of `history`: a difference applied to stored rows that
    /// missed a write (an older build appended without this projection) would leave the gap in
    /// place for ever, while the whole set replaces it. Pinned by
    /// `crates/mayfly-testkit/src/contracts/projections.rs::inline_rows_commit` (PC-03),
    /// `rebuild_equals_live` (PC-01) and `missed_rows_heal` (PC-10).
    ///
    /// # Errors
    /// [`ProjectionError::Decode`] or [`ProjectionError::Row`].
    pub fn inline(
        &self,
        stream: &StreamId,
        history: &[RecordedEvent],
        batch: &[RecordedEvent],
    ) -> Result<ProjectionWrite, ProjectionError> {
        let mut events = Vec::with_capacity(history.len() + batch.len());
        events.extend_from_slice(history);
        events.extend_from_slice(batch);
        let rows = (self.fold)(self.spec, stream, &events)?
            .into_iter()
            .map(|(key, row)| RowWrite { key, row })
            .collect();
        Ok(ProjectionWrite {
            projection: self.name.clone(),
            stream: stream.clone(),
            rows,
        })
    }
}

/// The inline projections a store keeps: each written by every unit that appends to a stream
/// of its aggregate.
#[derive(Debug, Clone, Default)]
pub struct Projections(Vec<Projection>);

impl Projections {
    /// No projection.
    #[must_use]
    pub const fn none() -> Self {
        Self(Vec::new())
    }

    /// `projections`, named uniquely.
    ///
    /// # Errors
    /// [`ProjectionError::InvalidSpec`] naming the first name used twice.
    pub fn new(projections: Vec<Projection>) -> Result<Self, ProjectionError> {
        let mut seen = std::collections::BTreeSet::new();
        if let Some(twice) = projections.iter().find(|p| !seen.insert(p.name.clone())) {
            return Err(ProjectionError::InvalidSpec { name: twice.spec });
        }
        Ok(Self(projections))
    }

    /// Every projection, in the order given.
    #[must_use]
    pub fn all(&self) -> &[Projection] {
        &self.0
    }

    /// The projections of `aggregate`'s streams.
    pub fn of<'a>(&'a self, aggregate: &'a AggregateName) -> impl Iterator<Item = &'a Projection> {
        self.0.iter().filter(move |p| p.aggregate == *aggregate)
    }
}

/// The first character of a hashed suffix ([`key_of`]).
const HASHED: char = '#';

/// The key of the row `suffix` of `stream`: the stream id for `""`, else `<stream>/<suffix>`,
/// or `<stream>/#<SHA-256 of the suffix, hex>` when that would be longer than [`MAX_ROW_KEY`]
/// or the suffix itself starts with `#`. A spec's suffix is unbounded (an image digest and an
/// attempt id, say), but a stream id is at most 128 bytes, so the hashed key (at most 194) is
/// always one the journal accepts and a valid command never becomes
/// `Malformed(InvalidProjectionKey)`; hashing every `#` suffix keeps a hashed key from ever
/// meeting a spec's own suffix. Rows keep their suffix inside the fold, and queries split a key
/// at its first `/` only, so nothing reads the suffix back from the key. Pinned by
/// `should_hash_a_long_suffix_when_the_key_would_be_too_long`.
fn key_of(stream: &StreamId, suffix: &str) -> String {
    if suffix.is_empty() {
        return stream.as_str().to_owned();
    }
    if stream.as_str().len() + 1 + suffix.len() <= MAX_ROW_KEY && !suffix.starts_with(HASHED) {
        return format!("{stream}/{suffix}");
    }
    let digest = Sha256::digest(suffix.as_bytes());
    let mut key = format!("{stream}/{HASHED}");
    for byte in digest {
        let _ = write!(key, "{byte:02x}");
    }
    key
}

fn encode<R: serde::Serialize>(
    projection: &'static str,
    stream: &StreamId,
    rows: &Rows<R>,
) -> Result<EncodedRows, ProjectionError> {
    let bad = || ProjectionError::Row {
        projection,
        stream: stream.clone(),
    };
    rows.iter()
        .map(|(suffix, row)| {
            let key = key_of(stream, suffix);
            let text = serde_json::to_string(row).map_err(|_| bad())?;
            Ok((key, Payload::new(text)))
        })
        .collect()
}

/// Folds `events` (the stream from revision 0) through `S`: the aggregate's state evolves on
/// every event and `S::apply` runs on the subscribed ones. Returns the encoded rows after all
/// of them.
fn fold_spec<S: ProjectionSpec>(
    projection: &'static str,
    stream: &StreamId,
    events: &[RecordedEvent],
) -> Result<EncodedRows, ProjectionError> {
    let mut state = <S::Source as Aggregate>::initial();
    let mut rows: Rows<S::Row> = BTreeMap::new();
    for recorded in events {
        let event = decode_event::<<S::Source as Aggregate>::Event>(recorded).map_err(|e| {
            ProjectionError::Decode {
                stream: stream.clone(),
                what: match e {
                    EventDecodeError::UnknownKind => "unknown_kind",
                    EventDecodeError::Malformed => "malformed",
                },
            }
        })?;
        state = <S::Source as Aggregate>::evolve(state, &event);
        if S::SUBSCRIBES.contains(&event.kind()) {
            let at = EventMeta {
                revision: recorded.revision,
                recorded_at: recorded.recorded_at,
            };
            for change in S::apply(&rows, &event, &state, at) {
                if change.suffix.chars().any(char::is_control) {
                    return Err(ProjectionError::Row {
                        projection,
                        stream: stream.clone(),
                    });
                }
                match change.row {
                    Some(row) => rows.insert(change.suffix, row),
                    None => rows.remove(&change.suffix),
                };
            }
        }
    }
    encode(projection, stream, &rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(name: &str) -> StreamId {
        StreamId::new(name).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn should_hash_a_long_suffix_when_the_key_would_be_too_long() {
        // given
        let longest = stream(&format!("pool-{}", "a".repeat(123)));
        let short = stream("pool-a");
        let suffixes = [
            ("", short.clone()),
            ("sha256:abc/7", short.clone()),
            ("#mine", short.clone()),
            (&*"x".repeat(127), longest.clone()),
            (&*"x".repeat(128), longest.clone()),
            (&*"y".repeat(10_000), longest.clone()),
        ];

        // when
        let keys: Vec<String> = suffixes.iter().map(|(s, at)| key_of(at, s)).collect();

        // then
        let hashed = |k: &String| k.split_once('/').is_some_and(|(_, s)| s.starts_with('#'));
        let shape: Vec<(bool, bool)> = keys
            .iter()
            .zip(&suffixes)
            .map(|(k, (_, at))| (hashed(k), crate::journal::row_key_in_stream(at, k)))
            .collect();
        assert_eq!(
            shape,
            [
                (false, true),
                (false, true),
                (true, true),
                (false, true),
                (true, true),
                (true, true),
            ]
        );
        assert_eq!(keys[0], "pool-a");
        assert_eq!(keys[1], "pool-a/sha256:abc/7");
        assert!(keys.iter().all(|k| k.len() <= MAX_ROW_KEY), "{keys:?}");
        assert_ne!(keys[4], keys[5]);
    }
}
