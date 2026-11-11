//! The `GuestTransport` contract's own tests (ver §5): the in-memory guests pass every case, and
//! the deliberately broken `UncheckedLockTransport` fails the cases that exist to catch it.

use std::collections::BTreeSet;

use mayfly_testkit_data::guest_contract::{
    GuestCase, MemoryGuestWorld, check_guest_transport_contract,
};

#[test]
fn should_pass_every_case_when_the_transport_is_the_in_memory_guests() {
    // given
    let make = MemoryGuestWorld::new;

    // when
    let report = check_guest_transport_contract(&make);

    // then
    assert!(report.is_green(), "{report}");
}

#[test]
fn should_fail_the_identity_cases_when_the_transport_skips_the_lock_mac_and_guest_check() {
    // given
    let make = MemoryGuestWorld::unchecked;

    // when
    let report = check_guest_transport_contract(&make);

    // then
    assert_eq!(
        report.failed_cases(),
        BTreeSet::from([
            GuestCase::ForeignLockRefused,
            GuestCase::ChangedMacRefused,
            GuestCase::LaunchMisroutedRefused,
        ]),
        "{report}"
    );
}
