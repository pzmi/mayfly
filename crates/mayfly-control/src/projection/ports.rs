//! The projection store's ports (`doc/event-store.md` §3.6): [`ProjectionStore`] reads the
//! active generation, [`ProjectionRebuild`] writes a new one. They are two traits so a query
//! takes the read port only and never a write port (plan §5.4).

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;

use crate::journal::{Payload, ProjectionName, StorageError, StreamId, StreamRevision};

/// The build that wrote a projection's rows, as `projection_meta.written_by` records it: a
/// start whose build differs rebuilds (`doc/event-store.md` §6.1, §6.4), so a rolled-back build
/// never reads rows a newer one wrote with a meaning it does not know.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BuildId(String);

impl BuildId {
    /// `[a-z0-9.+-]{1,64}`, such as `0.1.0` or `0.2.0-rc.1+abc`.
    ///
    /// # Errors
    /// [`InvalidBuildId`] for anything else.
    pub fn new(value: &str) -> Result<Self, InvalidBuildId> {
        let valid = (1..=64).contains(&value.len())
            && value.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'+' | b'-')
            });
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(InvalidBuildId)
        }
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BuildId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A build id is not `[a-z0-9.+-]{1,64}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidBuildId;

impl fmt::Display for InvalidBuildId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid build id")
    }
}

impl std::error::Error for InvalidBuildId {}

/// A projection's generation: a set of rows readers see as one. A store without a meta row for
/// a spec reads generation 0, where units wrote before the spec's first rebuild.
pub type Generation = u64;

/// What the store records of one projection (`projection_meta`, §4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionMeta {
    /// The projection.
    pub spec: ProjectionName,
    /// The spec version its active rows were built by.
    pub version: u32,
    /// The generation readers see and units write.
    pub active_generation: Generation,
    /// The build that built them.
    pub written_by: BuildId,
}

/// One stream's rows of a projection in one generation, with the revision of the stream they
/// are the fold of.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamRows {
    /// The stream revision the rows have applied: the head a unit wrote them at, or the last
    /// revision a rebuild folded (never one read from a head, ES-I11); `None` when nothing was
    /// applied for the stream.
    pub applied: Option<StreamRevision>,
    /// The rows, by key.
    pub rows: BTreeMap<String, Payload>,
}

/// Reads of the projection rows readers see: the active generation of each spec.
///
/// A native async trait used through generics (D8). Implemented by the store that commits the
/// units (`MemoryJournal`, `SqliteJournal`), since inline rows commit in the unit's
/// transaction (ES-I9). Every read is one statement and cancel-safe.
///
/// Pinned by: `crates/mayfly-testkit/src/contracts/projections.rs` (PC-01..PC-06), run on memory
/// by `crates/mayfly-testkit/tests/projection_contract.rs` and on SQLite by
/// `crates/mayfly-adapter-sqlite/tests/projection_contract.rs`.
pub trait ProjectionStore: Send + Sync {
    /// Every projection the store has a meta row for, by name.
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn meta(&self) -> impl Future<Output = Result<Vec<ProjectionMeta>, StorageError>> + Send;

    /// `stream`'s rows of `spec` in the active generation.
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn stream_rows(
        &self,
        spec: &ProjectionName,
        stream: &StreamId,
    ) -> impl Future<Output = Result<StreamRows, StorageError>> + Send;

    /// The row `key` of `spec` in the active generation.
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn row(
        &self,
        spec: &ProjectionName,
        key: &str,
    ) -> impl Future<Output = Result<Option<Payload>, StorageError>> + Send;

    /// Up to `limit` (clamped to [`crate::journal::MAX_PAGE`]) rows of `spec` in the active
    /// generation with a key above `after`, in byte order of the key: a query pages through a
    /// projection by key, never by time or position (ES-I17).
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn rows(
        &self,
        spec: &ProjectionName,
        after: Option<&str>,
        limit: u32,
    ) -> impl Future<Output = Result<Vec<(String, Payload)>, StorageError>> + Send;
}

/// The offline rebuild's writes (`doc/event-store.md` §3.6; ES-12 makes it online): a new
/// generation, built stream by stream, made active in one transaction.
///
/// **Offline.** Units that commit while a rebuild runs write the active generation only, so a
/// rebuild runs where no unit does: at start-up, after the migrations and the stored-kinds
/// check, before any writer (`crate::projection::prepare`). ES-12 adds the dual writes and the
/// cutover fence that let it run beside writers.
pub trait ProjectionRebuild: Send + Sync {
    /// Starts a rebuild of `spec`: returns a generation above the active one, emptied of
    /// anything an earlier, interrupted rebuild left there.
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn begin_rebuild(
        &self,
        spec: &ProjectionName,
    ) -> impl Future<Output = Result<Generation, StorageError>> + Send;

    /// Replaces `stream`'s rows of `spec` in `generation` with `rows`, folded through revision
    /// `applied`, in one transaction. Refused when `generation` is the active one (readers
    /// would see a half-built projection, PC-02) or a key is not one of `stream`'s
    /// (`row_outside_stream`, ES-I10).
    ///
    /// # Errors
    /// [`StorageError`] as above or when the store fails.
    fn apply_stream(
        &self,
        spec: &ProjectionName,
        generation: Generation,
        stream: &StreamId,
        rows: BTreeMap<String, Payload>,
        applied: StreamRevision,
    ) -> impl Future<Output = Result<(), StorageError>> + Send;

    /// Makes `generation` the active one of `spec`, recording `version` and `written_by`, and
    /// removes every other generation's rows, in one transaction: readers see the old rows or
    /// the new, never a mix (PC-02).
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn cutover(
        &self,
        spec: &ProjectionName,
        generation: Generation,
        version: u32,
        written_by: &BuildId,
    ) -> impl Future<Output = Result<(), StorageError>> + Send;
}
