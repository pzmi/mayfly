//! The demand rules of plan §7.2 (`mayfly_control_domain::demand`): desired from statistics,
//! the shortfall, the wire max of each mode, trust classification and `PlanAcquire`. The named
//! killers of curated mutants 5, 6, 11 and 19 (verification §9) are here; `formal/scaleset.qnt`
//! kills 4, 5, 6, 7 and 19 in the model.

use std::collections::BTreeSet;

use mayfly_control_domain::attempt::RunnerRequestId;
use mayfly_control_domain::demand::{
    self, Budget, Capacity, DeclineReason, Declined, EventName, Offer, OfferDecision, RunnerCounts,
    Statistics, TrustClass, WireMode,
};
use mayfly_control_domain::pool::TrustTier;
use proptest::prelude::*;

fn statistics(assigned: u32, running: u32) -> Statistics {
    Statistics {
        total_assigned_jobs: assigned,
        total_running_jobs: running,
        ..Statistics::default()
    }
}

fn capacity(deliverable: u32, mode: WireMode) -> Capacity {
    Capacity {
        revision: 1,
        deliverable,
        reported_max: deliverable,
        mode,
    }
}

fn offer(id: u64, event_name: EventName) -> Offer {
    Offer {
        request_id: RunnerRequestId(id),
        event_name,
    }
}

fn pushes(n: u64) -> Vec<Offer> {
    (1..=n).map(|id| offer(id, EventName::Push)).collect()
}

fn budget(deliverable: u32, assigned: u32) -> Budget {
    Budget {
        tier: TrustTier::Trusted,
        capacity: capacity(deliverable, WireMode::Open),
        frozen: false,
        assigned: Some(assigned),
        reserved: BTreeSet::new(),
    }
}

// ---- desired -----------------------------------------------------------------------------

/// Mutant 5 (count `JobAssigned` messages): a batch holds at most 50 job messages, so a backlog
/// is truncated; demand is `totalAssignedJobs`.
#[test]
fn should_take_demand_from_statistics_when_a_batch_shows_fewer_jobs_than_assigned() {
    // given: 120 jobs assigned, of which a batch could show at most 50
    let stats = statistics(120, 0);
    // when
    let desired = demand::desired(&stats, 200);
    // then
    assert_eq!(desired, 120);
}

/// Mutant 6 (add `TotalRunningJobs`): running jobs are part of `totalAssignedJobs` already.
#[test]
fn should_not_add_running_jobs_when_desired_is_computed() {
    // given: 5 assigned, 3 of them running
    let stats = statistics(5, 3);
    // when
    let desired = demand::desired(&stats, 10);
    // then
    assert_eq!(desired, 5);
}

/// No registered standby (ADR 0028): desired never exceeds what is assigned, nor deliverable.
#[test]
fn should_cap_desired_at_deliverable_when_more_is_assigned() {
    // given
    let stats = statistics(9, 0);
    // when
    let capped = demand::desired(&stats, 4);
    let idle = demand::desired(&statistics(0, 0), 4);
    // then
    assert_eq!((capped, idle), (4, 0));
}

// ---- shortfall ---------------------------------------------------------------------------

/// Mutant 11 (count a Prepared VM as a registered runner): a Prepared VM serves the shortfall
/// first and is not counted in it.
#[test]
fn should_not_count_prepared_vms_when_the_shortfall_is_computed() {
    // given: 5 desired, 1 activating, 1 listening, 1 busy and 2 prepared
    let runners = RunnerCounts {
        activating: 1,
        listening: 1,
        busy: 1,
        prepared: 2,
    };
    // when
    let shortfall = demand::shortfall(5, &runners);
    // then
    assert_eq!(shortfall, 2);
}

#[test]
fn should_floor_the_shortfall_at_zero_when_runners_exceed_desired() {
    // given
    let runners = RunnerCounts {
        activating: 0,
        listening: 3,
        busy: 2,
        prepared: 0,
    };
    // when
    let shortfall = demand::shortfall(4, &runners);
    // then
    assert_eq!(shortfall, 0);
}

// ---- wire max (ADR 0029) -----------------------------------------------------------------

#[test]
fn should_send_zero_when_draining_or_in_safe_mode() {
    // given
    let with = |mode, reported_max| Capacity {
        revision: 1,
        deliverable: 4,
        reported_max,
        mode,
    };
    // when
    let sent = [
        demand::wire_max(&with(WireMode::Open, 4)),
        demand::wire_max(&with(WireMode::Paused, 2)),
        demand::wire_max(&with(WireMode::Draining, 2)),
        demand::wire_max(&with(WireMode::SafeMode, 4)),
    ];
    // then: open sends the reported max, paused the live runners, drain and safe mode zero
    assert_eq!(sent, [4, 2, 0, 0]);
}

// ---- trust (ADR 0027, I32) ---------------------------------------------------------------

#[test]
fn should_decline_untrusted_and_unclassifiable_offers_when_the_pool_is_trusted() {
    // given
    let untrusted = [
        EventName::PullRequestTarget,
        EventName::WorkflowRun,
        EventName::IssueComment,
    ];
    let trusted = [
        EventName::Push,
        EventName::PullRequest,
        EventName::WorkflowDispatch,
        EventName::Schedule,
        EventName::MergeGroup,
        EventName::Release,
    ];
    // when
    let classes: Vec<TrustClass> = untrusted
        .iter()
        .chain(&trusted)
        .chain(&[EventName::Unknown])
        .map(|e| demand::trust(&offer(1, *e)))
        .collect();
    let decisions: Vec<OfferDecision> = [EventName::PullRequestTarget, EventName::Unknown]
        .iter()
        .map(|e| demand::decide_offer(&budget(10, 0), &offer(1, *e)))
        .collect();
    // then
    assert_eq!(&classes[..3], &[TrustClass::Untrusted; 3]);
    assert_eq!(&classes[3..9], &[TrustClass::Trusted; 6]);
    assert_eq!(classes[9], TrustClass::Unclassifiable);
    assert_eq!(
        decisions,
        vec![
            OfferDecision::Decline(DeclineReason::Tier),
            OfferDecision::Decline(DeclineReason::Unclassifiable)
        ]
    );
}

// ---- PlanAcquire (I23, mutant 19) --------------------------------------------------------

/// Mutant 19 (acquire every offer), and fault F41: 100 offers on 2 deliverable slots acquire 2;
/// the other 98 are declined for capacity.
#[test]
fn should_acquire_only_while_assigned_is_below_deliverable_when_offers_exceed_it() {
    // given
    let offers = pushes(100);
    // when
    let plan = demand::plan_acquire(&budget(2, 0), &offers);
    // then
    assert_eq!(plan.acquire, vec![RunnerRequestId(1), RunnerRequestId(2)]);
    assert_eq!(plan.declined.len(), 98);
    assert!(
        plan.declined
            .iter()
            .all(|d| d.reason == DeclineReason::NoCapacity)
    );
}

#[test]
fn should_acquire_nothing_when_assigned_has_reached_deliverable() {
    // given: 3 deliverable, 3 assigned
    let offers = pushes(1);
    // when
    let plan = demand::plan_acquire(&budget(3, 3), &offers);
    // then
    assert!(plan.acquire.is_empty());
}

/// A2: an open reservation is counted on top of the statistics, which may not show it yet.
#[test]
fn should_count_open_reservations_when_the_statistics_may_not_show_them() {
    // given: 3 deliverable, 1 assigned, 1 reserved
    let mut budget = budget(3, 1);
    budget.reserved.insert(RunnerRequestId(50));
    // when
    let plan = demand::plan_acquire(&budget, &pushes(3));
    // then
    assert_eq!(plan.acquire, vec![RunnerRequestId(1)]);
}

#[test]
fn should_never_reserve_a_request_twice_when_it_is_offered_again() {
    // given: 1 already reserved, offered again twice, with room for many
    let mut budget = budget(10, 0);
    budget.reserved.insert(RunnerRequestId(1));
    let offers = vec![
        offer(1, EventName::Push),
        offer(2, EventName::Push),
        offer(2, EventName::Push),
    ];
    // when
    let plan = demand::plan_acquire(&budget, &offers);
    // then
    assert_eq!(plan.acquire, vec![RunnerRequestId(2)]);
    assert_eq!(
        plan.declined,
        vec![
            Declined {
                request_id: RunnerRequestId(1),
                reason: DeclineReason::AlreadyReserved
            },
            Declined {
                request_id: RunnerRequestId(2),
                reason: DeclineReason::AlreadyReserved
            },
        ]
    );
}

#[test]
fn should_acquire_nothing_when_paused_draining_or_in_safe_mode() {
    // given
    let held = [WireMode::Paused, WireMode::Draining, WireMode::SafeMode];
    // when
    let plans: Vec<_> = held
        .iter()
        .map(|mode| {
            let mut budget = budget(10, 0);
            budget.capacity.mode = *mode;
            demand::plan_acquire(&budget, &pushes(2))
        })
        .collect();
    // then
    for plan in plans {
        assert!(plan.acquire.is_empty());
        assert!(
            plan.declined
                .iter()
                .all(|d| d.reason == DeclineReason::Held)
        );
    }
}

/// P21 and the KER-06 review (LOW6): a degraded or closing session plans no offer, whatever
/// the budget; the offer is declined as held.
#[test]
fn should_acquire_nothing_when_the_session_is_frozen() {
    // given
    let mut budget = budget(10, 0);
    budget.frozen = true;
    // when
    let plan = demand::plan_acquire(&budget, &pushes(2));
    // then
    assert!(plan.acquire.is_empty());
    assert!(
        plan.declined
            .iter()
            .all(|d| d.reason == DeclineReason::Held)
    );
}

#[test]
fn should_acquire_nothing_when_no_statistics_were_ever_received() {
    // given
    let mut budget = budget(10, 0);
    budget.assigned = None;
    // when
    let plan = demand::plan_acquire(&budget, &pushes(1));
    // then
    assert_eq!(plan.acquire, Vec::<RunnerRequestId>::new());
    assert_eq!(plan.declined[0].reason, DeclineReason::NoCapacity);
}

fn event_names() -> impl Strategy<Value = EventName> {
    prop::sample::select(vec![
        EventName::Push,
        EventName::PullRequest,
        EventName::PullRequestTarget,
        EventName::WorkflowDispatch,
        EventName::Schedule,
        EventName::MergeGroup,
        EventName::Release,
        EventName::WorkflowRun,
        EventName::IssueComment,
        EventName::Unknown,
    ])
}

fn modes() -> impl Strategy<Value = WireMode> {
    prop::sample::select(vec![
        WireMode::Open,
        WireMode::Paused,
        WireMode::Draining,
        WireMode::SafeMode,
    ])
}

proptest! {
    /// I23, I32, no double reservation and ADR 0029 over random batches and budgets.
    #[test]
    fn should_keep_every_acquire_rule_when_batches_are_random(
        deliverable in 0u32..6,
        assigned in 0u32..6,
        reserved in prop::collection::btree_set(0u64..8, 0..4),
        mode in modes(),
        frozen in any::<bool>(),
        offers in prop::collection::vec((0u64..8, event_names()), 0..20),
    ) {
        // given
        let budget = Budget {
            tier: TrustTier::Trusted,
            capacity: Capacity { revision: 1, deliverable, reported_max: deliverable, mode },
            frozen,
            assigned: Some(assigned),
            reserved: reserved.iter().copied().map(RunnerRequestId).collect(),
        };
        let offers: Vec<Offer> = offers.into_iter().map(|(id, e)| offer(id, e)).collect();
        // when
        let plan = demand::plan_acquire(&budget, &offers);
        // then
        let taken: BTreeSet<RunnerRequestId> = plan.acquire.iter().copied().collect();
        prop_assert_eq!(taken.len(), plan.acquire.len(), "a request planned twice");
        prop_assert!(taken.is_disjoint(&budget.reserved), "a reserved request planned again");
        prop_assert_eq!(plan.acquire.len() + plan.declined.len(), offers.len());
        if !plan.acquire.is_empty() {
            let open = budget.reserved.len() + taken.len();
            prop_assert!(assigned as usize + open <= deliverable as usize, "I23");
            prop_assert_eq!(mode, WireMode::Open, "acquired while held");
            prop_assert!(!frozen, "acquired while the session is frozen");
        }
        for id in &plan.acquire {
            let trusted = offers
                .iter()
                .any(|o| o.request_id == *id && demand::trust(o) == TrustClass::Trusted);
            prop_assert!(trusted, "I32");
        }
    }
}
