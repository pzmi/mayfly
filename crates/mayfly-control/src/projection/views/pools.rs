//! `pools` and `recent_failures`: each pool's mode, configuration and breakers, and the
//! failures its breakers count, kept inline from each pool stream (`doc/domain-model.md` §8).

use mayfly_control_domain::attempt::GuestOs;
use mayfly_control_domain::pool::{
    AttemptFailureCounted, Pool, PoolBreakerReset, PoolEvent, PoolMode, PoolState, TrustTier,
};
use mayfly_control_domain::{EventKind, EventSet};
use mayfly_domain::{AttemptId, ImageDigest, Millis, Resources};
use serde::{Deserialize, Serialize};

use crate::projection::spec::{EventMeta, ProjectionSpec, RowChange, Rows, sync_rows};

/// A tripped breaker of a pool: the image whose failures tripped it, and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrippedBreaker {
    /// The image.
    pub digest: ImageDigest,
    /// When it tripped.
    pub at: Millis,
}

/// One pool (`mayfly pool list`; an input of `AdmissionExplain`). Key: the pool's stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolRow {
    /// The mode, `snake_case` (`active`, `paused`, `draining`, …).
    pub mode: String,
    /// The configuration's revision, when configured.
    pub config_revision: Option<u64>,
    /// The trust tier.
    pub tier: Option<TrustTier>,
    /// The guest OS.
    pub os: Option<GuestOs>,
    /// What each VM needs.
    pub vm_resources: Option<Resources>,
    /// The most VMs it runs.
    pub max_running: Option<u32>,
    /// How many prepared VMs it keeps.
    pub min_prepared: Option<u32>,
    /// The pinned image.
    pub image_pin: Option<ImageDigest>,
    /// The tripped breakers, by image.
    pub tripped: Vec<TrippedBreaker>,
    /// The stream revision this row is the fold of.
    pub revision: u64,
}

/// The `pools` projection: one row per pool stream.
#[derive(Debug, Clone, Copy, Default)]
pub struct Pools;

impl ProjectionSpec for Pools {
    const NAME: &'static str = "pools";
    const VERSION: u32 = 1;
    const SUBSCRIBES: &'static [EventKind] = <PoolEvent as EventSet>::KINDS;
    type Source = Pool;
    type Row = PoolRow;

    fn apply(
        rows: &Rows<PoolRow>,
        _event: &PoolEvent,
        after: &PoolState,
        at: EventMeta,
    ) -> Vec<RowChange<PoolRow>> {
        let config = after.config;
        let row = PoolRow {
            mode: mode_code(after.mode).to_owned(),
            config_revision: config.map(|c| c.revision),
            tier: config.map(|c| c.tier),
            os: config.map(|c| c.os),
            vm_resources: config.map(|c| c.vm_resources),
            max_running: config.map(|c| c.max_running),
            min_prepared: config.map(|c| c.min_prepared),
            image_pin: after.image_pin.clone(),
            tripped: after
                .breakers
                .iter()
                .filter_map(|(digest, breaker)| {
                    breaker.tripped_at.map(|at| TrippedBreaker {
                        digest: digest.clone(),
                        at,
                    })
                })
                .collect(),
            revision: at.revision.get(),
        };
        sync_rows(rows, [(String::new(), row)].into_iter().collect())
    }
}

/// The mode's `snake_case` code.
const fn mode_code(mode: PoolMode) -> &'static str {
    match mode {
        PoolMode::Unconfigured => "unconfigured",
        PoolMode::Active => "active",
        PoolMode::Paused => "paused",
        PoolMode::Draining => "draining",
        PoolMode::Drained => "drained",
        PoolMode::Decommissioned => "decommissioned",
    }
}

/// One attempt failure a pool's breaker still counts: within the breaker's window as of the
/// pool's last counted failure, and not reset. Key: `<pool stream>/<digest>/<attempt id>`;
/// deleted when the window drops it or an operator resets the breaker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureRow {
    /// The image the attempt ran.
    pub digest: ImageDigest,
    /// The attempt that failed.
    pub attempt_id: AttemptId,
    /// When the failure was counted (the command's time, from the event).
    pub at: Millis,
}

/// The `recent_failures` projection: the failures each pool's breakers count. A count across
/// pools is a query over these rows ([`crate::queries::failures_by_pool`]), never an inline
/// write.
#[derive(Debug, Clone, Copy, Default)]
pub struct RecentFailures;

impl ProjectionSpec for RecentFailures {
    const NAME: &'static str = "recent_failures";
    const VERSION: u32 = 1;
    /// A failure is counted (the window is pruned first) or a breaker reset (its failures go).
    const SUBSCRIBES: &'static [EventKind] = &[
        EventKind::of::<AttemptFailureCounted>(),
        EventKind::of::<PoolBreakerReset>(),
    ];
    type Source = Pool;
    type Row = FailureRow;

    fn apply(
        rows: &Rows<FailureRow>,
        _event: &PoolEvent,
        after: &PoolState,
        _at: EventMeta,
    ) -> Vec<RowChange<FailureRow>> {
        let desired = after
            .breakers
            .iter()
            .flat_map(|(digest, breaker)| {
                breaker.failures.iter().map(move |failure| {
                    (
                        format!("{digest}/{}", failure.attempt_id),
                        FailureRow {
                            digest: digest.clone(),
                            attempt_id: failure.attempt_id,
                            at: failure.at,
                        },
                    )
                })
            })
            .collect();
        sync_rows(rows, desired)
    }
}
