//! `ProjectionStore` and `ProjectionRebuild` over a SQLite journal's file (`doc/event-store.md`
//! §3.6, §4.1; migration 0004).
//!
//! Rows are documents `(spec, generation, key) -> (stream_id, payload)`, with no
//! projection-specific SQL; `projection_stream` holds the revision each stream's rows applied,
//! `projection_meta` the active generation, `projection_rebuild` (migration 0005) the
//! generation a running rebuild claims. Readers read the active generation inside the
//! statement that reads the rows, so a read sees one generation, and `stream_rows` reads the
//! applied revision and the rows in one read transaction; a rebuild writes its claimed
//! generation one stream per transaction and the cutover checks every stream of the aggregate
//! against its head, then flips `projection_meta`, drops the other generations and ends the
//! claim in one `BEGIN IMMEDIATE` transaction (PC-02, PC-11, PC-12). Rows are paged by key,
//! never by time or position (ES-I17).

use std::collections::BTreeMap;

use mayfly_control::journal::{
    AggregateName, MAX_PAGE, Payload, ProjectionName, StorageError, StreamId, StreamRevision,
    row_key_in_stream,
};
use mayfly_control::projection::{
    BuildId, Cutover, Generation, ProjectionMeta, ProjectionRebuild, ProjectionStore, StreamRows,
};
use rusqlite::{Connection, OptionalExtension, params};

use crate::classify::{Failure, classify};
use crate::hooks::StoreHooks;
use crate::journal::SqliteJournal;
use crate::ledger::{exec, read_transaction, transaction};
use crate::rows::{ACTIVE_GENERATION, revision_of, sql_revision, unreadable};

fn engine(error: &rusqlite::Error) -> Failure {
    classify(error)
}

fn generation_of(value: i64) -> Result<Generation, Failure> {
    u64::try_from(value).map_err(|_| unreadable())
}

fn sql_generation(generation: Generation) -> Result<i64, Failure> {
    i64::try_from(generation)
        .map_err(|_| Failure::Storage(StorageError::new("generation_exhausted")))
}

fn active(conn: &Connection, spec: &ProjectionName) -> Result<Generation, Failure> {
    conn.prepare_cached(&format!("SELECT {ACTIVE_GENERATION}"))
        .and_then(|mut s| s.query_row(params![spec.as_str()], |r| r.get::<_, i64>(0)))
        .map_err(|e| engine(&e))
        .and_then(generation_of)
}

fn meta(conn: &Connection) -> Result<Vec<ProjectionMeta>, Failure> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT spec, version, active_generation, written_by FROM projection_meta \
             ORDER BY spec",
        )
        .map_err(|e| engine(&e))?;
    let raw = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(|e| engine(&e))?;
    raw.into_iter()
        .map(|(spec, version, generation, written_by)| {
            Ok(ProjectionMeta {
                spec: ProjectionName::new(&spec).map_err(|_| unreadable())?,
                version: u32::try_from(version).map_err(|_| unreadable())?,
                active_generation: generation_of(generation)?,
                written_by: BuildId::new(&written_by).map_err(|_| unreadable())?,
            })
        })
        .collect()
}

/// `stream`'s applied revision and rows of `spec` in the active generation, read in one read
/// transaction so a unit committing between the two statements cannot pair one state's
/// revision with another's rows; `between` runs between them (a test's seam, a no-op here).
fn stream_rows(
    conn: &Connection,
    spec: &ProjectionName,
    stream: &StreamId,
    between: impl FnOnce(),
) -> Result<StreamRows, Failure> {
    read_transaction(conn, |conn| {
        let applied = conn
            .prepare_cached(&format!(
                "SELECT applied FROM projection_stream WHERE spec = ?1 \
                 AND generation = {ACTIVE_GENERATION} AND stream_id = ?2"
            ))
            .and_then(|mut s| {
                s.query_row(params![spec.as_str(), stream.as_str()], |r| {
                    r.get::<_, i64>(0)
                })
                .optional()
            })
            .map_err(|e| engine(&e))?
            .map(revision_of)
            .transpose()?;
        between();
        let rows = conn
            .prepare_cached(&format!(
                "SELECT key, payload FROM projection_row WHERE spec = ?1 \
                 AND generation = {ACTIVE_GENERATION} AND stream_id = ?2 ORDER BY key"
            ))
            .and_then(|mut s| {
                s.query_map(params![spec.as_str(), stream.as_str()], |r| {
                    Ok((r.get::<_, String>(0)?, Payload::new(r.get::<_, String>(1)?)))
                })
                .and_then(Iterator::collect::<Result<BTreeMap<_, _>, _>>)
            })
            .map_err(|e| engine(&e))?;
        Ok(StreamRows { applied, rows })
    })
}

fn page(
    conn: &Connection,
    spec: &ProjectionName,
    after: Option<&str>,
    limit: u32,
) -> Result<Vec<(String, Payload)>, Failure> {
    conn.prepare_cached(&format!(
        "SELECT key, payload FROM projection_row WHERE spec = ?1 \
         AND generation = {ACTIVE_GENERATION} AND (?2 IS NULL OR key > ?2) ORDER BY key LIMIT ?3"
    ))
    .and_then(|mut s| {
        s.query_map(params![spec.as_str(), after, i64::from(limit)], |r| {
            Ok((r.get::<_, String>(0)?, Payload::new(r.get::<_, String>(1)?)))
        })
        .and_then(Iterator::collect)
    })
    .map_err(|e| engine(&e))
}

/// Removes `spec`'s rows and applied revisions: of every generation but `generation` when
/// `keep`, else of `generation` alone.
fn clear(
    conn: &Connection,
    spec: &ProjectionName,
    generation: i64,
    keep: bool,
) -> Result<(), Failure> {
    let op = if keep { "<>" } else { "=" };
    exec(
        conn,
        &format!("DELETE FROM projection_row WHERE spec = ?1 AND generation {op} ?2"),
        params![spec.as_str(), generation],
    )?;
    exec(
        conn,
        &format!("DELETE FROM projection_stream WHERE spec = ?1 AND generation {op} ?2"),
        params![spec.as_str(), generation],
    )?;
    Ok(())
}

impl<H: StoreHooks> ProjectionStore for SqliteJournal<H> {
    async fn meta(&self) -> Result<Vec<ProjectionMeta>, StorageError> {
        self.read(meta).await
    }

    async fn stream_rows(
        &self,
        spec: &ProjectionName,
        stream: &StreamId,
    ) -> Result<StreamRows, StorageError> {
        let (spec, stream) = (spec.clone(), stream.clone());
        self.read(move |c| stream_rows(c, &spec, &stream, || {}))
            .await
    }

    async fn row(&self, spec: &ProjectionName, key: &str) -> Result<Option<Payload>, StorageError> {
        let (spec, key) = (spec.clone(), key.to_owned());
        self.read(move |c| crate::rows::projection(c, &spec, &key))
            .await
    }

    async fn rows(
        &self,
        spec: &ProjectionName,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<(String, Payload)>, StorageError> {
        let (spec, after) = (spec.clone(), after.map(str::to_owned));
        let limit = limit.min(MAX_PAGE);
        self.read(move |c| page(c, &spec, after.as_deref(), limit))
            .await
    }
}

/// The generation `spec`'s rebuild claims, if one runs.
fn claim(conn: &Connection, spec: &ProjectionName) -> Result<Option<i64>, Failure> {
    conn.prepare_cached("SELECT generation FROM projection_rebuild WHERE spec = ?1")
        .and_then(|mut s| {
            s.query_row(params![spec.as_str()], |r| r.get::<_, i64>(0))
                .optional()
        })
        .map_err(|e| engine(&e))
}

/// Refused `rebuild_superseded` unless `generation` is the one `spec`'s rebuild claims.
fn claimed(conn: &Connection, spec: &ProjectionName, generation: i64) -> Result<(), Failure> {
    if claim(conn, spec)? == Some(generation) {
        Ok(())
    } else {
        Err(Failure::Storage(StorageError::new("rebuild_superseded")))
    }
}

/// Whether a stream of `aggregate` has, in `spec`'s `generation` (`None`: the active one), no
/// applied revision or one other than its head: one statement over the heads of the aggregate.
fn behind(
    conn: &Connection,
    spec: &ProjectionName,
    aggregate: &AggregateName,
    generation: Option<i64>,
) -> Result<bool, Failure> {
    conn.prepare_cached(&format!(
        "SELECT EXISTS (SELECT 1 FROM stream_head h WHERE h.aggregate = ?2 AND NOT EXISTS ( \
         SELECT 1 FROM projection_stream p WHERE p.spec = ?1 \
         AND p.generation = coalesce(?3, {ACTIVE_GENERATION}) \
         AND p.stream_id = h.stream_id AND p.applied = h.revision))"
    ))
    .and_then(|mut s| {
        s.query_row(
            params![spec.as_str(), aggregate.as_str(), generation],
            |r| r.get::<_, bool>(0),
        )
    })
    .map_err(|e| engine(&e))
}

impl<H: StoreHooks> ProjectionRebuild for SqliteJournal<H> {
    async fn begin_rebuild(&self, spec: &ProjectionName) -> Result<Generation, StorageError> {
        let spec = spec.clone();
        self.write(move |c| {
            transaction(c, |c| {
                let active = sql_generation(active(c, &spec)?)?;
                let used = c
                    .prepare_cached(
                        "SELECT max(generation) FROM (\
                         SELECT generation FROM projection_rebuild WHERE spec = ?1 UNION ALL \
                         SELECT generation FROM projection_stream WHERE spec = ?1 UNION ALL \
                         SELECT generation FROM projection_row WHERE spec = ?1)",
                    )
                    .and_then(|mut s| {
                        s.query_row(params![spec.as_str()], |r| r.get::<_, Option<i64>>(0))
                    })
                    .map_err(|e| engine(&e))?;
                let next = used
                    .unwrap_or(active)
                    .max(active)
                    .checked_add(1)
                    .ok_or(Failure::Storage(StorageError::new("generation_exhausted")))?;
                clear(c, &spec, active, true)?;
                exec(
                    c,
                    "INSERT INTO projection_rebuild (spec, generation) VALUES (?1, ?2) \
                     ON CONFLICT (spec) DO UPDATE SET generation = excluded.generation",
                    params![spec.as_str(), next],
                )?;
                generation_of(next)
            })
        })
        .await
    }

    async fn apply_stream(
        &self,
        spec: &ProjectionName,
        generation: Generation,
        stream: &StreamId,
        rows: BTreeMap<String, Payload>,
        applied: StreamRevision,
    ) -> Result<(), StorageError> {
        if !rows.keys().all(|key| row_key_in_stream(stream, key)) {
            return Err(StorageError::new("row_outside_stream"));
        }
        let (spec, stream) = (spec.clone(), stream.clone());
        self.write(move |c| {
            transaction(c, |c| {
                let sql_gen = sql_generation(generation)?;
                claimed(c, &spec, sql_gen)?;
                if active(c, &spec)? == generation {
                    return Err(Failure::Storage(StorageError::new("generation_active")));
                }
                let generation = sql_gen;
                exec(
                    c,
                    "DELETE FROM projection_row WHERE spec = ?1 AND generation = ?2 \
                     AND stream_id = ?3",
                    params![spec.as_str(), generation, stream.as_str()],
                )?;
                exec(
                    c,
                    "DELETE FROM projection_stream WHERE spec = ?1 AND generation = ?2 \
                     AND stream_id = ?3",
                    params![spec.as_str(), generation, stream.as_str()],
                )?;
                for (key, payload) in &rows {
                    exec(
                        c,
                        "INSERT INTO projection_row (spec, generation, key, stream_id, payload) \
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            spec.as_str(),
                            generation,
                            key,
                            stream.as_str(),
                            payload.as_str()
                        ],
                    )?;
                }
                exec(
                    c,
                    "INSERT INTO projection_stream (spec, generation, stream_id, applied) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        spec.as_str(),
                        generation,
                        stream.as_str(),
                        sql_revision(applied)?
                    ],
                )?;
                Ok(())
            })
        })
        .await
    }

    async fn cutover(
        &self,
        spec: &ProjectionName,
        aggregate: &AggregateName,
        generation: Generation,
        version: u32,
        written_by: &BuildId,
    ) -> Result<Cutover, StorageError> {
        let (spec, aggregate, written_by) = (spec.clone(), aggregate.clone(), written_by.clone());
        self.write(move |c| {
            transaction(c, |c| {
                let generation = sql_generation(generation)?;
                claimed(c, &spec, generation)?;
                if behind(c, &spec, &aggregate, Some(generation))? {
                    return Ok(Cutover::StreamsMoved);
                }
                exec(
                    c,
                    "INSERT INTO projection_meta (spec, version, active_generation, written_by) \
                     VALUES (?1, ?2, ?3, ?4) ON CONFLICT (spec) DO UPDATE SET \
                     version = excluded.version, active_generation = excluded.active_generation, \
                     written_by = excluded.written_by",
                    params![
                        spec.as_str(),
                        i64::from(version),
                        generation,
                        written_by.as_str()
                    ],
                )?;
                clear(c, &spec, generation, true)?;
                exec(
                    c,
                    "DELETE FROM projection_rebuild WHERE spec = ?1",
                    params![spec.as_str()],
                )?;
                Ok(Cutover::Active)
            })
        })
        .await
    }

    async fn behind(
        &self,
        spec: &ProjectionName,
        aggregate: &AggregateName,
    ) -> Result<bool, StorageError> {
        let (spec, aggregate) = (spec.clone(), aggregate.clone());
        self.read(move |c| behind(c, &spec, &aggregate, None)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SqliteJournal, SqliteOptions};
    use mayfly_domain::Millis;

    fn exec_raw(conn: &Connection, sql: &str) {
        conn.execute_batch(sql).unwrap_or_else(|e| panic!("{e}"));
    }

    #[test]
    fn should_read_rows_and_applied_together_when_a_unit_commits_between_them() {
        // given
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let path = dir.path().join("journal.sqlite");
        drop(
            SqliteJournal::open(&path, SqliteOptions::new(Millis::new(1)))
                .unwrap_or_else(|e| panic!("{e}")),
        );
        let reader = Connection::open(&path).unwrap_or_else(|e| panic!("{e}"));
        let writer = Connection::open(&path).unwrap_or_else(|e| panic!("{e}"));
        exec_raw(
            &writer,
            "INSERT INTO projection_row (spec, generation, key, stream_id, payload) \
             VALUES ('pools', 0, 'pool-a', 'pool-a', '{\"v\":1}'); \
             INSERT INTO projection_stream (spec, generation, stream_id, applied) \
             VALUES ('pools', 0, 'pool-a', 0);",
        );
        let spec = ProjectionName::new("pools").unwrap_or_else(|e| panic!("{e}"));
        let stream = StreamId::new("pool-a").unwrap_or_else(|e| panic!("{e}"));
        let before =
            stream_rows(&reader, &spec, &stream, || {}).unwrap_or_else(|e| panic!("{e:?}"));

        // when
        let read = stream_rows(&reader, &spec, &stream, || {
            exec_raw(
                &writer,
                "BEGIN IMMEDIATE; \
                 INSERT INTO projection_row (spec, generation, key, stream_id, payload) \
                 VALUES ('pools', 0, 'pool-a/x', 'pool-a', '{\"v\":2}'); \
                 UPDATE projection_stream SET applied = 1 WHERE spec = 'pools' \
                 AND generation = 0 AND stream_id = 'pool-a'; COMMIT;",
            );
        });

        // then
        assert_eq!(read, Ok(before));
        let after = stream_rows(&reader, &spec, &stream, || {}).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(
            (after.applied.map(StreamRevision::get), after.rows.len()),
            (Some(1), 2)
        );
        assert!(reader.is_autocommit());
    }
}
