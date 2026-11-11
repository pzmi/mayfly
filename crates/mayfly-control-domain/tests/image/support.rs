//! Paths through an image's life, driven through its public `decide` and `evolve`.

use mayfly_control_domain::attempt::GuestOs;
use mayfly_control_domain::image::{
    Image, ImageCommand, ImageEvent, ImageManifest, ImageRejection, ImageState, RejectReason,
    RunnerVersion, Verification,
};
use mayfly_control_domain::{Actor, Aggregate, Command};
use mayfly_domain::{CommandId, Epoch, ImageDigest, Millis, Sample};

pub type Step = Command<ImageCommand>;

pub const POLICY: Actor = Actor::Automation {
    policy: "image_catalog",
};
pub const OPERATOR: Actor = Actor::Operator { uid: 501 };

pub fn at(run_epoch: u64, actor: Actor, body: ImageCommand) -> Step {
    Command::new(
        CommandId::from_u128(0xC1),
        Millis::new(1_000),
        Epoch::new(run_epoch),
        actor,
        body,
    )
}

pub fn auto(body: ImageCommand) -> Step {
    at(1, POLICY, body)
}

pub fn manifest(revision: u64) -> ImageManifest {
    ImageManifest {
        os: GuestOs::Macos,
        runner_version: RunnerVersion::new(2, 330, 0),
        revision,
    }
}

pub fn register(revision: u64) -> ImageCommand {
    ImageCommand::RegisterImage {
        digest: ImageDigest::sample(),
        manifest: manifest(revision),
    }
}

pub const fn passed() -> ImageCommand {
    ImageCommand::RecordVerification {
        outcome: Verification::Passed,
    }
}

pub const fn failed(reason: RejectReason) -> ImageCommand {
    ImageCommand::RecordVerification {
        outcome: Verification::Failed { reason },
    }
}

pub const fn expired(minor: u32) -> ImageCommand {
    ImageCommand::RecordRunnerVersionExpired {
        minimum: RunnerVersion::new(2, minor, 0),
    }
}

/// Decides `step` and folds what it accepted.
pub fn apply(
    state: &ImageState,
    step: Step,
) -> Result<(ImageState, Vec<ImageEvent>), ImageRejection> {
    let events = Image::decide(state, step)?;
    let next = events.iter().fold(state.clone(), Image::evolve);
    Ok((next, events))
}

/// The state after `steps`, every one accepted.
///
/// # Panics
/// When a step is refused: the path is wrong, not the aggregate under test.
pub fn reach(steps: impl IntoIterator<Item = Step>) -> ImageState {
    steps
        .into_iter()
        .enumerate()
        .fold(Image::initial(), |state, (index, step)| {
            apply(&state, step)
                .unwrap_or_else(|r| panic!("step {index} of the path refused: {r:?}"))
                .0
        })
}

/// The refusal code of `step` on `state`, or `"accepted"`.
pub fn outcome(state: &ImageState, step: Step) -> String {
    match Image::decide(state, step) {
        Ok(_) => "accepted".to_owned(),
        Err(rejection) => mayfly_control_domain::Rejection::code(&rejection).to_owned(),
    }
}

/// One state per status, and an image whose runner expired.
pub fn matrix_states() -> Vec<(&'static str, ImageState)> {
    let registered = vec![auto(register(1))];
    let with = |more: Vec<Step>| {
        let mut path = registered.clone();
        path.extend(more);
        reach(path)
    };
    vec![
        ("unregistered", Image::initial()),
        ("registered", reach(registered.clone())),
        ("verified", with(vec![auto(passed())])),
        (
            "rejected",
            with(vec![auto(failed(RejectReason::VerificationFailed))]),
        ),
        (
            "withdrawn",
            with(vec![
                auto(passed()),
                at(1, OPERATOR, ImageCommand::WithdrawImage),
            ]),
        ),
        ("expired", with(vec![auto(passed()), auto(expired(331))])),
    ]
}

/// One stimulus of every command type (and the variants that decide differently).
pub fn stimuli() -> Vec<(&'static str, Step)> {
    vec![
        ("register_same", auto(register(1))),
        ("register_other", auto(register(2))),
        ("verification_passed", auto(passed())),
        (
            "verification_failed",
            auto(failed(RejectReason::RunnerMismatch)),
        ),
        ("expired_below", auto(expired(331))),
        ("expired_not_below", auto(expired(330))),
        (
            "withdraw_by_operator",
            at(1, OPERATOR, ImageCommand::WithdrawImage),
        ),
        ("withdraw_by_automation", auto(ImageCommand::WithdrawImage)),
        ("stale_run", at(0, POLICY, passed())),
    ]
}
