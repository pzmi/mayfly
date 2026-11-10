//! Queries (plan §5.4): read models computed at query time over projection rows. A count across
//! streams (failures per pool, leases per host, unknown effects) is computed here from
//! stream-local rows, never kept by an inline write that would make one row a contention point
//! of many streams (ES-I10, `doc/event-store.md` §3.6).
//!
//! Queries take the read port [`ProjectionStore`] only, never a write port, and page through a
//! projection by key ([`ProjectionStore::rows`]), never by time or position (ES-I17).

use std::collections::BTreeMap;
use std::fmt;

use serde::de::DeserializeOwned;

use crate::journal::{MAX_PAGE, ProjectionName, StorageError};
use crate::projection::views::{FailureRow, PendingEffectRow, SlotRow};
use crate::projection::{ProjectionSpec, ProjectionStore, views};

/// Why a query failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum QueryError {
    /// The store failed.
    Storage(StorageError),
    /// A stored row is not of its projection's shape (rows of another version are rebuilt at
    /// start, so this is corruption).
    Row {
        /// The projection.
        projection: &'static str,
    },
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(e) => write!(f, "query failed: {e}"),
            Self::Row { projection } => write!(f, "query failed: a {projection} row is unreadable"),
        }
    }
}

impl std::error::Error for QueryError {}

/// Every row of projection `S`, by key.
///
/// # Errors
/// [`QueryError`].
pub async fn all_rows<S: ProjectionSpec, P: ProjectionStore>(
    store: &P,
) -> Result<Vec<(String, S::Row)>, QueryError> {
    let spec = ProjectionName::new(S::NAME).map_err(|_| QueryError::Row {
        projection: S::NAME,
    })?;
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = store
            .rows(&spec, after.as_deref(), MAX_PAGE)
            .await
            .map_err(QueryError::Storage)?;
        let Some((last, _)) = page.last() else {
            break;
        };
        after = Some(last.clone());
        for (key, payload) in page {
            out.push((key, decode::<S::Row>(S::NAME, payload.as_str())?));
        }
    }
    Ok(out)
}

fn decode<R: DeserializeOwned>(projection: &'static str, text: &str) -> Result<R, QueryError> {
    serde_json::from_str(text).map_err(|_| QueryError::Row { projection })
}

/// The stream a row key belongs to: the key up to its first `/`.
fn stream_of_key(key: &str) -> &str {
    key.split_once('/').map_or(key, |(stream, _)| stream)
}

/// The failures each pool's breakers count, by pool stream: the breaker panel's numbers.
/// Pinned by `crates/mayfly-control/tests/store/projections.rs::should_count_failures_per_pool_when_queried_across_streams`.
///
/// # Errors
/// [`QueryError`].
pub async fn failures_by_pool<P: ProjectionStore>(
    store: &P,
) -> Result<BTreeMap<String, Vec<FailureRow>>, QueryError> {
    let mut out: BTreeMap<String, Vec<FailureRow>> = BTreeMap::new();
    for (key, row) in all_rows::<views::RecentFailures, P>(store).await? {
        out.entry(stream_of_key(&key).to_owned())
            .or_default()
            .push(row);
    }
    Ok(out)
}

/// The live leases of each host, by host stream: the allocation side of `CapacityBoard`.
/// Pinned by `crates/mayfly-control/tests/store/projections.rs::should_list_leases_per_host_when_queried_across_streams`.
///
/// # Errors
/// [`QueryError`].
pub async fn leases_by_host<P: ProjectionStore>(
    store: &P,
) -> Result<BTreeMap<String, Vec<SlotRow>>, QueryError> {
    let mut out: BTreeMap<String, Vec<SlotRow>> = BTreeMap::new();
    for (key, row) in all_rows::<views::Slots, P>(store).await? {
        out.entry(stream_of_key(&key).to_owned())
            .or_default()
            .push(row);
    }
    Ok(out)
}

/// Every directive in flight or of unknown outcome, and how many are unknown: the
/// `PendingEffects` view and the `mayfly_unknown_effects` gauge's input (P19). Pinned by
/// `crates/mayfly-control/tests/store/projections.rs::should_list_pending_effects_and_count_the_unknown_when_queried`.
///
/// # Errors
/// [`QueryError`].
pub async fn pending_effects<P: ProjectionStore>(
    store: &P,
) -> Result<(Vec<PendingEffectRow>, usize), QueryError> {
    let rows: Vec<PendingEffectRow> = all_rows::<views::PendingEffects, P>(store)
        .await?
        .into_iter()
        .map(|(_, row)| row)
        .collect();
    let unknown = rows.iter().filter(|r| r.unknown_since.is_some()).count();
    Ok((rows, unknown))
}
