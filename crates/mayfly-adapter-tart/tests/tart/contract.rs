//! The `VmProvider` contract (`mayfly_testkit_data::provider_contract`) run against the Tart
//! adapter over the fake `tart` binary, one test per case so the cases run in parallel.

use mayfly_testkit_data::provider_contract::{ProviderCase, check_case};

use crate::support::FakeTartWorld;

#[test]
fn should_give_each_clone_a_fresh_mac_when_cloning_from_a_local_base() {
    // given
    let world = FakeTartWorld::new;

    // when
    let outcome = check_case(&world, ProviderCase::CloneGivesFreshIdentity);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_refuse_definite_when_cloning_onto_an_existing_name() {
    // given
    let world = FakeTartWorld::new;

    // when
    let outcome = check_case(&world, ProviderCase::CloneOntoExistingRefused);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_refuse_definite_when_the_base_is_not_local() {
    // given
    let world = FakeTartWorld::new;

    // when
    let outcome = check_case(&world, ProviderCase::CloneNeedsLocalBase);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_run_the_vm_under_its_shim_when_started() {
    // given
    let world = FakeTartWorld::new;

    // when
    let outcome = check_case(&world, ProviderCase::StartRuns);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_refuse_definite_when_starting_a_running_vm() {
    // given
    let world = FakeTartWorld::new;

    // when
    let outcome = check_case(&world, ProviderCase::StartOfRunningRefused);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_stop_the_vm_with_tart_stop_when_the_proof_matches() {
    // given
    let world = FakeTartWorld::new;

    // when
    let outcome = check_case(&world, ProviderCase::StopByProof);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_refuse_a_stop_when_the_instance_id_differs() {
    // given
    let world = FakeTartWorld::new;

    // when
    let outcome = check_case(&world, ProviderCase::StopRefusesMismatch);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_leave_the_vm_absent_when_deleted_twice() {
    // given
    let world = FakeTartWorld::new;

    // when
    let outcome = check_case(&world, ProviderCase::DeleteEnsuresAbsent);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_refuse_a_delete_when_the_instance_id_differs() {
    // given
    let world = FakeTartWorld::new;

    // when
    let outcome = check_case(&world, ProviderCase::DeleteRefusesMismatch);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_delete_only_the_exact_name_when_neighbours_share_its_prefix() {
    // given
    let world = FakeTartWorld::new;

    // when
    let outcome = check_case(&world, ProviderCase::DeleteTouchesOnlyTarget);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_answer_ambiguous_and_reinspect_when_a_call_hangs() {
    // given
    let world = FakeTartWorld::new;

    // when
    let outcome = check_case(&world, ProviderCase::HangIsAmbiguous);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_report_incomplete_when_the_listing_fails_or_is_garbled() {
    // given
    let world = FakeTartWorld::new;

    // when
    let outcome = check_case(&world, ProviderCase::ListingFailureIncomplete);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_list_exactly_what_exists_when_the_listing_is_complete() {
    // given
    let world = FakeTartWorld::new;

    // when
    let outcome = check_case(&world, ProviderCase::InventoryExact);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_readopt_a_running_vm_when_the_provider_restarts() {
    // given
    let world = FakeTartWorld::new;

    // when
    let outcome = check_case(&world, ProviderCase::RestartReadopts);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_send_no_signal_to_tart_run_when_vms_are_stopped() {
    // given
    let world = FakeTartWorld::new;

    // when
    let outcome = check_case(&world, ProviderCase::NoSignalToRun);

    // then
    assert_eq!(outcome, Ok(()));
}

#[test]
fn should_give_every_child_only_the_allowlist_when_the_daemon_has_more() {
    // given
    let world = FakeTartWorld::new;

    // when
    let outcome = check_case(&world, ProviderCase::ChildEnvironmentCleared);

    // then
    assert_eq!(outcome, Ok(()));
}
