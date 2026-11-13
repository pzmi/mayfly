//! `hosts` and `scale_sets`: what the reconciler reads of each host besides its leases (its
//! latest applied report, reserves, gate and pool terms) and of each pool's scale set (its
//! latest statistics' demand and the capacity input it holds), kept inline from each host and
//! scale-set stream (`doc/domain-model.md` §8, `CapacityBoard`; CP-03).

use mayfly_control_domain::capacity::ReportedCapacity;
use mayfly_control_domain::demand::Capacity;
use mayfly_control_domain::host::{Drain, Host, HostEvent, HostState};
use mayfly_control_domain::scaleset::{Phase, ScaleSet, ScaleSetEvent, ScaleSetState};
use mayfly_control_domain::{EventKind, EventSet};
use mayfly_domain::{PoolName, Resources};
use serde::{Deserialize, Serialize};

use crate::projection::spec::{EventMeta, ProjectionSpec, RowChange, Rows, sync_rows};

/// A pool's terms as applied on a host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostTermsRow {
    /// The pool.
    pub pool: PoolName,
    /// The terms revision a reservation must carry.
    pub revision: u64,
    /// Whether the pool admits here.
    pub open: bool,
    /// Its `max_running`.
    pub max_running: u32,
}

/// One host's admission inputs. Key: the host's stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRow {
    /// The latest applied report's id: the reconciler allocates within this report only.
    pub report_id: Option<u64>,
    /// Its content.
    pub report: Option<ReportedCapacity>,
    /// The operator's reserves.
    pub reserves: Resources,
    /// Whether the gate is open (no drain, no low disk, no closing cause).
    pub admission_open: bool,
    /// The drain: `open`, `draining` or `drained`.
    pub drain: String,
    /// Pool terms as applied here, in pool order.
    pub terms: Vec<HostTermsRow>,
    /// The stream revision this row is the fold of.
    pub revision: u64,
}

/// The `hosts` projection: one row per registered host.
#[derive(Debug, Clone, Copy, Default)]
pub struct Hosts;

impl ProjectionSpec for Hosts {
    const NAME: &'static str = "hosts";
    const VERSION: u32 = 1;
    const SUBSCRIBES: &'static [EventKind] = <HostEvent as EventSet>::KINDS;
    type Source = Host;
    type Row = HostRow;

    fn apply(
        rows: &Rows<HostRow>,
        _event: &HostEvent,
        after: &HostState,
        at: EventMeta,
    ) -> Vec<RowChange<HostRow>> {
        if !after.registered {
            return sync_rows(rows, Rows::new());
        }
        let row = HostRow {
            report_id: after.capacity.map(|r| r.report_id),
            report: after.capacity.map(|r| r.capacity),
            reserves: after.reserves,
            admission_open: after.gate.is_open(),
            drain: match after.gate.drain {
                Drain::Open => "open",
                Drain::Draining { .. } => "draining",
                Drain::Drained => "drained",
            }
            .to_owned(),
            terms: after
                .pool_terms
                .iter()
                .map(|(pool, terms)| HostTermsRow {
                    pool: pool.clone(),
                    revision: terms.revision,
                    open: terms.open,
                    max_running: terms.max_running,
                })
                .collect(),
            revision: at.revision.get(),
        };
        sync_rows(rows, [(String::new(), row)].into_iter().collect())
    }
}

/// One pool's scale set. Key: the scale set's stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScaleSetRow {
    /// The pool it serves, once ensured.
    pub pool: Option<PoolName>,
    /// The session's phase, `snake_case`.
    pub phase: String,
    /// `totalAssignedJobs` of the latest statistics applied; none before any.
    pub assigned: Option<u32>,
    /// The capacity input it holds.
    pub capacity: Capacity,
    /// Jobs it classified trusted that wait for a runner (`ScaleSetState::waiting`, ADR 0090):
    /// what the reconciler's grant is gated on (`demand::runner_may_start`, CP-09). Absent from
    /// rows written by version 1, read as none: no grant. Pinned by
    /// `tests/store/activation.rs::should_show_the_trusted_jobs_waiting_on_the_scale_sets_row_when_a_job_is_acquired`.
    #[serde(default)]
    pub waiting: u32,
    /// Its session is active (`ScaleSetState::plans_offers`). Absent from rows written by
    /// version 1, read as not active: no grant.
    #[serde(default)]
    pub active: bool,
    /// The stream revision this row is the fold of.
    pub revision: u64,
}

/// The `scale_sets` projection: one row per scale-set stream.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScaleSets;

impl ProjectionSpec for ScaleSets {
    const NAME: &'static str = "scale_sets";
    /// 2: `waiting` and `active` (CP-09); compatible additions.
    const VERSION: u32 = 2;
    const SUBSCRIBES: &'static [EventKind] = <ScaleSetEvent as EventSet>::KINDS;
    type Source = ScaleSet;
    type Row = ScaleSetRow;

    fn apply(
        rows: &Rows<ScaleSetRow>,
        _event: &ScaleSetEvent,
        after: &ScaleSetState,
        at: EventMeta,
    ) -> Vec<RowChange<ScaleSetRow>> {
        let row = ScaleSetRow {
            pool: after.pool.clone(),
            phase: phase_code(after.phase).to_owned(),
            assigned: after.statistics.map(|s| s.total_assigned_jobs),
            capacity: after.capacity,
            waiting: after.waiting(),
            active: after.plans_offers(),
            revision: at.revision.get(),
        };
        sync_rows(rows, [(String::new(), row)].into_iter().collect())
    }
}

/// The session phase's `snake_case` code.
const fn phase_code(phase: Phase) -> &'static str {
    match phase {
        Phase::Unconfigured => "unconfigured",
        Phase::Ensured => "ensured",
        Phase::Opening => "opening",
        Phase::Active => "active",
        Phase::Backoff(..) => "backoff",
        Phase::Degraded(..) => "degraded",
        Phase::Closing => "closing",
        Phase::Closed => "closed",
    }
}
