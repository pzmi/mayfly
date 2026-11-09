//! The pool's folded state, its commands and its refusals.

use std::collections::BTreeMap;

use mayfly_domain::{AttemptId, Epoch, EventId, ImageDigest, Millis, Resources};
use serde::{Deserialize, Serialize};

use crate::aggregate::{CommandBody, Rejection};
use crate::attempt::{GuestOs, Stage};

/// `min_prepared` when the configuration names none: no prepared VM ahead of demand, for every
/// pool, macOS included (ADR 0028). Pinned by
/// `tests/pool/rules.rs::should_default_min_prepared_to_zero_when_the_configuration_names_none`.
pub const MIN_PREPARED_DEFAULT: u32 = 0;

/// The trust tier a pool serves (I32). v0.x has one: trusted workflows only (ADR 0027); an
/// untrusted tier is refused when the configuration is read, before a command exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustTier {
    /// Workflows of the organisation's own repositories, never from forks.
    Trusted,
}

/// The pool's mode (`doc/domain-model.md` §6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolMode {
    /// No `PoolConfigured` yet.
    Unconfigured,
    /// Admitting.
    Active,
    /// Closed for new admission; existing attempts continue (OD15).
    Paused,
    /// Closed, and prepared attempts are retired; Busy finish.
    Draining,
    /// The drain completed: no lease is left in scope.
    Drained,
    /// The pool left the configuration; the stream takes nothing more (attempts reference it).
    Decommissioned,
}

/// The pool's configuration at one revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolConfig {
    /// The configuration revision; only a higher one replaces it.
    pub revision: u64,
    /// The trust tier, fixed for the revision.
    pub tier: TrustTier,
    /// The guest OS of its VMs.
    pub os: GuestOs,
    /// What one VM needs.
    pub vm_resources: Resources,
    /// At most this many attempts at once (live and quarantined).
    pub max_running: u32,
    /// Prepared VMs held ahead of demand.
    pub min_prepared: u32,
    /// Stage failures per window that trip the breaker (at least 1).
    pub breaker_threshold: u32,
    /// The breaker's window.
    pub breaker_window: Millis,
}

/// One stage failure counted by the breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failure {
    /// The attempt that failed: counted once (`CountAttemptFailure` is idempotent per attempt).
    pub attempt_id: AttemptId,
    /// When.
    pub at: Millis,
}

/// The breaker of one image digest (I27).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Breaker {
    /// The failures inside the window, oldest first.
    pub failures: Vec<Failure>,
    /// When it tripped, if it is open; only an operator resets it.
    pub tripped_at: Option<Millis>,
}

/// The state of one pool stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolState {
    /// The mode.
    pub mode: PoolMode,
    /// The highest run epoch accepted (I18).
    pub epoch: Epoch,
    /// The configuration, from `PoolConfigured`.
    pub config: Option<PoolConfig>,
    /// The pinned image: only a digest the image policy saw verified.
    pub image_pin: Option<ImageDigest>,
    /// Breakers by digest; a digest with no failure has no entry.
    pub breakers: BTreeMap<ImageDigest, Breaker>,
    /// Bumped by every change that affects admission; the `Host` applies terms only at a higher
    /// revision (`ApplyPoolTerms`).
    pub terms_revision: u64,
}

impl PoolState {
    /// The state of a stream with no events.
    #[must_use]
    pub const fn initial() -> Self {
        Self {
            mode: PoolMode::Unconfigured,
            epoch: Epoch::new(0),
            config: None,
            image_pin: None,
            breakers: BTreeMap::new(),
            terms_revision: 0,
        }
    }
}

/// The terms a pool offers the hosts (`PoolTermsApplied` through policy P4): what the `Host`
/// checks a reservation and a grant against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolTerms {
    /// The pool's terms revision.
    pub revision: u64,
    /// Open: the pool is active, has a pinned image and that image's breaker is closed.
    pub open: bool,
    /// Its `max_running`.
    pub max_running: u32,
    /// Its tier.
    pub tier: TrustTier,
}

/// The terms of `state`, or `None` before the pool is configured. Pinned by
/// `tests/pool/rules.rs::should_close_the_terms_when_the_breaker_of_the_pin_trips`.
#[must_use]
pub fn terms(state: &PoolState) -> Option<PoolTerms> {
    let config = state.config?;
    let active = match state.mode {
        PoolMode::Active => true,
        PoolMode::Unconfigured
        | PoolMode::Paused
        | PoolMode::Draining
        | PoolMode::Drained
        | PoolMode::Decommissioned => false,
    };
    let pin_closed = state.image_pin.as_ref().is_some_and(|pin| {
        state
            .breakers
            .get(pin)
            .is_none_or(|b| b.tripped_at.is_none())
    });
    Some(PoolTerms {
        revision: state.terms_revision,
        open: active && pin_closed,
        max_running: config.max_running,
        tier: config.tier,
    })
}

/// `ConfigurePool`: the configuration at a revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ConfigurePool {
    /// The revision; must be higher than the stored one.
    pub revision: u64,
    /// The trust tier.
    pub tier: TrustTier,
    /// The guest OS.
    pub os: GuestOs,
    /// What one VM needs.
    pub vm_resources: Resources,
    /// At most this many attempts at once.
    pub max_running: u32,
    /// Prepared VMs ahead of demand; `None` is [`MIN_PREPARED_DEFAULT`].
    pub min_prepared: Option<u32>,
    /// Failures per window that trip the breaker; 0 is read as 1.
    pub breaker_threshold: u32,
    /// The breaker's window.
    pub breaker_window: Millis,
}

/// The commands of `doc/domain-model.md` §6.2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum PoolCommand {
    /// Configure the pool at a new revision.
    ConfigurePool(ConfigurePool),
    /// Pin an image the image policy saw verified.
    PinImage {
        /// The digest.
        digest: ImageDigest,
        /// The image's `ImageVerified` event the policy read; `None` is refused.
        verified_ref: Option<EventId>,
    },
    /// Close for new admission.
    PausePool,
    /// Open again after a pause or a drain.
    ResumePool,
    /// Close and retire prepared attempts.
    StartPoolDrain,
    /// The drain policy saw no lease left in scope.
    CompletePoolDrain,
    /// An attempt failed a stage with this digest (command id: the attempt's event id).
    CountAttemptFailure {
        /// The attempt.
        attempt_id: AttemptId,
        /// The image it ran.
        digest: ImageDigest,
        /// The stage that failed.
        stage: Stage,
        /// When.
        at: Millis,
    },
    /// An operator closes a tripped breaker.
    ResetBreaker {
        /// The digest.
        digest: ImageDigest,
    },
    /// The pool left the configuration.
    Decommission,
}

impl CommandBody for PoolCommand {
    const TYPES: &'static [&'static str] = &[
        "configure_pool",
        "pin_image",
        "pause_pool",
        "resume_pool",
        "start_pool_drain",
        "complete_pool_drain",
        "count_attempt_failure",
        "reset_breaker",
        "decommission",
    ];

    fn command_type(&self) -> &'static str {
        match self {
            Self::ConfigurePool(_) => "configure_pool",
            Self::PinImage { .. } => "pin_image",
            Self::PausePool => "pause_pool",
            Self::ResumePool => "resume_pool",
            Self::StartPoolDrain => "start_pool_drain",
            Self::CompletePoolDrain => "complete_pool_drain",
            Self::CountAttemptFailure { .. } => "count_attempt_failure",
            Self::ResetBreaker { .. } => "reset_breaker",
            Self::Decommission => "decommission",
        }
    }
}

/// A refusal of the `Pool` aggregate. Its serde form is its code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoolRejection {
    /// The stream has no `PoolConfigured`.
    NotConfigured,
    /// The pool is decommissioned.
    Decommissioned,
    /// The command's run epoch is older than the pool's (I18).
    StaleRun,
    /// The configuration revision is not higher than the stored one.
    StaleRevision,
    /// A pin without the image's verification.
    ImageNotVerified,
    /// Already paused.
    AlreadyPaused,
    /// Resume of an active pool.
    NotPaused,
    /// A drain is started or done.
    AlreadyDraining,
    /// No drain to complete.
    NotDraining,
    /// Decommission before the drain completed.
    NotDrained,
    /// Only an operator resets a breaker (I27).
    OperatorOnly,
    /// The digest's breaker is not tripped.
    NotTripped,
}

impl Rejection for PoolRejection {
    const CODES: &'static [&'static str] = &[
        "not_configured",
        "decommissioned",
        "stale_run",
        "stale_revision",
        "image_not_verified",
        "already_paused",
        "not_paused",
        "already_draining",
        "not_draining",
        "not_drained",
        "operator_only",
        "not_tripped",
    ];

    fn code(&self) -> &'static str {
        match self {
            Self::NotConfigured => "not_configured",
            Self::Decommissioned => "decommissioned",
            Self::StaleRun => "stale_run",
            Self::StaleRevision => "stale_revision",
            Self::ImageNotVerified => "image_not_verified",
            Self::AlreadyPaused => "already_paused",
            Self::NotPaused => "not_paused",
            Self::AlreadyDraining => "already_draining",
            Self::NotDraining => "not_draining",
            Self::NotDrained => "not_drained",
            Self::OperatorOnly => "operator_only",
            Self::NotTripped => "not_tripped",
        }
    }
}
