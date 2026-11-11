//! The quarantine case's `DETERMINISM_CASES` (ver §3.1): accepted cases along a case's life and
//! one refused case per [`QuarantineCaseRejection`] code. Histories are produced by `decide`
//! itself from command paths (each path is checked to be accepted step by step by
//! `should_accept_every_step_when_a_case_path_is_replayed`).

use mayfly_domain::{
    AttemptId, CommandId, Epoch, EventId, Millis, ObservationId, ProviderInstanceId, Sample,
};

use super::events::QuarantineCaseEvent;
use super::state::{
    Disposition, EscalationLevel, Holds, QuarantineCaseCommand, QuarantineCaseRejection,
    QuarantineCaseState,
};
use super::{QuarantineCase, decide, evolve};
use crate::aggregate::{DeterminismCase, Expect};
use crate::attempt::QuarantineReason;
use crate::command::{Actor, Command};

type Step = Command<QuarantineCaseCommand>;

const POLICY: Actor = Actor::Automation {
    policy: "quarantine",
};
const OPERATOR: Actor = Actor::Operator { uid: 501 };

fn at(run_epoch: u64, actor: Actor, body: QuarantineCaseCommand) -> Step {
    Command::new(
        CommandId::from_u128(0xC1),
        Millis::new(1_000),
        Epoch::new(run_epoch),
        actor,
        body,
    )
}

fn auto(body: QuarantineCaseCommand) -> Step {
    at(1, POLICY, body)
}

fn by_operator(body: QuarantineCaseCommand) -> Step {
    at(1, OPERATOR, body)
}

fn open_with(reason: QuarantineReason, proven: bool) -> QuarantineCaseCommand {
    QuarantineCaseCommand::OpenCase {
        attempt_id: AttemptId::from_u128(0xA001),
        reason,
        instance_id: proven.then(ProviderInstanceId::sample),
        holds: Holds {
            disk_bytes: 60 << 30,
            memory_bytes: Some(8 << 30),
            macos_guest: true,
        },
        evidence: vec![EventId::from_u128(0xE1)],
    }
}

fn open() -> QuarantineCaseCommand {
    open_with(QuarantineReason::ProviderIdentityMismatch, true)
}

fn resolve(disposition: Disposition, absence: bool) -> QuarantineCaseCommand {
    QuarantineCaseCommand::ResolveCase {
        disposition,
        absence: absence.then(|| ObservationId::from_u128(0x0B1)),
    }
}

fn escalate(level: EscalationLevel) -> QuarantineCaseCommand {
    QuarantineCaseCommand::EscalateCase { level }
}

fn opened() -> Vec<Step> {
    vec![auto(open())]
}

fn unproven() -> Vec<Step> {
    vec![auto(open_with(QuarantineReason::OwnershipDoubt, false))]
}

fn inspected() -> Vec<Step> {
    let mut path = opened();
    path.push(by_operator(QuarantineCaseCommand::InspectCase));
    path
}

fn escalated() -> Vec<Step> {
    let mut path = opened();
    path.push(auto(escalate(EscalationLevel::Ticket)));
    path
}

fn resolved() -> Vec<Step> {
    let mut path = inspected();
    path.push(by_operator(resolve(Disposition::Keep, false)));
    path
}

fn at_epoch_two() -> Vec<Step> {
    let mut path = opened();
    path.push(at(2, POLICY, escalate(EscalationLevel::Ticket)));
    path
}

/// Folds `path` through `decide` and `evolve`; the first refused step, if any.
fn replay_path(
    path: Vec<Step>,
) -> (
    Vec<QuarantineCaseEvent>,
    Option<(usize, QuarantineCaseRejection)>,
) {
    let mut state = QuarantineCaseState::initial();
    let mut history = Vec::new();
    for (index, step) in path.into_iter().enumerate() {
        match decide(&state, step) {
            Ok(events) => {
                for event in &events {
                    state = evolve(state, event);
                }
                history.extend(events);
            }
            Err(rejection) => return (history, Some((index, rejection))),
        }
    }
    (history, None)
}

fn case(
    name: &'static str,
    path: Vec<Step>,
    command: Step,
    expect: Expect,
) -> DeterminismCase<QuarantineCase> {
    DeterminismCase {
        name,
        history: replay_path(path).0,
        command,
        expect,
    }
}

/// Every path a case starts from, for the replay check.
#[cfg(test)]
fn paths() -> Vec<Vec<Step>> {
    vec![
        opened(),
        unproven(),
        inspected(),
        escalated(),
        resolved(),
        at_epoch_two(),
    ]
}

pub(super) fn cases() -> Vec<DeterminismCase<QuarantineCase>> {
    use Expect::{Accepted, Refused};
    vec![
        // accepted
        case("open_case", Vec::new(), auto(open()), Accepted),
        case(
            "open_again_records_nothing",
            opened(),
            auto(open()),
            Accepted,
        ),
        case(
            "reopen_after_resolution",
            resolved(),
            auto(open_with(QuarantineReason::GuestIdentityRefused, true)),
            Accepted,
        ),
        case(
            "inspect_case",
            opened(),
            by_operator(QuarantineCaseCommand::InspectCase),
            Accepted,
        ),
        case(
            "escalate_case",
            opened(),
            auto(escalate(EscalationLevel::Ticket)),
            Accepted,
        ),
        case(
            "escalate_to_page",
            escalated(),
            auto(escalate(EscalationLevel::Page)),
            Accepted,
        ),
        case(
            "same_level_records_nothing",
            escalated(),
            auto(escalate(EscalationLevel::Ticket)),
            Accepted,
        ),
        case(
            "resolve_delete_with_proof",
            inspected(),
            by_operator(resolve(Disposition::DeleteWithProof, false)),
            Accepted,
        ),
        case(
            "resolve_released_observed_absent",
            unproven(),
            by_operator(resolve(Disposition::ReleasedObservedAbsent, true)),
            Accepted,
        ),
        case(
            "resolve_keep",
            escalated(),
            by_operator(resolve(Disposition::Keep, false)),
            Accepted,
        ),
        case(
            "newer_epoch_is_recorded",
            opened(),
            at(2, POLICY, escalate(EscalationLevel::Page)),
            Accepted,
        ),
        // refused, one per code
        case(
            "wrong_attempt",
            opened(),
            auto(QuarantineCaseCommand::OpenCase {
                attempt_id: AttemptId::from_u128(0xA002),
                reason: QuarantineReason::ProviderIdentityMismatch,
                instance_id: Some(ProviderInstanceId::sample()),
                holds: Holds {
                    disk_bytes: 60 << 30,
                    memory_bytes: Some(8 << 30),
                    macos_guest: true,
                },
                evidence: vec![EventId::from_u128(0xE1)],
            }),
            Refused,
        ),
        case(
            "already_open",
            opened(),
            auto(open_with(QuarantineReason::GuestIdentityRefused, true)),
            Refused,
        ),
        case(
            "stale_run",
            at_epoch_two(),
            by_operator(QuarantineCaseCommand::InspectCase),
            Refused,
        ),
        case(
            "not_open",
            Vec::new(),
            by_operator(QuarantineCaseCommand::InspectCase),
            Refused,
        ),
        case(
            "resolved",
            resolved(),
            by_operator(resolve(Disposition::DeleteWithProof, false)),
            Refused,
        ),
        case(
            "vanished_not_quarantined",
            Vec::new(),
            auto(open_with(QuarantineReason::VmVanishedExternally, true)),
            Refused,
        ),
        case(
            "operator_only",
            inspected(),
            auto(resolve(Disposition::DeleteWithProof, false)),
            Refused,
        ),
        case(
            "no_proof",
            unproven(),
            by_operator(resolve(Disposition::DeleteWithProof, false)),
            Refused,
        ),
        case(
            "absence_not_observed",
            opened(),
            by_operator(resolve(Disposition::ReleasedObservedAbsent, false)),
            Refused,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_accept_every_step_when_a_case_path_is_replayed() {
        // given
        let paths = paths();
        // when
        let refusals: Vec<(usize, Option<(usize, QuarantineCaseRejection)>)> = paths
            .into_iter()
            .map(|path| replay_path(path).1)
            .enumerate()
            .filter(|(_, refusal)| refusal.is_some())
            .collect();
        // then
        assert!(refusals.is_empty(), "{refusals:?}");
    }
}
