//! The `ImageStore` contract's own tests (ver §5): the in-memory store passes every case, and
//! each deliberately broken store of ver §5 fails exactly the cases that exist to catch it.

use std::collections::BTreeSet;

use mayfly_testkit_data::image_contract::{ImageCase, MemoryWorld, check_image_store_contract};
use mayfly_testkit_data::{
    BaseEvictingStore, GuessingSizeStore, NameEvictingStore, TagPullStore, TrustingMeasureStore,
};

#[test]
fn should_pass_every_case_when_the_store_is_the_in_memory_images() {
    // given
    let make = MemoryWorld::new;

    // when
    let report = check_image_store_contract(&make);

    // then
    assert!(report.is_green(), "{report}");
}

#[test]
fn should_fail_the_digest_and_drift_cases_when_a_store_pulls_by_tag() {
    // given
    let make = || MemoryWorld::wrapped(TagPullStore);

    // when
    let report = check_image_store_contract(&make);

    // then
    assert_eq!(
        report.failed_cases(),
        BTreeSet::from([ImageCase::PullByDigest, ImageCase::PullRefusesDrift]),
        "{report}"
    );
}

#[test]
fn should_fail_the_reference_case_when_a_store_evicts_a_referenced_base() {
    // given
    let make = || MemoryWorld::wrapped(BaseEvictingStore);

    // when
    let report = check_image_store_contract(&make);

    // then
    assert_eq!(
        report.failed_cases(),
        BTreeSet::from([ImageCase::EvictRefusesReferenced]),
        "{report}"
    );
}

#[test]
fn should_fail_the_foreign_and_identity_cases_when_a_store_evicts_by_name() {
    // given
    let make = || MemoryWorld::wrapped(NameEvictingStore);

    // when
    let report = check_image_store_contract(&make);

    // then
    assert_eq!(
        report.failed_cases(),
        BTreeSet::from([
            ImageCase::EvictRefusesForeign,
            ImageCase::EvictRefusesIdentityMismatch
        ]),
        "{report}"
    );
}

#[test]
fn should_fail_the_measure_case_when_a_store_guesses_a_disk() {
    // given
    let make = || MemoryWorld::wrapped(GuessingSizeStore);

    // when
    let report = check_image_store_contract(&make);

    // then
    assert_eq!(
        report.failed_cases(),
        BTreeSet::from([ImageCase::MeasureFromManifest]),
        "{report}"
    );
}

#[test]
fn should_fail_the_measure_case_when_a_store_measures_without_checking_the_digest() {
    // given
    let make = || MemoryWorld::wrapped(TrustingMeasureStore);

    // when
    let report = check_image_store_contract(&make);

    // then
    assert_eq!(
        report.failed_cases(),
        BTreeSet::from([ImageCase::MeasureFromManifest]),
        "{report}"
    );
}
