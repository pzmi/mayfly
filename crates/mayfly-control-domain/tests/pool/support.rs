//! Paths through a pool's life, driven through its public `decide` and `evolve`.

use mayfly_control_domain::attempt::{GuestOs, Stage};
use mayfly_control_domain::pool::{
    ConfigurePool, Pool, PoolCommand, PoolEvent, PoolRejection, PoolState, TripReason, TrustTier,
};
use mayfly_control_domain::{Actor, Aggregate, Command};
use mayfly_domain::{AttemptId, CommandId, Epoch, EventId, ImageDigest, Millis, Resources, Sample};

pub type Step = Command<PoolCommand>;

pub const POLICY: Actor = Actor::Automation {
    policy: "pool_config",
};
pub const OPERATOR: Actor = Actor::Operator { uid: 501 };

pub fn at(run_epoch: u64, actor: Actor, body: PoolCommand) -> Step {
    Command::new(
        CommandId::from_u128(0xC1),
        Millis::new(1_000),
        Epoch::new(run_epoch),
        actor,
        body,
    )
}

pub fn auto(body: PoolCommand) -> Step {
    at(1, POLICY, body)
}

pub fn digest() -> ImageDigest {
    ImageDigest::sample()
}

pub fn configure(revision: u64, min_prepared: Option<u32>) -> PoolCommand {
    PoolCommand::ConfigurePool(ConfigurePool {
        revision,
        tier: TrustTier::Trusted,
        os: GuestOs::Macos,
        vm_resources: Resources::new(8 << 30, 60 << 30),
        max_running: 2,
        min_prepared,
        breaker_threshold: 3,
        breaker_window: Millis::new(600_000),
    })
}

pub fn pin() -> PoolCommand {
    PoolCommand::PinImage {
        digest: digest(),
        verified_ref: Some(EventId::from_u128(0xE1)),
    }
}

pub fn failure(n: u128, at_ms: u64) -> PoolCommand {
    PoolCommand::CountAttemptFailure {
        attempt_id: AttemptId::from_u128(0xA000 + n),
        digest: digest(),
        stage: Stage::Verify,
        at: Millis::new(at_ms),
    }
}

/// P18: the pin's runner version expired at `at_ms`.
pub fn runner_version_expired(at_ms: u64) -> PoolCommand {
    PoolCommand::TripBreaker {
        digest: digest(),
        reason: TripReason::RunnerVersion,
        at: Millis::new(at_ms),
    }
}

/// Decides `step` and folds what it accepted.
pub fn apply(state: &PoolState, step: Step) -> Result<(PoolState, Vec<PoolEvent>), PoolRejection> {
    let events = Pool::decide(state, step)?;
    let next = events.iter().fold(state.clone(), Pool::evolve);
    Ok((next, events))
}

/// The state after `bodies`, every one accepted.
///
/// # Panics
/// When a step is refused: the path is wrong, not the aggregate under test.
pub fn reach(bodies: impl IntoIterator<Item = PoolCommand>) -> PoolState {
    bodies
        .into_iter()
        .enumerate()
        .fold(Pool::initial(), |state, (index, body)| {
            apply(&state, auto(body))
                .unwrap_or_else(|r| panic!("step {index} of the path refused: {r:?}"))
                .0
        })
}

/// The refusal code of `step` on `state`, or `"accepted"`.
pub fn outcome(state: &PoolState, step: Step) -> String {
    match Pool::decide(state, step) {
        Ok(_) => "accepted".to_owned(),
        Err(rejection) => mayfly_control_domain::Rejection::code(&rejection).to_owned(),
    }
}

/// One state per mode, and a pool with a tripped breaker.
pub fn matrix_states() -> Vec<(&'static str, PoolState)> {
    let active = vec![configure(1, None), pin()];
    let with = |more: Vec<PoolCommand>| {
        let mut path = active.clone();
        path.extend(more);
        reach(path)
    };
    vec![
        ("unconfigured", Pool::initial()),
        ("active", reach(active.clone())),
        ("paused", with(vec![PoolCommand::PausePool])),
        ("draining", with(vec![PoolCommand::StartPoolDrain])),
        (
            "drained",
            with(vec![
                PoolCommand::StartPoolDrain,
                PoolCommand::CompletePoolDrain,
            ]),
        ),
        (
            "decommissioned",
            with(vec![
                PoolCommand::StartPoolDrain,
                PoolCommand::CompletePoolDrain,
                PoolCommand::Decommission,
            ]),
        ),
        (
            "tripped",
            with(vec![
                failure(1, 1_000),
                failure(2, 2_000),
                failure(3, 3_000),
            ]),
        ),
    ]
}

/// One stimulus of every command type (and the variants that decide differently).
pub fn stimuli() -> Vec<(&'static str, Step)> {
    vec![
        ("configure_newer", auto(configure(9, None))),
        ("configure_stale", auto(configure(1, None))),
        ("pin_verified", auto(pin())),
        (
            "pin_unverified",
            auto(PoolCommand::PinImage {
                digest: digest(),
                verified_ref: None,
            }),
        ),
        ("pause", auto(PoolCommand::PausePool)),
        ("resume", auto(PoolCommand::ResumePool)),
        ("start_drain", auto(PoolCommand::StartPoolDrain)),
        ("complete_drain", auto(PoolCommand::CompletePoolDrain)),
        ("count_failure", auto(failure(9, 4_000))),
        ("trip_runner_version", auto(runner_version_expired(5_000))),
        (
            "reset_by_operator",
            at(1, OPERATOR, PoolCommand::ResetBreaker { digest: digest() }),
        ),
        (
            "reset_by_automation",
            auto(PoolCommand::ResetBreaker { digest: digest() }),
        ),
        ("decommission", auto(PoolCommand::Decommission)),
        ("stale_run", at(0, POLICY, PoolCommand::PausePool)),
    ]
}
