//! The image's folded state, the value types its events carry, its commands and its refusals.

use mayfly_domain::{Epoch, ImageDigest, Millis};
use serde::{Deserialize, Serialize};

use crate::aggregate::{CommandBody, Rejection};
use crate::attempt::GuestOs;

/// A GitHub Actions runner version, `major.minor.patch` (ADR 0030: baked into the image at a
/// pinned version). Three numbers rather than a string: it orders by number (`2.330.0` is
/// newer than `2.99.9`), and it names a base on disk ([`base_name`]) with no free text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RunnerVersion {
    /// The major version.
    pub major: u32,
    /// The minor version.
    pub minor: u32,
    /// The patch version.
    pub patch: u32,
}

impl RunnerVersion {
    /// `major.minor.patch`.
    #[must_use]
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

/// The first twelve hex digits of `digest`: how a stream and a base name abbreviate it
/// (`doc/domain-model.md` §5.2, §6.7).
#[must_use]
pub fn digest12(digest: &ImageDigest) -> &str {
    let hex = digest
        .as_str()
        .strip_prefix("sha256:")
        .unwrap_or(digest.as_str());
    hex.get(..12).unwrap_or(hex)
}

/// The image base materialised from `digest` with `runner` installed:
/// `mfb-<digest12>-r<major>-<minor>-<patch>` (ADR 0039 decision 2, `doc/tasks.md` KER-01). The
/// name is built from numbers and hex digits only, so it is always a valid `mfb-*` base name
/// (`[a-z0-9-]`, at most 50 characters), and distinct runner versions of one digest name
/// distinct bases. Pinned by `tests/placement/rules.rs::should_name_the_base_by_digest_and_runner_version_when_materialising`.
#[must_use]
pub fn base_name(digest: &ImageDigest, runner: RunnerVersion) -> String {
    format!(
        "mfb-{}-r{}-{}-{}",
        digest12(digest),
        runner.major,
        runner.minor,
        runner.patch
    )
}

/// What an image is, from its manifest (OPS-02 writes the manifest; the launcher hash and the
/// required host join it there, as optional fields).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageManifest {
    /// The guest OS.
    pub os: GuestOs,
    /// The runner baked into it (ADR 0030).
    pub runner_version: RunnerVersion,
    /// The image revision a VM reports (I14 reads "image revision").
    pub revision: u64,
}

/// Why an image was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// The verification boot or its guest checks failed.
    VerificationFailed,
    /// The guest reported a runner version other than the manifest's.
    RunnerMismatch,
    /// The content re-hashed to another digest on a host (`ImageDriftDetected`, P17).
    Drift,
}

/// An image's status. It only moves forward (`rank`): `Registered` → `Verified` →
/// `Rejected` → `Withdrawn`, any step skippable, none undone; a rejected or withdrawn image is
/// never verified again (`doc/domain-model.md` §6.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageStatus {
    /// No `ImageRegistered` yet.
    Unregistered,
    /// Registered, not verified.
    Registered,
    /// Verified: a pool may pin it.
    Verified,
    /// Rejected: never verified again.
    Rejected,
    /// Withdrawn: never verified again, and takes no command but a repeated withdrawal.
    Withdrawn,
}

impl ImageStatus {
    /// The status's place in the only order it moves in. Pinned by
    /// `tests/image/properties.rs::should_never_move_the_status_back_when_any_commands_are_decided`.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Unregistered => 0,
            Self::Registered => 1,
            Self::Verified => 2,
            Self::Rejected => 3,
            Self::Withdrawn => 4,
        }
    }
}

/// The runner version expired (P18): the minimum GitHub accepts and when it was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunnerExpiry {
    /// The lowest runner version GitHub accepts.
    pub minimum: RunnerVersion,
    /// When the expiry was recorded.
    pub at: Millis,
}

/// The state of one image stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageState {
    /// The status.
    pub status: ImageStatus,
    /// The highest run epoch accepted (I18).
    pub epoch: Epoch,
    /// The digest, immutable once registered.
    pub digest: Option<ImageDigest>,
    /// The manifest, immutable once registered.
    pub manifest: Option<ImageManifest>,
    /// Why it was rejected, once it was.
    pub rejection: Option<RejectReason>,
    /// The runner version's expiry, once recorded.
    pub runner_expiry: Option<RunnerExpiry>,
}

impl ImageState {
    /// The state of a stream with no events.
    #[must_use]
    pub const fn initial() -> Self {
        Self {
            status: ImageStatus::Unregistered,
            epoch: Epoch::new(0),
            digest: None,
            manifest: None,
            rejection: None,
            runner_expiry: None,
        }
    }
}

/// The result of a verification run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verification {
    /// Every check passed.
    Passed,
    /// A check failed.
    Failed {
        /// Which.
        reason: RejectReason,
    },
}

/// The commands of `doc/domain-model.md` §6.6.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ImageCommand {
    /// Register an image by digest with its manifest.
    RegisterImage {
        /// The digest: the stream's identity, never a tag.
        digest: ImageDigest,
        /// The manifest.
        manifest: ImageManifest,
    },
    /// Record a verification run's result.
    RecordVerification {
        /// The result.
        outcome: Verification,
    },
    /// GitHub no longer accepts the image's runner (P18).
    RecordRunnerVersionExpired {
        /// The lowest version GitHub accepts.
        minimum: RunnerVersion,
    },
    /// An operator withdraws the image (P17 refuses it first while a pool pins it).
    WithdrawImage,
}

impl CommandBody for ImageCommand {
    const TYPES: &'static [&'static str] = &[
        "register_image",
        "record_verification",
        "record_runner_version_expired",
        "withdraw_image",
    ];

    fn command_type(&self) -> &'static str {
        match self {
            Self::RegisterImage { .. } => "register_image",
            Self::RecordVerification { .. } => "record_verification",
            Self::RecordRunnerVersionExpired { .. } => "record_runner_version_expired",
            Self::WithdrawImage => "withdraw_image",
        }
    }
}

/// A refusal of the `Image` aggregate. Its serde form is its code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageRejection {
    /// The stream has no `ImageRegistered`.
    NotRegistered,
    /// The command's run epoch is older than the image's (I18).
    StaleRun,
    /// A registration with another digest or manifest: both are immutable.
    ManifestImmutable,
    /// A passed verification of a rejected image: never verified again.
    AlreadyRejected,
    /// The image is withdrawn: never verified again, nothing more recorded.
    Withdrawn,
    /// The image's runner is not older than the minimum named.
    RunnerNotExpired,
    /// Only an operator withdraws an image.
    OperatorOnly,
}

impl Rejection for ImageRejection {
    const CODES: &'static [&'static str] = &[
        "not_registered",
        "stale_run",
        "manifest_immutable",
        "already_rejected",
        "withdrawn",
        "runner_not_expired",
        "operator_only",
    ];

    fn code(&self) -> &'static str {
        match self {
            Self::NotRegistered => "not_registered",
            Self::StaleRun => "stale_run",
            Self::ManifestImmutable => "manifest_immutable",
            Self::AlreadyRejected => "already_rejected",
            Self::Withdrawn => "withdrawn",
            Self::RunnerNotExpired => "runner_not_expired",
            Self::OperatorOnly => "operator_only",
        }
    }
}
