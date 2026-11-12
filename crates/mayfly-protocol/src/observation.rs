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
use crate::version::{ProtocolVersion, UNKNOWN_FACTS, VANISHED_TARGET};

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

    /// The observation as a frame of a session at `version` may carry it: a frame never uses
    /// more than the minor the session accepted (plan §3.2). Below [`VANISHED_TARGET`] (1.2) a
    /// `target_absent` refusal is written `proof_mismatch` (a definite refusal either way, which
    /// a control plane of 1.1 or older quarantines for a stop; ADR 0066). Below
    /// [`UNKNOWN_FACTS`] (1.1) a
    /// `capacity_unknown` refusal is written `insufficient_resources` of the same dimension (a
    /// definite refusal either way), a `listing_incomplete` unknown reason `deadline` (an
    /// unknown outcome either way), and a report's `unknown` dimensions and a listed VM's facts
    /// are left out; a report's `free` is still zero in every unknown dimension, though a
    /// control plane that admits from totals rather than `free` may propose there, and the
    /// data plane refuses it either way (ADR 0052, correction of 2026-11-10). Pinned by
    /// `should_write_only_what_the_session_accepted_when_it_is_older_than_the_unknown_facts`
    /// and `should_write_proof_mismatch_when_the_session_is_older_than_the_vanished_target`.
    #[must_use]
    pub fn written_in(self, version: ProtocolVersion) -> Self {
        if version >= VANISHED_TARGET {
            return self;
        }
        let before_facts = version < UNKNOWN_FACTS;
        match self {
            Self::DirectiveRefused {
                reason: RefusalReason::TargetAbsent,
            } => Self::DirectiveRefused {
                reason: RefusalReason::ProofMismatch,
            },
            Self::DirectiveRefused {
                reason: RefusalReason::CapacityUnknown { dimension },
            } if before_facts => Self::DirectiveRefused {
                reason: RefusalReason::InsufficientResources { dimension },
            },
            Self::OutcomeUnknown {
                reason: UnknownReason::ListingIncomplete,
            } if before_facts => Self::OutcomeUnknown {
                reason: UnknownReason::Deadline,
            },
            Self::CapacityReported(report) if before_facts => {
                Self::CapacityReported(CapacityReport {
                    unknown: Vec::new(),
                    ..report
                })
            }
            Self::Inventory { vms } if before_facts => Self::Inventory {
                vms: vms
                    .into_iter()
                    .map(|vm| VmObservation {
                        guest: None,
                        memory_bytes: None,
                        disk_bytes: None,
                        ..vm
                    })
                    .collect(),
            },
            other @ (Self::VmCloned { .. }
            | Self::VmBooted
            | Self::GuestVerified { .. }
            | Self::GuestRejected { .. }
            | Self::RunnerLaunched
            | Self::RunnerLaunchUnknown
            | Self::RunnerExited { .. }
            | Self::DiagnosticsExported { .. }
            | Self::DiagnosticsLost { .. }
            | Self::VmStopped
            | Self::VmDeleted
            | Self::VmObservedStopped { .. }
            | Self::InventoryIncomplete { .. }
            | Self::HostFacts(..)
            | Self::HypervisorLimitHit
            | Self::ImagePulled { .. }
            | Self::ImageMaterialised { .. }
            | Self::ImageEvicted { .. }
            | Self::DirectiveRefused { .. }
            | Self::OutcomeUnknown { .. }
            | Self::CapacityReported(..)
            | Self::Inventory { .. }) => other,
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
    /// It needs a dimension whose free amount the data plane cannot determine: a VM's size or
    /// guest is not known, or no listing has been read since the start (ADR 0052). Nothing is
    /// assumed in its place; the dimension comes back when a listing describes it. Written
    /// from protocol 1.1 ([`crate::UNKNOWN_FACTS`]).
    CapacityUnknown {
        /// The first dimension that is not known.
        dimension: ResourceDimension,
    },
    /// The re-observed VM does not match the ownership proof (ADR 0026).
    ProofMismatch,
    /// The `StopVm` or `StartVm` target, a VM the control plane recorded cloning, is absent
    /// from a complete listing: the data plane did not remove it, so the journal and the
    /// provider have desynced, and the control plane quarantines the attempt for review
    /// (ADR 0066). Never touched; a `DeleteVm` of an absent target is done instead (ADR 0053).
    /// Written from protocol 1.2 ([`crate::VANISHED_TARGET`]), `proof_mismatch` before.
    TargetAbsent,
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

impl ResourceDimension {
    /// Every dimension, in the order the ledger checks them.
    pub const ALL: [Self; 4] = [Self::VmSlots, Self::MacosGuests, Self::Memory, Self::Disk];
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
    /// The target of a `StopVm` or `DeleteVm` could not be re-observed: the listing failed, did
    /// not parse or did not finish, so it says nothing about the target (I25), and an earlier
    /// delivery may have taken effect; never `Done` (ADR 0053). Written from protocol 1.1.
    ListingIncomplete,
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
    /// Its guest operating system, when the provider describes it; `None` is not known, never
    /// a default (ADR 0052). From protocol 1.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest: Option<GuestOs>,
    /// The memory it runs with, when the provider describes it. From protocol 1.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    /// Its disk ceiling, when the provider describes it. From protocol 1.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
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
    /// The dimensions whose free amount the data plane cannot determine, each with why (ADR
    /// 0052). In each, `used` counts only what is known and `free` is zero; the data plane
    /// refuses what needs the dimension (`capacity_unknown`) until a listing describes it, and
    /// the control plane reads this list to deliver nothing there (CP-04). Empty (and left out)
    /// when everything is known; from protocol 1.1.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unknown: Vec<UnknownCapacity>,
}

/// One dimension the data plane cannot determine, and why (ADR 0052).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnknownCapacity {
    /// The dimension.
    pub dimension: ResourceDimension,
    /// Every cause, so `mayfly status` and `doctor` can name what to fix.
    pub causes: BTreeSet<UnknownCause>,
}

/// Why a dimension is not known (ADR 0052).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownCause {
    /// No complete inventory has been read since the data plane started: our VMs are not known.
    InventoryNotRead,
    /// No image-store listing has been read since the data plane started, or a listed image or
    /// base's disk has not been measured from its manifest (its kept manifest did not read and
    /// measuring it again failed; ADR 0052's amendment of 2026-11-12, which reuses this cause).
    ImagesNotRead,
    /// The host probe could not size a VM that is not ours.
    ForeignVmUnsized,
    /// A listed VM with the `mf-` prefix whose fact for this dimension is not known (its guest,
    /// its memory while it runs, or its disk).
    VmFactUnknown {
        /// The VM's name as listed.
        vm_name: String,
    },
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

    #[test]
    fn should_write_proof_mismatch_when_the_session_is_older_than_the_vanished_target() {
        // given
        let absent = ObservationKind::DirectiveRefused {
            reason: RefusalReason::TargetAbsent,
        };
        let sessions = [
            ProtocolVersion::new(1, 0),
            UNKNOWN_FACTS,
            VANISHED_TARGET,
            ProtocolVersion::new(1, 3),
        ];

        // when
        let written = sessions.map(|v| absent.clone().written_in(v));

        // then
        let mismatch = ObservationKind::DirectiveRefused {
            reason: RefusalReason::ProofMismatch,
        };
        assert_eq!(
            written,
            [mismatch.clone(), mismatch, absent.clone(), absent.clone()]
        );
    }

    fn report_with_unknown_memory() -> CapacityReport {
        CapacityReport {
            report_id: 1,
            totals: Capacity::default(),
            used: Capacity::default(),
            free: Capacity::default(),
            foreign: Capacity::default(),
            reasons: BTreeSet::new(),
            unknown: vec![UnknownCapacity {
                dimension: ResourceDimension::Memory,
                causes: BTreeSet::from([UnknownCause::VmFactUnknown {
                    vm_name: "mf-x".to_owned(),
                }]),
            }],
        }
    }

    #[test]
    fn should_write_only_what_the_session_accepted_when_it_is_older_than_the_unknown_facts() {
        // given
        let v1_0 = ProtocolVersion::new(1, 0);
        let described = VmObservation {
            name: "mf-x".to_owned(),
            instance_id: None,
            running: true,
            guest: Some(GuestOs::Linux),
            memory_bytes: Some(1),
            disk_bytes: Some(2),
        };
        let kinds = [
            ObservationKind::DirectiveRefused {
                reason: RefusalReason::CapacityUnknown {
                    dimension: ResourceDimension::Disk,
                },
            },
            ObservationKind::OutcomeUnknown {
                reason: UnknownReason::ListingIncomplete,
            },
            ObservationKind::CapacityReported(report_with_unknown_memory()),
            ObservationKind::Inventory {
                vms: vec![described.clone()],
            },
            ObservationKind::VmDeleted,
        ];

        // when
        let old = kinds.clone().map(|k| k.written_in(v1_0));
        let current = kinds.clone().map(|k| k.written_in(UNKNOWN_FACTS));

        // then
        assert_eq!(current, kinds, "a 1.1 session carries everything");
        assert_eq!(
            old,
            [
                ObservationKind::DirectiveRefused {
                    reason: RefusalReason::InsufficientResources {
                        dimension: ResourceDimension::Disk,
                    },
                },
                ObservationKind::OutcomeUnknown {
                    reason: UnknownReason::Deadline,
                },
                ObservationKind::CapacityReported(CapacityReport {
                    unknown: Vec::new(),
                    ..report_with_unknown_memory()
                }),
                ObservationKind::Inventory {
                    vms: vec![VmObservation {
                        guest: None,
                        memory_bytes: None,
                        disk_bytes: None,
                        ..described
                    }],
                },
                ObservationKind::VmDeleted,
            ]
        );
        for kind in &old {
            let text = serde_json::to_string(kind).expect("encodes");
            assert!(
                !text.contains("capacity_unknown")
                    && !text.contains("listing_incomplete")
                    && !text.contains("\"unknown\":")
                    && !text.contains("\"guest\":"),
                "{text}"
            );
        }
    }
}
