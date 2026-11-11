//! The quarantine case's rules (`doc/domain-model.md` §6.10, ADRs 0025, 0066).

use mayfly_control_domain::attempt::QuarantineReason;
use mayfly_control_domain::operations::quarantine::{
    CasePhase, Disposition, EscalationLevel, QuarantineCaseCommand,
};

use super::support::{
    ACTORS, OPERATOR, apply, at, auto, by_operator, escalate, open, open_with, outcome, reach,
    resolve,
};

#[test]
fn should_resolve_only_by_an_operator_when_any_actor_tries() {
    // given
    let state = reach([auto(open())]);
    let dispositions = [
        resolve(Disposition::DeleteWithProof, false),
        resolve(Disposition::ReleasedObservedAbsent, true),
        resolve(Disposition::Keep, false),
    ];
    let state = &state;
    // when
    let outcomes: Vec<(&str, String)> = ACTORS
        .into_iter()
        .flat_map(|(name, actor)| {
            dispositions
                .iter()
                .map(move |body| (name, outcome(state, at(1, actor, body.clone()))))
                .collect::<Vec<_>>()
        })
        .collect();
    // then
    for (name, result) in outcomes {
        let expected = if name == "operator" {
            "accepted"
        } else {
            "operator_only"
        };
        assert_eq!(result, expected, "{name}");
    }
}

#[test]
fn should_refuse_to_open_a_case_when_the_vm_vanished() {
    // given: ADR 0066 as amended: a vanished VM's attempt is closed as a desync
    let state = reach([]);
    // when
    let vanished = outcome(
        &state,
        auto(open_with(QuarantineReason::VmVanishedExternally, true)),
    );
    let mismatch = outcome(
        &state,
        auto(open_with(QuarantineReason::ProviderIdentityMismatch, true)),
    );
    // then
    assert_eq!(
        (vanished.as_str(), mismatch.as_str()),
        ("vanished_not_quarantined", "accepted")
    );
}

#[test]
fn should_release_only_on_absence_and_delete_only_with_proof_when_an_operator_resolves() {
    // given
    let proven = reach([auto(open())]);
    let unproven = reach([auto(open_with(QuarantineReason::OwnershipDoubt, false))]);
    // when
    let outcomes = [
        outcome(
            &proven,
            by_operator(resolve(Disposition::ReleasedObservedAbsent, false)),
        ),
        outcome(
            &proven,
            by_operator(resolve(Disposition::ReleasedObservedAbsent, true)),
        ),
        outcome(
            &unproven,
            by_operator(resolve(Disposition::DeleteWithProof, false)),
        ),
        outcome(
            &proven,
            by_operator(resolve(Disposition::DeleteWithProof, false)),
        ),
    ];
    // then
    assert_eq!(
        outcomes,
        ["absence_not_observed", "accepted", "no_proof", "accepted"]
    );
}

#[test]
fn should_change_only_the_level_when_a_case_is_escalated() {
    // given
    let state = reach([auto(open())]);
    // when
    let (paged, _) =
        apply(&state, auto(escalate(EscalationLevel::Page))).unwrap_or_else(|r| panic!("{r:?}"));
    let (_, again) =
        apply(&paged, auto(escalate(EscalationLevel::Ticket))).unwrap_or_else(|r| panic!("{r:?}"));
    // then
    assert_eq!(paged.phase, state.phase);
    assert_eq!(paged.case, state.case);
    assert_eq!(paged.escalation, Some(EscalationLevel::Page));
    assert!(again.is_empty(), "escalation only rises");
}

#[test]
fn should_open_a_new_case_when_a_resolved_attempt_is_quarantined_again() {
    // given
    let state = reach([
        auto(open()),
        by_operator(QuarantineCaseCommand::InspectCase),
        at(1, OPERATOR, resolve(Disposition::Keep, false)),
    ]);
    // when
    let (after, _) = apply(
        &state,
        auto(open_with(QuarantineReason::GuestIdentityRefused, true)),
    )
    .unwrap_or_else(|r| panic!("{r:?}"));
    // then
    assert_eq!(after.phase, CasePhase::Open);
    assert_eq!(
        after.case.map(|c| c.reason),
        Some(QuarantineReason::GuestIdentityRefused)
    );
}
