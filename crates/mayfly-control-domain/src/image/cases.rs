//! The image's `DETERMINISM_CASES` (ver §3.1): accepted cases along the image's life and one
//! refused case per [`ImageRejection`] code. Histories are produced by `decide` itself from
//! command paths (each path is checked to be accepted step by step by
//! `should_accept_every_step_when_a_case_path_is_replayed`).

use mayfly_domain::{CommandId, Epoch, ImageDigest, Millis, Sample};

use super::events::ImageEvent;
use super::state::{
    ImageCommand, ImageManifest, ImageRejection, ImageState, RejectReason, RunnerVersion,
    Verification,
};
use super::{Image, decide, evolve};
use crate::aggregate::{DeterminismCase, Expect};
use crate::attempt::GuestOs;
use crate::command::{Actor, Command};

type Step = Command<ImageCommand>;

const POLICY: Actor = Actor::Automation {
    policy: "image_catalog",
};
const OPERATOR: Actor = Actor::Operator { uid: 501 };

fn at(run_epoch: u64, actor: Actor, body: ImageCommand) -> Step {
    Command::new(
        CommandId::from_u128(0xC1),
        Millis::new(1_000),
        Epoch::new(run_epoch),
        actor,
        body,
    )
}

fn auto(body: ImageCommand) -> Step {
    at(1, POLICY, body)
}

fn manifest(revision: u64) -> ImageManifest {
    ImageManifest {
        os: GuestOs::Macos,
        runner_version: RunnerVersion::new(2, 330, 0),
        revision,
    }
}

fn register(revision: u64) -> ImageCommand {
    ImageCommand::RegisterImage {
        digest: ImageDigest::sample(),
        manifest: manifest(revision),
    }
}

const fn passed() -> ImageCommand {
    ImageCommand::RecordVerification {
        outcome: Verification::Passed,
    }
}

const fn failed() -> ImageCommand {
    ImageCommand::RecordVerification {
        outcome: Verification::Failed {
            reason: RejectReason::VerificationFailed,
        },
    }
}

const fn expired(minor: u32) -> ImageCommand {
    ImageCommand::RecordRunnerVersionExpired {
        minimum: RunnerVersion::new(2, minor, 0),
    }
}

fn registered() -> Vec<Step> {
    vec![auto(register(1))]
}

fn verified() -> Vec<Step> {
    let mut path = registered();
    path.push(auto(passed()));
    path
}

fn rejected() -> Vec<Step> {
    let mut path = verified();
    path.push(auto(failed()));
    path
}

fn withdrawn() -> Vec<Step> {
    let mut path = verified();
    path.push(at(1, OPERATOR, ImageCommand::WithdrawImage));
    path
}

fn expired_path() -> Vec<Step> {
    let mut path = verified();
    path.push(auto(expired(331)));
    path
}

fn at_epoch_two() -> Vec<Step> {
    let mut path = registered();
    path.push(at(2, POLICY, passed()));
    path
}

/// Folds `path` through `decide` and `evolve`; the first refused step, if any.
fn replay_path(path: Vec<Step>) -> (Vec<ImageEvent>, Option<(usize, ImageRejection)>) {
    let mut state = ImageState::initial();
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
) -> DeterminismCase<Image> {
    DeterminismCase {
        name,
        history: replay_path(path).0,
        command,
        expect,
    }
}

fn accepted(name: &'static str, path: Vec<Step>, body: ImageCommand) -> DeterminismCase<Image> {
    case(name, path, auto(body), Expect::Accepted)
}

fn refused(name: &'static str, path: Vec<Step>, body: ImageCommand) -> DeterminismCase<Image> {
    case(name, path, auto(body), Expect::Refused)
}

/// Every path a case starts from, for the replay check.
#[cfg(test)]
fn paths() -> Vec<Vec<Step>> {
    vec![
        registered(),
        verified(),
        rejected(),
        withdrawn(),
        expired_path(),
        at_epoch_two(),
    ]
}

pub(super) fn cases() -> Vec<DeterminismCase<Image>> {
    vec![
        // accepted
        accepted("register_image", Vec::new(), register(1)),
        accepted("same_registration_records_nothing", verified(), register(1)),
        accepted("verify_image", registered(), passed()),
        accepted("verified_again_records_nothing", verified(), passed()),
        accepted("reject_registered_image", registered(), failed()),
        accepted("reject_verified_image", verified(), failed()),
        accepted("rejected_again_records_nothing", rejected(), failed()),
        accepted("runner_version_expired", verified(), expired(331)),
        accepted(
            "expired_again_records_nothing",
            expired_path(),
            expired(332),
        ),
        case(
            "operator_withdraws_image",
            rejected(),
            at(1, OPERATOR, ImageCommand::WithdrawImage),
            Expect::Accepted,
        ),
        case(
            "withdrawn_again_records_nothing",
            withdrawn(),
            at(1, OPERATOR, ImageCommand::WithdrawImage),
            Expect::Accepted,
        ),
        case(
            "newer_epoch_is_recorded",
            registered(),
            at(2, POLICY, passed()),
            Expect::Accepted,
        ),
        // refused, one per code
        refused("not_registered", Vec::new(), passed()),
        refused("stale_run", at_epoch_two(), failed()),
        refused("manifest_immutable", verified(), register(2)),
        refused("already_rejected", rejected(), passed()),
        refused("withdrawn", withdrawn(), passed()),
        refused("runner_not_expired", verified(), expired(330)),
        refused("operator_only", verified(), ImageCommand::WithdrawImage),
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
        let refusals: Vec<(usize, Option<(usize, ImageRejection)>)> = paths
            .into_iter()
            .map(|path| replay_path(path).1)
            .enumerate()
            .filter(|(_, refusal)| refusal.is_some())
            .collect();
        // then
        assert!(refusals.is_empty(), "{refusals:?}");
    }
}
