//! The pool's own rules (`doc/domain-model.md` §6.2): the breaker (I27), the verified pin,
//! configuration revisions, `min_prepared`'s default and the run-epoch fence (I18).

use mayfly_control_domain::pool::{MIN_PREPARED_DEFAULT, PoolCommand, PoolEvent, terms};

use super::support::{
    OPERATOR, POLICY, apply, at, auto, configure, digest, failure, outcome, pin, reach,
};

#[test]
fn should_default_min_prepared_to_zero_when_the_configuration_names_none() {
    // given
    let state = reach([]);

    // when
    let (configured, _) =
        apply(&state, auto(configure(1, None))).unwrap_or_else(|r| panic!("{r:?}"));
    let (overridden, _) =
        apply(&configured, auto(configure(2, Some(2)))).unwrap_or_else(|r| panic!("{r:?}"));

    // then: ADR 0028, macOS pools included
    assert_eq!(MIN_PREPARED_DEFAULT, 0);
    assert_eq!(configured.config.map(|c| c.min_prepared), Some(0));
    assert_eq!(overridden.config.map(|c| c.min_prepared), Some(2));
}

#[test]
fn should_refuse_a_configuration_when_its_revision_is_not_newer() {
    // given
    let state = reach([configure(3, None)]);

    // when
    let same = outcome(&state, auto(configure(3, None)));
    let older = outcome(&state, auto(configure(2, None)));

    // then
    assert_eq!(same, "stale_revision");
    assert_eq!(older, "stale_revision");
}

#[test]
fn should_refuse_a_pin_when_the_image_is_not_verified() {
    // given
    let state = reach([configure(1, None)]);

    // when
    let unverified = outcome(
        &state,
        auto(PoolCommand::PinImage {
            digest: digest(),
            verified_ref: None,
        }),
    );

    // then
    assert_eq!(unverified, "image_not_verified");
    assert!(
        terms(&state).is_some_and(|t| !t.open),
        "no pin, no admission"
    );
    let (pinned, _) = apply(&state, auto(pin())).unwrap_or_else(|r| panic!("{r:?}"));
    assert!(terms(&pinned).is_some_and(|t| t.open));
}

#[test]
fn should_trip_at_the_threshold_and_count_nothing_more_when_failures_repeat() {
    // given: threshold 3 in a ten-minute window
    let state = reach([
        configure(1, None),
        pin(),
        failure(1, 1_000),
        failure(2, 2_000),
    ]);

    // when
    let (tripped, events) =
        apply(&state, auto(failure(3, 3_000))).unwrap_or_else(|r| panic!("{r:?}"));
    let (after, more) =
        apply(&tripped, auto(failure(4, 4_000))).unwrap_or_else(|r| panic!("{r:?}"));

    // then
    assert!(
        matches!(events.as_slice(), [PoolEvent::AttemptFailureCounted(_), PoolEvent::PoolBreakerTripped(t)] if t.failures == 3)
    );
    assert!(
        more.is_empty(),
        "a tripped breaker counts nothing: the count stays bounded"
    );
    assert_eq!(
        after.breakers.get(&digest()).map(|b| b.failures.len()),
        Some(3)
    );
    let repeat = apply(&state, auto(failure(2, 2_500)))
        .unwrap_or_else(|r| panic!("{r:?}"))
        .1;
    assert!(repeat.is_empty(), "one attempt is counted once");
}

#[test]
fn should_not_trip_when_failures_fall_outside_the_window() {
    // given: two failures, the second eleven minutes after the first
    let state = reach([
        configure(1, None),
        pin(),
        failure(1, 0),
        failure(2, 660_000),
    ]);

    // when
    let (state, events) =
        apply(&state, auto(failure(3, 700_000))).unwrap_or_else(|r| panic!("{r:?}"));

    // then: only two are inside the window
    assert_eq!(events.len(), 1);
    assert_eq!(
        state.breakers.get(&digest()).map(|b| b.failures.len()),
        Some(2)
    );
}

#[test]
fn should_close_the_terms_when_the_breaker_of_the_pin_trips() {
    // given
    let state = reach([configure(1, None), pin()]);
    let open = terms(&state);

    // when
    let tripped = reach([
        configure(1, None),
        pin(),
        failure(1, 1_000),
        failure(2, 2_000),
        failure(3, 3_000),
    ]);

    // then: the terms close at a higher revision, which the Host must apply
    let closed = terms(&tripped);
    assert!(open.is_some_and(|t| t.open));
    assert!(closed.is_some_and(|t| !t.open));
    assert!(closed.map(|t| t.revision) > open.map(|t| t.revision));
}

#[test]
fn should_reset_only_by_an_operator_when_a_breaker_is_tripped() {
    // given
    let tripped = reach([
        configure(1, None),
        pin(),
        failure(1, 1_000),
        failure(2, 2_000),
        failure(3, 3_000),
    ]);

    // when
    let by_automation = outcome(
        &tripped,
        auto(PoolCommand::ResetBreaker { digest: digest() }),
    );
    let (reset, _) = apply(
        &tripped,
        at(1, OPERATOR, PoolCommand::ResetBreaker { digest: digest() }),
    )
    .unwrap_or_else(|r| panic!("{r:?}"));

    // then
    assert_eq!(by_automation, "operator_only");
    assert!(terms(&reset).is_some_and(|t| t.open));
    assert!(!reset.breakers.contains_key(&digest()));
}

#[test]
fn should_refuse_an_older_run_epoch_when_a_newer_one_was_accepted() {
    // given
    let state = reach([configure(1, None), pin()]);

    // when
    let (newer, events) =
        apply(&state, at(2, POLICY, PoolCommand::PausePool)).unwrap_or_else(|r| panic!("{r:?}"));

    // then
    assert!(matches!(events.first(), Some(PoolEvent::PoolEpochAdvanced(e)) if e.epoch.get() == 2));
    assert_eq!(
        outcome(&newer, at(1, POLICY, PoolCommand::ResumePool)),
        "stale_run"
    );
    assert_eq!(
        outcome(&newer, at(2, POLICY, PoolCommand::ResumePool)),
        "accepted"
    );
}
