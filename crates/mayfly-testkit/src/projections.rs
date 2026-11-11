//! The projection rows of a [`MemoryJournal`]'s store: generations, applied revisions and meta
//! rows (`doc/event-store.md` §3.6, §4.1), in the same store as the journal so a unit's inline
//! rows commit with it (ES-I9). `MemoryJournal` implements `ProjectionStore` and
//! `ProjectionRebuild` over them; it passes the projection contract
//! (`tests/projection_contract.rs`: `should_pass_projection_contract_when_store_is_in_memory`).

use std::collections::BTreeMap;

use mayfly_control::journal::{
    AggregateName, MAX_PAGE, Payload, ProjectionName, ProjectionWrite, StorageError, StreamHead,
    StreamId, StreamRevision, row_key_in_stream,
};
use mayfly_control::projection::{
    BuildId, Cutover, Generation, ProjectionMeta, ProjectionRebuild, ProjectionStore, StreamRows,
};

use crate::journal::MemoryJournal;

/// Every projection row of a store.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProjectionRows {
    rows: BTreeMap<(ProjectionName, Generation, String), (StreamId, Payload)>,
    applied: BTreeMap<(ProjectionName, Generation, StreamId), StreamRevision>,
    meta: BTreeMap<ProjectionName, ProjectionMeta>,
    /// The generation each spec's rebuild claims (`begin_rebuild`), until its cutover.
    claims: BTreeMap<ProjectionName, Generation>,
}

impl ProjectionRows {
    /// The generation readers see and units write: the meta row's, or 0 before the first
    /// rebuild.
    pub(crate) fn active(&self, spec: &ProjectionName) -> Generation {
        self.meta.get(spec).map_or(0, |m| m.active_generation)
    }

    /// Puts a committed unit's rows for `write.stream` in place of the stream's rows in the
    /// active generation (the whole set, ES-04F), with `applied` (the stream's new head) as the
    /// revision applied.
    pub(crate) fn write(&mut self, write: ProjectionWrite, applied: StreamRevision) {
        let generation = self.active(&write.projection);
        let rows = write.rows.into_iter().map(|r| (r.key, r.row)).collect();
        self.replace(&write.projection, generation, &write.stream, rows, applied);
    }

    /// Replaces `stream`'s rows of `spec` in `generation` with `rows`, applied at `applied`.
    fn replace(
        &mut self,
        spec: &ProjectionName,
        generation: Generation,
        stream: &StreamId,
        rows: BTreeMap<String, Payload>,
        applied: StreamRevision,
    ) {
        self.rows
            .retain(|(s, g, _), (owner, _)| s != spec || *g != generation || owner != stream);
        for (key, payload) in rows {
            self.rows
                .insert((spec.clone(), generation, key), (stream.clone(), payload));
        }
        self.applied
            .insert((spec.clone(), generation, stream.clone()), applied);
    }

    /// Whether a stream of `aggregate` (among `heads`) has, in `generation`, no applied
    /// revision or one other than its head.
    fn behind(
        &self,
        spec: &ProjectionName,
        generation: Generation,
        aggregate: &AggregateName,
        heads: &BTreeMap<StreamId, StreamHead>,
    ) -> bool {
        heads
            .values()
            .filter(|h| h.aggregate == *aggregate)
            .any(|h| {
                self.applied
                    .get(&(spec.clone(), generation, h.stream.clone()))
                    != Some(&h.revision)
            })
    }

    /// The highest generation `spec` has used: active, claimed or holding a row.
    fn highest(&self, spec: &ProjectionName) -> Generation {
        let used = self
            .rows
            .keys()
            .filter(|(s, _, _)| s == spec)
            .map(|(_, g, _)| *g)
            .chain(
                self.applied
                    .keys()
                    .filter(|(s, _, _)| s == spec)
                    .map(|(_, g, _)| *g),
            );
        used.chain(self.claims.get(spec).copied())
            .fold(self.active(spec), Generation::max)
    }

    /// The row `key` of `spec` in the active generation.
    pub(crate) fn row(&self, spec: &ProjectionName, key: &str) -> Option<Payload> {
        let generation = self.active(spec);
        self.rows
            .get(&(spec.clone(), generation, key.to_owned()))
            .map(|(_, payload)| payload.clone())
    }

    fn stream_rows(&self, spec: &ProjectionName, stream: &StreamId) -> StreamRows {
        let generation = self.active(spec);
        StreamRows {
            applied: self
                .applied
                .get(&(spec.clone(), generation, stream.clone()))
                .copied(),
            rows: self
                .rows
                .iter()
                .filter(|((s, g, _), (owner, _))| s == spec && *g == generation && owner == stream)
                .map(|((_, _, key), (_, payload))| (key.clone(), payload.clone()))
                .collect(),
        }
    }

    fn page(
        &self,
        spec: &ProjectionName,
        after: Option<&str>,
        limit: u32,
    ) -> Vec<(String, Payload)> {
        let generation = self.active(spec);
        let limit = usize::try_from(limit.min(MAX_PAGE)).unwrap_or(0);
        self.rows
            .iter()
            .filter(|((s, g, key), _)| {
                s == spec && *g == generation && after.is_none_or(|a| key.as_str() > a)
            })
            .take(limit)
            .map(|((_, _, key), (_, payload))| (key.clone(), payload.clone()))
            .collect()
    }

    fn clear(&mut self, spec: &ProjectionName, keep: impl Fn(Generation) -> bool) {
        self.rows.retain(|(s, g, _), _| s != spec || keep(*g));
        self.applied.retain(|(s, g, _), _| s != spec || keep(*g));
    }
}

impl ProjectionStore for MemoryJournal {
    async fn meta(&self) -> Result<Vec<ProjectionMeta>, StorageError> {
        Ok(self
            .lock()
            .state
            .projections
            .meta
            .values()
            .cloned()
            .collect())
    }

    async fn stream_rows(
        &self,
        spec: &ProjectionName,
        stream: &StreamId,
    ) -> Result<StreamRows, StorageError> {
        Ok(self.lock().state.projections.stream_rows(spec, stream))
    }

    async fn row(&self, spec: &ProjectionName, key: &str) -> Result<Option<Payload>, StorageError> {
        Ok(self.lock().state.projections.row(spec, key))
    }

    async fn rows(
        &self,
        spec: &ProjectionName,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<(String, Payload)>, StorageError> {
        Ok(self.lock().state.projections.page(spec, after, limit))
    }
}

impl ProjectionRebuild for MemoryJournal {
    async fn begin_rebuild(&self, spec: &ProjectionName) -> Result<Generation, StorageError> {
        let mut shared = self.lock();
        let projections = &mut shared.state.projections;
        let generation = projections
            .highest(spec)
            .checked_add(1)
            .ok_or(StorageError::new("generation_exhausted"))?;
        let active = projections.active(spec);
        projections.clear(spec, |g| g == active);
        projections.claims.insert(spec.clone(), generation);
        Ok(generation)
    }

    async fn apply_stream(
        &self,
        spec: &ProjectionName,
        generation: Generation,
        stream: &StreamId,
        rows: BTreeMap<String, Payload>,
        applied: StreamRevision,
    ) -> Result<(), StorageError> {
        let mut shared = self.lock();
        let projections = &mut shared.state.projections;
        if projections.claims.get(spec) != Some(&generation) {
            return Err(StorageError::new("rebuild_superseded"));
        }
        if generation == projections.active(spec) {
            return Err(StorageError::new("generation_active"));
        }
        if !rows.keys().all(|key| row_key_in_stream(stream, key)) {
            return Err(StorageError::new("row_outside_stream"));
        }
        projections.replace(spec, generation, stream, rows, applied);
        Ok(())
    }

    async fn cutover(
        &self,
        spec: &ProjectionName,
        aggregate: &AggregateName,
        generation: Generation,
        version: u32,
        written_by: &BuildId,
    ) -> Result<Cutover, StorageError> {
        let mut shared = self.lock();
        let state = &mut shared.state;
        if state.projections.claims.get(spec) != Some(&generation) {
            return Err(StorageError::new("rebuild_superseded"));
        }
        if state
            .projections
            .behind(spec, generation, aggregate, &state.heads)
        {
            return Ok(Cutover::StreamsMoved);
        }
        state
            .projections
            .activate(spec, generation, version, written_by);
        Ok(Cutover::Active)
    }

    async fn behind(
        &self,
        spec: &ProjectionName,
        aggregate: &AggregateName,
    ) -> Result<bool, StorageError> {
        let shared = self.lock();
        let state = &shared.state;
        let active = state.projections.active(spec);
        Ok(state
            .projections
            .behind(spec, active, aggregate, &state.heads))
    }
}

impl ProjectionRows {
    /// The cutover's writes: `generation` active, every other one gone, the claim ended.
    fn activate(
        &mut self,
        spec: &ProjectionName,
        generation: Generation,
        version: u32,
        written_by: &BuildId,
    ) {
        self.meta.insert(
            spec.clone(),
            ProjectionMeta {
                spec: spec.clone(),
                version,
                active_generation: generation,
                written_by: written_by.clone(),
            },
        );
        self.clear(spec, |g| g == generation);
        self.claims.remove(spec);
    }
}

/// Writes that judge nothing, for the broken stores of the projection contract's own tests
/// (`tests/projection_contract.rs`, `doc/event-store.md` §7.6): each is what a store with one
/// fault would do.
impl MemoryJournal {
    /// Writes `write`'s rows in the active generation with `applied`, judging nothing: for
    /// broken stores built over this one (rows outside their unit, PC-04 and PC-05 must catch).
    pub fn force_rows(&self, write: ProjectionWrite, applied: StreamRevision) {
        self.lock().state.projections.write(write, applied);
    }

    /// Puts `rows` back among `stream`'s rows of `spec` in the active generation, over what is
    /// stored and removing nothing: with the rows a stream had before a unit, what a store that
    /// patches its rows instead of replacing them keeps (`MergingRowsJournal`; PC-03 and PC-10
    /// must catch it).
    pub fn keep_rows(
        &self,
        spec: &ProjectionName,
        stream: &StreamId,
        rows: BTreeMap<String, Payload>,
    ) {
        let mut shared = self.lock();
        let projections = &mut shared.state.projections;
        let generation = projections.active(spec);
        for (key, payload) in rows {
            projections
                .rows
                .entry((spec.clone(), generation, key))
                .or_insert_with(|| (stream.clone(), payload));
        }
    }

    /// Replaces `stream`'s rows of `spec` in the **active** generation, judging nothing: for
    /// the broken store that rebuilds in place (PC-02 must catch it).
    pub fn overwrite_active_rows(
        &self,
        spec: &ProjectionName,
        stream: &StreamId,
        rows: BTreeMap<String, Payload>,
        applied: StreamRevision,
    ) {
        let mut shared = self.lock();
        let projections = &mut shared.state.projections;
        let generation = projections.active(spec);
        projections.replace(spec, generation, stream, rows, applied);
    }

    /// Starts a rebuild at the active generation plus one, clearing it, and claims it, as the
    /// store did before claims (`SharedGenerationStore`: a second rebuild takes the first's
    /// generation; PC-12 must catch it).
    pub fn begin_rebuild_unclaimed(&self, spec: &ProjectionName) -> Generation {
        let mut shared = self.lock();
        let projections = &mut shared.state.projections;
        let generation = projections.active(spec).saturating_add(1);
        projections.clear(spec, |g| g != generation);
        projections.claims.insert(spec.clone(), generation);
        generation
    }

    /// Writes a rebuild's rows into `generation` whatever claims it (refusing the active one
    /// only), for `SharedGenerationStore`.
    pub fn apply_stream_unclaimed(
        &self,
        spec: &ProjectionName,
        generation: Generation,
        stream: &StreamId,
        rows: BTreeMap<String, Payload>,
        applied: StreamRevision,
    ) -> Result<(), StorageError> {
        let mut shared = self.lock();
        let projections = &mut shared.state.projections;
        if generation == projections.active(spec) {
            return Err(StorageError::new("generation_active"));
        }
        projections.replace(spec, generation, stream, rows, applied);
        Ok(())
    }

    /// Makes `generation` active with no check of its streams against their heads, the claim
    /// still checked (`UncheckedCutoverStore`; PC-11 must catch it).
    ///
    /// # Errors
    /// `rebuild_superseded` when `generation` is not the claimed one.
    pub fn cutover_unchecked(
        &self,
        spec: &ProjectionName,
        generation: Generation,
        version: u32,
        written_by: &BuildId,
    ) -> Result<(), StorageError> {
        let mut shared = self.lock();
        let projections = &mut shared.state.projections;
        if projections.claims.get(spec) != Some(&generation) {
            return Err(StorageError::new("rebuild_superseded"));
        }
        projections.activate(spec, generation, version, written_by);
        Ok(())
    }
}
