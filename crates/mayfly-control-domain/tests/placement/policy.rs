//! The placement manager's pure pass (P17, P18): `placement::policy::plan` over hand-built
//! inputs.

use std::collections::BTreeMap;

use mayfly_control_domain::placement::policy::{
    DueDirective, ImageView, PinView, PlacementAction, PlacementView, PolicyInputs, pinned_by, plan,
};
use mayfly_control_domain::placement::{
    Pending, PendingOutcome, PlacementDirective, PlacementPhase,
};
use mayfly_domain::{Deadline, DirectiveId, HostId, ImageDigest, Millis, PoolName};

use super::support::{digest, other_digest, runner};

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap_or_else(|e| panic!("{e}"))
}

fn pool(name: &str) -> PoolName {
    PoolName::new(name).unwrap_or_else(|e| panic!("{e}"))
}

fn pin(name: &str, image: ImageDigest) -> PinView {
    PinView {
        pool: pool(name),
        digest: image,
        tripped: false,
    }
}

fn image(image: ImageDigest) -> ImageView {
    ImageView {
        digest: image,
        runner_version: Some(runner(330)),
        expired_at: None,
    }
}

fn placement(on: &str, image: ImageDigest, phase: PlacementPhase) -> PlacementView {
    let materialised = matches!(phase, PlacementPhase::Materialised);
    PlacementView {
        host_id: host(on),
        digest: image,
        phase,
        wanted: Some(runner(330)),
        base: materialised.then_some(runner(330)),
        pending: None,
    }
}

fn inputs(pins: Vec<PinView>, placements: Vec<PlacementView>) -> PolicyInputs {
    PolicyInputs {
        now: Millis::new(100_000),
        reply_grace: Millis::new(5_000),
        hosts: vec![host("h1"), host("h2")],
        pins,
        images: vec![image(digest()), image(other_digest())],
        placements,
        references: BTreeMap::new(),
    }
}

#[test]
fn should_request_the_pin_on_every_host_when_a_pool_pins_an_image() {
    // given: h2 already has it materialised
    let given = inputs(
        vec![pin("mac", digest())],
        vec![placement("h2", digest(), PlacementPhase::Materialised)],
    );

    // when
    let actions = plan(&given);

    // then: P17, a placement per host serving the pool, once
    assert_eq!(
        actions,
        vec![PlacementAction::Request {
            host_id: host("h1"),
            digest: digest(),
            runner_version: runner(330),
        }]
    );
}

#[test]
fn should_issue_the_directive_due_when_nothing_is_pending() {
    // given
    let mut pending = placement("h2", digest(), PlacementPhase::Pulling);
    pending.pending = Some(Pending {
        directive_id: DirectiveId::from_u128(1),
        directive: PlacementDirective::PullImage,
        deadline: Deadline::after(Millis::new(0), Millis::new(600_000)),
        sent: true,
        sent_epoch: None,
        outcome: PendingOutcome::InFlight,
    });
    let given = inputs(
        vec![pin("mac", digest())],
        vec![placement("h1", digest(), PlacementPhase::Wanted), pending],
    );

    // when
    let actions = plan(&given);

    // then: a pull on h1; h2's pull is in flight, before its deadline: nothing
    assert_eq!(
        actions,
        vec![PlacementAction::Issue {
            host_id: host("h1"),
            digest: digest(),
            kind: DueDirective::Pull,
        }]
    );
}

#[test]
fn should_make_the_outcome_unknown_when_a_sent_directive_passes_its_deadline() {
    // given: sent with a deadline at 90 s, read at 100 s with 5 s of grace
    let mut late = placement("h1", digest(), PlacementPhase::Pulling);
    late.pending = Some(Pending {
        directive_id: DirectiveId::from_u128(7),
        directive: PlacementDirective::PullImage,
        deadline: Deadline::after(Millis::new(0), Millis::new(90_000)),
        sent: true,
        sent_epoch: None,
        outcome: PendingOutcome::InFlight,
    });
    let mut unsent = late.clone();
    unsent.host_id = host("h2");
    if let Some(p) = unsent.pending.as_mut() {
        p.sent = false;
    }
    let given = inputs(vec![pin("mac", digest())], vec![late, unsent]);

    // when
    let actions = plan(&given);

    // then: I10, only the sent one; a directive held unsent did nothing
    assert_eq!(
        actions,
        vec![PlacementAction::Timeout {
            host_id: host("h1"),
            digest: digest(),
            directive_id: DirectiveId::from_u128(7),
        }]
    );
}

#[test]
fn should_evict_an_unpinned_base_only_when_no_clone_references_it() {
    // given: the pool moved to the other image; the old one is materialised on both hosts and a
    // live clone on h1 still runs it
    let mut given = inputs(
        vec![pin("mac", other_digest())],
        vec![
            placement("h1", digest(), PlacementPhase::Materialised),
            placement("h2", digest(), PlacementPhase::Materialised),
            placement("h1", other_digest(), PlacementPhase::Materialised),
            placement("h2", other_digest(), PlacementPhase::Materialised),
        ],
    );
    given.references.insert((host("h1"), digest()), 1);

    // when
    let actions = plan(&given);

    // then: P17, h2's old base only; the pinned image's bases stay
    assert_eq!(
        actions,
        vec![PlacementAction::Evict {
            host_id: host("h2"),
            digest: digest(),
        }]
    );
}

#[test]
fn should_not_request_again_when_a_placement_failed() {
    // given
    let given = inputs(
        vec![pin("mac", digest())],
        vec![
            placement("h1", digest(), PlacementPhase::Failed),
            placement("h2", digest(), PlacementPhase::Evicted),
        ],
    );

    // when
    let actions = plan(&given);

    // then: the failure stays shown on h1; h2's evicted placement starts again
    assert_eq!(
        actions,
        vec![PlacementAction::Request {
            host_id: host("h2"),
            digest: digest(),
            runner_version: runner(330),
        }]
    );
}

#[test]
fn should_trip_the_breaker_once_when_the_pinned_runner_version_expired() {
    // given
    let mut given = inputs(
        vec![pin("amac", digest()), {
            let mut tripped = pin("blin", digest());
            tripped.tripped = true;
            tripped
        }],
        vec![
            placement("h1", digest(), PlacementPhase::Materialised),
            placement("h2", digest(), PlacementPhase::Materialised),
        ],
    );
    given.images[0].expired_at = Some(Millis::new(50_000));

    // when
    let actions = plan(&given);

    // then: P18, the pool whose breaker is still closed
    assert_eq!(
        actions,
        vec![PlacementAction::Trip {
            pool: pool("amac"),
            digest: digest(),
            at: Millis::new(50_000),
        }]
    );
}

#[test]
fn should_plan_nothing_new_when_a_pass_reads_its_own_result() {
    // given: every pin materialised everywhere, nothing pending, nothing unreferenced
    let given = inputs(
        vec![pin("mac", digest())],
        vec![
            placement("h1", digest(), PlacementPhase::Materialised),
            placement("h2", digest(), PlacementPhase::Materialised),
        ],
    );

    // when
    let actions = plan(&given);

    // then
    assert!(actions.is_empty(), "{actions:?}");
}

#[test]
fn should_name_the_pinning_pool_when_a_withdrawal_is_checked() {
    // given
    let pins = vec![pin("mac", digest())];

    // when
    let pinned = pinned_by(&pins, &digest());
    let free = pinned_by(&pins, &other_digest());

    // then: P17, `WithdrawImage` is refused while a pool pins the image
    assert_eq!(pinned, Some(&pool("mac")));
    assert_eq!(free, None);
}
