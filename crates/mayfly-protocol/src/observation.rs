//! Observations: data plane → control plane (plan §3.2).
//!
//! Observations are facts, deduplicated by `observation_id` in the control plane's inbox. An
//! answer names the directive it answers; a definite refusal means nothing happened for that
//! directive in that epoch, and an ambiguous outcome is `OutcomeUnknown`, which the control
//! plane leaves only on a later observation (I10, ADR 0025; `formal/link.qnt`,
//! `refusedMeansNothingHappened`).

use std::collections::BTreeSet;

use mayfly_domain::{
    DirectiveId, Epoch, ImageDigest, Millis, ObservationId, OwnedVmName, ProviderInstanceId,
    Resources,
};
use serde::{Deserialize, Serialize};

use crate::directive::{AttemptRef, BaseName};

/// One observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    /// The inbox key.
    pub observation_id: ObservationId,
    /// The session's run epoch when it was observed; the inbox ignores one older than the
    /// newest it applied for the same stream (I18).
    pub epoch: Epoch,
    /// The directive it answers; `None` for an unsolicited fact.
    pub answers: Option<DirectiveRef>,
    /// When it was observed, by the data plane's clock: for display only, never for order.
    pub observed_at_dp: Millis,
    /// What was observed.
    pub kind: ObservationKind,
}

/// The directive an observation answers, echoed from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DirectiveRef {
    /// The directive.
    pub directive_id: DirectiveId,
    /// The directive's attempt and generation, when it had one.
    pub attempt: Option<AttemptRef>,
}

/// What was observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationKind {
    /// The clone exists, with this provider instance id (ADR 0026).
    VmCloned {
        /// The random MAC read back after `tart set --random-mac`.
        instance_id: ProviderInstanceId,
    },
    /// The VM runs under its shim.
    VmBooted,
    /// The guest answered the challenge.
    GuestVerified {
        /// The guest's own typed claims; they never decide ownership (I30).
        facts: GuestFacts,
    },
    /// The guest failed the challenge.
    GuestRejected {
        /// Why.
        reason: GuestRejection,
    },
    /// The launcher took the JIT config and the runner started.
    RunnerLaunched,
    /// Whether the launcher took the JIT config is not known: presumed leaked (I22).
    RunnerLaunchUnknown,
    /// The runner process exited.
    RunnerExited {
        /// The VM.
        vm_name: OwnedVmName,
        /// Its exit code.
        code: i32,
    },
    /// Diagnostics were exported to a `0600` artefact; only its size and digest cross.
    DiagnosticsExported {
        /// Size in bytes.
        bytes: u64,
        /// SHA-256 of the artefact.
        sha256: [u8; 32],
    },
    /// Diagnostics could not be exported.
    DiagnosticsLost {
        /// Why.
        reason: DiagnosticsLoss,
    },
    /// The VM stopped on a `StopVm`.
    VmStopped,
    /// The VM is gone after a `DeleteVm`.
    VmDeleted,
    /// The provider reports the VM stopped, not by a directive.
    VmObservedStopped {
        /// The VM.
        vm_name: OwnedVmName,
    },
    /// A complete inventory: exactly what the provider lists. The control plane derives each
    /// attempt's `VmPresent`/`VmAbsent` from it.
    Inventory {
        /// Every VM listed, foreign ones included.
        vms: Vec<VmObservation>,
    },
    /// The inventory could not be read completely; never read as the absence of a VM (I25).
    InventoryIncomplete {
        /// Why.
        reason: InventoryGap,
    },
    /// The data plane's resource ledger (ADR 0024).
    CapacityReported(CapacityReport),
    /// Facts about the host.
    HostFacts(HostFacts),
    /// The hypervisor refused another running macOS guest.
    HypervisorLimitHit,
    /// An image was pulled and re-hashed to its digest.
    ImagePulled {
        /// The digest.
        digest: ImageDigest,
    },
    /// An image base was materialised.
    ImageMaterialised {
        /// The image it came from.
        digest: ImageDigest,
        /// The base.
        base_name: BaseName,
    },
    /// An image base was evicted.
    ImageEvicted {
        /// The base.
        base_name: BaseName,
    },
    /// The directive was not executed in this epoch, and will not be: nothing happened.
    DirectiveRefused {
        /// Why.
        reason: RefusalReason,
    },
    /// Whether the directive took effect is not known.
    OutcomeUnknown {
        /// Why.
        reason: UnknownReason,
    },
}

/// What a directive's answer says about its effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectiveOutcome {
    /// It was executed; the observation's kind says what was observed, success or a definite
    /// failure (a rejected guest, lost diagnostics).
    Applied,
    /// Definitely not executed in that epoch: nothing happened.
    Refused(RefusalReason),
    /// Whether it took effect is not known; resolved only by a later observation.
    Unknown,
}

impl ObservationKind {
    /// What the observation says about the directive it answers, or `None` for a fact that is
    /// never an answer. Pinned by `should_classify_every_answer_when_its_outcome_is_asked`.
    #[must_use]
    pub const fn outcome(&self) -> Option<DirectiveOutcome> {
        match self {
            Self::DirectiveRefused { reason } => Some(DirectiveOutcome::Refused(*reason)),
            Self::OutcomeUnknown { .. } | Self::RunnerLaunchUnknown => {
                Some(DirectiveOutcome::Unknown)
            }
            Self::VmCloned { .. }
            | Self::VmBooted
            | Self::GuestVerified { .. }
            | Self::GuestRejected { .. }
            | Self::RunnerLaunched
            | Self::DiagnosticsExported { .. }
            | Self::DiagnosticsLost { .. }
            | Self::VmStopped
            | Self::VmDeleted
            | Self::Inventory { .. }
            | Self::InventoryIncomplete { .. }
            | Self::CapacityReported(..)
            | Self::HostFacts(..)
            | Self::ImagePulled { .. }
            | Self::ImageMaterialised { .. }
            | Self::ImageEvicted { .. } => Some(DirectiveOutcome::Applied),
            Self::RunnerExited { .. }
            | Self::VmObservedStopped { .. }
            | Self::HypervisorLimitHit => None,
        }
    }
}

/// The guest's claims after a successful challenge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestFacts {
    /// Its operating system.
    pub os: GuestOs,
    /// How long it has been up, by its own clock.
    pub uptime: Millis,
}

/// A guest operating system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestOs {
    /// macOS.
    Macos,
    /// Linux.
    Linux,
}

/// Why a guest was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestRejection {
    /// The answer to the challenge was wrong.
    ChallengeFailed,
    /// The image has no guest agent (ADR 0038).
    AgentMissing,
    /// The guest's facts contradict the image's manifest.
    FactsMismatch,
}

/// Why diagnostics were lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticsLoss {
    /// The data plane refused the export.
    Refused,
    /// The guest did not answer.
    GuestUnreachable,
    /// The output exceeded its bound.
    OverLimit,
}

/// Why a directive was refused. Every reason is definite: the effect did not start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    /// It does not fit the resource ledger (I33, ADR 0024).
    InsufficientResources {
        /// The first dimension that does not fit.
        dimension: ResourceDimension,
    },
    /// The re-observed VM does not match the ownership proof (ADR 0026).
    ProofMismatch,
    /// The session is in safe mode and the kind is held (I29).
    SafeMode,
    /// A destructive directive names an image base or a VM the proof cannot cover (ADR 0026).
    ProtectedTarget,
    /// The directive arrived after its deadline.
    DeadlineExpired,
    /// The effect failed definitely (the provider refused the clone, the guest was not ready).
    EffectFailed,
}

/// A dimension of the resource ledger (ADR 0024).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceDimension {
    /// VM slots.
    VmSlots,
    /// Running macOS guests (two per host, foreign ones included).
    MacosGuests,
    /// Memory.
    Memory,
    /// Disk.
    Disk,
}

/// Why an outcome is not known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownReason {
    /// The effect was still running at its deadline and was stopped there.
    Deadline,
    /// The process running the effect was lost (a data-plane restart).
    ProcessLost,
    /// The host slept while the effect ran.
    HostSlept,
    /// The frame was of an older session (I09): it was not executed now, and an earlier
    /// delivery of the same directive may have taken effect (ADR 0042).
    StaleEpoch,
}

/// One VM the provider lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmObservation {
    /// Its name as listed: any text, foreign names included; the control plane classifies it
    /// by shape (`mayfly_domain::classify_vm_name`) and never trusts it further.
    pub name: String,
    /// Its provider instance id, when the provider reports one.
    pub instance_id: Option<ProviderInstanceId>,
    /// Whether it runs.
    pub running: bool,
}

/// Why an inventory is incomplete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryGap {
    /// The listing command failed.
    ListFailed,
    /// Its output did not parse.
    Unparseable,
    /// It did not finish by its deadline.
    Deadline,
}

/// The resource ledger at one moment (ADR 0024).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityReport {
    /// Orders reports within one epoch: a higher `(epoch, report_id)` wins.
    pub report_id: u64,
    /// What the host has for VMs, after operator reserves.
    pub totals: Capacity,
    /// What running and in-flight VMs use, ours and foreign.
    pub used: Capacity,
    /// What is free.
    pub free: Capacity,
    /// The part of `used` that foreign VMs take.
    pub foreign: Capacity,
    /// What currently constrains the host.
    pub reasons: BTreeSet<CapacityConstraint>,
}

/// One amount per ledger dimension.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capacity {
    /// VM slots.
    pub vm_slots: u32,
    /// Running macOS guests.
    pub macos_guests: u32,
    /// Memory and disk.
    pub resources: Resources,
}

/// What constrains a host's capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityConstraint {
    /// Free disk is under the low-water mark.
    DiskLowWater,
    /// The host reports memory pressure.
    MemoryPressure,
    /// Foreign VMs take capacity.
    ForeignGuests,
    /// The operator reserved capacity.
    OperatorReserve,
    /// The hypervisor's guest limit is reached.
    HypervisorLimit,
}

/// Facts about the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostFacts {
    /// Free disk on the volume of `TART_HOME`, in bytes.
    pub disk_free_bytes: u64,
    /// Whether free disk is under the low-water mark.
    pub disk_low_water: bool,
    /// Whether the host woke from sleep since the last report.
    pub woke: bool,
    /// The provider's version string.
    pub tart_version: String,
    /// Running guests mayfly does not own.
    pub external_guests: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_classify_every_answer_when_its_outcome_is_asked() {
        // given
        let kinds = [
            ObservationKind::DirectiveRefused {
                reason: RefusalReason::SafeMode,
            },
            ObservationKind::OutcomeUnknown {
                reason: UnknownReason::StaleEpoch,
            },
            ObservationKind::RunnerLaunchUnknown,
            ObservationKind::VmBooted,
            ObservationKind::HypervisorLimitHit,
            ObservationKind::VmObservedStopped {
                vm_name: mayfly_domain::Sample::sample(),
            },
        ];

        // when
        let outcomes = kinds.map(|k| k.outcome());

        // then
        assert_eq!(
            outcomes,
            [
                Some(DirectiveOutcome::Refused(RefusalReason::SafeMode)),
                Some(DirectiveOutcome::Unknown),
                Some(DirectiveOutcome::Unknown),
                Some(DirectiveOutcome::Applied),
                None,
                None,
            ]
        );
    }
}
