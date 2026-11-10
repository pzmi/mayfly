//! The attempt's `DETERMINISM_CASES` (ver §3.1): accepted cases along the lifecycle and one
//! refused case per [`AttemptRejection`] code. Histories are produced by `decide` itself from
//! command paths, so they are histories the aggregate can really write (every path is checked
//! to be accepted step by step by `should_accept_every_step_when_a_case_path_is_replayed`).

use mayfly_domain::{
    AttemptId, CommandId, Deadline, DirectiveId, Epoch, EventId, Generation, HostId, ImageDigest,
    Millis, ObservationId, PoolName, ProviderInstanceId, Sample, SecretHandleId, Slot,
};

use super::command::{
    AttemptCommand, Fact, GuestFacts, GuestOs, GuestRejection, Observation, RefusalReason,
    StartAttempt,
};
use super::events::AttemptEvent;
use super::rejection::AttemptRejection;
use super::state::{AttemptState, QuarantineReason, RetireReason, RunnerRequestId};
use super::{Attempt, decide, evolve};
use crate::aggregate::{DeterminismCase, Expect};
use crate::command::{Actor, Command};

type Step = Command<AttemptCommand>;

const SAGA: Actor = Actor::Automation {
    policy: "attempt_saga",
};
const OPERATOR: Actor = Actor::Operator { uid: 501 };

fn at(run_epoch: u64, actor: Actor, body: AttemptCommand) -> Step {
    Command::new(
        CommandId::from_u128(0xC0),
        Millis::new(1_000),
        Epoch::new(run_epoch),
        actor,
        body,
    )
}

fn saga(body: AttemptCommand) -> Step {
    at(1, SAGA, body)
}

/// The relay's record that directive `n` went out, with the deadline [`issue`] gives.
fn sent(n: u128) -> AttemptCommand {
    AttemptCommand::RecordDirectiveSent {
        directive_id: directive(n),
        deadline: Deadline::at(Millis::new(60_000)),
    }
}

/// The saga's timer, at the deadline [`issue`] gives.
fn timeout(n: u128, at_ms: u64) -> Step {
    Command::new(
        CommandId::from_u128(0xC0),
        Millis::new(at_ms),
        Epoch::new(1),
        SAGA,
        AttemptCommand::RecordDirectiveTimeout {
            directive_id: directive(n),
        },
    )
}

fn directive(n: u128) -> DirectiveId {
    DirectiveId::from_u128(0xD000 + n)
}

fn event(n: u128) -> EventId {
    EventId::from_u128(0xE000 + n)
}

fn issue(n: u128) -> AttemptCommand {
    AttemptCommand::IssueNext {
        directive_id: directive(n),
        deadline: Deadline::at(Millis::new(60_000)),
    }
}

fn observe(fact: Fact) -> AttemptCommand {
    AttemptCommand::RecordObservation(Observation {
        observation_id: ObservationId::from_u128(0x0B5),
        epoch: Epoch::FIRST,
        generation: Generation::FIRST,
        fact,
    })
}

fn facts() -> GuestFacts {
    GuestFacts {
        os: GuestOs::Macos,
        uptime: Millis::new(30_000),
    }
}

fn start() -> AttemptCommand {
    AttemptCommand::StartAttempt(StartAttempt {
        attempt_id: AttemptId::from_u128(0xA77E),
        host_id: HostId::sample(),
        slot: Slot::new(0),
        generation: Generation::FIRST,
        pool: PoolName::sample(),
        image_digest: ImageDigest::sample(),
        retry_budget: 2,
    })
}

fn then(mut path: Vec<Step>, more: impl IntoIterator<Item = AttemptCommand>) -> Vec<Step> {
    path.extend(more.into_iter().map(saga));
    path
}

fn started() -> Vec<Step> {
    vec![saga(start())]
}

fn cloning() -> Vec<Step> {
    then(started(), [issue(1)])
}

fn booting() -> Vec<Step> {
    then(
        cloning(),
        [observe(Fact::VmCloned {
            directive_id: directive(1),
            instance_id: ProviderInstanceId::sample(),
        })],
    )
}

fn verifying() -> Vec<Step> {
    then(
        booting(),
        [
            issue(2),
            observe(Fact::VmBooted {
                directive_id: directive(2),
            }),
        ],
    )
}

fn prepared() -> Vec<Step> {
    then(
        verifying(),
        [
            issue(3),
            observe(Fact::GuestVerified {
                directive_id: directive(3),
                facts: facts(),
            }),
        ],
    )
}

fn activating() -> Vec<Step> {
    then(
        prepared(),
        [AttemptCommand::BeginActivation {
            grant_ref: event(1),
        }],
    )
}

fn verified_activation() -> Vec<Step> {
    then(
        activating(),
        [
            issue(4),
            observe(Fact::GuestVerified {
                directive_id: directive(4),
                facts: facts(),
            }),
        ],
    )
}

fn jit_issued() -> Vec<Step> {
    then(
        verified_activation(),
        [AttemptCommand::RecordJitHandle {
            handle_id: SecretHandleId::from_u128(0x5EC),
            registration_ref: event(2),
        }],
    )
}

fn listening() -> Vec<Step> {
    then(
        jit_issued(),
        [
            issue(5),
            observe(Fact::RunnerLaunched {
                directive_id: directive(5),
            }),
        ],
    )
}

fn busy() -> Vec<Step> {
    then(
        listening(),
        [AttemptCommand::BindJob {
            request_id: RunnerRequestId(1),
        }],
    )
}

fn retirement_requested() -> Vec<Step> {
    then(
        listening(),
        [AttemptCommand::RequestRetirement {
            reason: RetireReason::Surplus,
        }],
    )
}

fn finished() -> Vec<Step> {
    then(busy(), [observe(Fact::RunnerExited { code: 0 })])
}

fn retiring() -> Vec<Step> {
    then(
        finished(),
        [AttemptCommand::RequestRetirement {
            reason: RetireReason::JobDone,
        }],
    )
}

fn stopped() -> Vec<Step> {
    then(
        retiring(),
        [
            issue(6),
            observe(Fact::DiagnosticsExported {
                directive_id: directive(6),
                bytes: 4_096,
                sha256: [7; 32],
            }),
            issue(7),
            observe(Fact::VmStopped {
                directive_id: directive(7),
            }),
        ],
    )
}

fn deleted() -> Vec<Step> {
    then(
        stopped(),
        [
            issue(8),
            observe(Fact::VmDeleted {
                directive_id: directive(8),
            }),
        ],
    )
}

fn closed() -> Vec<Step> {
    then(deleted(), [AttemptCommand::Close])
}

fn quarantined() -> Vec<Step> {
    then(
        verifying(),
        [
            issue(3),
            observe(Fact::GuestRejected {
                directive_id: directive(3),
                reason: GuestRejection::ChallengeFailed,
            }),
        ],
    )
}

fn unknown_clone() -> Vec<Step> {
    then(
        cloning(),
        [observe(Fact::OutcomeUnknown {
            directive_id: directive(1),
        })],
    )
}

/// A VM of our exact name that this attempt never recorded cloning: quarantined without an
/// instance id, so no proof can ever be built for it.
fn doubted() -> Vec<Step> {
    then(
        started(),
        [observe(Fact::VmPresent {
            instance_id: ProviderInstanceId::sample(),
            running: false,
        })],
    )
}

/// A restart while the launch is in flight: the handle is orphaned, and the launch the data
/// plane re-observes in the new epoch is recorded.
fn launched_after_restart() -> Vec<Step> {
    let mut path = then(jit_issued(), [issue(5)]);
    path.push(at(
        2,
        Actor::Startup,
        AttemptCommand::Adopt {
            inventory_ref: event(3),
        },
    ));
    path.push(at(
        2,
        SAGA,
        AttemptCommand::RecordObservation(Observation {
            observation_id: ObservationId::from_u128(0x0B9),
            epoch: Epoch::new(2),
            generation: Generation::FIRST,
            fact: Fact::RunnerLaunched {
                directive_id: directive(5),
            },
        }),
    ));
    path
}

fn adopted() -> Vec<Step> {
    let mut path = prepared();
    path.push(at(
        2,
        Actor::Startup,
        AttemptCommand::Adopt {
            inventory_ref: event(3),
        },
    ));
    path
}

/// Folds a path through `decide` and `evolve`; stops at the first refusal and reports it.
pub(super) fn replay_path(
    path: Vec<Step>,
) -> (
    AttemptState,
    Vec<AttemptEvent>,
    Option<(usize, AttemptRejection)>,
) {
    let mut state = AttemptState::initial();
    let mut history = Vec::new();
    for (index, step) in path.into_iter().enumerate() {
        match decide(&state, step) {
            Ok(events) => {
                for event in &events {
                    state = evolve(state, event);
                }
                history.extend(events);
            }
            Err(rejection) => return (state, history, Some((index, rejection))),
        }
    }
    (state, history, None)
}

fn history(path: Vec<Step>) -> Vec<AttemptEvent> {
    replay_path(path).1
}

fn case(
    name: &'static str,
    path: Vec<Step>,
    command: Step,
    expect: Expect,
) -> DeterminismCase<Attempt> {
    DeterminismCase {
        name,
        history: history(path),
        command,
        expect,
    }
}

fn accepted(name: &'static str, path: Vec<Step>, body: AttemptCommand) -> DeterminismCase<Attempt> {
    case(name, path, saga(body), Expect::Accepted)
}

fn refused(name: &'static str, path: Vec<Step>, body: AttemptCommand) -> DeterminismCase<Attempt> {
    case(name, path, saga(body), Expect::Refused)
}

/// Every path a case starts from, for the replay check.
#[cfg(test)]
pub(super) fn paths() -> Vec<Vec<Step>> {
    vec![
        started(),
        cloning(),
        booting(),
        verifying(),
        prepared(),
        activating(),
        verified_activation(),
        jit_issued(),
        listening(),
        busy(),
        retirement_requested(),
        finished(),
        retiring(),
        stopped(),
        deleted(),
        closed(),
        quarantined(),
        unknown_clone(),
        doubted(),
        adopted(),
        launched_after_restart(),
    ]
}

pub(super) fn cases() -> Vec<DeterminismCase<Attempt>> {
    vec![
        // accepted, along the lifecycle
        accepted("start_attempt", Vec::new(), start()),
        accepted("issue_clone", started(), issue(1)),
        accepted(
            "record_clone",
            cloning(),
            observe(Fact::VmCloned {
                directive_id: directive(1),
                instance_id: ProviderInstanceId::sample(),
            }),
        ),
        accepted(
            "verification_prepares",
            then(verifying(), [issue(3)]),
            observe(Fact::GuestVerified {
                directive_id: directive(3),
                facts: facts(),
            }),
        ),
        accepted(
            "begin_activation",
            prepared(),
            AttemptCommand::BeginActivation {
                grant_ref: event(1),
            },
        ),
        accepted(
            "record_jit_handle",
            verified_activation(),
            AttemptCommand::RecordJitHandle {
                handle_id: SecretHandleId::from_u128(0x5EC),
                registration_ref: event(2),
            },
        ),
        accepted("launch_runner", jit_issued(), issue(5)),
        accepted(
            "bind_job",
            listening(),
            AttemptCommand::BindJob {
                request_id: RunnerRequestId(1),
            },
        ),
        accepted(
            "retirement_waits_for_deregistration",
            listening(),
            AttemptCommand::RequestRetirement {
                reason: RetireReason::Surplus,
            },
        ),
        accepted(
            "withdraw_retirement_when_github_says_busy",
            retirement_requested(),
            AttemptCommand::WithdrawRetirement,
        ),
        accepted(
            "retire_finished",
            finished(),
            AttemptCommand::RequestRetirement {
                reason: RetireReason::JobDone,
            },
        ),
        accepted("delete_with_proof", stopped(), issue(8)),
        accepted(
            "unknown_left_by_inventory",
            unknown_clone(),
            observe(Fact::VmPresent {
                instance_id: ProviderInstanceId::sample(),
                running: false,
            }),
        ),
        accepted(
            "stage_failure_backs_off",
            cloning(),
            observe(Fact::DirectiveRefused {
                directive_id: directive(1),
                reason: RefusalReason::EffectFailed,
            }),
        ),
        accepted(
            "vanished_vm_quarantines",
            then(booting(), [issue(2)]),
            observe(Fact::DirectiveRefused {
                directive_id: directive(2),
                reason: RefusalReason::TargetAbsent,
            }),
        ),
        accepted(
            "guest_rejected_quarantines",
            then(verifying(), [issue(3)]),
            observe(Fact::GuestRejected {
                directive_id: directive(3),
                reason: GuestRejection::ChallengeFailed,
            }),
        ),
        case(
            "adopt_orphans_handle",
            jit_issued(),
            at(
                2,
                Actor::Startup,
                AttemptCommand::Adopt {
                    inventory_ref: event(3),
                },
            ),
            Expect::Accepted,
        ),
        accepted(
            "stale_generation_ignored",
            cloning(),
            AttemptCommand::RecordObservation(Observation {
                observation_id: ObservationId::from_u128(0x0B6),
                epoch: Epoch::FIRST,
                generation: Generation::FIRST.next().unwrap_or(Generation::FIRST),
                fact: Fact::VmAbsent,
            }),
        ),
        case(
            "operator_resolves_quarantine",
            quarantined(),
            at(1, OPERATOR, AttemptCommand::ResolveQuarantine),
            Expect::Accepted,
        ),
        accepted("close_deleted", deleted(), AttemptCommand::Close),
        accepted("record_sent", cloning(), sent(1)),
        case(
            "timeout_makes_unknown",
            then(cloning(), [sent(1)]),
            timeout(1, 60_000),
            Expect::Accepted,
        ),
        // refused, one per code
        refused("not_started", Vec::new(), issue(1)),
        refused("already_started", started(), start()),
        refused("closed", closed(), AttemptCommand::Close),
        case(
            "stale_run",
            adopted(),
            saga(AttemptCommand::BeginActivation {
                grant_ref: event(1),
            }),
            Expect::Refused,
        ),
        case(
            "not_adopted",
            prepared(),
            at(
                2,
                SAGA,
                AttemptCommand::BeginActivation {
                    grant_ref: event(1),
                },
            ),
            Expect::Refused,
        ),
        refused(
            "outcome_unknown",
            unknown_clone(),
            AttemptCommand::RequestRetirement {
                reason: RetireReason::Surplus,
            },
        ),
        refused("directive_pending", cloning(), issue(2)),
        case(
            "deadline_not_reached",
            then(cloning(), [sent(1)]),
            timeout(1, 59_999),
            Expect::Refused,
        ),
        case(
            "directive_not_sent",
            cloning(),
            timeout(1, 60_000),
            Expect::Refused,
        ),
        case(
            "directive_not_pending",
            booting(),
            timeout(1, 60_000),
            Expect::Refused,
        ),
        refused("nothing_to_issue", prepared(), issue(4)),
        refused(
            "observation_mismatch",
            cloning(),
            observe(Fact::VmBooted {
                directive_id: directive(1),
            }),
        ),
        refused(
            "not_prepared",
            listening(),
            AttemptCommand::BeginActivation {
                grant_ref: event(1),
            },
        ),
        refused(
            "not_activating",
            listening(),
            AttemptCommand::RecordJitHandle {
                handle_id: SecretHandleId::from_u128(0x5ED),
                registration_ref: event(2),
            },
        ),
        refused(
            "no_grant",
            prepared(),
            AttemptCommand::RecordJitHandle {
                handle_id: SecretHandleId::from_u128(0x5EC),
                registration_ref: event(2),
            },
        ),
        refused(
            "no_fresh_verification",
            activating(),
            AttemptCommand::RecordJitHandle {
                handle_id: SecretHandleId::from_u128(0x5EC),
                registration_ref: event(2),
            },
        ),
        refused(
            "jit_already_issued",
            jit_issued(),
            AttemptCommand::RecordJitHandle {
                handle_id: SecretHandleId::from_u128(0x5ED),
                registration_ref: event(2),
            },
        ),
        refused(
            "not_listening",
            prepared(),
            AttemptCommand::BindJob {
                request_id: RunnerRequestId(1),
            },
        ),
        refused(
            "job_already_bound",
            busy(),
            AttemptCommand::BindJob {
                request_id: RunnerRequestId(2),
            },
        ),
        case(
            "jit_presumed_leaked",
            launched_after_restart(),
            at(
                2,
                SAGA,
                AttemptCommand::BindJob {
                    request_id: RunnerRequestId(1),
                },
            ),
            Expect::Refused,
        ),
        refused(
            "busy",
            busy(),
            AttemptCommand::RequestRetirement {
                reason: RetireReason::Surplus,
            },
        ),
        refused(
            "already_retiring",
            retiring(),
            AttemptCommand::RequestRetirement {
                reason: RetireReason::Drain,
            },
        ),
        refused(
            "terminal",
            deleted(),
            AttemptCommand::Quarantine {
                reason: QuarantineReason::ForensicRetention,
            },
        ),
        refused(
            "quarantined",
            quarantined(),
            AttemptCommand::RequestRetirement {
                reason: RetireReason::Drain,
            },
        ),
        refused(
            "no_retirement_requested",
            listening(),
            AttemptCommand::ConfirmRetirement,
        ),
        refused(
            "already_quarantined",
            quarantined(),
            AttemptCommand::Quarantine {
                reason: QuarantineReason::ForensicRetention,
            },
        ),
        case(
            "already_current",
            prepared(),
            at(
                1,
                Actor::Startup,
                AttemptCommand::Adopt {
                    inventory_ref: event(3),
                },
            ),
            Expect::Refused,
        ),
        case(
            "not_quarantined",
            prepared(),
            at(1, OPERATOR, AttemptCommand::ResolveQuarantine),
            Expect::Refused,
        ),
        refused(
            "operator_only",
            quarantined(),
            AttemptCommand::ResolveQuarantine,
        ),
        case(
            "no_proof",
            doubted(),
            at(1, OPERATOR, AttemptCommand::ResolveQuarantine),
            Expect::Refused,
        ),
        refused("not_terminal", prepared(), AttemptCommand::Close),
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
        let refusals: Vec<(usize, Option<(usize, AttemptRejection)>)> = paths
            .into_iter()
            .map(|path| replay_path(path).2)
            .enumerate()
            .filter(|(_, refusal)| refusal.is_some())
            .collect();
        // then
        assert!(refusals.is_empty(), "{refusals:?}");
    }
}
