//! Directives: control plane → data plane (plan §3.2).
//!
//! A directive's intent is journaled as `DirectiveIssued` before it is sent; the relay stamps
//! the session's epoch at send time and re-sends until an answer is definite (at-least-once).
//! The data plane executes each directive id at most once per epoch and answers a repeat with
//! the recorded outcome (`formal/link.qnt`, `executesAtMostOncePerEpoch`).

use std::fmt;
use std::str::FromStr;

use mayfly_domain::{
    AttemptId, Deadline, DirectiveId, Epoch, Generation, HostId, ImageDigest, Millis, OwnedVmName,
    ProviderInstanceId, Resources, SecretHandleId, Slot,
};
use serde::{Deserialize, Serialize};

/// One directive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Directive {
    /// Idempotency key: executed at most once per epoch.
    pub directive_id: DirectiveId,
    /// The session's run epoch, stamped by the relay at send time (I09).
    pub epoch: Epoch,
    /// The attempt and slot generation a VM directive serves; `None` for image and host
    /// directives. Answers echo it, so a stale one is recognised without a lookup.
    pub attempt: Option<AttemptRef>,
    /// After this instant the data plane refuses to start the effect
    /// ([`crate::RefusalReason::DeadlineExpired`]) and bounds one already running.
    pub deadline: Deadline,
    /// What to do.
    pub kind: DirectiveKind,
}

/// The attempt a directive serves and its slot generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AttemptRef {
    /// The attempt.
    pub attempt_id: AttemptId,
    /// Its slot generation.
    pub generation: Generation,
}

/// What a directive asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectiveKind {
    /// Pull an image by digest, never by tag (dm §9.3).
    PullImage {
        /// The image.
        digest: ImageDigest,
    },
    /// Prepare a local image base from a pulled image and a runner version (ADR 0039).
    MaterialiseImage {
        /// The pulled image.
        digest: ImageDigest,
        /// The base to create.
        base_name: BaseName,
        /// The runner version installed into it.
        runner_version: String,
    },
    /// Evict an image base no pool or live clone references (P17).
    EvictImage {
        /// The base.
        base_name: BaseName,
    },
    /// Clone the attempt's VM from a local base (ADR 0039), then give it a random MAC, the
    /// provider instance id `VmCloned` reports (ADR 0026).
    CloneVm {
        /// The local base.
        base_name: BaseName,
        /// The exact name of the clone.
        vm_name: OwnedVmName,
    },
    /// Start the VM under its per-VM shim (ADR 0037).
    StartVm {
        /// The VM.
        vm_name: OwnedVmName,
        /// How it runs.
        run_options: RunOptions,
    },
    /// Challenge the guest over `tart exec` (ADR 0038).
    VerifyGuest {
        /// The VM.
        vm_name: OwnedVmName,
        /// The challenge only this clone can answer.
        nonce: Nonce,
    },
    /// Launch the runner with the JIT config behind `launch`, over the launcher's stdin
    /// (ADR 0038). The stored and sent form names only the handle; the link adapter resolves it
    /// from the control plane's broker at send time into a frame that is not `Serialize`
    /// (ADR 0014, R14).
    LaunchRunner {
        /// The VM.
        vm_name: OwnedVmName,
        /// The handle of the JIT config, never the config.
        launch: SecretHandle,
    },
    /// Export bounded, redacted diagnostics.
    ExportDiagnostics {
        /// The VM.
        vm_name: OwnedVmName,
    },
    /// Stop the VM with `tart stop`, after re-observing it against the proof (ADR 0026).
    StopVm {
        /// How long the guest may take to shut down.
        grace: Millis,
        /// The ownership proof.
        proof: OwnershipProof,
    },
    /// Delete the VM, after re-observing it against the proof (ADR 0026).
    DeleteVm {
        /// The ownership proof.
        proof: OwnershipProof,
    },
    /// Answer with the provider's inventory.
    ReportInventory,
    /// Answer with a capacity report (ADR 0024).
    ReportCapacity,
    /// Answer with the host's facts.
    ReportHostFacts,
}

/// A directive kind without its payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DirectiveTag {
    /// [`DirectiveKind::PullImage`].
    PullImage,
    /// [`DirectiveKind::MaterialiseImage`].
    MaterialiseImage,
    /// [`DirectiveKind::EvictImage`].
    EvictImage,
    /// [`DirectiveKind::CloneVm`].
    CloneVm,
    /// [`DirectiveKind::StartVm`].
    StartVm,
    /// [`DirectiveKind::VerifyGuest`].
    VerifyGuest,
    /// [`DirectiveKind::LaunchRunner`].
    LaunchRunner,
    /// [`DirectiveKind::ExportDiagnostics`].
    ExportDiagnostics,
    /// [`DirectiveKind::StopVm`].
    StopVm,
    /// [`DirectiveKind::DeleteVm`].
    DeleteVm,
    /// [`DirectiveKind::ReportInventory`].
    ReportInventory,
    /// [`DirectiveKind::ReportCapacity`].
    ReportCapacity,
    /// [`DirectiveKind::ReportHostFacts`].
    ReportHostFacts,
}

impl DirectiveTag {
    /// Every tag.
    pub const ALL: [Self; 13] = [
        Self::PullImage,
        Self::MaterialiseImage,
        Self::EvictImage,
        Self::CloneVm,
        Self::StartVm,
        Self::VerifyGuest,
        Self::LaunchRunner,
        Self::ExportDiagnostics,
        Self::StopVm,
        Self::DeleteVm,
        Self::ReportInventory,
        Self::ReportCapacity,
        Self::ReportHostFacts,
    ];

    /// Whether a session in safe mode refuses this kind (I29): everything that creates,
    /// starts, stops or deletes a VM or a base, pulls, and the launch that takes a secret.
    /// Verification, diagnostics and the reports run, because observation continues in safe
    /// mode. Pinned by `should_hold_exactly_the_effect_kinds_when_in_safe_mode`.
    #[must_use]
    pub const fn held_in_safe_mode(self) -> bool {
        match self {
            Self::PullImage
            | Self::MaterialiseImage
            | Self::EvictImage
            | Self::CloneVm
            | Self::StartVm
            | Self::LaunchRunner
            | Self::StopVm
            | Self::DeleteVm => true,
            Self::VerifyGuest
            | Self::ExportDiagnostics
            | Self::ReportInventory
            | Self::ReportCapacity
            | Self::ReportHostFacts => false,
        }
    }

    /// Whether the data plane checks this kind against its resource ledger first and refuses it
    /// when it does not fit (I33, ADR 0024). Pinned by
    /// `should_check_the_ledger_for_create_start_pull_and_materialise_when_asked`.
    #[must_use]
    pub const fn needs_resource_fit(self) -> bool {
        match self {
            Self::PullImage | Self::MaterialiseImage | Self::CloneVm | Self::StartVm => true,
            Self::EvictImage
            | Self::VerifyGuest
            | Self::LaunchRunner
            | Self::ExportDiagnostics
            | Self::StopVm
            | Self::DeleteVm
            | Self::ReportInventory
            | Self::ReportCapacity
            | Self::ReportHostFacts => false,
        }
    }
}

impl DirectiveKind {
    /// The kind without its payload.
    #[must_use]
    pub const fn tag(&self) -> DirectiveTag {
        match self {
            Self::PullImage { .. } => DirectiveTag::PullImage,
            Self::MaterialiseImage { .. } => DirectiveTag::MaterialiseImage,
            Self::EvictImage { .. } => DirectiveTag::EvictImage,
            Self::CloneVm { .. } => DirectiveTag::CloneVm,
            Self::StartVm { .. } => DirectiveTag::StartVm,
            Self::VerifyGuest { .. } => DirectiveTag::VerifyGuest,
            Self::LaunchRunner { .. } => DirectiveTag::LaunchRunner,
            Self::ExportDiagnostics { .. } => DirectiveTag::ExportDiagnostics,
            Self::StopVm { .. } => DirectiveTag::StopVm,
            Self::DeleteVm { .. } => DirectiveTag::DeleteVm,
            Self::ReportInventory => DirectiveTag::ReportInventory,
            Self::ReportCapacity => DirectiveTag::ReportCapacity,
            Self::ReportHostFacts => DirectiveTag::ReportHostFacts,
        }
    }

    /// See [`DirectiveTag::held_in_safe_mode`].
    #[must_use]
    pub const fn held_in_safe_mode(&self) -> bool {
        self.tag().held_in_safe_mode()
    }

    /// See [`DirectiveTag::needs_resource_fit`].
    #[must_use]
    pub const fn needs_resource_fit(&self) -> bool {
        self.tag().needs_resource_fit()
    }

    /// The VM a VM directive targets.
    #[must_use]
    pub const fn vm_name(&self) -> Option<&OwnedVmName> {
        match self {
            Self::CloneVm { vm_name, .. }
            | Self::StartVm { vm_name, .. }
            | Self::VerifyGuest { vm_name, .. }
            | Self::LaunchRunner { vm_name, .. }
            | Self::ExportDiagnostics { vm_name } => Some(vm_name),
            Self::StopVm { proof, .. } | Self::DeleteVm { proof } => Some(&proof.vm_name),
            Self::PullImage { .. }
            | Self::MaterialiseImage { .. }
            | Self::EvictImage { .. }
            | Self::ReportInventory
            | Self::ReportCapacity
            | Self::ReportHostFacts => None,
        }
    }
}

/// How a VM runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunOptions {
    /// What the VM is given.
    pub resources: Resources,
}

/// The claim a destructive directive carries (dm §5.3): the data plane re-observes the VM
/// immediately before the effect and refuses on any mismatch, with
/// [`crate::RefusalReason::ProofMismatch`] (ADR 0026). The run epoch is the directive's own.
/// Defence in depth, not a signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipProof {
    /// The attempt that cloned the VM.
    pub attempt_id: AttemptId,
    /// The host it runs on.
    pub host_id: HostId,
    /// The slot it holds.
    pub slot: Slot,
    /// The slot generation of its name.
    pub generation: Generation,
    /// The exact VM name.
    pub vm_name: OwnedVmName,
    /// The provider instance id `VmCloned` reported (ADR 0026).
    pub instance_id: ProviderInstanceId,
}

/// The reference to a secret held by the control plane's single-take broker, scoped to one
/// `(attempt, directive, epoch)` (dm §5.3). Secrets cross the link only as this handle
/// (ADR 0010, ADR 0014); the secret itself is never `Serialize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SecretHandle {
    /// The handle's id, the only thing journaled.
    pub handle_id: SecretHandleId,
}

/// An image base's name: `mfb-` and 1 to 60 of `[a-z0-9-]` (dm §5.2, ADR 0039). It reaches
/// `tart` argv, so it is validated where it is decoded: no separator, path or shell
/// metacharacter can pass. Pinned by `should_refuse_a_base_name_when_it_is_malformed`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BaseName(String);

/// A value that does not have the shape its type requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MalformedValue;

impl fmt::Display for MalformedValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("malformed value")
    }
}

const BASE_PREFIX: &str = "mfb-";

fn valid_base(s: &str) -> bool {
    s.strip_prefix(BASE_PREFIX).is_some_and(|rest| {
        (1..=60).contains(&rest.len())
            && rest
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    })
}

impl BaseName {
    /// Validates the shape.
    ///
    /// # Errors
    /// [`MalformedValue`] for anything else.
    pub fn new(value: &str) -> Result<Self, MalformedValue> {
        if valid_base(value) {
            Ok(Self(value.to_owned()))
        } else {
            Err(MalformedValue)
        }
    }

    /// The validated text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BaseName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for BaseName {
    type Error = MalformedValue;
    fn try_from(s: String) -> Result<Self, MalformedValue> {
        if valid_base(&s) {
            Ok(Self(s))
        } else {
            Err(MalformedValue)
        }
    }
}

impl From<BaseName> for String {
    fn from(v: BaseName) -> Self {
        v.0
    }
}

/// A guest challenge: 128 bits from the caller's id port, written as 32 lowercase hex digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Nonce(u128);

impl Nonce {
    /// Wraps 128 bits.
    #[must_use]
    pub const fn from_u128(value: u128) -> Self {
        Self(value)
    }

    /// The 128 bits.
    #[must_use]
    pub const fn as_u128(self) -> u128 {
        self.0
    }
}

impl fmt::Display for Nonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

impl FromStr for Nonce {
    type Err = MalformedValue;
    fn from_str(s: &str) -> Result<Self, MalformedValue> {
        if s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            u128::from_str_radix(s, 16)
                .map(Self)
                .map_err(|_| MalformedValue)
        } else {
            Err(MalformedValue)
        }
    }
}

impl TryFrom<String> for Nonce {
    type Error = MalformedValue;
    fn try_from(s: String) -> Result<Self, MalformedValue> {
        s.parse()
    }
}

impl From<Nonce> for String {
    fn from(v: Nonce) -> Self {
        v.to_string()
    }
}

#[cfg(test)]
mod tests {
    use mayfly_domain::{PoolName, Sample};

    use super::*;

    fn vm() -> OwnedVmName {
        OwnedVmName::new(
            PoolName::sample(),
            HostId::sample(),
            Slot::new(1),
            Generation::FIRST,
            AttemptId::sample(),
        )
    }

    fn proof() -> OwnershipProof {
        OwnershipProof {
            attempt_id: AttemptId::sample(),
            host_id: HostId::sample(),
            slot: Slot::new(1),
            generation: Generation::FIRST,
            vm_name: vm(),
            instance_id: ProviderInstanceId::sample(),
        }
    }

    /// One value of every kind, in [`DirectiveTag::ALL`] order.
    fn every_kind() -> Vec<DirectiveKind> {
        let base = BaseName::new("mfb-base").expect("a valid base name");
        let digest = ImageDigest::sample();
        vec![
            DirectiveKind::PullImage {
                digest: digest.clone(),
            },
            DirectiveKind::MaterialiseImage {
                digest,
                base_name: base.clone(),
                runner_version: "2.330.0".to_owned(),
            },
            DirectiveKind::EvictImage {
                base_name: base.clone(),
            },
            DirectiveKind::CloneVm {
                base_name: base,
                vm_name: vm(),
            },
            DirectiveKind::StartVm {
                vm_name: vm(),
                run_options: RunOptions {
                    resources: Resources::new(1, 1),
                },
            },
            DirectiveKind::VerifyGuest {
                vm_name: vm(),
                nonce: Nonce::from_u128(1),
            },
            DirectiveKind::LaunchRunner {
                vm_name: vm(),
                launch: SecretHandle {
                    handle_id: SecretHandleId::from_u128(1),
                },
            },
            DirectiveKind::ExportDiagnostics { vm_name: vm() },
            DirectiveKind::StopVm {
                grace: Millis::new(1),
                proof: proof(),
            },
            DirectiveKind::DeleteVm { proof: proof() },
            DirectiveKind::ReportInventory,
            DirectiveKind::ReportCapacity,
            DirectiveKind::ReportHostFacts,
        ]
    }

    #[test]
    fn should_tag_every_kind_in_order_when_the_tags_are_listed() {
        // given
        let kinds = every_kind();

        // when
        let tags: Vec<DirectiveTag> = kinds.iter().map(DirectiveKind::tag).collect();

        // then
        assert_eq!(tags, DirectiveTag::ALL);
    }

    #[test]
    fn should_hold_exactly_the_effect_kinds_when_in_safe_mode() {
        // given
        let kinds = every_kind();

        // when
        let held: Vec<DirectiveTag> = kinds
            .iter()
            .filter(|k| k.held_in_safe_mode())
            .map(DirectiveKind::tag)
            .collect();

        // then
        assert_eq!(
            held,
            [
                DirectiveTag::PullImage,
                DirectiveTag::MaterialiseImage,
                DirectiveTag::EvictImage,
                DirectiveTag::CloneVm,
                DirectiveTag::StartVm,
                DirectiveTag::LaunchRunner,
                DirectiveTag::StopVm,
                DirectiveTag::DeleteVm,
            ]
        );
    }

    #[test]
    fn should_check_the_ledger_for_create_start_pull_and_materialise_when_asked() {
        // given
        let kinds = every_kind();

        // when
        let fit: Vec<DirectiveTag> = kinds
            .iter()
            .filter(|k| k.needs_resource_fit())
            .map(DirectiveKind::tag)
            .collect();

        // then
        assert_eq!(
            fit,
            [
                DirectiveTag::PullImage,
                DirectiveTag::MaterialiseImage,
                DirectiveTag::CloneVm,
                DirectiveTag::StartVm,
            ]
        );
        assert!(fit.iter().all(|t| t.held_in_safe_mode()));
    }

    #[test]
    fn should_name_the_vm_when_a_kind_targets_one() {
        // given
        let kinds = every_kind();

        // when
        let named: Vec<bool> = kinds.iter().map(|k| k.vm_name() == Some(&vm())).collect();

        // then
        assert_eq!(
            named,
            [
                false, false, false, true, true, true, true, true, true, true, false, false, false
            ]
        );
    }

    #[test]
    fn should_refuse_a_base_name_when_it_is_malformed() {
        // given
        let bad = [
            "",
            "mfb-",
            "mf-pool0",
            "base",
            "mfb-UPPER",
            "mfb-a/b",
            "mfb-a b",
            "mfb-a;rm",
            "mfb-a.b",
        ];
        let long = format!("mfb-{}", "a".repeat(61));

        // when
        let parsed: Vec<bool> = bad
            .iter()
            .copied()
            .chain([long.as_str()])
            .map(|name| BaseName::new(name).is_ok() || BaseName::try_from(name.to_owned()).is_ok())
            .collect();
        let good = BaseName::new(&format!("mfb-{}", "a".repeat(60)));

        // then
        assert!(parsed.iter().all(|ok| !ok), "{parsed:?}");
        let good = good.expect("60 characters after the prefix is valid");
        assert_eq!(good.to_string(), good.as_str());
        assert_eq!(String::from(good.clone()), good.as_str());
        assert_eq!(MalformedValue.to_string(), "malformed value");
    }

    #[test]
    fn should_round_trip_a_nonce_when_it_is_written_as_hex() {
        // given
        let nonce = Nonce::from_u128(0x0123_4567_89ab_cdef_0011_2233_4455_6677);

        // when
        let text = String::from(nonce);

        // then
        assert_eq!(text, "0123456789abcdef0011223344556677");
        assert_eq!(Nonce::try_from(text), Ok(nonce));
        assert_eq!(nonce.as_u128(), 0x0123_4567_89ab_cdef_0011_2233_4455_6677);
        assert!("0123456789ABCDEF0011223344556677".parse::<Nonce>().is_err());
        assert!("123".parse::<Nonce>().is_err());
    }
}
