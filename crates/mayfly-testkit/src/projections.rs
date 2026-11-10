//! The projection rows of a [`MemoryJournal`]'s store: generations, applied revisions and meta
//! rows (`doc/event-store.md` §3.6, §4.1), in the same store as the journal so a unit's inline
//! rows commit with it (ES-I9). `MemoryJournal` implements `ProjectionStore` and
//! `ProjectionRebuild` over them; it passes the projection contract
//! (`tests/projection_contract.rs`: `should_pass_projection_contract_when_store_is_in_memory`).

use std::collections::BTreeMap;

use mayfly_control::journal::{
    MAX_PAGE, Payload, ProjectionName, ProjectionWrite, StorageError, StreamId, StreamRevision,
    row_key_in_stream,
};
use mayfly_control::projection::{
    BuildId, Generation, ProjectionMeta, ProjectionRebuild, ProjectionStore, StreamRows,
};

use crate::journal::MemoryJournal;

/// Every projection row of a store.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProjectionRows {
    rows: BTreeMap<(ProjectionName, Generation, String), (StreamId, Payload)>,
    applied: BTreeMap<(ProjectionName, Generation, StreamId), StreamRevision>,
    meta: BTreeMap<ProjectionName, ProjectionMeta>,
}

impl ProjectionRows {
    /// The generation readers see and units write: the meta row's, or 0 before the first
    /// rebuild.
    pub(crate) fn active(&self, spec: &ProjectionName) -> Generation {
        self.meta.get(spec).map_or(0, |m| m.active_generation)
    }

    /// Writes a committed unit's rows for `write.stream` in the active generation, with
    /// `applied` (the stream's new head) as the revision applied.
    pub(crate) fn write(&mut self, write: ProjectionWrite, applied: StreamRevision) {
        let generation = self.active(&write.projection);
        for row in write.rows {
            let key = (write.projection.clone(), generation, row.key);
            match row.row {
                Some(payload) => self.rows.insert(key, (write.stream.clone(), payload)),
                None => self.rows.remove(&key),
            };
        }
        self.applied
            .insert((write.projection, generation, write.stream), applied);
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
            .active(spec)
            .checked_add(1)
            .ok_or(StorageError::new("generation_exhausted"))?;
        projections.clear(spec, |g| g != generation);
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
        if generation == projections.active(spec) {
            return Err(StorageError::new("generation_active"));
        }
        if !rows.keys().all(|key| row_key_in_stream(stream, key)) {
            return Err(StorageError::new("row_outside_stream"));
        }
        projections
            .rows
            .retain(|(s, g, _), (owner, _)| s != spec || *g != generation || owner != stream);
        for (key, payload) in rows {
            projections
                .rows
                .insert((spec.clone(), generation, key), (stream.clone(), payload));
        }
        projections
            .applied
            .insert((spec.clone(), generation, stream.clone()), applied);
        Ok(())
    }

    async fn cutover(
        &self,
        spec: &ProjectionName,
        generation: Generation,
        version: u32,
        written_by: &BuildId,
    ) -> Result<(), StorageError> {
        let mut shared = self.lock();
        let projections = &mut shared.state.projections;
        projections.meta.insert(
            spec.clone(),
            ProjectionMeta {
                spec: spec.clone(),
                version,
                active_generation: generation,
                written_by: written_by.clone(),
            },
        );
        projections.clear(spec, |g| g == generation);
        Ok(())
    }
}

impl MemoryJournal {
    /// Writes `write`'s rows in the active generation with `applied`, judging nothing: for
    /// broken stores built over this one (rows outside their unit, PC-04 and PC-05 must catch).
    pub fn force_rows(&self, write: ProjectionWrite, applied: StreamRevision) {
        self.lock().state.projections.write(write, applied);
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
        projections
            .rows
            .retain(|(s, g, _), (owner, _)| s != spec || *g != generation || owner != stream);
        for (key, payload) in rows {
            projections
                .rows
                .insert((spec.clone(), generation, key), (stream.clone(), payload));
        }
        projections
            .applied
            .insert((spec.clone(), generation, stream.clone()), applied);
    }
}
