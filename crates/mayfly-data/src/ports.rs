//! The data plane's ports (plan §8.1, ADR 0009): native async traits used through generics.
//!
//! Every effect answers definitely or ambiguously ([`VmError`]); a listing answers complete or
//! not ([`Inventory`]), never an error that a caller could read as "nothing listed" (I25).
//! Time and ids arrive only through [`ProcessClock`] and [`ObservationIds`] (fnd §2.3).
//!
//! **Effects are never dropped.** The executor polls an effect's future (`clone_vm`, `start`,
//! `stop`, `delete`, the image and guest effects) to completion, whatever its directive's
//! deadline: at the deadline it answers `Unknown{deadline}` and keeps the target busy, the
//! effect's slot taken and every listing tainted until the future completes. So an adapter
//! bounds each call itself (its per-call deadline, DP-04), and its future completes only once
//! nothing it started still runs: a hung `tart` child is stopped and reaped first, and a
//! `start` whose shim may still be bringing the VM up returns `Ambiguous` only once the shim is
//! settled (ADR 0037 forbids signalling `tart run`, and no `Drop` can run `tart stop`). Reads
//! (`inventory`, `list`, `resources`, `facts`) may be dropped at their deadline: they change
//! nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;

use mayfly_data_domain::Inventory;
use mayfly_domain::{Epoch, ImageDigest, Millis, ObservationId, OwnedVmName, ProviderInstanceId};
use mayfly_protocol::{
    BaseName, DiagnosticsLoss, GuestFacts, GuestOs, GuestRejection, HostFacts, Nonce,
    OwnershipProof, RunOptions, SecretHandle, UnknownReason,
};

/// Why an effect's outcome is not known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ambiguity {
    /// The call did not finish by its own deadline and was stopped there.
    Deadline,
    /// The process that ran it was lost.
    ProcessLost,
    /// The host slept while it ran.
    HostSlept,
}

impl Ambiguity {
    /// The unknown reason the answer carries.
    #[must_use]
    pub const fn reason(self) -> UnknownReason {
        match self {
            Self::Deadline => UnknownReason::Deadline,
            Self::ProcessLost => UnknownReason::ProcessLost,
            Self::HostSlept => UnknownReason::HostSlept,
        }
    }
}

/// How an effect failed (plan §8.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmError {
    /// Nothing happened: the provider refused before acting. Answered `effect_failed`.
    Definite,
    /// It may or may not have happened. Answered `Unknown`, never success and never a refusal
    /// (I10, ADR 0025).
    Ambiguous(Ambiguity),
    /// Nothing happened: the hypervisor refused another running macOS guest (a `start` only,
    /// domain-model §9.2). Definite like [`VmError::Definite`] and answered `effect_failed` the
    /// same way (pinned by `executor::tests::should_answer_a_hypervisor_refusal_as_a_definite_failure_when_a_start_meets_the_limit`);
    /// kept apart so the refusal can be reported as `HypervisorLimitHit`, which the executor
    /// does not emit yet.
    HypervisorLimit,
}

/// The VM provider (plan §8.1; Tart in DP-04).
pub trait VmProvider: Send + Sync {
    /// Every VM the provider lists, foreign ones included, or [`Inventory::Incomplete`] when the
    /// listing failed, did not parse or did not finish (I25).
    fn inventory(&self) -> impl Future<Output = Inventory> + Send;

    /// Clones `name` from the local base `base` and gives it a random MAC; returns that
    /// instance id (ADR 0026, ADR 0039). Cloning onto a name that exists is an error, as it is
    /// for Tart; the executor re-observes the name before it clones.
    ///
    /// # Errors
    /// [`VmError`].
    fn clone_vm(
        &self,
        base: &BaseName,
        name: &OwnedVmName,
    ) -> impl Future<Output = Result<ProviderInstanceId, VmError>> + Send;

    /// Starts `name` under its per-VM shim (ADR 0037). Starting a VM that runs is an error, as
    /// it is for Tart; the executor re-observes before it starts one.
    ///
    /// # Errors
    /// [`VmError`].
    fn start(
        &self,
        name: &OwnedVmName,
        options: &RunOptions,
    ) -> impl Future<Output = Result<(), VmError>> + Send;

    /// Stops the proven VM with the provider's own stop, giving its guest `grace`.
    ///
    /// # Errors
    /// [`VmError`].
    fn stop(
        &self,
        proof: &OwnershipProof,
        grace: Millis,
    ) -> impl Future<Output = Result<(), VmError>> + Send;

    /// Deletes the proven VM.
    ///
    /// # Errors
    /// [`VmError`].
    fn delete(&self, proof: &OwnershipProof) -> impl Future<Output = Result<(), VmError>> + Send;
}

/// What the image store holds: pulled images and materialised bases with their guests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageListing {
    /// Pulled images.
    pub images: BTreeSet<ImageDigest>,
    /// Materialised bases and the guest their clones run.
    pub bases: BTreeMap<BaseName, GuestOs>,
}

/// The local image cache (plan §8.1; DP-07 completes it).
pub trait ImageStore: Send + Sync {
    /// What is held now.
    ///
    /// # Errors
    /// [`VmError`]: the listing says nothing, and the ledger keeps what it had.
    fn list(&self) -> impl Future<Output = Result<ImageListing, VmError>> + Send;

    /// Pulls `digest`, by digest only.
    ///
    /// # Errors
    /// [`VmError`].
    fn pull(&self, digest: &ImageDigest) -> impl Future<Output = Result<(), VmError>> + Send;

    /// Materialises `base` from `digest` with the runner `runner_version` installed.
    ///
    /// # Errors
    /// [`VmError`].
    fn materialise(
        &self,
        digest: &ImageDigest,
        base: &BaseName,
        runner_version: &str,
    ) -> impl Future<Output = Result<(), VmError>> + Send;

    /// Evicts `base`.
    ///
    /// # Errors
    /// [`VmError`].
    fn evict(&self, base: &BaseName) -> impl Future<Output = Result<(), VmError>> + Send;
}

/// What a guest challenge showed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestAnswer {
    /// The guest answered; its claims never decide ownership (I30).
    Verified(GuestFacts),
    /// The guest failed the challenge.
    Rejected(GuestRejection),
}

/// What a diagnostics export produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Diagnostics {
    /// A `0600` artefact; only its size and digest cross the link.
    Exported {
        /// Size in bytes.
        bytes: u64,
        /// SHA-256 of the artefact.
        sha256: [u8; 32],
    },
    /// Nothing exported, and why.
    Lost(DiagnosticsLoss),
}

/// The guest transport (plan §8.3; `tart exec` in DP-05).
pub trait GuestTransport: Send + Sync {
    /// Challenges the guest of `vm` with `nonce`.
    ///
    /// # Errors
    /// [`VmError`].
    fn verify(
        &self,
        vm: &OwnedVmName,
        nonce: Nonce,
    ) -> impl Future<Output = Result<GuestAnswer, VmError>> + Send;

    /// Launches the runner of `vm` with the JIT config behind `launch`. How the handle's config
    /// reaches the launcher's stdin is DP-05's and the link's (ADR 0014, ADR 0038).
    ///
    /// # Errors
    /// [`VmError`]; an ambiguous one is answered `RunnerLaunchUnknown` (I22).
    fn launch(
        &self,
        vm: &OwnedVmName,
        launch: SecretHandle,
    ) -> impl Future<Output = Result<(), VmError>> + Send;

    /// Exports bounded, redacted diagnostics of `vm`.
    ///
    /// # Errors
    /// [`VmError`].
    fn export_diagnostics(
        &self,
        vm: &OwnedVmName,
    ) -> impl Future<Output = Result<Diagnostics, VmError>> + Send;
}

/// A probe that could not read the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeFailed;

/// The host's resources and facts (DP-06).
pub trait HostProbe: Send + Sync {
    /// Totals, the operator reserve, foreign use and the pressure flags. Foreign use counts
    /// only VMs whose names are not `mf-*`: the ledger counts malformed `mf-*` VMs itself.
    ///
    /// # Errors
    /// [`ProbeFailed`]: foreign use is then unknown in every dimension (ADR 0052).
    fn resources(
        &self,
    ) -> impl Future<Output = Result<mayfly_data_domain::HostProbe, ProbeFailed>> + Send;

    /// The facts `ReportHostFacts` answers with.
    ///
    /// # Errors
    /// [`ProbeFailed`].
    fn facts(&self) -> impl Future<Output = Result<HostFacts, ProbeFailed>> + Send;
}

/// The data plane's clocks (ADR 0020, amendment proposed 2026-11-10). Every wait and every
/// cadence (the observer, a deadline's timer) is on the monotonic clock, so a step of the wall
/// clock moves none of them. The wall clock is read where a reading must be compared with the
/// control plane's (a directive's deadline, at receipt) and for an observation's display-only
/// `observed_at_dp`; the executor reads deadlines in one place, so the amendment's other option
/// (a clock offset taken at the handshake) changes only that place.
pub trait ProcessClock: Send + Sync {
    /// The wall clock: milliseconds since the Unix epoch, the clock the control plane writes
    /// directive deadlines in on this host.
    fn wall(&self) -> Millis;

    /// A monotonic reading, meaningful only within this process.
    fn monotonic(&self) -> Millis;

    /// Completes once the monotonic reading is at or past `at`.
    fn sleep_until(&self, at: Millis) -> impl Future<Output = ()> + Send;
}

/// Mints observation ids (the inbox key, plan §3.2).
pub trait ObservationIds: Send + Sync {
    /// A fresh id, never minted before.
    fn next_id(&self) -> ObservationId;
}

/// The epoch floor kept in `TART_HOME` (ADR 0026): the one durable thing the data plane writes.
pub trait FloorStore: Send + Sync {
    /// Makes `epoch` the floor and syncs it to disk before returning, so no frame of a session
    /// runs before its floor is durable (I17).
    ///
    /// # Errors
    /// [`FloorFailed`]: the session is not entered.
    fn raise(&self, epoch: Epoch) -> impl Future<Output = Result<(), FloorFailed>> + Send;
}

/// The floor could not be made durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloorFailed;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_answer_the_matching_unknown_reason_when_an_effect_is_ambiguous() {
        // given
        let causes = [
            Ambiguity::Deadline,
            Ambiguity::ProcessLost,
            Ambiguity::HostSlept,
        ];

        // when
        let reasons = causes.map(Ambiguity::reason);

        // then
        assert_eq!(
            reasons,
            [
                UnknownReason::Deadline,
                UnknownReason::ProcessLost,
                UnknownReason::HostSlept
            ]
        );
    }
}
