//! The projection spec: what one read model is, as a pure fold over one stream
//! (`doc/event-store.md` §3.6, plan §5.5).

use std::collections::BTreeMap;
use std::fmt::Debug;

use mayfly_control_domain::{Aggregate, EventKind};
use mayfly_domain::Millis;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::journal::StreamRevision;

/// One stream's rows of a projection while it is folded, by suffix: `""` is the stream's own
/// row (stored under the stream id), any other suffix `s` the row stored under `<stream>/<s>`.
pub type Rows<R> = BTreeMap<String, R>;

/// The event being applied: its stream's aggregate event type.
pub type SpecEvent<S> = <<S as ProjectionSpec>::Source as Aggregate>::Event;

/// The source aggregate's state after the event: what most rows are a view of.
pub type SpecState<S> = <<S as ProjectionSpec>::Source as Aggregate>::State;

/// Where the event sits in its stream. Both come from the journal, never from a clock, so a
/// rebuild sees exactly what the live fold saw (ES-I11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventMeta {
    /// The event's revision.
    pub revision: StreamRevision,
    /// Its unit's `recorded_at`: informational, for display (ADR 0020).
    pub recorded_at: Millis,
}

/// One row the fold writes or removes, by suffix within the stream ([`Rows`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowChange<R> {
    /// `""` for the stream's own row, else the part after `<stream>/`. Never with a control
    /// character (the fold refuses it, `ProjectionError::Row`). Any length: a suffix that would
    /// make the stored key longer than `MAX_ROW_KEY`, or that starts with `#`, is stored under
    /// `<stream>/#<its SHA-256>` (the fold's `key_of`).
    pub suffix: String,
    /// The new row, or `None` to delete it (a delete target).
    pub row: Option<R>,
}

impl<R> RowChange<R> {
    /// Writes `row` under `suffix`.
    pub fn put(suffix: impl Into<String>, row: R) -> Self {
        Self {
            suffix: suffix.into(),
            row: Some(row),
        }
    }

    /// Deletes the row under `suffix`.
    pub fn delete(suffix: impl Into<String>) -> Self {
        Self {
            suffix: suffix.into(),
            row: None,
        }
    }
}

/// A read model kept inline by the unit that changes it (`doc/event-store.md` §3.6): its name,
/// version, source aggregate, the event kinds it reads, and a pure fold from one stream's rows
/// and one event to the rows that change.
///
/// **Locality (ES-I10).** `apply` sees one stream: its own rows, the event and the aggregate's
/// state after it. It never sees a stream id and returns suffixes, which the fold turns into
/// keys of that stream only, so a spec cannot write another stream's row; a cross-stream count
/// is a query over rows ([`crate::queries`]). `ARCH-projection-locality` keeps the code of an
/// `impl ProjectionSpec` from naming a stream, a projection write or the journal.
///
/// **Purity.** `apply` reads no clock, randomness, environment or global state: the live fold
/// (inline, outside the transaction) and a rebuild must compute the same rows from the same
/// events (contract case PC-01, which `ClockReadingProjection` and `OrderDependentProjection`
/// fail: `crates/mayfly-testkit/tests/projection_contract.rs`).
///
/// **Evolution (§6.1).** A compatible change of the rows bumps `VERSION`, which rebuilds them at
/// the next start (PC-02, also when the stored version is the higher one, PC-06); a change of
/// meaning is a new `NAME` beside the old spec.
pub trait ProjectionSpec {
    /// The projection's name, `[a-z][a-z0-9_]{0,63}`.
    const NAME: &'static str;
    /// The version of its rows, from 1.
    const VERSION: u32;
    /// The exact event kinds `apply` is called for; every other event of the stream still
    /// evolves the state. No catch-all: a kind missing here leaves the rows behind, which
    /// `mayfly_testkit::contracts::projections::check_subscriptions` finds.
    const SUBSCRIBES: &'static [EventKind];
    /// The aggregate whose streams the projection reads.
    type Source: Aggregate;
    /// A row, stored as its serde form (a JSON document).
    type Row: Clone + Debug + PartialEq + Serialize + DeserializeOwned;

    /// The rows that change when `event` is applied to a stream whose rows are `rows`; `after`
    /// is the aggregate's state with the event applied.
    fn apply(
        rows: &Rows<Self::Row>,
        event: &SpecEvent<Self>,
        after: &SpecState<Self>,
        at: EventMeta,
    ) -> Vec<RowChange<Self::Row>>;
}

/// The rows of a projection that are a pure view of the stream's state: `desired` replaces
/// `rows`, as the changes that turn one into the other (puts for new or changed rows, deletes
/// for rows no longer desired). Pinned by `should_diff_rows_into_puts_and_deletes_when_synced`.
#[must_use]
pub fn sync_rows<R: Clone + PartialEq>(rows: &Rows<R>, desired: Rows<R>) -> Vec<RowChange<R>> {
    let mut changes: Vec<RowChange<R>> = rows
        .keys()
        .filter(|suffix| !desired.contains_key(*suffix))
        .map(|suffix| RowChange::delete(suffix.clone()))
        .collect();
    changes.extend(
        desired
            .into_iter()
            .filter(|(suffix, row)| rows.get(suffix) != Some(row))
            .map(|(suffix, row)| RowChange::put(suffix, row)),
    );
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_diff_rows_into_puts_and_deletes_when_synced() {
        // given
        let rows: Rows<u32> = [
            ("a".to_owned(), 1),
            ("b".to_owned(), 2),
            ("c".to_owned(), 3),
        ]
        .into_iter()
        .collect();
        let desired: Rows<u32> = [
            ("b".to_owned(), 2),
            ("c".to_owned(), 4),
            ("d".to_owned(), 5),
        ]
        .into_iter()
        .collect();

        // when
        let changes = sync_rows(&rows, desired);

        // then
        assert_eq!(
            changes,
            [
                RowChange::delete("a"),
                RowChange::put("c", 4),
                RowChange::put("d", 5),
            ]
        );
    }
}
