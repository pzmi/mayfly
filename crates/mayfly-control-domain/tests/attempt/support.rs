//! Paths through the attempt's lifecycle, driven through its public `decide` and `evolve`.

use mayfly_control_domain::attempt::{
    Attempt, AttemptCommand, AttemptEvent, AttemptRejection, AttemptState, DiagnosticsLoss, Fact,
    GuestFacts, GuestOs, GuestRejection, Observation, QuarantineReason, RefusalReason,
    RetireReason, RunnerRequestId, StartAttempt,
};
use mayfly_control_domain::{Actor, Aggregate, Command};
use mayfly_domain::{
    AttemptId, CommandId, Deadline, DirectiveId, Epoch, EventId, Generation, HostId, ImageDigest,
    Millis, ObservationId, PoolName, ProviderInstanceId, Sample, SecretHandleId, Slot,
};

pub type Step = Command<AttemptCommand>;

pub const SAGA: Actor = Actor::Automation {
    policy: "attempt_saga",
};
pub const OPERATOR: Actor = Actor::Operator { uid: 501 };

pub fn at(run_epoch: u64, actor: Actor, body: AttemptCommand) -> Step {
    Command::new(
        CommandId::from_u128(0xC0),
        Millis::new(1_000),
        Epoch::new(run_epoch),
        actor,
        body,
    )
}

pub fn saga(body: AttemptCommand) -> Step {
    at(1, SAGA, body)
}

pub fn directive(n: u128) -> DirectiveId {
    DirectiveId::from_u128(0xD000 + n)
}

pub fn event(n: u128) -> EventId {
    EventId::from_u128(0xE000 + n)
}

pub fn handle() -> SecretHandleId {
    SecretHandleId::from_u128(0x5EC)
}

pub fn instance() -> ProviderInstanceId {
    ProviderInstanceId::sample()
}

pub fn issue(n: u128) -> AttemptCommand {
    AttemptCommand::IssueNext {
        directive_id: directive(n),
        deadline: Deadline::at(Millis::new(60_000)),
    }
}

pub fn observe(fact: Fact) -> AttemptCommand {
    AttemptCommand::RecordObservation(Observation {
        observation_id: ObservationId::from_u128(0x0B5),
        epoch: Epoch::FIRST,
        generation: Generation::FIRST,
        fact,
    })
}

pub fn facts() -> GuestFacts {
    GuestFacts {
        os: GuestOs::Macos,
        uptime: Millis::new(30_000),
    }
}

pub fn start() -> AttemptCommand {
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

/// The state and events after `steps`, every one accepted.
///
/// # Panics
/// When a step is refused: the path is wrong, not the aggregate under test.
pub fn reach(steps: &[Step]) -> (AttemptState, Vec<AttemptEvent>) {
    let mut state = Attempt::initial();
    let mut history = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        match Attempt::decide(&state, step.clone()) {
            Ok(events) => {
                for e in &events {
                    state = Attempt::evolve(state, e);
                }
                history.extend(events);
            }
            Err(rejection) => panic!("step {index} of the path refused: {rejection:?}"),
        }
    }
    (state, history)
}

/// Decides `step` and folds what it accepted.
pub fn apply(
    state: &AttemptState,
    step: Step,
) -> Result<(AttemptState, Vec<AttemptEvent>), AttemptRejection> {
    let events = Attempt::decide(state, step)?;
    let next = events.iter().fold(state.clone(), Attempt::evolve);
    Ok((next, events))
}

pub fn then(mut path: Vec<Step>, more: impl IntoIterator<Item = AttemptCommand>) -> Vec<Step> {
    path.extend(more.into_iter().map(saga));
    path
}

pub fn started() -> Vec<Step> {
    vec![saga(start())]
}

pub fn cloning() -> Vec<Step> {
    then(started(), [issue(1)])
}

pub fn booting() -> Vec<Step> {
    then(
        cloning(),
        [observe(Fact::VmCloned {
            directive_id: directive(1),
            instance_id: instance(),
        })],
    )
}

pub fn verifying() -> Vec<Step> {
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

pub fn prepared() -> Vec<Step> {
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

pub fn activating() -> Vec<Step> {
    then(
        prepared(),
        [AttemptCommand::BeginActivation {
            grant_ref: event(1),
        }],
    )
}

pub fn verified_activation() -> Vec<Step> {
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

pub fn jit_issued() -> Vec<Step> {
    then(
        verified_activation(),
        [AttemptCommand::RecordJitHandle {
            handle_id: handle(),
            registration_ref: event(2),
        }],
    )
}

pub fn listening() -> Vec<Step> {
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

pub fn busy() -> Vec<Step> {
    then(
        listening(),
        [AttemptCommand::BindJob {
            request_id: RunnerRequestId(1),
        }],
    )
}

pub fn finished() -> Vec<Step> {
    then(busy(), [observe(Fact::RunnerExited { code: 0 })])
}

pub fn retiring() -> Vec<Step> {
    then(
        finished(),
        [AttemptCommand::RequestRetirement {
            reason: RetireReason::JobDone,
        }],
    )
}

pub fn stopped() -> Vec<Step> {
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

pub fn deleting() -> Vec<Step> {
    then(stopped(), [issue(8)])
}

pub fn deleted() -> Vec<Step> {
    then(
        deleting(),
        [observe(Fact::VmDeleted {
            directive_id: directive(8),
        })],
    )
}

pub fn closed() -> Vec<Step> {
    then(deleted(), [AttemptCommand::Close])
}

pub fn lost() -> Vec<Step> {
    then(prepared(), [observe(Fact::VmAbsent)])
}

pub fn backoff() -> Vec<Step> {
    then(
        cloning(),
        [observe(Fact::DirectiveRefused {
            directive_id: directive(1),
            reason: RefusalReason::EffectFailed,
        })],
    )
}

pub fn quarantined() -> Vec<Step> {
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

/// Quarantined because the start found the cloned VM absent from a complete listing (ADR 0066).
pub fn vanished() -> Vec<Step> {
    then(
        booting(),
        [
            issue(2),
            observe(Fact::DirectiveRefused {
                directive_id: directive(2),
                reason: RefusalReason::TargetAbsent,
            }),
        ],
    )
}

pub fn unknown_clone() -> Vec<Step> {
    then(
        cloning(),
        [observe(Fact::OutcomeUnknown {
            directive_id: directive(1),
        })],
    )
}

/// A restart while the launch is in flight, then the launch re-observed in the new epoch: a
/// runner whose JIT config is presumed leaked.
pub fn launched_after_restart() -> Vec<Step> {
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
        observe_in(
            2,
            Fact::RunnerLaunched {
                directive_id: directive(5),
            },
        ),
    ));
    path
}

/// KER-04G (I21): a launch of unknown outcome, so a runner may be registered in the VM; then a
/// restart, and the fresh verification of the new epoch refused: Backoff in the activate stage
/// with the registration held.
pub fn backoff_holding_registration() -> Vec<Step> {
    let mut path = then(
        jit_issued(),
        [
            issue(5),
            observe(Fact::RunnerLaunchUnknown {
                directive_id: directive(5),
            }),
        ],
    );
    path.push(at(
        2,
        Actor::Startup,
        AttemptCommand::Adopt {
            inventory_ref: event(3),
        },
    ));
    path.push(at(2, SAGA, issue(6)));
    path.push(at(
        2,
        SAGA,
        observe_in(
            2,
            Fact::DirectiveRefused {
                directive_id: directive(6),
                reason: RefusalReason::EffectFailed,
            },
        ),
    ));
    path
}

/// [`backoff_holding_registration`] with a retirement requested there, waiting for its
/// deregistration.
pub fn backoff_retirement_waiting() -> Vec<Step> {
    let mut path = backoff_holding_registration();
    path.push(at(
        2,
        SAGA,
        AttemptCommand::RequestRetirement {
            reason: RetireReason::Surplus,
        },
    ));
    path
}

/// An observation from data-plane epoch `epoch`.
pub fn observe_in(epoch: u64, fact: Fact) -> AttemptCommand {
    AttemptCommand::RecordObservation(Observation {
        observation_id: ObservationId::from_u128(0x0B8),
        epoch: Epoch::new(epoch),
        generation: Generation::FIRST,
        fact,
    })
}

/// A VM of our exact name that this attempt never recorded cloning: quarantined for
/// ownership doubt, with no instance id to prove anything with.
pub fn doubted() -> Vec<Step> {
    then(
        started(),
        [observe(Fact::VmPresent {
            instance_id: instance(),
            running: true,
        })],
    )
}

fn unknown(path: Vec<Step>, n: u128) -> Vec<Step> {
    then(
        path,
        [observe(Fact::OutcomeUnknown {
            directive_id: directive(n),
        })],
    )
}

/// One state per phase and pending overlay that the lifecycle can reach: idle, a directive in
/// flight, its outcome unknown. Named `<phase>` or `<phase>/<overlay>`.
pub fn matrix_states() -> Vec<(&'static str, AttemptState)> {
    let paths: Vec<(&'static str, Vec<Step>)> = vec![
        ("not_started", Vec::new()),
        ("reserved", started()),
        ("cloning/in_flight", cloning()),
        ("cloning/unknown", unknown_clone()),
        (
            "cloning/sent",
            then(
                cloning(),
                [AttemptCommand::RecordDirectiveSent {
                    directive_id: directive(1),
                    deadline: Deadline::at(Millis::new(60_000)),
                }],
            ),
        ),
        ("booting", booting()),
        ("booting/in_flight", then(booting(), [issue(2)])),
        ("booting/unknown", unknown(then(booting(), [issue(2)]), 2)),
        ("verifying", verifying()),
        ("verifying/in_flight", then(verifying(), [issue(3)])),
        (
            "verifying/unknown",
            unknown(then(verifying(), [issue(3)]), 3),
        ),
        ("prepared", prepared()),
        ("activating", activating()),
        ("activating/in_flight", then(activating(), [issue(4)])),
        (
            "activating/unknown",
            unknown(then(activating(), [issue(4)]), 4),
        ),
        ("activating/launching", then(jit_issued(), [issue(5)])),
        ("listening", listening()),
        ("listening/leaked", launched_after_restart()),
        ("busy", busy()),
        ("finished", finished()),
        ("retiring", retiring()),
        ("retiring/in_flight", then(retiring(), [issue(6)])),
        (
            "retiring/unknown",
            unknown(
                then(
                    retiring(),
                    [
                        issue(6),
                        observe(Fact::DiagnosticsLost {
                            directive_id: directive(6),
                            reason: DiagnosticsLoss::GuestUnreachable,
                        }),
                        issue(7),
                    ],
                ),
                7,
            ),
        ),
        (
            "deleting",
            then(
                deleting(),
                [observe(Fact::DirectiveRefused {
                    directive_id: directive(8),
                    reason: RefusalReason::SafeMode,
                })],
            ),
        ),
        ("deleting/in_flight", deleting()),
        ("deleting/unknown", unknown(deleting(), 8)),
        ("deleted", deleted()),
        ("lost", lost()),
        ("backoff", backoff()),
        ("backoff/registered", backoff_holding_registration()),
        ("backoff/retirement_waiting", backoff_retirement_waiting()),
        ("quarantined", quarantined()),
        ("quarantined/in_flight", then(quarantined(), [issue(10)])),
        (
            "quarantined/unknown",
            unknown(then(quarantined(), [issue(10)]), 10),
        ),
        ("quarantined/doubted", doubted()),
        ("quarantined/vanished", vanished()),
        ("closed", closed()),
    ];
    paths
        .into_iter()
        .map(|(name, path)| (name, reach(&path).0))
        .collect()
}

/// Every stimulus, named, aimed at `state`: an answer names the pending directive when there
/// is one. Commands carry the state's run epoch.
pub fn stimuli(state: &AttemptState) -> Vec<(&'static str, Step)> {
    let answered = state
        .pending
        .map_or(directive(99), |pending| pending.directive_id);
    let epoch = state.epoch.get().max(1);
    let cmd = |body| at(epoch, SAGA, body);
    let fact = |fact| {
        at(
            epoch,
            SAGA,
            AttemptCommand::RecordObservation(Observation {
                observation_id: ObservationId::from_u128(0x0B7),
                epoch: Epoch::new(epoch),
                generation: Generation::FIRST,
                fact,
            }),
        )
    };
    vec![
        ("start_attempt", cmd(start())),
        ("issue_next", cmd(issue(50))),
        (
            "begin_activation",
            cmd(AttemptCommand::BeginActivation {
                grant_ref: event(1),
            }),
        ),
        (
            "record_jit_handle",
            cmd(AttemptCommand::RecordJitHandle {
                handle_id: SecretHandleId::from_u128(0x5ED),
                registration_ref: event(2),
            }),
        ),
        (
            "bind_job",
            cmd(AttemptCommand::BindJob {
                request_id: RunnerRequestId(1),
            }),
        ),
        (
            "request_retirement",
            cmd(AttemptCommand::RequestRetirement {
                reason: RetireReason::Surplus,
            }),
        ),
        ("confirm_retirement", cmd(AttemptCommand::ConfirmRetirement)),
        (
            "withdraw_retirement",
            cmd(AttemptCommand::WithdrawRetirement),
        ),
        (
            "quarantine",
            cmd(AttemptCommand::Quarantine {
                reason: QuarantineReason::RedactionViolation,
            }),
        ),
        (
            "adopt",
            at(
                epoch + 1,
                Actor::Startup,
                AttemptCommand::Adopt {
                    inventory_ref: event(3),
                },
            ),
        ),
        (
            "resolve_quarantine",
            at(epoch, OPERATOR, AttemptCommand::ResolveQuarantine),
        ),
        ("close", cmd(AttemptCommand::Close)),
        (
            "vm_cloned",
            fact(Fact::VmCloned {
                directive_id: answered,
                instance_id: instance(),
            }),
        ),
        (
            "vm_booted",
            fact(Fact::VmBooted {
                directive_id: answered,
            }),
        ),
        (
            "guest_verified",
            fact(Fact::GuestVerified {
                directive_id: answered,
                facts: facts(),
            }),
        ),
        (
            "guest_rejected",
            fact(Fact::GuestRejected {
                directive_id: answered,
                reason: GuestRejection::FactsMismatch,
            }),
        ),
        (
            "directive_refused",
            fact(Fact::DirectiveRefused {
                directive_id: answered,
                reason: RefusalReason::InsufficientResources,
            }),
        ),
        (
            "directive_refused_proof",
            fact(Fact::DirectiveRefused {
                directive_id: answered,
                reason: RefusalReason::ProofMismatch,
            }),
        ),
        (
            "directive_refused_absent",
            fact(Fact::DirectiveRefused {
                directive_id: answered,
                reason: RefusalReason::TargetAbsent,
            }),
        ),
        (
            "outcome_unknown",
            fact(Fact::OutcomeUnknown {
                directive_id: answered,
            }),
        ),
        (
            "runner_launched",
            fact(Fact::RunnerLaunched {
                directive_id: answered,
            }),
        ),
        (
            "runner_launch_unknown",
            fact(Fact::RunnerLaunchUnknown {
                directive_id: answered,
            }),
        ),
        (
            "diagnostics_exported",
            fact(Fact::DiagnosticsExported {
                directive_id: answered,
                bytes: 1,
                sha256: [1; 32],
            }),
        ),
        (
            "diagnostics_lost",
            fact(Fact::DiagnosticsLost {
                directive_id: answered,
                reason: DiagnosticsLoss::GuestUnreachable,
            }),
        ),
        (
            "vm_stopped",
            fact(Fact::VmStopped {
                directive_id: answered,
            }),
        ),
        (
            "vm_deleted",
            fact(Fact::VmDeleted {
                directive_id: answered,
            }),
        ),
        ("runner_exited", fact(Fact::RunnerExited { code: 1 })),
        ("vm_observed_stopped", fact(Fact::VmObservedStopped)),
        (
            "vm_present",
            fact(Fact::VmPresent {
                instance_id: instance(),
                running: true,
            }),
        ),
        (
            "vm_present_replaced",
            fact(Fact::VmPresent {
                instance_id: ProviderInstanceId::new("0a:00:00:00:00:02")
                    .unwrap_or_else(|e| panic!("{e}")),
                running: true,
            }),
        ),
        ("vm_absent", fact(Fact::VmAbsent)),
        (
            "directive_sent",
            cmd(AttemptCommand::RecordDirectiveSent {
                directive_id: answered,
                deadline: Deadline::at(Millis::new(60_000)),
            }),
        ),
        (
            "directive_timeout",
            Command::new(
                CommandId::from_u128(0xC0),
                Millis::new(60_000),
                Epoch::new(epoch),
                SAGA,
                AttemptCommand::RecordDirectiveTimeout {
                    directive_id: answered,
                },
            ),
        ),
    ]
}
