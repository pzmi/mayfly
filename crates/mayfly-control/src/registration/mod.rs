//! The registration manager and the reaper (CP-05; `doc/domain-model.md` §6.5, policies P9 to
//! P14; ADRs 0029, 0038, 0076): every attempt's JIT runner registration at GitHub, from its one
//! mint to its removal by id.
//!
//! - [`RegistrationManager`] is a durable consumer of the `attempt-*` and `registration-*`
//!   streams and a level-triggered [`RegistrationManager::pass`]. For one attempt it reads both
//!   streams and takes the next step ([`RegistrationManager::step`]):
//!   - **P9, the mint**: an activating attempt with its grant and a fresh verification asks for
//!     its registration (`RequestRegistration`, under its VM name, once per stream: I19), mints
//!     the JIT config through [`ScaleSetApi::generate_jit_config`] once, never again, and
//!     records the answer: `RecordIssued{runner_id}`, a definite refusal as `RecordAbsent`, any
//!     other failure as `RecordIssueUnknown`. The config goes to the [`JitBroker`] and only its
//!     handle to the attempt (`RecordJitHandle`): no journal, ledger or log sees it.
//!   - **P13, presumed leaked**: a handle orphaned by a restart, a launch of unknown outcome, a
//!     mint whose config never reached a handle (a crash between the two: F32), or a delivered
//!     runner GitHub does not show online by [`RegistrationConfig::online_deadline`] (ADR 0038)
//!     is `PresumeLeaked`, removed by its journaled id, and its attempt retired, never launched
//!     again (I22).
//!   - **The online confirmation** after delivery: `RecordDelivered`, then `RecordConnected`
//!     once GitHub shows the issued id online or busy.
//!   - **P11, deregister before stop**: an attempt's requested retirement, or the stop of its
//!     quarantined VM (ADR 0076), keyed by its latest `RetirementRequested` or
//!     `StopDeregistrationRequested` event, is answered through the pure
//!     [`retirement_step`](mayfly_control_domain::registration::retirement_step): the removal by
//!     id first, then `ConfirmRetirement`, or `WithdrawRetirement` on a busy answer (I21).
//!   - **P10, an unknown issue** (in the pass): the reaper lists runners by the registration's
//!     exact name; one listed is `RecordFoundByName` (journaled, presumed leaked, removed by its
//!     id), and none is `RecordAbsent` only after a second empty listing at least
//!     [`RegistrationConfig::listing_grace`] after the first, since GitHub's listing may lag
//!     behind a lost mint (`formal/registration.qnt`, `absentOnlyWhenNoRunner`).
//! - [`RegistrationManager::sweep`] is P14: at most one listing of every runner per
//!   [`RegistrationConfig::listing_interval`] (run start, a long outage, daily, `doctor`). A
//!   runner whose id a registration journaled and whose attempt's VM is gone is removed by that
//!   id (`NotLive`); a runner of mayfly's name shape that no registration journaled is reported,
//!   never removed; nothing is ever removed by name (ADR 0029).
//!
//! **Effects are observed, never assumed** (plan §6): a removal GitHub did not answer leaves the
//! request pending and is asked again; GitHub unreachable leaves a registration on the reaper's
//! list (I20); a mint whose outcome is unknown is never retried under the same name (I19).
//!
//! **What this module does not do yet.** The grant that starts an activation (P9's first half)
//! is CP-09's; the frame that carries a config from the broker to the data plane's
//! `LaunchStore` is LNK-02's; P12's first half (job facts to `BindJob`, `RecordConsumed` and
//! `RecordCompleted`) is the listener's [`JobFacts`](crate::listener::JobFacts) consumer, and
//! its second half, a finished attempt whose job completed retired `job_done`, is a step here
//! (GH-04); and the
//! daemon's wiring (the GitHub client, the consumer, the pass and sweep timers, the
//! [`ScaleSetDirectory`]) is RT-07's.
//!
//! [`ScaleSetApi::generate_jit_config`]: crate::github::ScaleSetApi::generate_jit_config

mod broker;
mod manager;
mod reaper;

pub use broker::{AlreadyHeld, JitBroker, JitScope};
pub use manager::{PassReport, RegistrationManager, Step};
pub use reaper::{SweepError, SweepReport, SweepTrigger, UnrecognisedRunner};

use std::collections::BTreeSet;

use mayfly_control_domain::registration::{ScaleSetId, SessionHealth};
use mayfly_domain::{AttemptId, CommandId, Millis, PoolName, SecretHandleId};
use sha2::{Digest, Sha256};

/// The manager's name: the delivery ledger's consumer and the actor of its commands.
pub const REGISTRATION_MANAGER: &str = "registration_manager";

/// The scale set serving each pool, as the journal records it: the registration manager's
/// read of the `ScaleSet` streams (composition builds it over the `scale_sets` projection,
/// RT-07; tests give a fixed one).
pub trait ScaleSetDirectory: Send + Sync {
    /// The scale set serving `pool` and its session's health, if one is journaled.
    fn scale_set(&self, pool: &PoolName) -> Option<(ScaleSetId, SessionHealth)>;

    /// Every scale set mayfly serves: the reaper reads only their runners.
    fn scale_sets(&self) -> BTreeSet<ScaleSetId>;
}

/// The manager's timing and the JIT's work folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationConfig {
    /// The runner's work folder inside the guest (spec §6.3).
    pub work_folder: String,
    /// How long after the delivery GitHub has to show the issued runner online before it is
    /// presumed leaked (ADR 0038, T_jit).
    pub online_deadline: Millis,
    /// How long after a first empty listing by name the second is taken before an unknown
    /// issue is recorded absent: at least GitHub's listing lag behind a lost mint, which the
    /// model assumes (`graceElapses`). A value below [`MIN_LISTING_GRACE`] is raised to it.
    pub listing_grace: Millis,
    /// The least time between two listings of every runner (P14: at most one an hour).
    pub listing_interval: Millis,
    /// How long the removal of a leaked runner GitHub answered busy for waits before it is asked
    /// again, unless the journal shows its job over first.
    pub busy_retry: Millis,
}

/// The least listing grace (ten minutes): GitHub's listing lag behind a lost mint is assumed
/// shorter ([open: HW-02], `doc/github-scaleset.md` §12), and a shorter configured grace would
/// let an absence be recorded while a runner lives unlisted.
pub const MIN_LISTING_GRACE: Millis = Millis::new(600_000);

impl Default for RegistrationConfig {
    /// `_work`, two minutes to be seen online, ten minutes of listing grace, one listing an
    /// hour, five minutes before a busy leaked runner is asked again.
    fn default() -> Self {
        Self {
            work_folder: "_work".to_owned(),
            online_deadline: Millis::new(120_000),
            listing_grace: MIN_LISTING_GRACE,
            listing_interval: Millis::new(3_600_000),
            busy_retry: Millis::new(300_000),
        }
    }
}

const ID_DOMAIN: &[u8] = b"mayfly-registration-manager-v1\n";

fn derive(tag: &[u8], attempt: AttemptId, revision: Option<u64>) -> u128 {
    let mut hasher = Sha256::new();
    hasher.update(ID_DOMAIN);
    hasher.update(tag);
    hasher.update(attempt.as_u128().to_be_bytes());
    match revision {
        Some(revision) => {
            hasher.update([1]);
            hasher.update(revision.to_be_bytes());
        }
        None => hasher.update([0]),
    }
    let digest: [u8; 32] = hasher.finalize().into();
    let mut head = [0_u8; 16];
    head.copy_from_slice(&digest[..16]);
    u128::from_be_bytes(head)
}

/// The id of a command the manager issues for `attempt` with step `tag`, decided on a stream
/// at `revision` (`None` for a stream with no event, or a step taken once per attempt): a
/// repeat of the same decision on the same state is answered by its receipt, and a decision on
/// a stream that moved is a new command.
#[must_use]
pub fn command_of(tag: &str, attempt: AttemptId, revision: Option<u64>) -> CommandId {
    CommandId::from_u128(derive(tag.as_bytes(), attempt, revision))
}

/// The handle of `attempt`'s JIT config: one per attempt, since its registration mints once
/// (I19). A handle is a name, not a secret: the journal records it.
#[must_use]
pub fn handle_of(attempt: AttemptId) -> SecretHandleId {
    SecretHandleId::from_u128(derive(b"handle", attempt, None))
}
