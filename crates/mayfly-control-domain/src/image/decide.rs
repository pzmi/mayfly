//! `decide` and `evolve` for the image: every status against every command, as explicit match
//! arms with no wildcard (`ARCH-exhaustive-transition`); the outcome of every pair is pinned by
//! `tests/image/matrix.rs::should_define_every_pair_when_each_status_meets_each_command`.

use mayfly_domain::{Epoch, ImageDigest, Millis};

use super::events::{
    ImageEpochAdvanced, ImageEvent, ImageRegistered, ImageRejected, ImageRunnerVersionExpired,
    ImageVerified, ImageWithdrawn,
};
use super::state::{
    ImageCommand, ImageManifest, ImageRejection, ImageState, ImageStatus, RunnerExpiry,
    RunnerVersion, Verification,
};
use crate::command::{Actor, Command};

/// What `decide` returns.
pub type Decision = Result<Vec<ImageEvent>, ImageRejection>;

/// Decides `command` against `state`. An older run epoch is refused (I18); an accepted command
/// under a newer one records `ImageEpochAdvanced` ahead of its events.
///
/// # Errors
/// The [`ImageRejection`] that names why the command is not allowed.
pub fn decide(state: &ImageState, command: Command<ImageCommand>) -> Decision {
    let Command {
        run_epoch,
        actor,
        issued_at,
        body,
        ..
    } = command;
    if run_epoch < state.epoch {
        return Err(ImageRejection::StaleRun);
    }
    let events = match body {
        ImageCommand::RegisterImage { digest, manifest } => {
            decide_register(state, digest, manifest)?
        }
        ImageCommand::RecordVerification { outcome } => decide_verification(state, outcome)?,
        ImageCommand::RecordRunnerVersionExpired { minimum } => {
            decide_expiry(state, minimum, issued_at)?
        }
        ImageCommand::WithdrawImage => decide_withdraw(state, actor)?,
    };
    Ok(with_epoch(state.epoch, run_epoch, events))
}

fn with_epoch(stored: Epoch, run_epoch: Epoch, events: Vec<ImageEvent>) -> Vec<ImageEvent> {
    if events.is_empty() || run_epoch <= stored {
        return events;
    }
    let mut all = Vec::with_capacity(events.len() + 1);
    all.push(ImageEvent::ImageEpochAdvanced(ImageEpochAdvanced {
        epoch: run_epoch,
    }));
    all.extend(events);
    all
}

/// The digest and manifest are immutable: the same registration again records nothing, another
/// one is refused, whatever the status.
fn decide_register(state: &ImageState, digest: ImageDigest, manifest: ImageManifest) -> Decision {
    match state.status {
        ImageStatus::Unregistered => Ok(vec![ImageEvent::ImageRegistered(ImageRegistered {
            digest,
            manifest,
        })]),
        ImageStatus::Registered
        | ImageStatus::Verified
        | ImageStatus::Rejected
        | ImageStatus::Withdrawn => {
            if state.digest.as_ref() == Some(&digest) && state.manifest == Some(manifest) {
                Ok(vec![])
            } else {
                Err(ImageRejection::ManifestImmutable)
            }
        }
    }
}

/// Status monotonic: a pass verifies only a registered image (a verified one again records
/// nothing); a failure rejects a registered or verified image; a rejected or withdrawn image is
/// never verified. Pinned by
/// `tests/image/rules.rs::should_never_verify_when_the_image_is_withdrawn` and
/// `tests/image/properties.rs::should_never_verify_a_withdrawn_image_when_any_commands_are_decided`.
fn decide_verification(state: &ImageState, outcome: Verification) -> Decision {
    match (state.status, outcome) {
        (ImageStatus::Unregistered, Verification::Passed | Verification::Failed { .. }) => {
            Err(ImageRejection::NotRegistered)
        }
        (ImageStatus::Registered, Verification::Passed) => {
            Ok(vec![ImageEvent::ImageVerified(ImageVerified {})])
        }
        (ImageStatus::Verified, Verification::Passed)
        | (ImageStatus::Rejected, Verification::Failed { .. }) => Ok(vec![]),
        (ImageStatus::Registered | ImageStatus::Verified, Verification::Failed { reason }) => {
            Ok(vec![ImageEvent::ImageRejected(ImageRejected { reason })])
        }
        (ImageStatus::Rejected, Verification::Passed) => Err(ImageRejection::AlreadyRejected),
        (ImageStatus::Withdrawn, Verification::Passed | Verification::Failed { .. }) => {
            Err(ImageRejection::Withdrawn)
        }
    }
}

/// P18: the first expiry is recorded with its instant; a later one records nothing. A minimum
/// the image's runner already meets is refused.
fn decide_expiry(state: &ImageState, minimum: RunnerVersion, at: Millis) -> Decision {
    match state.status {
        ImageStatus::Unregistered => return Err(ImageRejection::NotRegistered),
        ImageStatus::Withdrawn => return Err(ImageRejection::Withdrawn),
        ImageStatus::Registered | ImageStatus::Verified | ImageStatus::Rejected => {}
    }
    let Some(manifest) = state.manifest else {
        return Err(ImageRejection::NotRegistered);
    };
    if manifest.runner_version >= minimum {
        return Err(ImageRejection::RunnerNotExpired);
    }
    if state.runner_expiry.is_some() {
        return Ok(vec![]);
    }
    Ok(vec![ImageEvent::ImageRunnerVersionExpired(
        ImageRunnerVersionExpired { minimum, at },
    )])
}

/// Only an operator withdraws; a second withdrawal records nothing. Whether a pool still pins
/// the image is P17's check, before this command (`doc/domain-model.md` §6.6).
fn decide_withdraw(state: &ImageState, actor: Actor) -> Decision {
    if state.status == ImageStatus::Unregistered {
        return Err(ImageRejection::NotRegistered);
    }
    let operator_uid = match actor {
        Actor::Operator { uid } => uid,
        Actor::Automation { .. } | Actor::Startup => return Err(ImageRejection::OperatorOnly),
    };
    match state.status {
        ImageStatus::Unregistered => Err(ImageRejection::NotRegistered),
        ImageStatus::Withdrawn => Ok(vec![]),
        ImageStatus::Registered | ImageStatus::Verified | ImageStatus::Rejected => {
            Ok(vec![ImageEvent::ImageWithdrawn(ImageWithdrawn {
                operator_uid,
            })])
        }
    }
}

/// Applies one recorded event.
#[must_use]
pub fn evolve(mut state: ImageState, event: &ImageEvent) -> ImageState {
    match event {
        ImageEvent::ImageEpochAdvanced(e) => state.epoch = e.epoch,
        ImageEvent::ImageRegistered(e) => {
            state.status = ImageStatus::Registered;
            state.digest = Some(e.digest.clone());
            state.manifest = Some(e.manifest);
        }
        ImageEvent::ImageVerified(_) => state.status = ImageStatus::Verified,
        ImageEvent::ImageRejected(e) => {
            state.status = ImageStatus::Rejected;
            state.rejection = Some(e.reason);
        }
        ImageEvent::ImageRunnerVersionExpired(e) => {
            state.runner_expiry = Some(RunnerExpiry {
                minimum: e.minimum,
                at: e.at,
            });
        }
        ImageEvent::ImageWithdrawn(_) => state.status = ImageStatus::Withdrawn,
    }
    state
}
