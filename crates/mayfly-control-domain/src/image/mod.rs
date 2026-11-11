//! The `Image` aggregate (`doc/domain-model.md` §6.6): one image's digest, manifest, status and
//! runner expiry, stream `image-<digest>`.
//!
//! Plain data and two pure functions (ADR 0022). It protects alone:
//!
//! - **Digest and manifest immutability**: a registration with another digest or manifest is
//!   refused (`tests/image/rules.rs::should_refuse_another_manifest_when_the_image_is_registered`).
//! - **Status monotonic**: `Registered` → `Verified` → `Rejected` → `Withdrawn`, never back; a
//!   rejected or withdrawn image is never verified
//!   (`tests/image/rules.rs::should_never_verify_when_the_image_is_withdrawn`,
//!   `tests/image/rules.rs::should_never_verify_when_the_image_is_rejected`,
//!   `tests/image/properties.rs::should_never_move_the_status_back_when_any_commands_are_decided`,
//!   `tests/image/properties.rs::should_never_verify_a_withdrawn_image_when_any_commands_are_decided`).
//! - **The runner's expiry** (ADR 0030, P18): recorded once, only below the minimum
//!   (`tests/image/rules.rs::should_record_the_expiry_once_when_the_runner_is_below_the_minimum`).
//! - **I18**: an older run epoch is refused (`stale_run`); a newer one is recorded as
//!   `ImageEpochAdvanced` ahead of the command's events
//!   (`tests/image/rules.rs::should_refuse_an_older_run_epoch_when_a_newer_one_was_accepted`).
//!
//! It must not protect "not withdrawn while pinned" (policy P17 checks the pinned-by projection
//! before `WithdrawImage`) nor placement ([`crate::placement`]).
//!
//! Not modelled in `formal/`: one stream, no effect and no other stream's state; its rules are
//! single-stream safety properties, checked over every reachable pair by the matrix and over
//! random command sequences by the properties (`doc/formal-models.md`, "Not modelled").

mod cases;
mod decide;
mod events;
mod state;

pub use decide::{Decision, decide, evolve};
pub use events::{
    ImageEpochAdvanced, ImageEvent, ImageRegistered, ImageRejected, ImageRunnerVersionExpired,
    ImageVerified, ImageWithdrawn,
};
pub use state::{
    ImageCommand, ImageManifest, ImageRejection, ImageState, ImageStatus, RejectReason,
    RunnerExpiry, RunnerVersion, Verification, base_name, digest12,
};

use crate::aggregate::{Aggregate, DeterminismCase, DeterminismCases};
use crate::command::Command;

/// The `Image` aggregate: a marker grouping its data and functions (ADR 0022).
#[derive(Debug, Clone, Copy, Default)]
pub struct Image;

impl Aggregate for Image {
    const NAME: &'static str = "image";
    type State = ImageState;
    type Body = ImageCommand;
    type Event = ImageEvent;
    type Rejection = ImageRejection;

    fn initial() -> ImageState {
        ImageState::initial()
    }

    fn decide(state: &ImageState, command: Command<ImageCommand>) -> Decision {
        decide::decide(state, command)
    }

    fn evolve(state: ImageState, event: &ImageEvent) -> ImageState {
        decide::evolve(state, event)
    }
}

impl DeterminismCases for Image {
    fn determinism_cases() -> Vec<DeterminismCase<Self>> {
        cases::cases()
    }
}
