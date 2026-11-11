//! The offline rebuild (`doc/event-store.md` §3.6, ES-I11; mayfly I16): at start-up, every
//! projection whose stored version or build differs from this build's, or with a stream behind
//! its head, is rebuilt into a new generation by enumerating the journal's streams by key, then
//! made active in one transaction.

use std::fmt;

use crate::journal::{Journal, MAX_PAGE, StorageError, StreamId};
use crate::projection::fold::{Projection, ProjectionError, Projections};
use crate::projection::ports::{
    BuildId, Cutover, Generation, ProjectionMeta, ProjectionRebuild, ProjectionStore,
};

/// How many times [`rebuild_projection`] folds the streams again after a refused cutover before
/// it gives up: at start-up no unit runs, so one pass is the rule and the others are for a
/// writer that should not be there.
pub const REBUILD_PASSES: u32 = 3;

/// Why `prepare` stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RebuildError {
    /// The store failed.
    Storage(StorageError),
    /// A stream did not fold (an unknown kind: safe mode, `doc/event-store.md` §6.3).
    Projection(ProjectionError),
    /// Every cutover of [`REBUILD_PASSES`] passes was refused because a stream moved after it
    /// was folded: units keep committing during an offline rebuild.
    StreamsMoved,
}

impl fmt::Display for RebuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(e) => write!(f, "projection rebuild failed: {e}"),
            Self::Projection(e) => write!(f, "projection rebuild failed: {e}"),
            Self::StreamsMoved => {
                f.write_str("projection rebuild failed: streams kept moving under it")
            }
        }
    }
}

impl std::error::Error for RebuildError {}

/// What `prepare` did for one projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    /// The projection.
    pub spec: String,
    /// Whether it was rebuilt; `false` when its rows were already this build's and no stream
    /// was behind its head.
    pub rebuilt: bool,
    /// The streams folded into the new generation.
    pub streams: u64,
}

/// Whether `projection`'s rows must be rebuilt for build `build` given what the store holds:
/// no meta row, another version **in either direction** (a rolled-back build meets rows a
/// newer spec wrote, PC-06), or another build (`doc/event-store.md` §6.1, §6.4). Pinned by
/// `should_rebuild_when_the_version_or_build_differs_in_either_direction`.
#[must_use]
pub fn needs_rebuild(
    projection: &Projection,
    stored: Option<&ProjectionMeta>,
    build: &BuildId,
) -> bool {
    stored.is_none_or(|meta| meta.version != projection.version() || meta.written_by != *build)
}

/// Rebuilds, offline, every projection of `projections` that [`needs_rebuild`] or has a stream
/// behind its head ([`ProjectionRebuild::behind`]: a write it missed, such as the events an
/// older build without the projection appended, PC-10); leaves the others as they are. Runs
/// before any writer (the rebuild's writes are not fenced against units until ES-12; the
/// cutover refuses a generation a unit overtook, PC-11): composition calls it at start-up after
/// the migrations and the stored-kinds check (`doc/event-store.md` §6.3, CP-04).
///
/// Per projection: [`rebuild_projection`]. A crash before the cutover leaves the old generation
/// active, and the next start begins again. Pinned by
/// `crates/mayfly-testkit/src/contracts/projections.rs` (PC-01, PC-02, PC-06, PC-10..PC-12) and
/// `crates/mayfly-control/tests/store/projections.rs::should_record_the_revision_folded_when_a_stream_moves_after_its_head_was_paged`.
///
/// # Errors
/// [`RebuildError`]; the projection being rebuilt keeps its old generation.
pub async fn prepare<J, P>(
    journal: &J,
    store: &P,
    projections: &Projections,
    build: &BuildId,
) -> Result<Vec<Prepared>, RebuildError>
where
    J: Journal,
    P: ProjectionStore + ProjectionRebuild,
{
    let stored = store.meta().await.map_err(RebuildError::Storage)?;
    let mut report = Vec::with_capacity(projections.all().len());
    for projection in projections.all() {
        let meta = stored.iter().find(|m| m.spec == *projection.name());
        let rebuild = needs_rebuild(projection, meta, build)
            || store
                .behind(projection.name(), projection.aggregate())
                .await
                .map_err(RebuildError::Storage)?;
        if !rebuild {
            report.push(Prepared {
                spec: projection.name().as_str().to_owned(),
                rebuilt: false,
                streams: 0,
            });
            continue;
        }
        let streams = rebuild_projection(journal, store, projection, build).await?;
        report.push(Prepared {
            spec: projection.name().as_str().to_owned(),
            rebuilt: true,
            streams,
        });
    }
    Ok(report)
}

/// Rebuilds `projection` offline whatever the store holds (what [`prepare`] does for each
/// projection it rebuilds); returns how many streams it folded in its last pass.
///
/// [`ProjectionRebuild::begin_rebuild`] claims a new generation; a pass pages the journal's
/// streams by key ([`Journal::streams`], never by time or position, ES-I17), reads each stream
/// of the projection's aggregate whole and folds it from revision 0 into the new generation
/// with the revision of the **last event it folded** as applied, never the paged head's (ES-I11:
/// a unit that lands between the page and the read is folded and recorded as applied); then
/// [`ProjectionRebuild::cutover`]. A cutover refused because a stream moved after its fold
/// ([`Cutover::StreamsMoved`]) runs another pass, at most [`REBUILD_PASSES`] in all.
///
/// # Errors
/// [`RebuildError`]; the projection keeps its old generation.
pub async fn rebuild_projection<J, P>(
    journal: &J,
    store: &P,
    projection: &Projection,
    build: &BuildId,
) -> Result<u64, RebuildError>
where
    J: Journal,
    P: ProjectionRebuild,
{
    let spec = projection.name();
    let generation = store
        .begin_rebuild(spec)
        .await
        .map_err(RebuildError::Storage)?;
    for _ in 0..REBUILD_PASSES {
        let folded = fold_streams(journal, store, projection, generation).await?;
        let cutover = store
            .cutover(
                spec,
                projection.aggregate(),
                generation,
                projection.version(),
                build,
            )
            .await
            .map_err(RebuildError::Storage)?;
        if cutover == Cutover::Active {
            return Ok(folded);
        }
    }
    Err(RebuildError::StreamsMoved)
}

/// One pass of [`rebuild_projection`]: every stream of the projection's aggregate folded into
/// `generation`.
async fn fold_streams<J, P>(
    journal: &J,
    store: &P,
    projection: &Projection,
    generation: Generation,
) -> Result<u64, RebuildError>
where
    J: Journal,
    P: ProjectionRebuild,
{
    let mut after: Option<StreamId> = None;
    let mut folded = 0_u64;
    loop {
        let page = journal
            .streams(after.as_ref(), MAX_PAGE)
            .await
            .map_err(RebuildError::Storage)?;
        let Some(last) = page.last() else {
            return Ok(folded);
        };
        after = Some(last.stream.clone());
        for head in page
            .iter()
            .filter(|h| h.aggregate == *projection.aggregate())
        {
            let events = journal
                .read_stream(&head.stream, None, None)
                .await
                .map_err(RebuildError::Storage)?;
            let Some(applied) = events.last().map(|e| e.revision) else {
                continue;
            };
            let rows = projection
                .fold(&head.stream, &events)
                .map_err(RebuildError::Projection)?;
            store
                .apply_stream(projection.name(), generation, &head.stream, rows, applied)
                .await
                .map_err(RebuildError::Storage)?;
            folded += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::ProjectionName;
    use crate::projection::views::Pools;

    fn meta(version: u32, build: &str) -> ProjectionMeta {
        ProjectionMeta {
            spec: ProjectionName::new("pools").unwrap_or_else(|e| panic!("{e}")),
            version,
            active_generation: 3,
            written_by: BuildId::new(build).unwrap_or_else(|e| panic!("{e}")),
        }
    }

    #[test]
    fn should_rebuild_when_the_version_or_build_differs_in_either_direction() {
        // given
        let pools = Projection::of::<Pools>().unwrap_or_else(|e| panic!("{e}"));
        let build = BuildId::new("0.1.0").unwrap_or_else(|e| panic!("{e}"));
        let stored = [
            None,
            Some(meta(1, "0.1.0")),
            Some(meta(2, "0.1.0")),
            Some(meta(0, "0.1.0")),
            Some(meta(1, "0.2.0")),
        ];

        // when
        let decided: Vec<bool> = stored
            .iter()
            .map(|m| needs_rebuild(&pools, m.as_ref(), &build))
            .collect();

        // then
        assert_eq!(decided, [true, false, true, true, true]);
    }
}
