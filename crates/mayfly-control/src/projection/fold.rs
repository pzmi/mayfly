//! Folding a stream through a projection, and the inline writes of a unit (`doc/event-store.md`
//! §3.6, §3.7 step 4).

use std::collections::BTreeMap;
use std::fmt;

use mayfly_control_domain::{Aggregate, EventKind, EventSet};

use crate::journal::{
    AggregateName, JournalValueError, Payload, ProjectionName, ProjectionWrite, RecordedEvent,
    RowWrite, StreamId,
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

/// The rows of a stream before its last events and after all of them.
type Folded = Result<(EncodedRows, EncodedRows), ProjectionError>;

/// A [`ProjectionSpec`] with its types erased, so specs of different aggregates sit in one
/// [`Projections`] list (a function pointer per spec; no trait object, D8).
#[derive(Clone)]
pub struct Projection {
    name: ProjectionName,
    spec: &'static str,
    version: u32,
    aggregate: AggregateName,
    subscribes: &'static [EventKind],
    fold: fn(&'static str, &StreamId, &[RecordedEvent], usize) -> Folded,
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
        (self.fold)(self.spec, stream, events, events.len()).map(|(_, after)| after)
    }

    /// The writes that take `stream`'s rows from the fold of `history` (the stream at the
    /// unit's expected revision) to the fold of `history` then `batch` (the events the unit
    /// appends): puts for new and changed rows, deletes for rows gone.
    ///
    /// Both folds start at revision 0 from the events the decision was made on, so the rows
    /// the unit writes are exactly those at its expected revision plus its batch, whatever
    /// another unit wrote meanwhile; that unit moved the head, so this one is refused
    /// `WrongExpectedRevision` and decided again (the model's `rowAfter`; a fold from the rows
    /// read *now* was the bug ES-06's first Apalache run found in the model). Pinned by
    /// `crates/mayfly-testkit/src/contracts/projections.rs::inline_rows_commit` (PC-03) and
    /// `rebuild_equals_live` (PC-01).
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
        let (before, after) = (self.fold)(self.spec, stream, &events, history.len())?;
        let mut rows: Vec<RowWrite> = before
            .keys()
            .filter(|key| !after.contains_key(*key))
            .map(|key| RowWrite {
                key: key.clone(),
                row: None,
            })
            .collect();
        rows.extend(
            after
                .into_iter()
                .filter(|(key, row)| before.get(key) != Some(row))
                .map(|(key, row)| RowWrite {
                    key,
                    row: Some(row),
                }),
        );
        rows.sort_by(|a, b| a.key.cmp(&b.key));
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

/// The key of the row `suffix` of `stream`: the stream id for `""`, else `<stream>/<suffix>`.
fn key_of(stream: &StreamId, suffix: &str) -> String {
    if suffix.is_empty() {
        stream.as_str().to_owned()
    } else {
        format!("{stream}/{suffix}")
    }
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
/// every event and `S::apply` runs on the subscribed ones. Returns the encoded rows after the
/// first `split` events and after all of them.
fn fold_spec<S: ProjectionSpec>(
    projection: &'static str,
    stream: &StreamId,
    events: &[RecordedEvent],
    split: usize,
) -> Folded {
    let mut state = <S::Source as Aggregate>::initial();
    let mut rows: Rows<S::Row> = BTreeMap::new();
    let mut before = None;
    for (index, recorded) in events.iter().enumerate() {
        if index == split {
            before = Some(encode(projection, stream, &rows)?);
        }
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
    let after = encode(projection, stream, &rows)?;
    Ok((before.unwrap_or_else(|| after.clone()), after))
}
