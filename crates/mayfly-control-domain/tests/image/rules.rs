//! The image's own rules (`doc/domain-model.md` §6.6): digest and manifest immutability, the
//! monotonic status (a rejected or withdrawn image is never verified), the runner's expiry
//! (ADR 0030, P18) and the run-epoch fence (I18).

use mayfly_control_domain::image::{ImageCommand, ImageStatus, RejectReason};
use mayfly_domain::ImageDigest;

use super::support::{
    OPERATOR, POLICY, apply, at, auto, expired, failed, manifest, outcome, passed, reach, register,
};

#[test]
fn should_refuse_another_manifest_when_the_image_is_registered() {
    // given
    let state = reach([auto(register(1)), auto(passed())]);

    // when
    let same = apply(&state, auto(register(1))).unwrap_or_else(|r| panic!("{r:?}"));
    let other = outcome(&state, auto(register(2)));

    // then
    assert!(same.1.is_empty(), "the same registration records nothing");
    assert_eq!(other, "manifest_immutable");
}

#[test]
fn should_never_verify_when_the_image_is_withdrawn() {
    // given: withdrawn from every status that can be withdrawn
    let paths = [
        vec![auto(register(1))],
        vec![auto(register(1)), auto(passed())],
        vec![
            auto(register(1)),
            auto(failed(RejectReason::VerificationFailed)),
        ],
    ];
    for path in paths {
        let mut path = path;
        path.push(at(1, OPERATOR, ImageCommand::WithdrawImage));
        let state = reach(path);

        // when
        let verified = outcome(&state, auto(passed()));
        let rejected = outcome(&state, auto(failed(RejectReason::Drift)));

        // then
        assert_eq!(state.status, ImageStatus::Withdrawn);
        assert_eq!(verified, "withdrawn");
        assert_eq!(rejected, "withdrawn");
    }
}

#[test]
fn should_never_verify_when_the_image_is_rejected() {
    // given: rejected after a verification, as a drift would
    let state = reach([
        auto(register(1)),
        auto(passed()),
        auto(failed(RejectReason::Drift)),
    ]);

    // when
    let verified = outcome(&state, auto(passed()));

    // then
    assert_eq!(state.status, ImageStatus::Rejected);
    assert_eq!(state.rejection, Some(RejectReason::Drift));
    assert_eq!(verified, "already_rejected");
}

#[test]
fn should_record_the_expiry_once_when_the_runner_is_below_the_minimum() {
    // given: the image's runner is 2.330.0
    let state = reach([auto(register(1)), auto(passed())]);

    // when
    let not_below = outcome(&state, auto(expired(330)));
    let (first, events) = apply(&state, auto(expired(331))).unwrap_or_else(|r| panic!("{r:?}"));
    let (second, again) = apply(&first, auto(expired(332))).unwrap_or_else(|r| panic!("{r:?}"));

    // then
    assert_eq!(not_below, "runner_not_expired");
    assert_eq!(events.len(), 1);
    assert_eq!(
        first.runner_expiry.map(|e| e.minimum.minor),
        Some(331),
        "the first expiry is recorded"
    );
    assert!(again.is_empty(), "a later expiry records nothing");
    assert_eq!(second, first);
}

#[test]
fn should_withdraw_only_by_an_operator_when_asked() {
    // given
    let state = reach([auto(register(1)), auto(passed())]);

    // when
    let automation = outcome(&state, auto(ImageCommand::WithdrawImage));
    let operator = outcome(&state, at(1, OPERATOR, ImageCommand::WithdrawImage));

    // then
    assert_eq!(automation, "operator_only");
    assert_eq!(operator, "accepted");
}

#[test]
fn should_refuse_an_older_run_epoch_when_a_newer_one_was_accepted() {
    // given
    let state = reach([auto(register(1)), at(3, POLICY, passed())]);

    // when
    let older = outcome(&state, at(2, POLICY, failed(RejectReason::Drift)));
    let same = outcome(&state, at(3, POLICY, failed(RejectReason::Drift)));

    // then
    assert_eq!(state.epoch.get(), 3);
    assert_eq!(older, "stale_run");
    assert_eq!(same, "accepted");
}

#[test]
fn should_refuse_another_digest_when_the_image_is_registered() {
    // given: the stream holds the image of the all-zero digest
    let state = reach([auto(register(1)), auto(passed())]);
    let other =
        ImageDigest::new(&format!("sha256:{}", "1".repeat(64))).unwrap_or_else(|e| panic!("{e}"));

    // when: the same manifest under another digest
    let refused = outcome(
        &state,
        auto(ImageCommand::RegisterImage {
            digest: other,
            manifest: manifest(1),
        }),
    );

    // then
    assert_eq!(refused, "manifest_immutable");
}
