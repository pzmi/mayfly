//! The pool's `DETERMINISM_CASES` (ver §3.1): accepted cases along the pool's life and one
//! refused case per [`PoolRejection`] code. Histories are produced by `decide` itself from
//! command paths (each path is checked to be accepted step by step by
//! `should_accept_every_step_when_a_case_path_is_replayed`).

use mayfly_domain::{AttemptId, CommandId, Epoch, EventId, ImageDigest, Millis, Resources, Sample};

use super::events::PoolEvent;
use super::state::{ConfigurePool, PoolCommand, PoolRejection, PoolState, TrustTier};
use super::{Pool, decide, evolve};
use crate::aggregate::{DeterminismCase, Expect};
use crate::attempt::{GuestOs, Stage};
use crate::command::{Actor, Command};

type Step = Command<PoolCommand>;

const POLICY: Actor = Actor::Automation {
    policy: "pool_config",
};
const OPERATOR: Actor = Actor::Operator { uid: 501 };

fn at(run_epoch: u64, actor: Actor, body: PoolCommand) -> Step {
    Command::new(
        CommandId::from_u128(0xC1),
        Millis::new(1_000),
        Epoch::new(run_epoch),
        actor,
        body,
    )
}

fn auto(body: PoolCommand) -> Step {
    at(1, POLICY, body)
}

fn digest() -> ImageDigest {
    ImageDigest::sample()
}

fn configure(revision: u64) -> PoolCommand {
    PoolCommand::ConfigurePool(ConfigurePool {
        revision,
        tier: TrustTier::Trusted,
        os: GuestOs::Macos,
        vm_resources: Resources::new(8 << 30, 60 << 30),
        max_running: 2,
        min_prepared: None,
        breaker_threshold: 2,
        breaker_window: Millis::new(600_000),
    })
}

fn pin() -> PoolCommand {
    PoolCommand::PinImage {
        digest: digest(),
        verified_ref: Some(EventId::from_u128(0xE1)),
    }
}

fn failure(n: u128, at_ms: u64) -> PoolCommand {
    PoolCommand::CountAttemptFailure {
        attempt_id: AttemptId::from_u128(0xA000 + n),
        digest: digest(),
        stage: Stage::Boot,
        at: Millis::new(at_ms),
    }
}

fn then(mut path: Vec<Step>, more: impl IntoIterator<Item = PoolCommand>) -> Vec<Step> {
    path.extend(more.into_iter().map(auto));
    path
}

fn configured() -> Vec<Step> {
    vec![auto(configure(1))]
}

fn pinned() -> Vec<Step> {
    then(configured(), [pin()])
}

fn paused() -> Vec<Step> {
    then(pinned(), [PoolCommand::PausePool])
}

fn draining() -> Vec<Step> {
    then(pinned(), [PoolCommand::StartPoolDrain])
}

fn drained() -> Vec<Step> {
    then(draining(), [PoolCommand::CompletePoolDrain])
}

fn decommissioned() -> Vec<Step> {
    then(drained(), [PoolCommand::Decommission])
}

fn one_failure() -> Vec<Step> {
    then(pinned(), [failure(1, 1_000)])
}

fn tripped() -> Vec<Step> {
    then(one_failure(), [failure(2, 2_000)])
}

fn at_epoch_two() -> Vec<Step> {
    let mut path = pinned();
    path.push(at(2, POLICY, PoolCommand::PausePool));
    path
}

/// Folds `path` through `decide` and `evolve`; the first refused step, if any.
fn replay_path(path: Vec<Step>) -> (Vec<PoolEvent>, Option<(usize, PoolRejection)>) {
    let mut state = PoolState::initial();
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
) -> DeterminismCase<Pool> {
    DeterminismCase {
        name,
        history: replay_path(path).0,
        command,
        expect,
    }
}

fn accepted(name: &'static str, path: Vec<Step>, body: PoolCommand) -> DeterminismCase<Pool> {
    case(name, path, auto(body), Expect::Accepted)
}

fn refused(name: &'static str, path: Vec<Step>, body: PoolCommand) -> DeterminismCase<Pool> {
    case(name, path, auto(body), Expect::Refused)
}

/// Every path a case starts from, for the replay check.
#[cfg(test)]
fn paths() -> Vec<Vec<Step>> {
    vec![
        configured(),
        pinned(),
        paused(),
        draining(),
        drained(),
        decommissioned(),
        one_failure(),
        tripped(),
        at_epoch_two(),
    ]
}

pub(super) fn cases() -> Vec<DeterminismCase<Pool>> {
    vec![
        // accepted
        accepted("configure_pool", Vec::new(), configure(1)),
        accepted("reconfigure_pool", pinned(), configure(2)),
        accepted("pin_image", configured(), pin()),
        accepted("same_pin_records_nothing", pinned(), pin()),
        accepted("pause_pool", pinned(), PoolCommand::PausePool),
        accepted("resume_pool", paused(), PoolCommand::ResumePool),
        accepted("start_pool_drain", paused(), PoolCommand::StartPoolDrain),
        accepted(
            "complete_pool_drain",
            draining(),
            PoolCommand::CompletePoolDrain,
        ),
        accepted("count_failure", pinned(), failure(1, 1_000)),
        accepted("failure_trips_breaker", one_failure(), failure(2, 2_000)),
        accepted(
            "failure_outside_window_does_not_trip",
            one_failure(),
            failure(2, 700_000),
        ),
        accepted(
            "same_attempt_counted_once",
            one_failure(),
            failure(1, 1_500),
        ),
        accepted("tripped_counts_nothing", tripped(), failure(3, 3_000)),
        case(
            "operator_resets_breaker",
            tripped(),
            at(1, OPERATOR, PoolCommand::ResetBreaker { digest: digest() }),
            Expect::Accepted,
        ),
        case(
            "newer_epoch_is_recorded",
            pinned(),
            at(2, POLICY, PoolCommand::PausePool),
            Expect::Accepted,
        ),
        accepted("decommission", drained(), PoolCommand::Decommission),
        // refused, one per code
        refused("not_configured", Vec::new(), pin()),
        refused("decommissioned", decommissioned(), PoolCommand::ResumePool),
        refused("stale_run", at_epoch_two(), PoolCommand::ResumePool),
        refused("stale_revision", pinned(), configure(1)),
        refused(
            "image_not_verified",
            configured(),
            PoolCommand::PinImage {
                digest: digest(),
                verified_ref: None,
            },
        ),
        refused("already_paused", paused(), PoolCommand::PausePool),
        refused("not_paused", pinned(), PoolCommand::ResumePool),
        refused("already_draining", draining(), PoolCommand::StartPoolDrain),
        refused("not_draining", pinned(), PoolCommand::CompletePoolDrain),
        refused("not_drained", pinned(), PoolCommand::Decommission),
        refused(
            "operator_only",
            tripped(),
            PoolCommand::ResetBreaker { digest: digest() },
        ),
        case(
            "not_tripped",
            one_failure(),
            at(1, OPERATOR, PoolCommand::ResetBreaker { digest: digest() }),
            Expect::Refused,
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
        let refusals: Vec<(usize, Option<(usize, PoolRejection)>)> = paths
            .into_iter()
            .map(|path| replay_path(path).1)
            .enumerate()
            .filter(|(_, refusal)| refusal.is_some())
            .collect();
        // then
        assert!(refusals.is_empty(), "{refusals:?}");
    }
}
