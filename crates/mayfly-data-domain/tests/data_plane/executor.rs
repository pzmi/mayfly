//! The executor's rules, one test per `Accept:` clause of KER-08 and per curated mutant it
//! kills (ver §9: 13 old epoch, 17 duplicate id, 24 no fit, 25 instance id not re-read), and
//! the idempotent destructive directives of ADR 0053.

use mayfly_data_domain::{
    DataPlane, Inventory, LinkIgnored, LinkOutcome, LinkStimulus, Misfit, Progress, Receipt,
    Stimulus, answered_kind, proof_of,
};
use mayfly_domain::{Deadline, DirectiveId, Epoch, Generation, Millis, ProviderInstanceId, Sample};
use mayfly_protocol::{
    DirectiveKind, DirectiveTag, InventoryGap, Mode, ObservationKind, RefusalReason,
    ResourceDimension, UnknownReason, Welcome,
};

use super::support::{
    EPOCH, NOW, V1_0, V1_1, applied, base, capacity, directive, every_kind, host, instance, ledger,
    listed_vm, listing, open, open_at, open_with, other_host, proof, roomy, seen, seen_as_proved,
    unlisted, vm, vm_on,
};

fn reply_kind(receipt: &Receipt) -> ObservationKind {
    match receipt {
        Receipt::Reply(reply) => reply.kind.clone(),
        other => panic!("expected a reply, got {other:?}"),
    }
}

fn progress_kind(progress: &Progress) -> ObservationKind {
    match progress {
        Progress::Reply(reply) => reply.kind.clone(),
        other => panic!("expected a reply, got {other:?}"),
    }
}

fn refused(reason: RefusalReason) -> ObservationKind {
    ObservationKind::DirectiveRefused { reason }
}

fn clone(n: u128) -> DirectiveKind {
    DirectiveKind::CloneVm {
        base_name: base(),
        vm_name: vm(n),
    }
}

fn table_len(dp: &DataPlane) -> usize {
    match &dp.link {
        mayfly_data_domain::Link::Open(session) => session.table.len(),
        _ => 0,
    }
}

/// Opens the next session of `dp` at `epoch`.
fn reconnect(dp: &mut DataPlane, epoch: u64) {
    dp.link(LinkStimulus::Hello);
    dp.link(LinkStimulus::Welcome(Welcome {
        epoch: Epoch::new(epoch),
        mode: Mode::Normal,
        accepted_version: V1_0,
    }));
    dp.link(LinkStimulus::FloorRaised(Epoch::new(epoch)));
}

#[test]
fn should_answer_unknown_stale_epoch_and_execute_nothing_when_a_frame_is_of_an_older_session() {
    // given
    let mut dp = open();
    let stale = directive(1, Epoch::new(4), clone(1));

    // when
    let got = dp.receive(V1_0, &stale, NOW);

    // then
    let Receipt::Reply(reply) = &got else {
        panic!("expected a reply, got {got:?}");
    };
    assert_eq!(
        reply.epoch, EPOCH,
        "stamped with the current session (ADR 0042)"
    );
    assert_eq!(
        reply.kind,
        ObservationKind::OutcomeUnknown {
            reason: UnknownReason::StaleEpoch
        }
    );
    assert_eq!(table_len(&dp), 0, "never executed, never recorded");
    assert!(dp.ledger.reservations.is_empty());
}

#[test]
fn should_execute_once_and_answer_the_first_outcome_when_a_directive_id_repeats() {
    // given
    let mut dp = open();
    let first = directive(1, EPOCH, clone(1));
    let mut altered = first.clone();
    altered.kind = clone(2);

    // when
    let receipts = [dp.receive(V1_0, &first, NOW), dp.receive(V1_0, &first, NOW)];
    let done = dp.finish(
        EPOCH,
        first.directive_id,
        Stimulus::Applied(applied(&first.kind)),
    );
    let repeats = [
        dp.receive(V1_0, &first, NOW),
        dp.receive(V1_0, &altered, NOW),
    ];

    // then
    assert_eq!(receipts, [Receipt::Execute, Receipt::Pending]);
    let answer = progress_kind(&done);
    assert_eq!(answer, applied(&first.kind));
    assert_eq!(repeats.map(|r| reply_kind(&r)), [answer.clone(), answer]);
    assert!(dp.ledger.vms.contains_key(&vm(1)) && !dp.ledger.vms.contains_key(&vm(2)));
}

#[test]
fn should_run_again_as_a_reobservation_when_the_same_id_arrives_in_a_new_session() {
    // given
    let mut dp = open();
    let first = directive(1, EPOCH, clone(1));
    dp.receive(V1_0, &first, NOW);
    dp.finish(
        EPOCH,
        first.directive_id,
        Stimulus::Applied(applied(&first.kind)),
    );
    reconnect(&mut dp, 6);
    let resent = directive(1, Epoch::new(6), clone(1));

    // when
    let got = dp.receive(V1_0, &resent, NOW);

    // then
    assert_eq!(got, Receipt::Execute);
    assert_eq!(
        dp.ledger
            .reservations
            .get(&(Epoch::new(6), resent.directive_id))
            .map(|r| r.demand),
        Some(capacity(0, 0, 0, 0)),
        "the clone is there: the re-run takes nothing more"
    );
}

#[test]
fn should_refuse_insufficient_resources_whatever_the_cp_allocated_when_a_clone_does_not_fit() {
    // given
    let mut probe = roomy();
    probe.foreign = capacity(4, 0, 0, 0);
    let mut dp = open_with(Mode::Normal, ledger(probe));
    let d = directive(1, EPOCH, clone(1));

    // when
    let got = dp.receive(V1_0, &d, NOW);
    let again = dp.receive(V1_0, &d, NOW);

    // then
    let want = refused(RefusalReason::InsufficientResources {
        dimension: ResourceDimension::VmSlots,
    });
    assert_eq!(reply_kind(&got), want);
    assert_eq!(reply_kind(&again), want, "the same id keeps its outcome");
    assert!(dp.ledger.reservations.is_empty() && dp.ledger.vms.is_empty());
}

#[test]
fn should_refuse_the_second_clone_when_the_first_reserved_the_last_slot() {
    // given
    let mut probe = roomy();
    probe.totals.vm_slots = 1;
    let mut dp = open_with(Mode::Normal, ledger(probe));

    // when
    let first = dp.receive(V1_0, &directive(1, EPOCH, clone(1)), NOW);
    let second = dp.receive(V1_0, &directive(2, EPOCH, clone(2)), NOW);

    // then
    assert_eq!(first, Receipt::Execute);
    assert_eq!(
        reply_kind(&second),
        refused(RefusalReason::InsufficientResources {
            dimension: ResourceDimension::VmSlots
        })
    );
}

#[test]
fn should_reobserve_before_any_effect_when_a_stop_or_delete_arrives() {
    // given
    let kinds = [
        DirectiveKind::DeleteVm { proof: proof(1) },
        DirectiveKind::StopVm {
            grace: Millis::new(1),
            proof: proof(1),
        },
    ];

    // when
    let got: Vec<(Receipt, Progress)> = kinds
        .into_iter()
        .map(|kind| {
            let mut dp = open();
            let d = directive(1, EPOCH, kind);
            let receipt = dp.receive(V1_0, &d, NOW);
            let early = dp.finish(EPOCH, d.directive_id, Stimulus::Applied(applied(&d.kind)));
            (receipt, early)
        })
        .collect();

    // then
    for (receipt, early) in got {
        assert_eq!(receipt, Receipt::ReObserve);
        assert_eq!(
            early,
            Progress::Misfit(Misfit::NotRunning),
            "no effect before the re-read"
        );
    }
}

#[test]
fn should_refuse_proof_mismatch_when_the_instance_id_changed() {
    // given
    let mut dp = open();
    let d = directive(1, EPOCH, DirectiveKind::DeleteVm { proof: proof(1) });
    dp.receive(V1_0, &d, NOW);

    // when
    let got = dp.reobserved(
        EPOCH,
        d.directive_id,
        &seen(&vm(1), instance("0a:00:00:00:00:02")),
    );
    let again = dp.receive(V1_0, &d, NOW);

    // then
    assert_eq!(progress_kind(&got), refused(RefusalReason::ProofMismatch));
    assert_eq!(reply_kind(&again), refused(RefusalReason::ProofMismatch));
}

#[test]
fn should_refuse_proof_mismatch_when_the_reobserved_name_generation_or_instance_id_differs() {
    // given
    let next_generation = mayfly_domain::OwnedVmName::new(
        mayfly_domain::PoolName::sample(),
        host(),
        mayfly_domain::Slot::new(1),
        Generation::new(2).expect("nonzero"),
        mayfly_domain::AttemptId::from_u128(1),
    );
    let observations = [
        (
            "name",
            seen(&vm(2), mayfly_domain::ProviderInstanceId::sample()),
        ),
        (
            "generation",
            seen(
                &next_generation,
                mayfly_domain::ProviderInstanceId::sample(),
            ),
        ),
        ("instance_id", seen(&vm(1), instance("0a:00:00:00:00:02"))),
    ];
    let kinds = [
        DirectiveKind::DeleteVm { proof: proof(1) },
        DirectiveKind::StopVm {
            grace: Millis::new(1),
            proof: proof(1),
        },
    ];

    // when
    let got: Vec<(String, ObservationKind)> = kinds
        .iter()
        .flat_map(|kind| {
            observations.iter().map(|(what, observed)| {
                let mut dp = open();
                let d = directive(1, EPOCH, kind.clone());
                dp.receive(V1_0, &d, NOW);
                let progress = dp.reobserved(EPOCH, d.directive_id, observed);
                (format!("{:?} {what}", kind.tag()), progress_kind(&progress))
            })
        })
        .collect();

    // then
    assert_eq!(got.len(), 6);
    for (case, kind) in got {
        assert_eq!(kind, refused(RefusalReason::ProofMismatch), "{case}");
    }
}

#[test]
fn should_check_the_proof_against_its_name_only_when_the_directive_has_no_attempt() {
    // given
    let mut unattempted = directive(1, EPOCH, DirectiveKind::DeleteVm { proof: proof(1) });
    unattempted.attempt = None;
    let mut inconsistent = proof(1);
    inconsistent.generation = Generation::new(2).expect("nonzero");
    let mut broken = directive(
        2,
        EPOCH,
        DirectiveKind::DeleteVm {
            proof: inconsistent,
        },
    );
    broken.attempt = None;
    let mut dp = open();

    // when
    let admitted = dp.receive(V1_0, &unattempted, NOW);
    let wrong_instance = dp.reobserved(
        EPOCH,
        unattempted.directive_id,
        &seen(&vm(1), instance("0a:00:00:00:00:02")),
    );
    let refused_on_its_name = dp.receive(V1_0, &broken, NOW);

    // then
    assert_eq!(admitted, Receipt::ReObserve, "no attempt to compare with");
    assert_eq!(
        progress_kind(&wrong_instance),
        refused(RefusalReason::ProofMismatch),
        "the re-observation still needs the exact target"
    );
    assert_eq!(
        reply_kind(&refused_on_its_name),
        refused(RefusalReason::ProofMismatch),
        "a proof that disagrees with its own name is refused without an attempt too"
    );
}

fn stop(n: u128) -> DirectiveKind {
    DirectiveKind::StopVm {
        grace: Millis::new(1),
        proof: proof(n),
    }
}

#[test]
fn should_refuse_proof_mismatch_when_a_stop_finds_its_target_gone() {
    // given
    let mut dp = open();
    let d = directive(1, EPOCH, stop(1));
    dp.receive(V1_0, &d, NOW);

    // when
    let got = dp.reobserved(EPOCH, d.directive_id, &listing(vec![]));

    // then
    assert_eq!(
        progress_kind(&got),
        refused(RefusalReason::ProofMismatch),
        "an absent VM is not a stopped one: the inventory reports the absence"
    );
}

/// A complete listing of VMs none of which may be attempt 1's: a foreign VM, this host's
/// slot 2 and another host's slot 1.
fn unrelated_but_complete() -> Inventory {
    let slot_2 = mayfly_domain::OwnedVmName::new(
        mayfly_domain::PoolName::sample(),
        host(),
        mayfly_domain::Slot::new(2),
        Generation::FIRST,
        mayfly_domain::AttemptId::from_u128(7),
    );
    listing(vec![
        listed_vm("macos-dev", Some(instance("0a:00:00:00:00:07")), true),
        listed_vm(
            &slot_2.to_string(),
            Some(instance("0a:00:00:00:00:08")),
            true,
        ),
        listed_vm(
            &vm_on(other_host(), 1).to_string(),
            Some(ProviderInstanceId::sample()),
            false,
        ),
    ])
}

#[test]
fn should_refuse_proof_mismatch_when_a_listed_target_has_no_instance_id() {
    // given: the exact target listed, running or stopped, without an instance id
    let listings = [true, false].map(|running| {
        (
            running,
            listing(vec![listed_vm(&vm(1).to_string(), None, running)]),
        )
    });
    let kinds = [DirectiveKind::DeleteVm { proof: proof(1) }, stop(1)];

    // when
    let got: Vec<(String, ObservationKind)> = kinds
        .iter()
        .flat_map(|kind| {
            listings.iter().map(|(running, observed)| {
                let mut dp = open();
                let d = directive(1, EPOCH, kind.clone());
                dp.receive(V1_0, &d, NOW);
                let progress = dp.reobserved(EPOCH, d.directive_id, observed);
                (
                    format!("{:?} running={running}", kind.tag()),
                    progress_kind(&progress),
                )
            })
        })
        .collect();

    // then
    assert_eq!(got.len(), 4);
    for (case, kind) in got {
        assert_eq!(
            kind,
            refused(RefusalReason::ProofMismatch),
            "present but unproven is never deleted, stopped or absent: {case}"
        );
    }
}

#[test]
fn should_refuse_proof_mismatch_when_the_target_name_is_listed_twice() {
    // given: the proof's instance id is one of two listings of the exact name
    let observed = listing(vec![
        listed_vm(
            &vm(1).to_string(),
            Some(ProviderInstanceId::sample()),
            false,
        ),
        listed_vm(
            &vm(1).to_string(),
            Some(instance("0a:00:00:00:00:02")),
            false,
        ),
    ]);
    let kinds = [DirectiveKind::DeleteVm { proof: proof(1) }, stop(1)];

    // when
    let got: Vec<ObservationKind> = kinds
        .iter()
        .map(|kind| {
            let mut dp = open();
            let d = directive(1, EPOCH, kind.clone());
            dp.receive(V1_0, &d, NOW);
            progress_kind(&dp.reobserved(EPOCH, d.directive_id, &observed))
        })
        .collect();

    // then
    assert_eq!(
        got,
        vec![refused(RefusalReason::ProofMismatch); 2],
        "two VMs under one name prove neither"
    );
}

#[test]
fn should_misfit_when_a_reobservation_arrives_for_a_directive_without_a_proof() {
    // given
    let mut dp = open();
    let d = directive(1, EPOCH, clone(1));
    dp.receive(V1_0, &d, NOW);

    // when
    let got = dp.reobserved(EPOCH, d.directive_id, &listing(vec![]));

    // then
    assert_eq!(got, Progress::Misfit(Misfit::NotReObserving));
}

#[test]
fn should_refuse_proof_mismatch_when_an_unparseable_mf_name_is_listed() {
    // given: the target is not listed by its name, but an `mf-` VM whose name does not parse is
    let malformed = format!("{}-", vm(1));
    let observed = listing(vec![listed_vm(
        &malformed,
        Some(ProviderInstanceId::sample()),
        false,
    )]);
    let kinds = [DirectiveKind::DeleteVm { proof: proof(1) }, stop(1)];

    // when
    let got: Vec<ObservationKind> = kinds
        .iter()
        .map(|kind| {
            let mut dp = open();
            let d = directive(1, EPOCH, kind.clone());
            dp.receive(V1_0, &d, NOW);
            progress_kind(&dp.reobserved(EPOCH, d.directive_id, &observed))
        })
        .collect();

    // then
    assert_eq!(
        mayfly_domain::classify_vm_name(&malformed),
        mayfly_domain::VmNameClass::MalformedManaged
    );
    assert_eq!(
        got,
        vec![refused(RefusalReason::ProofMismatch); 2],
        "a name that does not parse may be the target: absence is not proven"
    );
}

#[test]
fn should_answer_done_when_a_delete_is_resent_and_a_complete_listing_shows_the_target_absent() {
    // given: attempt 1's VM cloned and deleted in session 5
    let mut dp = open();
    let clone = directive(1, EPOCH, clone(1));
    dp.receive(V1_0, &clone, NOW);
    dp.finish(
        EPOCH,
        clone.directive_id,
        Stimulus::Applied(applied(&clone.kind)),
    );
    let first = directive(2, EPOCH, DirectiveKind::DeleteVm { proof: proof(1) });
    dp.receive(V1_0, &first, NOW);
    dp.reobserved(EPOCH, first.directive_id, &seen_as_proved(1));
    dp.finish(
        EPOCH,
        first.directive_id,
        Stimulus::Applied(ObservationKind::VmDeleted),
    );
    reconnect(&mut dp, 6);
    let resent = directive(
        3,
        Epoch::new(6),
        DirectiveKind::DeleteVm { proof: proof(1) },
    );

    // when
    let receipt = dp.receive(V1_0, &resent, NOW);
    let got = dp.reobserved(
        Epoch::new(6),
        resent.directive_id,
        &unrelated_but_complete(),
    );
    let repeat = dp.receive(V1_0, &resent, NOW);

    // then
    assert_eq!(
        receipt,
        Receipt::ReObserve,
        "the target is re-observed first"
    );
    assert_eq!(
        progress_kind(&got),
        ObservationKind::VmDeleted,
        "absent on a complete listing: done, whoever deleted it (ADR 0053)"
    );
    assert_eq!(
        reply_kind(&repeat),
        ObservationKind::VmDeleted,
        "same id, first outcome"
    );
    assert!(dp.ledger.vms.is_empty());
}

#[test]
fn should_answer_unknown_never_done_when_a_delete_reobservation_listing_is_incomplete() {
    // given
    let delete = directive(1, EPOCH, DirectiveKind::DeleteVm { proof: proof(1) });
    let mut current = open_at(V1_1, Mode::Normal, ledger(roomy()));
    let mut older = open();
    current.receive(V1_0, &delete, NOW);
    older.receive(V1_0, &delete, NOW);
    let incomplete = Inventory::Incomplete {
        reason: InventoryGap::Unparseable,
    };

    // when
    let got = current.reobserved(EPOCH, delete.directive_id, &incomplete);
    let got_in_1_0 = older.reobserved(EPOCH, delete.directive_id, &incomplete);

    // then
    assert_eq!(
        progress_kind(&got),
        ObservationKind::OutcomeUnknown {
            reason: UnknownReason::ListingIncomplete,
        },
        "an incomplete listing says nothing about the target (I25)"
    );
    assert_eq!(
        progress_kind(&got_in_1_0),
        ObservationKind::OutcomeUnknown {
            reason: UnknownReason::Deadline,
        },
        "a 1.0 session reads it as an unknown outcome too"
    );
}

#[test]
fn should_answer_done_when_a_stop_finds_its_proven_target_stopped() {
    // given
    let mut dp = open();
    let d = directive(1, EPOCH, stop(1));
    dp.receive(V1_0, &d, NOW);
    let stopped = listing(vec![listed_vm(
        &vm(1).to_string(),
        Some(ProviderInstanceId::sample()),
        false,
    )]);
    let mut other = open();
    let replaced = directive(1, EPOCH, stop(1));
    other.receive(V1_0, &replaced, NOW);

    // when
    let got = dp.reobserved(EPOCH, d.directive_id, &stopped);
    let stopped_but_replaced = other.reobserved(
        EPOCH,
        replaced.directive_id,
        &listing(vec![listed_vm(
            &vm(1).to_string(),
            Some(instance("0a:00:00:00:00:02")),
            false,
        )]),
    );

    // then
    assert_eq!(progress_kind(&got), ObservationKind::VmStopped);
    assert_eq!(
        progress_kind(&stopped_but_replaced),
        refused(RefusalReason::ProofMismatch),
        "stopped is not enough: the proof is checked first"
    );
}

#[test]
fn should_refuse_insufficient_resources_in_a_1_0_session_when_capacity_is_unknown() {
    // given: nothing listed since the start
    let d = directive(1, EPOCH, clone(1));
    let mut current = open_at(V1_1, Mode::Normal, unlisted(roomy()));
    let mut older = open_with(Mode::Normal, unlisted(roomy()));

    // when
    let got = current.receive(V1_0, &d, NOW);
    let got_in_1_0 = older.receive(V1_0, &d, NOW);

    // then
    assert_eq!(
        reply_kind(&got),
        refused(RefusalReason::CapacityUnknown {
            dimension: ResourceDimension::VmSlots,
        })
    );
    assert_eq!(
        reply_kind(&got_in_1_0),
        refused(RefusalReason::InsufficientResources {
            dimension: ResourceDimension::VmSlots,
        }),
        "a frame never uses more than the session's minor; refused either way"
    );
}

#[test]
fn should_delete_only_when_the_reobserved_instance_id_is_the_proofs() {
    // given
    let mut dp = open();
    let d = directive(1, EPOCH, clone(1));
    dp.receive(V1_0, &d, NOW);
    dp.finish(EPOCH, d.directive_id, Stimulus::Applied(applied(&d.kind)));
    let delete = directive(2, EPOCH, DirectiveKind::DeleteVm { proof: proof(1) });
    dp.receive(V1_0, &delete, NOW);

    // when
    let go = dp.reobserved(EPOCH, delete.directive_id, &seen_as_proved(1));
    let done = dp.finish(
        EPOCH,
        delete.directive_id,
        Stimulus::Applied(ObservationKind::VmDeleted),
    );

    // then
    assert_eq!(go, Progress::Execute);
    assert_eq!(progress_kind(&done), ObservationKind::VmDeleted);
    assert!(
        dp.ledger.vms.is_empty(),
        "the ledger frees what the delete removed"
    );
}

#[test]
fn should_refuse_proof_mismatch_when_the_proof_does_not_match_its_name_or_directive() {
    // given
    let mut other_generation = proof(1);
    other_generation.generation = Generation::new(2).expect("nonzero");
    let mut other_slot = proof(1);
    other_slot.slot = mayfly_domain::Slot::new(2);
    let mut other_attempt = proof(1);
    other_attempt.attempt_id = mayfly_domain::AttemptId::from_u128(2);
    let proofs = [other_generation, other_slot, other_attempt, proof(2)];

    // when
    let got: Vec<ObservationKind> = proofs
        .into_iter()
        .map(|p| {
            let mut dp = open();
            reply_kind(&dp.receive(
                V1_0,
                &directive(1, EPOCH, DirectiveKind::DeleteVm { proof: p }),
                NOW,
            ))
        })
        .collect();

    // then
    assert!(
        got.iter()
            .all(|k| *k == refused(RefusalReason::ProofMismatch)),
        "{got:?}"
    );
}

#[test]
fn should_refuse_a_protected_target_when_a_delete_names_another_host() {
    // given
    let mut foreign_host = proof(1);
    foreign_host.host_id = super::support::other_host();
    let mut dp = open();

    // when
    let host_refusal = dp.receive(
        V1_0,
        &directive(
            1,
            EPOCH,
            DirectiveKind::DeleteVm {
                proof: foreign_host,
            },
        ),
        NOW,
    );

    // then
    assert_eq!(
        reply_kind(&host_refusal),
        refused(RefusalReason::ProtectedTarget)
    );
}

#[test]
fn should_never_parse_a_base_name_as_a_vm_name_when_a_vm_directive_could_name_it() {
    // given
    let bases = ["mfb-x", "mfb-base", "mfb-sha256-0123-2-330-0"];

    // when
    let classes: Vec<mayfly_domain::VmNameClass> = bases
        .iter()
        .map(|name| {
            mayfly_protocol::BaseName::new(name).expect("a valid base name");
            mayfly_domain::classify_vm_name(name)
        })
        .collect();

    // then
    assert!(
        classes
            .iter()
            .all(|class| *class == mayfly_domain::VmNameClass::Foreign),
        "a VM directive names an OwnedVmName, which a base never is (I05): {classes:?}"
    );
}

#[test]
fn should_refuse_held_kinds_and_run_the_rest_when_the_session_is_safe() {
    // given
    let kinds = every_kind();

    // when
    let refused_tags: Vec<DirectiveTag> = kinds
        .iter()
        .filter(|kind| {
            let mut dp = open_with(Mode::Safe, ledger(roomy()));
            let got = dp.receive(V1_0, &directive(1, EPOCH, (*kind).clone()), NOW);
            matches!(&got, Receipt::Reply(r) if r.kind == refused(RefusalReason::SafeMode))
        })
        .map(DirectiveKind::tag)
        .collect();

    // then
    let held: Vec<DirectiveTag> = DirectiveTag::ALL
        .into_iter()
        .filter(|t| t.held_in_safe_mode())
        .collect();
    assert_eq!(refused_tags, held);
}

#[test]
fn should_refuse_deadline_expired_when_a_directive_arrives_at_its_deadline() {
    // given
    let mut dp = open();
    let mut d = directive(1, EPOCH, clone(1));
    d.deadline = Deadline::at(NOW);

    // when
    let got = dp.receive(V1_0, &d, NOW);

    // then
    assert_eq!(reply_kind(&got), refused(RefusalReason::DeadlineExpired));
}

#[test]
fn should_refuse_a_runner_version_when_it_is_empty_too_long_or_not_inert() {
    // given
    let versions = [
        String::new(),
        "1".repeat(mayfly_data_domain::MAX_RUNNER_VERSION + 1),
        "2.330.0 x".to_owned(),
        "2.330.0/..".to_owned(),
        "1".repeat(mayfly_data_domain::MAX_RUNNER_VERSION),
        "2.330.0-rc_1+b".to_owned(),
    ];

    // when
    let got: Vec<bool> = versions
        .into_iter()
        .map(|runner_version| {
            let mut dp = open();
            let d = directive(
                1,
                EPOCH,
                DirectiveKind::MaterialiseImage {
                    digest: mayfly_domain::ImageDigest::sample(),
                    base_name: base(),
                    runner_version,
                },
            );
            dp.receive(V1_0, &d, NOW) == Receipt::Execute
        })
        .collect();

    // then
    assert_eq!(got, [false, false, false, false, true, true]);
}

#[test]
fn should_accept_only_its_own_answer_when_an_effect_is_applied() {
    // given
    let mut dp = open();
    let d = directive(1, EPOCH, clone(1));
    dp.receive(V1_0, &d, NOW);

    // when
    let wrong = dp.finish(
        EPOCH,
        d.directive_id,
        Stimulus::Applied(ObservationKind::VmBooted),
    );
    let right = dp.finish(EPOCH, d.directive_id, Stimulus::Applied(applied(&d.kind)));

    // then
    assert_eq!(wrong, Progress::Misfit(Misfit::NotAnApplication));
    assert_eq!(progress_kind(&right), applied(&d.kind));
    for kind in every_kind() {
        assert_eq!(answered_kind(&applied(&kind)), Some(kind.tag()), "{kind:?}");
    }
    assert_eq!(answered_kind(&ObservationKind::HypervisorLimitHit), None);
}

#[test]
fn should_answer_unknown_and_hold_capacity_when_an_effect_is_ambiguous() {
    // given
    let mut dp = open();
    let start = directive(1, EPOCH, clone(1));
    let launch = directive(
        2,
        EPOCH,
        every_kind()
            .into_iter()
            .find(|k| k.tag() == DirectiveTag::LaunchRunner)
            .expect("a launch"),
    );
    dp.receive(V1_0, &start, NOW);
    dp.receive(V1_0, &launch, NOW);

    // when
    let clone_unknown = dp.finish(
        EPOCH,
        start.directive_id,
        Stimulus::Ambiguous(UnknownReason::Deadline),
    );
    let launch_unknown = dp.finish(
        EPOCH,
        launch.directive_id,
        Stimulus::Ambiguous(UnknownReason::ProcessLost),
    );

    // then
    assert_eq!(
        progress_kind(&clone_unknown),
        ObservationKind::OutcomeUnknown {
            reason: UnknownReason::Deadline
        }
    );
    assert_eq!(
        progress_kind(&launch_unknown),
        ObservationKind::RunnerLaunchUnknown
    );
    let held = dp.ledger.reservations.get(&(EPOCH, start.directive_id));
    assert!(
        held.is_some_and(|r| r.held),
        "kept until a complete inventory"
    );
}

#[test]
fn should_hold_reservations_and_orphan_late_results_when_the_session_ends() {
    // given
    let mut dp = open();
    let d = directive(1, EPOCH, clone(1));
    dp.receive(V1_0, &d, NOW);

    // when
    let hello = dp.link(LinkStimulus::Hello);
    let late = dp.finish(EPOCH, d.directive_id, Stimulus::Applied(applied(&d.kind)));
    let missing = {
        let mut other = open();
        other.reobserved(EPOCH, DirectiveId::from_u128(9), &listing(vec![]))
    };

    // then
    assert_eq!(hello, LinkOutcome::SendHello);
    assert_eq!(late, Progress::Orphaned);
    assert_eq!(missing, Progress::Orphaned);
    assert!(dp.ledger.reservations.values().all(|r| r.held));
    assert_eq!(dp.session_epoch(), None);
}

#[test]
fn should_never_reenter_a_session_when_its_welcome_is_duplicated_after_a_restart() {
    // given
    let mut dp = DataPlane::start(host(), EPOCH, ledger(roomy()));
    dp.link(LinkStimulus::Hello);

    // when
    let got = dp.link(LinkStimulus::Welcome(Welcome {
        epoch: EPOCH,
        mode: Mode::Normal,
        accepted_version: V1_0,
    }));

    // then
    assert_eq!(got, LinkOutcome::Ignored(LinkIgnored::AtOrBelowFloor));
    assert_eq!(dp.session_epoch(), None);
    assert_eq!(dp.floor, EPOCH);
}

#[test]
fn should_name_the_proof_only_when_the_kind_is_destructive() {
    // given
    let kinds = every_kind();

    // when
    let with_proof: Vec<DirectiveTag> = kinds
        .iter()
        .filter(|k| proof_of(k).is_some())
        .map(DirectiveKind::tag)
        .collect();

    // then
    assert_eq!(with_proof, [DirectiveTag::StopVm, DirectiveTag::DeleteVm]);
}
