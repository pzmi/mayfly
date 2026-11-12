//! Demand (plan §7.2, `doc/domain-model.md` §6.1): pure functions over a scale set's latest
//! statistics, its capacity input and its offers. The `ScaleSet` aggregate ([`crate::scaleset`])
//! calls them inside `decide`; the listener (GH-04) and the reconciler (CP-03) call them too, so
//! the rules have one implementation.
//!
//! - [`desired`]: `min(deliverable, totalAssignedJobs)`. There is no registered standby (ADR
//!   0028); job messages are never counted (a batch holds at most 50, so a backlog is truncated:
//!   mutant 5) and `totalRunningJobs` is never added (it is part of `totalAssignedJobs` already:
//!   mutant 6). Pinned by
//!   `tests/demand.rs::should_take_demand_from_statistics_when_a_batch_shows_fewer_jobs_than_assigned`
//!   and `tests/demand.rs::should_not_add_running_jobs_when_desired_is_computed`.
//! - [`shortfall`]: what the reconciler still has to start; a Prepared VM serves it first and is
//!   not counted as a runner (mutant 11, `tests/demand.rs::should_not_count_prepared_vms_when_the_shortfall_is_computed`).
//! - [`wire_max`]: the `maxCapacity` header of every poll (ADR 0029): the capacity policy's
//!   reported max while open or paused (paused, it reports only live runners), 0 while draining or
//!   in safe mode (`tests/demand.rs::should_send_zero_when_draining_or_in_safe_mode`).
//! - [`trust`] and [`admits`]: the offer's trust class (ADR 0027) and whether the pool's tier
//!   takes it (I32).
//! - [`decide_offer`] and [`plan_acquire`]: `PlanAcquire`. In order: an offer the pool's tier does
//!   not admit is declined (I32: `tier` for an untrusted trigger, `unclassifiable` for one the
//!   classifier cannot tell, A5); a request id already reserved is never reserved again; nothing
//!   is acquired while paused, draining or in safe mode (ADR 0029), nor while the session is
//!   degraded or closing ([`Budget::frozen`], P21); otherwise it is acquired only
//!   while the assigned jobs plus every open reservation stay below deliverable (I23, mutant 19:
//!   `tests/demand.rs::should_acquire_only_while_assigned_is_below_deliverable_when_offers_exceed_it`).
//!
//! The model is `formal/scaleset.qnt` (`desiredOf`, `wireMax`, `offerStep`).
//!
//! **Assumptions** where `doc/github-scaleset.md` marks a fact client-assumed or [open: HW-02]
//! (§12), each on the safe side and named in the model:
//!
//! - **A2** (§12 items 2 and 3): whether a message's `totalAssignedJobs` already counts the jobs
//!   mayfly acquired since is not known. The budget therefore counts every open reservation on
//!   top of it ([`Budget::reserved`]): a job may be counted twice (mayfly acquires less until its
//!   `JobStarted` releases the reservation), never once too few (mayfly never holds a job it
//!   cannot serve, I23). When the capture shows that a message's statistics count the jobs its
//!   `JobAssigned` items name, a reservation can be released on `JobAssigned` instead.
//! - **A5** (§12 items 1 and 8): which `eventName` values offers carry, and whether they carry
//!   one at all, comes from real payloads only. An offer whose event name is missing or not in
//!   [`EventName`] is unclassifiable and declined (ADR 0027): until the capture lists the values,
//!   mayfly fails closed and acquires nothing it cannot classify.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::attempt::RunnerRequestId;
use crate::pool::TrustTier;

/// A scale set's statistics as GitHub reports them with a session and with every message
/// (`doc/github-scaleset.md` §2, §5). Every field is kept so the journal shows what GitHub said;
/// demand reads `total_assigned_jobs` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Statistics {
    /// `totalAvailableJobs`.
    pub total_available_jobs: u32,
    /// `totalAcquiredJobs`.
    pub total_acquired_jobs: u32,
    /// `totalAssignedJobs`: jobs waiting for a runner and jobs running. Demand.
    pub total_assigned_jobs: u32,
    /// `totalRunningJobs`: part of `total_assigned_jobs`, never added to it.
    pub total_running_jobs: u32,
    /// `totalRegisteredRunners`.
    pub total_registered_runners: u32,
    /// `totalBusyRunners`.
    pub total_busy_runners: u32,
    /// `totalIdleRunners`.
    pub total_idle_runners: u32,
}

/// The pool's mode as the capacity input carries it: what the scale set tells GitHub and whether
/// it acquires (ADR 0029). No mode closes the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireMode {
    /// Admitting: send the reported max, acquire within the budget.
    Open,
    /// Paused: send the live runners (the capacity policy's reported max), acquire nothing.
    Paused,
    /// Draining: send 0, acquire nothing.
    Draining,
    /// Safe mode with a writable journal: send 0, acquire nothing, ack only recorded messages.
    /// (With an unwritable journal nothing is recorded and the listener stops polling.)
    SafeMode,
}

/// The per-scale-set capacity input of the capacity policy (`capacity::plan`, I24): the pool's
/// deliverable carve-out and reported max, with the revision that orders inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capacity {
    /// Increases with every input; an older one is refused.
    pub revision: u64,
    /// Runners the pool can deliver ([`crate::capacity::PoolCapacity::deliverable`]).
    pub deliverable: u32,
    /// The capacity policy's `reported_max` (≤ deliverable).
    pub reported_max: u32,
    /// The pool's mode.
    pub mode: WireMode,
}

impl Capacity {
    /// The input before any: nothing deliverable, nothing reported, so nothing is acquired.
    pub const NONE: Self = Self {
        revision: 0,
        deliverable: 0,
        reported_max: 0,
        mode: WireMode::Open,
    };
}

/// `desired = min(deliverable, totalAssignedJobs)` (plan §7.2, ADR 0028).
#[must_use]
pub const fn desired(statistics: &Statistics, deliverable: u32) -> u32 {
    if statistics.total_assigned_jobs < deliverable {
        statistics.total_assigned_jobs
    } else {
        deliverable
    }
}

/// The pool's runners as the reconciler counts them for [`shortfall`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunnerCounts {
    /// Attempts activating (granted, minting or delivering a JIT config).
    pub activating: u32,
    /// Runners online and idle.
    pub listening: u32,
    /// Runners running a job.
    pub busy: u32,
    /// Prepared VMs: booted, unregistered, holding no secret (ADR 0028). They serve a shortfall
    /// first and are not runners.
    pub prepared: u32,
}

/// `shortfall = max(0, desired − (Activating + Listening + Busy))` (plan §7.2).
#[must_use]
pub const fn shortfall(desired: u32, runners: &RunnerCounts) -> u32 {
    let counted = runners
        .activating
        .saturating_add(runners.listening)
        .saturating_add(runners.busy);
    desired.saturating_sub(counted)
}

/// The `maxCapacity` sent on every poll (ADR 0029, I07, I24).
#[must_use]
pub const fn wire_max(capacity: &Capacity) -> u32 {
    match capacity.mode {
        WireMode::Open | WireMode::Paused => capacity.reported_max,
        WireMode::Draining | WireMode::SafeMode => 0,
    }
}

/// An offer's `eventName`, as the GitHub ACL maps it (A5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventName {
    /// `push`.
    Push,
    /// `pull_request`: trusted only under GitHub's fork-approval settings, which the
    /// documentation requires (ADR 0027).
    PullRequest,
    /// `pull_request_target`: runs with the base repository's secrets on a fork's request.
    PullRequestTarget,
    /// `workflow_dispatch`.
    WorkflowDispatch,
    /// `schedule`.
    Schedule,
    /// `merge_group`.
    MergeGroup,
    /// `release`.
    Release,
    /// `workflow_run`: triggered by another workflow, possibly a fork's.
    WorkflowRun,
    /// `issue_comment`: triggered by anyone who can comment.
    IssueComment,
    /// Missing, or a value not listed here.
    Unknown,
}

/// A `JobAvailable` offer, as the ACL hands it to the domain: the request id and what trust
/// classification reads. Its strings never reach the domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    /// The job's `runnerRequestId`.
    pub request_id: RunnerRequestId,
    /// What triggered the workflow.
    pub event_name: EventName,
}

/// An offer's trust class (ADR 0027).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustClass {
    /// A trusted trigger.
    Trusted,
    /// `pull_request_target`, `workflow_run` or `issue_comment`.
    Untrusted,
    /// The classifier cannot tell (A5).
    Unclassifiable,
}

/// Classifies an offer (pinned by
/// `tests/demand.rs::should_decline_untrusted_and_unclassifiable_offers_when_the_pool_is_trusted`).
#[must_use]
pub const fn trust(offer: &Offer) -> TrustClass {
    match offer.event_name {
        EventName::Push
        | EventName::PullRequest
        | EventName::WorkflowDispatch
        | EventName::Schedule
        | EventName::MergeGroup
        | EventName::Release => TrustClass::Trusted,
        EventName::PullRequestTarget | EventName::WorkflowRun | EventName::IssueComment => {
            TrustClass::Untrusted
        }
        EventName::Unknown => TrustClass::Unclassifiable,
    }
}

/// Whether a pool of `tier` takes an offer of `class` (I32).
#[must_use]
pub const fn admits(tier: TrustTier, class: TrustClass) -> bool {
    match (tier, class) {
        (TrustTier::Trusted, TrustClass::Trusted) => true,
        (TrustTier::Trusted, TrustClass::Untrusted | TrustClass::Unclassifiable) => false,
    }
}

/// Why an offer was not acquired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclineReason {
    /// Assigned plus open reservations reached deliverable (I23).
    NoCapacity,
    /// The pool's tier does not admit its trigger (I32).
    Tier,
    /// Its trigger is missing or unknown, so it cannot be classified (A5, ADR 0027); declined
    /// apart from `Tier` so the operator can tell a payload not yet server-observed from an
    /// untrusted trigger. Pinned by
    /// `tests/demand.rs::should_decline_untrusted_and_unclassifiable_offers_when_the_pool_is_trusted`.
    Unclassifiable,
    /// The pool is paused, draining or in safe mode (ADR 0029), or the session is degraded or
    /// closing ([`Budget::frozen`]).
    Held,
    /// Its request id is reserved already: a redelivery, a second session's copy or a repeat
    /// offer is never reserved twice.
    AlreadyReserved,
}

/// One declined offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declined {
    /// The job.
    pub request_id: RunnerRequestId,
    /// Why.
    pub reason: DeclineReason,
}

/// What `PlanAcquire` reads besides the offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Budget {
    /// The pool's tier.
    pub tier: TrustTier,
    /// The capacity input's mode and deliverable.
    pub capacity: Capacity,
    /// The session plans nothing: it is degraded (minting frozen, P21) or closing.
    pub frozen: bool,
    /// The latest statistics' `totalAssignedJobs`; `None` before any statistics: nothing is
    /// acquired blind.
    pub assigned: Option<u32>,
    /// Every open reservation: planned and pending (`acquiring`) or acquired and unbound
    /// (`acquired`), counted on top of `assigned` (A2).
    pub reserved: BTreeSet<RunnerRequestId>,
}

/// What `PlanAcquire` does with one offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferDecision {
    /// Reserve it and acquire it.
    Acquire,
    /// Leave it.
    Decline(DeclineReason),
}

/// `PlanAcquire` for one offer against `budget` (I32, then no double reservation, then the
/// mode and the session, then I23). Pure; [`plan_acquire`] folds it over a batch.
#[must_use]
pub fn decide_offer(budget: &Budget, offer: &Offer) -> OfferDecision {
    let class = trust(offer);
    if !admits(budget.tier, class) {
        return OfferDecision::Decline(match class {
            TrustClass::Unclassifiable => DeclineReason::Unclassifiable,
            TrustClass::Trusted | TrustClass::Untrusted => DeclineReason::Tier,
        });
    }
    if budget.reserved.contains(&offer.request_id) {
        return OfferDecision::Decline(DeclineReason::AlreadyReserved);
    }
    if budget.frozen {
        return OfferDecision::Decline(DeclineReason::Held);
    }
    match budget.capacity.mode {
        WireMode::Open => {}
        WireMode::Paused | WireMode::Draining | WireMode::SafeMode => {
            return OfferDecision::Decline(DeclineReason::Held);
        }
    }
    let Some(assigned) = budget.assigned else {
        return OfferDecision::Decline(DeclineReason::NoCapacity);
    };
    let reserved = u32::try_from(budget.reserved.len()).unwrap_or(u32::MAX);
    if assigned.saturating_add(reserved) < budget.capacity.deliverable {
        OfferDecision::Acquire
    } else {
        OfferDecision::Decline(DeclineReason::NoCapacity)
    }
}

/// The plan of one batch of offers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AcquirePlan {
    /// The request ids to acquire, in offer order.
    pub acquire: Vec<RunnerRequestId>,
    /// The offers left, in offer order.
    pub declined: Vec<Declined>,
}

/// `PlanAcquire` (plan §7.2 step 2): every offer of a batch, in order, each acquired one counted
/// in the budget of the next.
#[must_use]
pub fn plan_acquire(budget: &Budget, offers: &[Offer]) -> AcquirePlan {
    let mut budget = budget.clone();
    let mut plan = AcquirePlan::default();
    for offer in offers {
        match decide_offer(&budget, offer) {
            OfferDecision::Acquire => {
                budget.reserved.insert(offer.request_id);
                plan.acquire.push(offer.request_id);
            }
            OfferDecision::Decline(reason) => plan.declined.push(Declined {
                request_id: offer.request_id,
                reason,
            }),
        }
    }
    plan
}
