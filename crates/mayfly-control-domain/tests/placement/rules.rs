//! The placement's own rules (`doc/domain-model.md` §6.7, P17, ADRs 0030 and 0039): one pull per
//! host and digest, bases named by digest and runner version, eviction only unreferenced from
//! `Materialised`, the base pointer with its previous base for rollback, unknown outcomes (I10)
//! and the run-epoch fence (I18).

use mayfly_control_domain::image::base_name;
use mayfly_control_domain::placement::{
    EvictionScope, PendingOutcome, PlacementCommand, PlacementDirective, PlacementEvent,
    PlacementPhase, placement_instance,
};
use mayfly_domain::HostId;
use mayfly_domain::Sample;

use super::support::{
    EARLY, LATE, POLICY, apply, at, auto, digest, evict, evicted, issue, materialised, not_applied,
    outcome, pulled, reach, refresh, refused, request, runner, sent, to_materialised, to_refreshed,
    unknown,
};

fn issued(events: &[PlacementEvent]) -> Vec<PlacementDirective> {
    events
        .iter()
        .filter_map(|e| match e {
            PlacementEvent::PlacementDirectiveIssued(i) => Some(i.directive),
            _ => None,
        })
        .collect()
}

#[test]
fn should_pull_once_when_a_pull_is_pending_or_unknown() {
    // given
    let requested = reach([auto(request())]);
    let (pulling, first) = apply(&requested, auto(issue())).unwrap_or_else(|r| panic!("{r:?}"));
    let (sent_pull, _) = apply(&pulling, auto(sent())).unwrap_or_else(|r| panic!("{r:?}"));
    let (unknown_pull, _) =
        apply(&sent_pull, at(1, LATE, unknown())).unwrap_or_else(|r| panic!("{r:?}"));

    // when
    let while_in_flight = outcome(&pulling, auto(issue()));
    let while_unknown = outcome(&unknown_pull, auto(issue()));
    let (cleared, _) =
        apply(&unknown_pull, auto(not_applied())).unwrap_or_else(|r| panic!("{r:?}"));
    let (_, again) = apply(&cleared, auto(issue())).unwrap_or_else(|r| panic!("{r:?}"));

    // then: a second pull only once a complete listing showed the first did not apply
    assert_eq!(issued(&first), vec![PlacementDirective::PullImage]);
    assert_eq!(while_in_flight, "directive_pending");
    assert_eq!(while_unknown, "directive_pending");
    assert_eq!(cleared.phase, PlacementPhase::Wanted);
    assert_eq!(issued(&again), vec![PlacementDirective::PullImage]);
}

#[test]
fn should_record_nothing_when_the_digest_is_requested_again() {
    // given: every phase in which the placement is under way
    let paths = [
        vec![auto(request())],
        vec![auto(request()), auto(issue())],
        vec![auto(request()), auto(issue()), auto(pulled())],
        to_materialised(),
    ];
    for path in paths {
        let state = reach(path);

        // when
        let (_, events) = apply(&state, auto(request())).unwrap_or_else(|r| panic!("{r:?}"));

        // then
        assert!(events.is_empty(), "{:?} records {events:?}", state.phase);
    }
}

#[test]
fn should_name_the_base_by_digest_and_runner_version_when_materialising() {
    // given
    let state = reach(to_materialised());
    let refreshed = reach(to_refreshed());

    // when
    let name = state.base_name();
    let refreshed_name = refreshed.base_name();

    // then: `mfb-<digest12>-r<runner>`, distinct per runner version, a valid `mfb-*` base name
    assert_eq!(name.as_deref(), Some("mfb-000000000000-r2-330-0"));
    assert_eq!(refreshed_name.as_deref(), Some("mfb-000000000000-r2-331-0"));
    assert_eq!(
        base_name(&digest(), runner(330)),
        "mfb-000000000000-r2-330-0"
    );
    for n in [name, refreshed_name].into_iter().flatten() {
        assert!(n.len() <= 64);
        assert!(
            n.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        );
    }
    assert_eq!(
        placement_instance(&HostId::sample(), &digest()),
        "host0-000000000000"
    );
}

#[test]
fn should_refuse_eviction_when_the_base_is_referenced() {
    // given
    let state = reach(to_materialised());

    // when
    let referenced = outcome(&state, auto(evict(EvictionScope::Placement, 1)));
    let unreferenced = outcome(&state, auto(evict(EvictionScope::Placement, 0)));

    // then
    assert_eq!(referenced, "base_referenced");
    assert_eq!(unreferenced, "accepted");
}

#[test]
fn should_refuse_eviction_when_the_placement_is_not_materialised() {
    // given: every phase before a base exists, and a materialised base with a refresh pending
    let paths = [
        vec![auto(request())],
        vec![auto(request()), auto(issue())],
        vec![auto(request()), auto(issue()), auto(pulled())],
        vec![auto(request()), auto(issue()), auto(refused())],
    ];
    let mut refreshing = to_materialised();
    refreshing.extend([auto(refresh(331)), auto(issue())]);

    // when
    let codes: Vec<String> = paths
        .into_iter()
        .map(|p| outcome(&reach(p), auto(evict(EvictionScope::Placement, 0))))
        .collect();
    let pending = outcome(&reach(refreshing), auto(evict(EvictionScope::Placement, 0)));

    // then
    assert!(codes.iter().all(|c| c == "not_materialised"), "{codes:?}");
    assert_eq!(pending, "directive_pending");
}

#[test]
fn should_keep_the_previous_base_when_a_refresh_is_materialised() {
    // given
    let state = reach(to_materialised());

    // when
    let refreshed = reach(to_refreshed());

    // then
    assert_eq!(state.base, Some(runner(330)));
    assert_eq!(state.previous, None);
    assert_eq!(refreshed.base, Some(runner(331)));
    assert_eq!(refreshed.previous, Some(runner(330)));
}

#[test]
fn should_point_back_at_the_previous_base_when_rolled_back() {
    // given
    let state = reach(to_refreshed());

    // when
    let (rolled, events) =
        apply(&state, auto(PlacementCommand::RollBackBase)).unwrap_or_else(|r| panic!("{r:?}"));
    let (forward, _) =
        apply(&rolled, auto(PlacementCommand::RollBackBase)).unwrap_or_else(|r| panic!("{r:?}"));

    // then: the pointer swaps and nothing is issued; rolling back again rolls forward
    assert_eq!(rolled.base, Some(runner(330)));
    assert_eq!(rolled.previous, Some(runner(331)));
    assert!(issued(&events).is_empty());
    assert_eq!(outcome(&rolled, auto(issue())), "nothing_to_issue");
    assert_eq!(forward.base, Some(runner(331)));
}

#[test]
fn should_refuse_a_refresh_when_the_previous_base_is_kept() {
    // given
    let state = reach(to_refreshed());

    // when
    let held = outcome(&state, auto(refresh(332)));
    let (released, _) = apply(&state, auto(evict(EvictionScope::Previous, 0)))
        .and_then(|(s, _)| apply(&s, auto(issue())))
        .and_then(|(s, _)| apply(&s, auto(evicted())))
        .unwrap_or_else(|r| panic!("{r:?}"));
    let after_eviction = outcome(&released, auto(refresh(332)));

    // then: at most two bases per placement
    assert_eq!(held, "previous_base_held");
    assert_eq!(released.phase, PlacementPhase::Materialised);
    assert_eq!(released.base, Some(runner(331)));
    assert_eq!(released.previous, None);
    assert_eq!(after_eviction, "accepted");
}

#[test]
fn should_evict_the_previous_base_then_the_current_when_the_placement_is_evicted() {
    // given
    let mut steps = to_refreshed();
    steps.push(auto(evict(EvictionScope::Placement, 0)));
    let evicting = reach(steps);

    // when
    let (first_state, first) = apply(&evicting, auto(issue())).unwrap_or_else(|r| panic!("{r:?}"));
    let (between, _) = apply(&first_state, auto(evicted())).unwrap_or_else(|r| panic!("{r:?}"));
    let (second_state, second) = apply(&between, auto(issue())).unwrap_or_else(|r| panic!("{r:?}"));
    let (done, _) = apply(&second_state, auto(evicted())).unwrap_or_else(|r| panic!("{r:?}"));

    // then
    assert_eq!(
        issued(&first),
        vec![PlacementDirective::EvictImage {
            runner_version: runner(330)
        }]
    );
    assert_eq!(between.phase, PlacementPhase::Evicting);
    assert_eq!(
        issued(&second),
        vec![PlacementDirective::EvictImage {
            runner_version: runner(331)
        }]
    );
    assert_eq!(done.phase, PlacementPhase::Evicted);
    assert_eq!((done.base, done.previous), (None, None));
}

#[test]
fn should_leave_unknown_only_on_an_observation_when_a_pull_times_out() {
    // given: a pull held unsent, and the same pull sent
    let unsent = reach([auto(request()), auto(issue())]);
    let pulling = reach([auto(request()), auto(issue()), auto(sent())]);

    // when
    let held = outcome(&unsent, at(1, LATE, unknown()));
    let early = outcome(&pulling, auto(unknown()));
    let (unknown_pull, _) =
        apply(&pulling, at(1, LATE, unknown())).unwrap_or_else(|r| panic!("{r:?}"));
    let (again, repeat) =
        apply(&unknown_pull, at(1, LATE, unknown())).unwrap_or_else(|r| panic!("{r:?}"));
    let (answered, _) = apply(&unknown_pull, auto(pulled())).unwrap_or_else(|r| panic!("{r:?}"));

    // then: a directive held unsent did nothing, so it never times out (ADR 0025)
    assert_eq!(held, "not_sent");
    assert_eq!(early, "deadline_not_passed");
    assert_eq!(
        unknown_pull.pending.map(|p| p.outcome),
        Some(PendingOutcome::Unknown {
            since: mayfly_domain::Millis::new(LATE)
        })
    );
    assert!(repeat.is_empty());
    assert_eq!(again, unknown_pull);
    assert_eq!(
        outcome(&pulling, auto(not_applied())),
        "outcome_not_unknown"
    );
    assert_eq!(answered.phase, PlacementPhase::Pulled);
    assert_eq!(answered.pending, None);
}

#[test]
fn should_fail_the_placement_when_the_pull_drifts_or_is_refused() {
    // given
    let pulling = reach([auto(request()), auto(issue())]);

    // when
    let (drifted, _) = apply(
        &pulling,
        auto(PlacementCommand::RecordDrift {
            directive_id: super::support::id(),
            observed: super::support::other_digest(),
        }),
    )
    .unwrap_or_else(|r| panic!("{r:?}"));
    let (refused_state, _) = apply(&pulling, auto(refused())).unwrap_or_else(|r| panic!("{r:?}"));
    let retried = outcome(&drifted, auto(request()));

    // then
    assert_eq!(drifted.phase, PlacementPhase::Failed);
    assert_eq!(refused_state.phase, PlacementPhase::Failed);
    assert_eq!(retried, "accepted");
}

#[test]
fn should_refuse_an_older_run_epoch_when_a_newer_one_was_accepted() {
    // given
    let state = reach([auto(request()), at(3, EARLY, issue())]);

    // when
    let older = outcome(&state, at(2, EARLY, pulled()));
    let same = outcome(&state, at(3, EARLY, materialised()));

    // then
    assert_eq!(state.epoch.get(), 3);
    assert_eq!(older, "stale_run");
    assert_eq!(same, "not_pending");
    let _ = POLICY;
}

#[test]
fn should_refuse_another_digest_when_it_shares_the_twelve_digit_prefix() {
    // given: a placement of the all-zero digest; another digest with the same first twelve hex
    // digits names the same stream
    let state = reach(to_materialised());
    let twin =
        mayfly_domain::ImageDigest::new(&format!("sha256:{}{}", "0".repeat(12), "1".repeat(52)))
            .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        placement_instance(&HostId::sample(), &twin),
        placement_instance(&HostId::sample(), &digest())
    );

    // when
    let refused = outcome(
        &state,
        auto(PlacementCommand::RequestPlacement {
            host_id: HostId::sample(),
            digest: twin,
            runner_version: runner(330),
        }),
    );
    let other_runner = apply(
        &state,
        auto(PlacementCommand::RequestPlacement {
            host_id: HostId::sample(),
            digest: digest(),
            runner_version: runner(331),
        }),
    )
    .unwrap_or_else(|r| panic!("{r:?}"));

    // then: the full digest is the proof; another runner version records nothing
    assert_eq!(refused, "placement_differs");
    assert!(other_runner.1.is_empty());
    assert_eq!(other_runner.0, state);
}
