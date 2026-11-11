//! The projection contract's own tests (ver §5): the memory store passes it, and each
//! deliberately broken store, spec or rebuild decision of `doc/event-store.md` §7.6 fails the
//! cases named for its fault. Each test pins the exact set of failed cases.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use mayfly_control::journal::{
    AggregateName, AppendError, Appended, CommandReceipt, EventLocation, InboxKey, Journal,
    Payload, ProjectionName, RecordedEvent, StorageError, StoredKind, StreamHead, StreamId,
    StreamRevision, UnitOfWork, row_key_in_stream,
};
use mayfly_control::projection::{
    BuildId, Cutover, EventMeta, Generation, Projection, ProjectionMeta, ProjectionRebuild,
    ProjectionSpec, ProjectionStore, Projections, RowChange, Rows, StreamRows,
};
use mayfly_control_domain::pool::{Pool, PoolEvent, PoolState};
use mayfly_control_domain::{EventKind, EventSet};
use mayfly_domain::{CommandId, EventId};
use mayfly_testkit::block_on;
use mayfly_testkit::contracts::journal::InjectedFault;
use mayfly_testkit::contracts::projections::{
    ProjectionFactory, SuiteOptions, SuitePrepare, check_projection_contract,
    check_projection_contract_with,
};
use mayfly_testkit::journal::{MemoryJournal, MemoryProjectionFactory};

/// The fault a broken store carries, named as in `doc/event-store.md` §7.6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flaw {
    /// `RowlessJournal`: commits a unit's events without its projection rows.
    Rowless,
    /// `MergingRowsJournal`: puts a unit's rows over the stream's stored ones and keeps those
    /// the unit no longer lists (the delete targets, and what a missed write left).
    Merging,
    /// `LeakyRowsJournal`: writes a refused unit's rows anyway.
    Leaky,
    /// `UnscopedRowsJournal`: no locality check; a write outside the unit lands.
    Unscoped,
    /// `InPlaceRebuildStore`: a rebuild writes the active generation, not a new one.
    InPlaceRebuild,
    /// `UncheckedCutoverStore`: the cutover activates whatever the generation holds.
    UncheckedCutover,
    /// `SharedGenerationStore`: every rebuild takes the active generation plus one, so a second
    /// rebuild shares (and clears) the first's.
    SharedGeneration,
}

#[derive(Debug, Clone)]
struct Broken {
    inner: MemoryJournal,
    flaw: Flaw,
}

impl Journal for Broken {
    async fn append(&self, mut unit: UnitOfWork) -> Result<Appended, AppendError> {
        match self.flaw {
            Flaw::Rowless => unit.projections.clear(),
            Flaw::Merging => {
                let mut stored = Vec::new();
                for write in &unit.projections {
                    let rows = self
                        .inner
                        .stream_rows(&write.projection, &write.stream)
                        .await
                        .map_err(AppendError::Storage)?;
                    stored.push((write.projection.clone(), write.stream.clone(), rows.rows));
                }
                let appended = self.inner.append(unit).await?;
                for (spec, stream, rows) in stored {
                    self.inner.keep_rows(&spec, &stream, rows);
                }
                return Ok(appended);
            }
            Flaw::Leaky => {
                let writes = unit.projections.clone();
                let result = self.inner.append(unit).await;
                if result.is_err() {
                    for write in writes {
                        self.inner.force_rows(write, StreamRevision::new(0));
                    }
                }
                return result;
            }
            Flaw::Unscoped => {
                let appended: Vec<StreamId> =
                    unit.appends.iter().map(|a| a.stream.clone()).collect();
                let (inside, outside): (Vec<_>, Vec<_>) =
                    unit.projections.drain(..).partition(|w| {
                        appended.contains(&w.stream)
                            && w.rows.iter().all(|r| row_key_in_stream(&w.stream, &r.key))
                    });
                unit.projections = inside;
                let result = self.inner.append(unit).await;
                if result.is_ok() {
                    for write in outside {
                        self.inner.force_rows(write, StreamRevision::new(0));
                    }
                }
                return result;
            }
            Flaw::InPlaceRebuild | Flaw::UncheckedCutover | Flaw::SharedGeneration => {}
        }
        self.inner.append(unit).await
    }

    async fn head(&self, stream: &StreamId) -> Result<Option<StreamHead>, StorageError> {
        self.inner.head(stream).await
    }

    async fn read_stream(
        &self,
        stream: &StreamId,
        after: Option<StreamRevision>,
        through: Option<StreamRevision>,
    ) -> Result<Vec<RecordedEvent>, StorageError> {
        self.inner.read_stream(stream, after, through).await
    }

    async fn lookup_event(&self, event_id: EventId) -> Result<Option<EventLocation>, StorageError> {
        self.inner.lookup_event(event_id).await
    }

    async fn receipt(&self, command_id: CommandId) -> Result<Option<CommandReceipt>, StorageError> {
        self.inner.receipt(command_id).await
    }

    async fn inbox_contains(&self, key: &InboxKey) -> Result<bool, StorageError> {
        self.inner.inbox_contains(key).await
    }

    async fn streams(
        &self,
        after: Option<&StreamId>,
        limit: u32,
    ) -> Result<Vec<StreamHead>, StorageError> {
        self.inner.streams(after, limit).await
    }

    async fn stored_kinds(&self) -> Result<Vec<StoredKind>, StorageError> {
        self.inner.stored_kinds().await
    }

    async fn projection(
        &self,
        projection: &ProjectionName,
        key: &str,
    ) -> Result<Option<Payload>, StorageError> {
        self.inner.projection(projection, key).await
    }
}

impl ProjectionStore for Broken {
    async fn meta(&self) -> Result<Vec<ProjectionMeta>, StorageError> {
        self.inner.meta().await
    }

    async fn stream_rows(
        &self,
        spec: &ProjectionName,
        stream: &StreamId,
    ) -> Result<StreamRows, StorageError> {
        self.inner.stream_rows(spec, stream).await
    }

    async fn row(&self, spec: &ProjectionName, key: &str) -> Result<Option<Payload>, StorageError> {
        self.inner.row(spec, key).await
    }

    async fn rows(
        &self,
        spec: &ProjectionName,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<(String, Payload)>, StorageError> {
        self.inner.rows(spec, after, limit).await
    }
}

impl ProjectionRebuild for Broken {
    async fn begin_rebuild(&self, spec: &ProjectionName) -> Result<Generation, StorageError> {
        if self.flaw == Flaw::SharedGeneration {
            return Ok(self.inner.begin_rebuild_unclaimed(spec));
        }
        self.inner.begin_rebuild(spec).await
    }

    async fn apply_stream(
        &self,
        spec: &ProjectionName,
        generation: Generation,
        stream: &StreamId,
        rows: BTreeMap<String, Payload>,
        applied: StreamRevision,
    ) -> Result<(), StorageError> {
        match self.flaw {
            Flaw::InPlaceRebuild => {
                self.inner
                    .overwrite_active_rows(spec, stream, rows, applied);
                Ok(())
            }
            Flaw::SharedGeneration => self
                .inner
                .apply_stream_unclaimed(spec, generation, stream, rows, applied),
            _ => {
                self.inner
                    .apply_stream(spec, generation, stream, rows, applied)
                    .await
            }
        }
    }

    async fn cutover(
        &self,
        spec: &ProjectionName,
        aggregate: &AggregateName,
        generation: Generation,
        version: u32,
        written_by: &BuildId,
    ) -> Result<Cutover, StorageError> {
        if self.flaw == Flaw::UncheckedCutover {
            self.inner
                .cutover_unchecked(spec, generation, version, written_by)?;
            return Ok(Cutover::Active);
        }
        self.inner
            .cutover(spec, aggregate, generation, version, written_by)
            .await
    }

    async fn behind(
        &self,
        spec: &ProjectionName,
        aggregate: &AggregateName,
    ) -> Result<bool, StorageError> {
        self.inner.behind(spec, aggregate).await
    }
}

struct BrokenFactory(Flaw);

impl ProjectionFactory for BrokenFactory {
    type Store = Broken;

    async fn fresh(&self) -> Broken {
        Broken {
            inner: MemoryJournal::new(),
            flaw: self.0,
        }
    }

    async fn reopen(&self, store: Broken) -> Broken {
        store
    }

    async fn inject(&self, store: &Broken, fault: InjectedFault) -> bool {
        store.inner.inject(fault)
    }
}

fn failed_with(flaw: Flaw) -> Vec<&'static str> {
    block_on(check_projection_contract(&BrokenFactory(flaw))).failed_ids()
}

/// A clock every read of which moves on, as a wall clock would between the live fold and a
/// rebuild.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// `ClockReadingProjection`: a row stamped with a clock read in `apply`.
struct ClockReading;

impl ProjectionSpec for ClockReading {
    const NAME: &'static str = "clock_reading";
    const VERSION: u32 = 1;
    const SUBSCRIBES: &'static [EventKind] = <PoolEvent as EventSet>::KINDS;
    type Source = Pool;
    type Row = u64;

    fn apply(
        _rows: &Rows<u64>,
        _event: &PoolEvent,
        _after: &PoolState,
        _at: EventMeta,
    ) -> Vec<RowChange<u64>> {
        vec![RowChange::put("", TICKS.fetch_add(1, Ordering::SeqCst))]
    }
}

/// The order in which streams were first folded, across every stream.
static FIRST_SEEN: AtomicU64 = AtomicU64::new(0);

/// `OrderDependentProjection`: a row numbering its stream by when the fold first met it, so
/// live order (command order) and rebuild order (key order) disagree.
struct OrderDependent;

impl ProjectionSpec for OrderDependent {
    const NAME: &'static str = "order_dependent";
    const VERSION: u32 = 1;
    const SUBSCRIBES: &'static [EventKind] = <PoolEvent as EventSet>::KINDS;
    type Source = Pool;
    type Row = u64;

    fn apply(
        rows: &Rows<u64>,
        _event: &PoolEvent,
        _after: &PoolState,
        at: EventMeta,
    ) -> Vec<RowChange<u64>> {
        if at.revision.get() == 0 || rows.is_empty() {
            return vec![RowChange::put(
                "",
                FIRST_SEEN.fetch_add(1, Ordering::SeqCst),
            )];
        }
        Vec::new()
    }
}

fn failed_with_spec(
    projection: Result<Projection, mayfly_control::projection::ProjectionError>,
) -> Vec<&'static str> {
    let projections = projection
        .and_then(|p| Projections::new(vec![p]))
        .unwrap_or_else(|e| panic!("{e}"));
    let options = SuiteOptions {
        projections,
        prepare: SuitePrepare::Production,
    };
    block_on(check_projection_contract_with(
        &MemoryProjectionFactory,
        &options,
    ))
    .failed_ids()
}

fn failed_with_prepare(prepare: SuitePrepare) -> Vec<&'static str> {
    let options = SuiteOptions {
        prepare,
        ..SuiteOptions::default()
    };
    block_on(check_projection_contract_with(
        &MemoryProjectionFactory,
        &options,
    ))
    .failed_ids()
}

#[test]
fn should_pass_projection_contract_when_store_is_in_memory() {
    // given
    let factory = MemoryProjectionFactory;

    // when
    let report = block_on(check_projection_contract(&factory));

    // then
    assert!(report.is_green(), "{report}");
}

#[test]
fn should_fail_every_case_reading_rows_when_a_store_drops_the_rows() {
    // given
    let flaw = Flaw::Rowless;

    // when
    let failed = failed_with(flaw);

    // then
    assert_eq!(
        failed,
        [
            "PC-01", "PC-02", "PC-03", "PC-05", "PC-06", "PC-10", "PC-11", "PC-12"
        ]
    );
}

#[test]
fn should_fail_inline_rows_commit_when_a_store_merges_rows_over_the_stored_ones() {
    // given
    let flaw = Flaw::Merging;

    // when
    let failed = failed_with(flaw);

    // then
    assert_eq!(failed, ["PC-01", "PC-03", "PC-10", "PC-11", "PC-12"]);
}

#[test]
fn should_fail_refused_unit_leaves_none_when_a_store_writes_refused_rows() {
    // given
    let flaw = Flaw::Leaky;

    // when
    let failed = failed_with(flaw);

    // then
    assert_eq!(failed, ["PC-04", "PC-05"]);
}

#[test]
fn should_fail_outside_unit_refused_when_a_store_skips_the_locality_check() {
    // given
    let flaw = Flaw::Unscoped;

    // when
    let failed = failed_with(flaw);

    // then
    assert_eq!(failed, ["PC-05"]);
}

#[test]
fn should_fail_version_bump_rebuilds_when_a_rebuild_writes_the_active_generation() {
    // given
    let flaw = Flaw::InPlaceRebuild;

    // when
    let failed = failed_with(flaw);

    // then
    assert_eq!(
        failed,
        ["PC-01", "PC-02", "PC-06", "PC-10", "PC-11", "PC-12"]
    );
}

#[test]
fn should_fail_rebuild_equals_live_when_a_projection_reads_a_clock() {
    // given
    let spec = Projection::of::<ClockReading>();

    // when
    let failed = failed_with_spec(spec);

    // then
    assert_eq!(failed, ["PC-01", "PC-03", "PC-10", "PC-11", "PC-12"]);
}

#[test]
fn should_fail_rebuild_equals_live_when_a_projection_depends_on_stream_order() {
    // given
    let spec = Projection::of::<OrderDependent>();

    // when
    let failed = failed_with_spec(spec);

    // then
    assert_eq!(failed, ["PC-01", "PC-03", "PC-10", "PC-11", "PC-12"]);
}

#[test]
fn should_fail_version_below_rebuilds_when_a_start_rebuilds_only_upward() {
    // given
    let prepare = SuitePrepare::UpwardOnly;

    // when
    let failed = failed_with_prepare(prepare);

    // then
    assert_eq!(failed, ["PC-06", "PC-10"]);
}

#[test]
fn should_fail_both_version_cases_when_a_start_ignores_versions() {
    // given
    let prepare = SuitePrepare::VersionBlind;

    // when
    let failed = failed_with_prepare(prepare);

    // then
    assert_eq!(failed, ["PC-02", "PC-06", "PC-10"]);
}

#[test]
fn should_fail_rebuild_waits_for_units_when_a_cutover_checks_no_stream() {
    // given
    let flaw = Flaw::UncheckedCutover;

    // when
    let failed = failed_with(flaw);

    // then
    assert_eq!(failed, ["PC-11"]);
}

#[test]
fn should_fail_one_rebuild_wins_when_rebuilds_share_a_generation() {
    // given
    let flaw = Flaw::SharedGeneration;

    // when
    let failed = failed_with(flaw);

    // then
    assert_eq!(failed, ["PC-12"]);
}

#[test]
fn should_fail_missed_rows_heal_when_a_start_ignores_streams_behind_their_head() {
    // given
    let prepare = SuitePrepare::StaleBlind;

    // when
    let failed = failed_with_prepare(prepare);

    // then
    assert_eq!(failed, ["PC-10"]);
}
