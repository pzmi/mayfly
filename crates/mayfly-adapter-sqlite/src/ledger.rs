//! `SqliteLedger`: the `DeliveryLedger` port over a SQLite journal's file (`doc/event-store.md`
//! §3.4, §4.2, §5.1).
//!
//! Each operation is one `BEGIN IMMEDIATE` transaction on one of the journal's writers, with
//! the store clock read inside it. A transaction takes a lease row, then its checkpoint, then
//! its mark, and never writes a stream head (ES-I16). Every lease check is in the data: the
//! row's fence must be the claim's and its `lease_until` after the store clock, and a claim
//! takes a lease only through an upsert whose `WHERE` requires the old one to have run out, so
//! two handles on one file cannot both hold a lease (DC-09) whatever thread or process they
//! run on (§4.3 rule 2). Nothing is ordered by time or position: candidates are found by
//! `(consumer, stream_id)` key, `retry_at` and `lease_until` are filters (ES-I17).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use mayfly_control::delivery::{
    Claim, ConsumerName, ConsumerState, DeliveryLedger, LeaseError, LeaseFence, StoreClock,
};
use mayfly_control::journal::{MAX_PAGE, StorageError, StreamId, StreamRevision};
use mayfly_domain::Millis;
use rusqlite::{Connection, OptionalExtension, params};

use crate::classify::{Failure, classify};
use crate::hooks::{NoHooks, StoreHooks};
use crate::journal::SqliteJournal;
use crate::rows::{revision_of, sql_millis, sql_revision, stream_of, unreadable};

/// The SQLite delivery ledger: a handle on a [`SqliteJournal`]'s connections and a store clock.
/// Clones share both and the round-robin position.
pub struct SqliteLedger<C, H: StoreHooks = NoHooks> {
    journal: SqliteJournal<H>,
    clock: Arc<C>,
    /// The last stream claimed per consumer, where the next discovery starts: fairness only,
    /// never a decision (§4.3 rule 3 forbids cached truth; this is none).
    cursor: Arc<Mutex<BTreeMap<ConsumerName, StreamId>>>,
}

impl<C, H: StoreHooks> Clone for SqliteLedger<C, H> {
    fn clone(&self) -> Self {
        Self {
            journal: self.journal.clone(),
            clock: Arc::clone(&self.clock),
            cursor: Arc::clone(&self.cursor),
        }
    }
}

impl<C, H: StoreHooks> fmt::Debug for SqliteLedger<C, H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqliteLedger").finish_non_exhaustive()
    }
}

impl<H: StoreHooks> SqliteJournal<H> {
    /// The delivery ledger of this journal's file, judging leases by `clock`. Its marks are
    /// written by this journal's appends, in their transactions (DC-02, DC-03).
    pub fn ledger<C: StoreClock + 'static>(&self, clock: C) -> SqliteLedger<C, H> {
        SqliteLedger {
            journal: self.clone(),
            clock: Arc::new(clock),
            cursor: Arc::default(),
        }
    }
}

/// Runs `f` in one `BEGIN IMMEDIATE` transaction: committed when it returns `Ok`, rolled back
/// otherwise.
fn transaction<T>(
    conn: &Connection,
    f: impl FnOnce(&Connection) -> Result<T, Failure>,
) -> Result<T, Failure> {
    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(|e| classify(&e))?;
    let rollback = |conn: &Connection| {
        if !conn.is_autocommit() {
            let _ = conn.execute_batch("ROLLBACK");
        }
    };
    match f(conn) {
        Ok(value) => match conn.execute_batch("COMMIT") {
            Ok(()) => Ok(value),
            Err(e) => {
                rollback(conn);
                Err(classify(&e))
            }
        },
        Err(failure) => {
            rollback(conn);
            Err(failure)
        }
    }
}

fn exec(conn: &Connection, sql: &str, values: impl rusqlite::Params) -> Result<usize, Failure> {
    conn.prepare_cached(sql)
        .and_then(|mut s| s.execute(values))
        .map_err(|e| classify(&e))
}

fn plus(now: Millis, by: Millis) -> Result<i64, Failure> {
    sql_millis(Millis::new(now.get().saturating_add(by.get())))
}

fn sql_fence(fence: LeaseFence) -> Result<i64, Failure> {
    i64::try_from(fence.get()).map_err(|_| Failure::Storage(StorageError::new("fence_exhausted")))
}

/// Whether the claim's lease is still the claim's and alive at `now`: its fence and an expiry
/// after the store clock, read inside the transaction.
fn held(conn: &Connection, claim: &Claim, now: i64) -> Result<bool, Failure> {
    let row = conn
        .prepare_cached(
            "SELECT fence, lease_until FROM consumer_lease WHERE consumer = ?1 AND stream_id = ?2",
        )
        .and_then(|mut s| {
            s.query_row(
                params![claim.consumer.as_str(), claim.stream.as_str()],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()
        })
        .map_err(|e| classify(&e))?;
    let fence = sql_fence(claim.fence)?;
    Ok(row.is_some_and(|(stored, until)| stored == fence && until > now))
}

/// A candidate mark: its stream, its revision and the checkpoint.
type Candidate = (String, i64, Option<i64>);

const CANDIDATES_AFTER: &str = "SELECT d.stream_id, d.marked, c.revision FROM stream_dispatch d \
     LEFT JOIN consumer_checkpoint c ON c.consumer = d.consumer AND c.stream_id = d.stream_id \
     LEFT JOIN consumer_lease l ON l.consumer = d.consumer AND l.stream_id = d.stream_id \
     WHERE d.consumer = ?1 AND (?2 IS NULL OR d.stream_id = ?2) \
     AND (?3 IS NULL OR d.stream_id > ?3) \
     AND (d.retry_at IS NULL OR d.retry_at <= ?4) \
     AND (l.lease_until IS NULL OR l.lease_until <= ?4) \
     AND (c.revision IS NULL OR c.revision < d.marked) \
     ORDER BY d.stream_id LIMIT ?5";

const CANDIDATES_THROUGH: &str = "SELECT d.stream_id, d.marked, c.revision FROM stream_dispatch d \
     LEFT JOIN consumer_checkpoint c ON c.consumer = d.consumer AND c.stream_id = d.stream_id \
     LEFT JOIN consumer_lease l ON l.consumer = d.consumer AND l.stream_id = d.stream_id \
     WHERE d.consumer = ?1 AND (?2 IS NULL OR d.stream_id = ?2) \
     AND d.stream_id <= ?3 \
     AND (d.retry_at IS NULL OR d.retry_at <= ?4) \
     AND (l.lease_until IS NULL OR l.lease_until <= ?4) \
     AND (c.revision IS NULL OR c.revision < d.marked) \
     ORDER BY d.stream_id LIMIT ?5";

fn candidates(
    conn: &Connection,
    sql: &str,
    consumer: &ConsumerName,
    only: Option<&str>,
    cursor: Option<&str>,
    now: i64,
    limit: i64,
) -> Result<Vec<Candidate>, Failure> {
    conn.prepare_cached(sql)
        .and_then(|mut s| {
            s.query_map(params![consumer.as_str(), only, cursor, now, limit], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<Result<Vec<Candidate>, _>>()
        })
        .map_err(|e| classify(&e))
}

fn claim_in(
    conn: &Connection,
    consumer: &ConsumerName,
    only: Option<&StreamId>,
    cursor: Option<&StreamId>,
    now: Millis,
    ttl: Millis,
    limit: u32,
) -> Result<Vec<Claim>, Failure> {
    let at = sql_millis(now)?;
    let until = plus(now, ttl)?;
    let only = only.map(StreamId::as_str);
    let mut found = candidates(
        conn,
        CANDIDATES_AFTER,
        consumer,
        only,
        cursor.map(StreamId::as_str),
        at,
        i64::from(limit),
    )?;
    let short = u32::try_from(found.len()).map_or(0, |n| limit.saturating_sub(n));
    if let Some(cursor) = cursor.filter(|_| short > 0) {
        found.extend(candidates(
            conn,
            CANDIDATES_THROUGH,
            consumer,
            only,
            Some(cursor.as_str()),
            at,
            i64::from(short),
        )?);
    }
    found.sort();
    let mut claims = Vec::with_capacity(found.len());
    for (stream, marked, checkpoint) in found {
        let fence: Option<i64> = conn
            .prepare_cached(
                "INSERT INTO consumer_lease (consumer, stream_id, lease_until, fence) \
                 VALUES (?1, ?2, ?3, 1) ON CONFLICT (consumer, stream_id) \
                 DO UPDATE SET lease_until = excluded.lease_until, fence = consumer_lease.fence + 1 \
                 WHERE consumer_lease.lease_until <= ?4 RETURNING fence",
            )
            .and_then(|mut s| {
                s.query_row(params![consumer.as_str(), stream, until, at], |r| r.get(0))
                    .optional()
            })
            .map_err(|e| classify(&e))?;
        let Some(fence) = fence else {
            // Another handle took the lease between the read and the upsert: not ours.
            continue;
        };
        claims.push(Claim {
            consumer: consumer.clone(),
            stream: stream_of(&stream)?,
            fence: LeaseFence::new(u64::try_from(fence).map_err(|_| unreadable())?),
            checkpoint: checkpoint.map(revision_of).transpose()?,
            marked: revision_of(marked)?,
        });
    }
    Ok(claims)
}

fn bootstrap_in(
    conn: &Connection,
    consumer: &ConsumerName,
    after: Option<&StreamId>,
    limit: u32,
) -> Result<Option<StreamId>, Failure> {
    let known = conn
        .prepare_cached("SELECT 1 FROM delivery_consumer WHERE consumer = ?1")
        .and_then(|mut s| {
            s.query_row(params![consumer.as_str()], |_| Ok(()))
                .optional()
        })
        .map_err(|e| classify(&e))?;
    if known.is_none() {
        return Err(Failure::Storage(StorageError::new("unknown_consumer")));
    }
    let page: Vec<(String, i64)> = conn
        .prepare_cached(
            "SELECT stream_id, revision FROM stream_head \
             WHERE ?1 IS NULL OR stream_id > ?1 ORDER BY stream_id LIMIT ?2",
        )
        .and_then(|mut s| {
            s.query_map(
                params![after.map(StreamId::as_str), i64::from(limit)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|e| classify(&e))?;
    for (stream, head) in &page {
        exec(
            conn,
            "INSERT INTO stream_dispatch (consumer, stream_id, marked, retry_at) \
             SELECT ?1, ?2, ?3, NULL WHERE NOT EXISTS (SELECT 1 FROM consumer_checkpoint \
             WHERE consumer = ?1 AND stream_id = ?2 AND revision >= ?3) \
             ON CONFLICT (consumer, stream_id) DO UPDATE SET marked = excluded.marked \
             WHERE excluded.marked > stream_dispatch.marked",
            params![consumer.as_str(), stream, head],
        )?;
    }
    if page.len() < usize::try_from(limit).unwrap_or(usize::MAX) {
        exec(
            conn,
            "UPDATE delivery_consumer SET bootstrapped = 1 WHERE consumer = ?1",
            params![consumer.as_str()],
        )?;
        return Ok(None);
    }
    page.last().map(|(stream, _)| stream_of(stream)).transpose()
}

impl<C: StoreClock + 'static, H: StoreHooks> SqliteLedger<C, H> {
    /// Runs `f` in one write transaction with the store clock's reading taken inside it.
    async fn transact<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection, Millis) -> Result<T, Failure> + Send + 'static,
    ) -> Result<T, StorageError> {
        let clock = Arc::clone(&self.clock);
        self.journal
            .write(move |conn| transaction(conn, |conn| f(conn, clock.now())))
            .await
    }

    /// Runs a fenced step: `f` runs only while the claim's lease is held.
    async fn fenced(
        &self,
        claim: &Claim,
        f: impl FnOnce(&Connection, i64) -> Result<(), Failure> + Send + 'static,
    ) -> Result<(), LeaseError> {
        let claim = claim.clone();
        let held = self
            .transact(move |conn, now| {
                let now = sql_millis(now)?;
                if !held(conn, &claim, now)? {
                    return Ok(false);
                }
                f(conn, now).map(|()| true)
            })
            .await?;
        if held { Ok(()) } else { Err(LeaseError::Lost) }
    }
}

impl<C: StoreClock + 'static, H: StoreHooks> DeliveryLedger for SqliteLedger<C, H> {
    async fn registered(&self) -> Result<Vec<ConsumerState>, StorageError> {
        let rows: Vec<(String, i64)> = self
            .journal
            .read(|conn| {
                conn.prepare_cached(
                    "SELECT consumer, bootstrapped FROM delivery_consumer ORDER BY consumer",
                )
                .and_then(|mut s| {
                    s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                        .collect::<Result<Vec<_>, _>>()
                })
                .map_err(|e| classify(&e))
            })
            .await?;
        rows.into_iter()
            .map(|(name, bootstrapped)| {
                Ok(ConsumerState {
                    name: ConsumerName::new(&name)
                        .map_err(|_| StorageError::new("row_unreadable"))?,
                    bootstrapped: bootstrapped == 1,
                })
            })
            .collect()
    }

    async fn register(&self, consumer: &ConsumerName) -> Result<(), StorageError> {
        let consumer = consumer.clone();
        self.transact(move |conn, now| {
            exec(
                conn,
                "INSERT INTO delivery_consumer (consumer, registered_at, bootstrapped) \
                 VALUES (?1, ?2, 0) ON CONFLICT (consumer) DO NOTHING",
                params![consumer.as_str(), sql_millis(now)?],
            )
            .map(|_| ())
        })
        .await
    }

    async fn bootstrap(
        &self,
        consumer: &ConsumerName,
        after: Option<&StreamId>,
        limit: u32,
    ) -> Result<Option<StreamId>, StorageError> {
        let (consumer, after) = (consumer.clone(), after.cloned());
        let limit = limit.clamp(1, MAX_PAGE);
        self.transact(move |conn, _| bootstrap_in(conn, &consumer, after.as_ref(), limit))
            .await
    }

    async fn claim(
        &self,
        consumer: &ConsumerName,
        ttl: Millis,
        limit: u32,
        only: Option<&StreamId>,
    ) -> Result<Vec<Claim>, StorageError> {
        let limit = limit.min(MAX_PAGE);
        if limit == 0 {
            return Ok(Vec::new());
        }
        let cursor = self
            .cursor
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(consumer)
            .cloned();
        let (name, only) = (consumer.clone(), only.cloned());
        let claims = self
            .transact(move |conn, now| {
                claim_in(conn, &name, only.as_ref(), cursor.as_ref(), now, ttl, limit)
            })
            .await?;
        if let Some(last) = claims.last() {
            self.cursor
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(consumer.clone(), last.stream.clone());
        }
        Ok(claims)
    }

    async fn renew(&self, claim: &Claim, ttl: Millis) -> Result<(), LeaseError> {
        let (consumer, stream, fence) = (
            claim.consumer.clone(),
            claim.stream.clone(),
            sql_fence(claim.fence).map_err(|f| LeaseError::Storage(f.storage_error()))?,
        );
        self.fenced(claim, move |conn, now| {
            exec(
                conn,
                "UPDATE consumer_lease SET lease_until = ?3 \
                 WHERE consumer = ?1 AND stream_id = ?2 AND fence = ?4",
                params![
                    consumer.as_str(),
                    stream.as_str(),
                    now.saturating_add(i64::try_from(ttl.get()).unwrap_or(i64::MAX)),
                    fence
                ],
            )
            .map(|_| ())
        })
        .await
    }

    async fn advance(
        &self,
        claim: &Claim,
        to: StreamRevision,
        ttl: Millis,
    ) -> Result<(), LeaseError> {
        let (consumer, stream, fence) = (
            claim.consumer.clone(),
            claim.stream.clone(),
            sql_fence(claim.fence).map_err(|f| LeaseError::Storage(f.storage_error()))?,
        );
        let to = sql_revision(to).map_err(|f| LeaseError::Storage(f.storage_error()))?;
        self.fenced(claim, move |conn, now| {
            let (c, s) = (consumer.as_str(), stream.as_str());
            exec(
                conn,
                "UPDATE consumer_lease SET lease_until = ?3 \
                 WHERE consumer = ?1 AND stream_id = ?2 AND fence = ?4",
                params![
                    c,
                    s,
                    now.saturating_add(i64::try_from(ttl.get()).unwrap_or(i64::MAX)),
                    fence
                ],
            )?;
            exec(
                conn,
                "INSERT INTO consumer_checkpoint (consumer, stream_id, revision) \
                 VALUES (?1, ?2, ?3) ON CONFLICT (consumer, stream_id) \
                 DO UPDATE SET revision = excluded.revision \
                 WHERE excluded.revision > consumer_checkpoint.revision",
                params![c, s, to],
            )?;
            exec(
                conn,
                "DELETE FROM stream_dispatch WHERE consumer = ?1 AND stream_id = ?2 AND marked <= ?3",
                params![c, s, to],
            )
            .map(|_| ())
        })
        .await
    }

    async fn release(&self, claim: &Claim, retry_after: Option<Millis>) -> Result<(), LeaseError> {
        let (consumer, stream, fence) = (
            claim.consumer.clone(),
            claim.stream.clone(),
            sql_fence(claim.fence).map_err(|f| LeaseError::Storage(f.storage_error()))?,
        );
        let retry = retry_after
            .map(|after| i64::try_from(after.get()))
            .transpose()
            .map_err(|_| LeaseError::Storage(StorageError::new("instant_out_of_range")))?;
        self.fenced(claim, move |conn, now| {
            let (c, s) = (consumer.as_str(), stream.as_str());
            exec(
                conn,
                "UPDATE consumer_lease SET lease_until = ?3 \
                 WHERE consumer = ?1 AND stream_id = ?2 AND fence = ?4",
                params![c, s, now, fence],
            )?;
            if let Some(after) = retry {
                exec(
                    conn,
                    "UPDATE stream_dispatch SET retry_at = ?3 WHERE consumer = ?1 AND stream_id = ?2",
                    params![c, s, now.saturating_add(after)],
                )?;
            }
            Ok(())
        })
        .await
    }

    async fn retire(&self, consumer: &ConsumerName) -> Result<(), StorageError> {
        let consumer = consumer.clone();
        self.transact(move |conn, _| {
            let c = consumer.as_str();
            exec(
                conn,
                "UPDATE consumer_lease SET lease_until = 0 WHERE consumer = ?1",
                params![c],
            )?;
            exec(
                conn,
                "DELETE FROM consumer_checkpoint WHERE consumer = ?1",
                params![c],
            )?;
            exec(
                conn,
                "DELETE FROM stream_dispatch WHERE consumer = ?1",
                params![c],
            )?;
            exec(
                conn,
                "DELETE FROM delivery_consumer WHERE consumer = ?1",
                params![c],
            )
            .map(|_| ())
        })
        .await
    }
}
