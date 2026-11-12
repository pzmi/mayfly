//! The image placement manager's pass (policies P17 and P18 of `doc/domain-model.md` §7): one
//! pure function, [`plan`], from what the read side shows of pins, images, placements and live
//! clones to the commands that move each host's image bases toward the pins. The driver
//! (`mayfly_control::placement`) reads the projections, calls it and issues each command through
//! `AggregateStore`, where the aggregate decides it again; it decides nothing itself.
//!
//! **Level-triggered**, like the reconciler's pass: a pass reads the whole state and plans from
//! it alone, and a pass over its own result plans nothing new (pinned by
//! `tests/placement/policy.rs::should_plan_nothing_new_when_a_pass_reads_its_own_result`).
//!
//! What it plans:
//!
//! - **`RequestPlacement`** of each pool's pin on every host (a host serves every pool), with
//!   the runner version of the pinned image's manifest, while the host has no placement of it or
//!   an evicted one. A failed placement is not requested again: the pool stays not admissible on
//!   that host with the failure shown (P17's failure path), never retried in a loop on a bad
//!   image.
//! - **`IssueDirective`** for each placement with a directive due and none pending: a pull, a
//!   materialisation (the first base, or a refresh's), an eviction. One at a time per placement,
//!   which the aggregate enforces (one pull per host and digest).
//! - **`RecordOutcomeUnknown`** for a directive sent whose deadline, plus the reply grace, has
//!   passed with no answer (I10: never a success; it leaves `Unknown` only on an observation).
//! - **`RequestEviction`** of a materialised placement whose digest no pool pins and no live
//!   lease on that host references (P17: "old digest unreferenced → `EvictionRequested`"); the
//!   aggregate refuses a referenced one, and the data plane re-checks its dependents (ADR 0087).
//! - **`TripBreaker{runner_version}`** for a pool whose pinned image's runner version expired
//!   and whose breaker for it is closed (P18, ADR 0030 decision 2): admission stops with a named
//!   reason.
//!
//! What it does not plan: the retirement of prepared attempts on an old digest is the
//! reconciler's (`image_replaced`, CP-03); a pool's admissibility on a host follows from the
//! placements through the reconciler's `HostView::placed` (CP-07); a refresh, a rollback and
//! the eviction of a previous base are the operator's (`mayfly image refresh`, OPS-06).

use std::collections::{BTreeMap, BTreeSet};

use mayfly_domain::{DirectiveId, HostId, ImageDigest, Millis, PoolName};

use super::state::{Pending, PendingOutcome, PlacementPhase};
use crate::image::RunnerVersion;

/// One placement as the pass reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementView {
    /// The host.
    pub host_id: HostId,
    /// The image.
    pub digest: ImageDigest,
    /// Its phase.
    pub phase: PlacementPhase,
    /// The runner version the next materialisation installs.
    pub wanted: Option<RunnerVersion>,
    /// The runner version of the base clones are made from.
    pub base: Option<RunnerVersion>,
    /// The pending directive.
    pub pending: Option<Pending>,
}

/// One pool's pin as the pass reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinView {
    /// The pool.
    pub pool: PoolName,
    /// Its pinned image.
    pub digest: ImageDigest,
    /// Its breaker for that image is tripped.
    pub tripped: bool,
}

/// One image as the pass reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageView {
    /// The image.
    pub digest: ImageDigest,
    /// Its manifest's runner version, once registered.
    pub runner_version: Option<RunnerVersion>,
    /// When its runner version's expiry was recorded (P18).
    pub expired_at: Option<Millis>,
}

/// Everything [`plan`] reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyInputs {
    /// The control plane's clock, read by the driver.
    pub now: Millis,
    /// Past a deadline, the time an answer sent at the deadline may still take to arrive.
    pub reply_grace: Millis,
    /// The registered hosts: each serves every pool.
    pub hosts: Vec<HostId>,
    /// Each pinned pool's pin.
    pub pins: Vec<PinView>,
    /// The images, by digest.
    pub images: Vec<ImageView>,
    /// Every placement.
    pub placements: Vec<PlacementView>,
    /// Live leases (quarantined ones included) by host and the image their attempt runs: the
    /// clones that reference a base.
    pub references: BTreeMap<(HostId, ImageDigest), u32>,
}

/// A command the pass plans; the driver gives each its id and envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementAction {
    /// `RequestPlacement` on the placement of `digest` on `host_id`.
    Request {
        /// The host.
        host_id: HostId,
        /// The image.
        digest: ImageDigest,
        /// The runner version of its first base.
        runner_version: RunnerVersion,
    },
    /// `IssueDirective` on the placement.
    Issue {
        /// The host.
        host_id: HostId,
        /// The image.
        digest: ImageDigest,
        /// The directive that is due.
        kind: DueDirective,
    },
    /// `RecordOutcomeUnknown` on the placement.
    Timeout {
        /// The host.
        host_id: HostId,
        /// The image.
        digest: ImageDigest,
        /// The directive whose deadline passed.
        directive_id: DirectiveId,
    },
    /// `RequestEviction{scope: placement, references: 0}` on the placement.
    Evict {
        /// The host.
        host_id: HostId,
        /// The image.
        digest: ImageDigest,
    },
    /// `TripBreaker{reason: runner_version}` on the pool.
    Trip {
        /// The pool.
        pool: PoolName,
        /// Its pinned image.
        digest: ImageDigest,
        /// When the expiry was recorded.
        at: Millis,
    },
}

/// Which directive a placement is due, for its deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DueDirective {
    /// A pull.
    Pull,
    /// A materialisation.
    Materialise,
    /// An eviction.
    Evict,
}

/// The directive due on `placement`, if one is and none is pending: the aggregate's own rule
/// (`decide`'s `due`), read from the view.
#[must_use]
pub fn due(placement: &PlacementView) -> Option<DueDirective> {
    if placement.pending.is_some() {
        return None;
    }
    match placement.phase {
        PlacementPhase::Wanted => Some(DueDirective::Pull),
        PlacementPhase::Pulled | PlacementPhase::Materialised => placement
            .wanted
            .filter(|wanted| placement.base != Some(*wanted))
            .map(|_| DueDirective::Materialise),
        PlacementPhase::Evicting => Some(DueDirective::Evict),
        PlacementPhase::Unrequested
        | PlacementPhase::Pulling
        | PlacementPhase::Evicted
        | PlacementPhase::Failed => None,
    }
}

/// Whether a placement in `phase` is requested and not evicted: a new `RequestPlacement` is due
/// only otherwise (a failed one is not requested again, P17's failure path).
const fn requested(phase: PlacementPhase) -> bool {
    match phase {
        PlacementPhase::Unrequested | PlacementPhase::Evicted => false,
        PlacementPhase::Wanted
        | PlacementPhase::Pulling
        | PlacementPhase::Pulled
        | PlacementPhase::Materialised
        | PlacementPhase::Evicting
        | PlacementPhase::Failed => true,
    }
}

/// Whether `pending` was sent and its deadline plus `grace` has passed at `now` unanswered.
fn timed_out(pending: &Pending, now: Millis, grace: Millis) -> bool {
    pending.sent
        && pending.outcome == PendingOutcome::InFlight
        && pending.deadline.instant().get().saturating_add(grace.get()) <= now.get()
}

/// One placement manager pass: pure, total and deterministic (the order of the inputs does not
/// matter; the commands come out ordered by host, then image, then pool). Pinned by
/// `tests/placement/policy.rs`.
#[must_use]
pub fn plan(inputs: &PolicyInputs) -> Vec<PlacementAction> {
    let hosts: BTreeSet<&HostId> = inputs.hosts.iter().collect();
    let pinned: BTreeSet<&ImageDigest> = inputs.pins.iter().map(|p| &p.digest).collect();
    let images: BTreeMap<&ImageDigest, &ImageView> =
        inputs.images.iter().map(|i| (&i.digest, i)).collect();
    let placements: BTreeMap<(&HostId, &ImageDigest), &PlacementView> = inputs
        .placements
        .iter()
        .map(|p| ((&p.host_id, &p.digest), p))
        .collect();
    let mut actions = Vec::new();
    for host in &hosts {
        for digest in &pinned {
            let Some(runner_version) = images.get(digest).and_then(|i| i.runner_version) else {
                continue;
            };
            let requested = placements
                .get(&(*host, *digest))
                .is_some_and(|p| requested(p.phase));
            if !requested {
                actions.push(PlacementAction::Request {
                    host_id: (*host).clone(),
                    digest: (*digest).clone(),
                    runner_version,
                });
            }
        }
    }
    for ((host, digest), placement) in &placements {
        if !hosts.contains(host) {
            continue;
        }
        if let Some(pending) = &placement.pending
            && timed_out(pending, inputs.now, inputs.reply_grace)
        {
            actions.push(PlacementAction::Timeout {
                host_id: (*host).clone(),
                digest: (*digest).clone(),
                directive_id: pending.directive_id,
            });
            continue;
        }
        if let Some(kind) = due(placement) {
            actions.push(PlacementAction::Issue {
                host_id: (*host).clone(),
                digest: (*digest).clone(),
                kind,
            });
            continue;
        }
        let referenced = inputs
            .references
            .get(&((*host).clone(), (*digest).clone()))
            .copied()
            .unwrap_or(0);
        if placement.phase == PlacementPhase::Materialised
            && placement.pending.is_none()
            && !pinned.contains(digest)
            && referenced == 0
        {
            actions.push(PlacementAction::Evict {
                host_id: (*host).clone(),
                digest: (*digest).clone(),
            });
        }
    }
    let mut pins: Vec<&PinView> = inputs.pins.iter().collect();
    pins.sort_by(|a, b| a.pool.cmp(&b.pool));
    for pin in pins {
        if pin.tripped {
            continue;
        }
        if let Some(at) = images.get(&pin.digest).and_then(|i| i.expired_at) {
            actions.push(PlacementAction::Trip {
                pool: pin.pool.clone(),
                digest: pin.digest.clone(),
                at,
            });
        }
    }
    actions
}

/// The pool that pins `digest`, if any: `WithdrawImage` is refused while one does (P17). The
/// driver saves the withdrawal with a read of every pool stream at the revision it read, so a
/// pin committed after this check refuses the withdrawal at the save.
#[must_use]
pub fn pinned_by<'p>(pins: &'p [PinView], digest: &ImageDigest) -> Option<&'p PoolName> {
    pins.iter().find(|p| p.digest == *digest).map(|p| &p.pool)
}
