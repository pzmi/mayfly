//! Paths through a quarantine case's life, driven through the public `decide` and `evolve`.

use mayfly_control_domain::attempt::QuarantineReason;
use mayfly_control_domain::operations::quarantine::{
    Disposition, EscalationLevel, Holds, QuarantineCase, QuarantineCaseCommand,
    QuarantineCaseEvent, QuarantineCaseRejection, QuarantineCaseState,
};
use mayfly_control_domain::{Actor, Aggregate, Command, Rejection};
use mayfly_domain::{
    AttemptId, CommandId, Epoch, EventId, Millis, ObservationId, ProviderInstanceId, Sample,
};

pub type Step = Command<QuarantineCaseCommand>;

pub const POLICY: Actor = Actor::Automation {
    policy: "quarantine",
};
pub const OPERATOR: Actor = Actor::Operator { uid: 501 };
/// Every kind of actor: only the first is an operator.
pub const ACTORS: [(&str, Actor); 3] = [
    ("operator", OPERATOR),
    ("automation", POLICY),
    ("startup", Actor::Startup),
];

pub fn at(run_epoch: u64, actor: Actor, body: QuarantineCaseCommand) -> Step {
    Command::new(
        CommandId::from_u128(0xC1),
        Millis::new(1_000),
        Epoch::new(run_epoch),
        actor,
        body,
    )
}

pub fn auto(body: QuarantineCaseCommand) -> Step {
    at(1, POLICY, body)
}

pub fn by_operator(body: QuarantineCaseCommand) -> Step {
    at(1, OPERATOR, body)
}

pub fn open_with(reason: QuarantineReason, proven: bool) -> QuarantineCaseCommand {
    QuarantineCaseCommand::OpenCase {
        attempt_id: AttemptId::from_u128(0xA001),
        reason,
        instance_id: proven.then(ProviderInstanceId::sample),
        holds: Holds {
            disk_bytes: 60 << 30,
            memory_bytes: None,
            macos_guest: true,
        },
        evidence: vec![EventId::from_u128(0xE1)],
    }
}

pub fn open() -> QuarantineCaseCommand {
    open_with(QuarantineReason::ProviderIdentityMismatch, true)
}

pub fn resolve(disposition: Disposition, absence: bool) -> QuarantineCaseCommand {
    QuarantineCaseCommand::ResolveCase {
        disposition,
        absence: absence.then(|| ObservationId::from_u128(0x0B1)),
    }
}

pub fn escalate(level: EscalationLevel) -> QuarantineCaseCommand {
    QuarantineCaseCommand::EscalateCase { level }
}

/// Decides `step` and folds what it accepted.
pub fn apply(
    state: &QuarantineCaseState,
    step: Step,
) -> Result<(QuarantineCaseState, Vec<QuarantineCaseEvent>), QuarantineCaseRejection> {
    let events = QuarantineCase::decide(state, step)?;
    let next = events.iter().fold(state.clone(), QuarantineCase::evolve);
    Ok((next, events))
}

/// The state after `steps`, every one accepted.
///
/// # Panics
/// When a step is refused: the path is wrong, not the aggregate under test.
pub fn reach(steps: impl IntoIterator<Item = Step>) -> QuarantineCaseState {
    steps
        .into_iter()
        .enumerate()
        .fold(QuarantineCase::initial(), |state, (index, step)| {
            apply(&state, step)
                .unwrap_or_else(|r| panic!("step {index} of the path refused: {r:?}"))
                .0
        })
}

/// The refusal code of `step` on `state`, or `"accepted"`.
pub fn outcome(state: &QuarantineCaseState, step: Step) -> String {
    match QuarantineCase::decide(state, step) {
        Ok(_) => "accepted".to_owned(),
        Err(rejection) => rejection.code().to_owned(),
    }
}

/// One state per phase (every disposition), and the variants that decide differently.
pub fn matrix_states() -> Vec<(&'static str, QuarantineCaseState)> {
    let opened = || vec![auto(open())];
    let with = |more: Vec<Step>| {
        let mut path = opened();
        path.extend(more);
        reach(path)
    };
    vec![
        ("not_opened", QuarantineCase::initial()),
        ("open", reach(opened())),
        (
            "open/unproven",
            reach([auto(open_with(QuarantineReason::OwnershipDoubt, false))]),
        ),
        (
            "open/paged",
            with(vec![auto(escalate(EscalationLevel::Page))]),
        ),
        (
            "inspected",
            with(vec![by_operator(QuarantineCaseCommand::InspectCase)]),
        ),
        (
            "resolved/delete_with_proof",
            with(vec![by_operator(resolve(
                Disposition::DeleteWithProof,
                false,
            ))]),
        ),
        (
            "resolved/released_observed_absent",
            with(vec![by_operator(resolve(
                Disposition::ReleasedObservedAbsent,
                true,
            ))]),
        ),
        (
            "resolved/keep",
            with(vec![by_operator(resolve(Disposition::Keep, false))]),
        ),
    ]
}

/// Every command type by every kind of actor (and the variants that decide differently).
pub fn stimuli() -> Vec<(String, Step)> {
    let bodies: Vec<(&str, QuarantineCaseCommand)> = vec![
        ("open", open()),
        (
            "open_vanished",
            open_with(QuarantineReason::VmVanishedExternally, true),
        ),
        ("inspect", QuarantineCaseCommand::InspectCase),
        ("escalate_ticket", escalate(EscalationLevel::Ticket)),
        ("escalate_page", escalate(EscalationLevel::Page)),
        (
            "resolve_delete",
            resolve(Disposition::DeleteWithProof, false),
        ),
        (
            "resolve_released",
            resolve(Disposition::ReleasedObservedAbsent, true),
        ),
        (
            "resolve_released_unobserved",
            resolve(Disposition::ReleasedObservedAbsent, false),
        ),
        ("resolve_keep", resolve(Disposition::Keep, false)),
    ];
    let mut stimuli: Vec<(String, Step)> = bodies
        .into_iter()
        .flat_map(|(name, body)| {
            ACTORS.into_iter().map(move |(actor_name, actor)| {
                (format!("{name}/{actor_name}"), at(1, actor, body.clone()))
            })
        })
        .collect();
    stimuli.push((
        "stale_run".to_owned(),
        at(0, OPERATOR, QuarantineCaseCommand::InspectCase),
    ));
    stimuli
}
