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
use mayfly_domain::{
    Epoch, ImageDigest, Millis, ObservationId, OwnedVmName, ProviderInstanceId, Secret,
};
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
    /// Pulled images, each verified against its digest.
    pub images: BTreeSet<ImageDigest>,
    /// The bases this deployment made that are Ready and pass the clone proof (ADR 0087), and
    /// the guest their clones run (macOS or Linux). An `mfb-*` VM not listed here is counted by
    /// the ledger from the inventory, in full (`Ledger::base_vm`), so every base is counted once.
    pub bases: BTreeMap<BaseName, GuestOs>,
}

/// The local image cache and the bases made from it (plan §8.1, domain-model §9.3, ADR 0039,
/// ADR 0087). Every promise below is a case of the `ImageStore` contract
/// (`mayfly-testkit-data`'s `image_contract`, IC-01..IC-14), run on the in-memory store and on
/// the Tart adapter over `mayfly-fake-tart`; the adapter's own fault cases pin the rest.
pub trait ImageStore: Send + Sync {
    /// What is held now.
    ///
    /// # Errors
    /// [`VmError`]: the listing says nothing, and the ledger keeps what it had.
    fn list(&self) -> impl Future<Output = Result<ImageListing, VmError>> + Send;

    /// Pulls `digest`, by digest only, never a tag (IC-01), and keeps it only when its raw
    /// manifest hashes to the digest and its content matches that manifest (IC-02). One pull per digest at a time: a concurrent pull of the
    /// same digest waits and finds it pulled (IC-04). A pulled image is done.
    ///
    /// # Errors
    /// [`VmError`]: `Definite` for an unknown digest or drifted content, nothing kept.
    fn pull(&self, digest: &ImageDigest) -> impl Future<Output = Result<(), VmError>> + Send;

    /// Materialises `base` from the pulled `digest` with the runner `runner_version` installed
    /// (one boot, the install through `tart exec`, stopped) and records it as this deployment's
    /// own (IC-05). `base` must be the name the digest and runner version give, and the runner
    /// must have a pinned checksum (IC-06); the image must be pulled already (IC-07); a VM of
    /// that name the store did not make is never touched (IC-08). A base already materialised
    /// from the same digest and runner is done.
    ///
    /// # Errors
    /// [`VmError`].
    fn materialise(
        &self,
        digest: &ImageDigest,
        base: &BaseName,
        runner_version: &str,
    ) -> impl Future<Output = Result<(), VmError>> + Send;

    /// Evicts `base`: only one this deployment made, with its recorded identity (IC-11,
    /// IC-12), that no live clone references (IC-10). A base that is gone is done (IC-09).
    ///
    /// # Errors
    /// [`VmError`]: `Definite` for a refusal, the base left as it is.
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

/// What a launch did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchAnswer {
    /// The launcher took the JIT config and started the runner, now or on an earlier delivery
    /// (its status file says so): answered `RunnerLaunched`.
    Delivered,
    /// The identity check right before delivery failed (ADR 0038 decision 2): the VM's run
    /// lock is not held by its shim's child, or its instance id is not the recorded one.
    /// Nothing was delivered; answered `proof_mismatch`, which quarantines the attempt.
    IdentityRefused,
}

/// What the launcher's status file in the guest says about the runner (plan §8.3: runner exit
/// after a restart is read from it, never from `tart run`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerStatus {
    /// No launch reached this guest's launcher.
    NotLaunched,
    /// The runner was started and has not exited.
    Running,
    /// The runner exited with `code`.
    Exited {
        /// Its exit code.
        code: i32,
    },
}

/// The JIT configs held for delivery on the data plane, by handle (ADR 0014, ADR 0082): the
/// link's data-plane end puts each config it receives in the non-`Serialize` wire frame there,
/// bound to its VM, instance id and deadline ([`crate::LaunchStore`]), in memory only, and the
/// guest transport takes it once, right before the launcher's stdin, and discards the handle
/// whatever the launch's outcome. Nothing journals, logs or persists it; a restart empties it.
pub trait LaunchSecrets: Send + Sync {
    /// The config behind `handle`, removed, when it was held for `vm` with `instance_id`; a
    /// config held for another VM or instance id is discarded and `None` answered.
    fn take(
        &self,
        handle: SecretHandle,
        vm: &OwnedVmName,
        instance_id: &ProviderInstanceId,
    ) -> Option<Secret<String>>;

    /// Drops the config behind `handle`, if any is held.
    fn discard(&self, handle: SecretHandle);
}

/// The guest transport (plan §8.3; `tart exec` in DP-05, ADR 0038, ADR 0082).
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

    /// Launches the runner of `vm` with the JIT config behind `launch`, taken from the data
    /// plane's [`LaunchSecrets`] and written only to the launcher's stdin (ADR 0038, ADR 0082),
    /// after checking, right before the delivery, that the VM's run lock is held by its shim's
    /// child (ADR 0037) and that its instance id is `instance_id` (ADR 0026); either mismatch
    /// is [`LaunchAnswer::IdentityRefused`]. A launcher that already started a runner answers
    /// [`LaunchAnswer::Delivered`] without a second delivery. Pinned by the `GuestTransport`
    /// contract (`mayfly-testkit-data`'s `guest_contract`).
    ///
    /// # Errors
    /// [`VmError`]: `Definite` when nothing was delivered (no config behind the handle, the
    /// launcher refused), `Ambiguous` when it may have been, answered `RunnerLaunchUnknown`
    /// (I22).
    fn launch(
        &self,
        vm: &OwnedVmName,
        instance_id: &ProviderInstanceId,
        launch: SecretHandle,
    ) -> impl Future<Output = Result<LaunchAnswer, VmError>> + Send;

    /// What the launcher's status file in the guest of `vm` says, readable by a data plane
    /// restarted since the launch.
    ///
    /// # Errors
    /// [`VmError`]: the guest did not answer, or answered something that does not parse.
    fn runner_status(
        &self,
        vm: &OwnedVmName,
    ) -> impl Future<Output = Result<RunnerStatus, VmError>> + Send;

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
    /// only VMs whose names are neither `mf-*` nor a base's `mfb-*`: the ledger counts
    /// malformed `mf-*` VMs and bases itself (`mayfly_data_domain::foreign_use`).
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
