//! Builders and names shared by the data-plane tests.

use mayfly_data_domain::{
    DataPlane, Discard, HostProbe, Ledger, LinkOutcome, LinkStimulus, Receipt, Reobservation,
    Sizing,
};
use mayfly_domain::{
    AttemptId, Deadline, DirectiveId, Epoch, Generation, HostId, ImageDigest, Millis, OwnedVmName,
    PoolName, ProviderInstanceId, Resources, Sample, SecretHandleId, Slot,
};
use mayfly_protocol::{
    AttemptRef, BaseName, Capacity, Directive, DirectiveKind, Mode, Nonce, ObservationKind,
    OwnershipProof, ProtocolVersion, RefusalReason, RunOptions, SecretHandle, Welcome,
};

pub const GIB: u64 = 1 << 30;
/// The session every test opens.
pub const EPOCH: Epoch = Epoch::new(5);
/// The clock reading every directive is received at, well before its deadline.
pub const NOW: Millis = Millis::new(1_000);
pub const V1_0: ProtocolVersion = ProtocolVersion::new(1, 0);

pub fn host() -> HostId {
    HostId::sample()
}

pub fn other_host() -> HostId {
    HostId::new("host9").expect("a valid host id")
}

pub fn attempt(n: u128) -> AttemptId {
    AttemptId::from_u128(n)
}

pub fn vm_on(host: HostId, n: u128) -> OwnedVmName {
    OwnedVmName::new(
        PoolName::sample(),
        host,
        Slot::new(1),
        Generation::FIRST,
        attempt(n),
    )
}

/// The VM of attempt `n` on this host, slot 1, generation 1.
pub fn vm(n: u128) -> OwnedVmName {
    vm_on(host(), n)
}

pub fn instance(text: &str) -> ProviderInstanceId {
    ProviderInstanceId::new(text).expect("a valid instance id")
}

/// The proof of attempt `n`'s VM with the sample instance id.
pub fn proof(n: u128) -> OwnershipProof {
    OwnershipProof {
        attempt_id: attempt(n),
        host_id: host(),
        slot: Slot::new(1),
        generation: Generation::FIRST,
        vm_name: vm(n),
        instance_id: ProviderInstanceId::sample(),
    }
}

/// A re-observation of `name` with `instance_id`.
pub fn seen(name: OwnedVmName, instance_id: ProviderInstanceId) -> Reobservation {
    Reobservation {
        vm_name: name,
        instance_id,
    }
}

/// The re-observation that matches [`proof`]`(n)` exactly.
pub fn seen_as_proved(n: u128) -> Reobservation {
    seen(vm(n), ProviderInstanceId::sample())
}

pub fn base() -> BaseName {
    BaseName::new("mfb-base").expect("a valid base name")
}

pub fn digest() -> ImageDigest {
    ImageDigest::sample()
}

pub fn run_options() -> RunOptions {
    RunOptions {
        resources: Resources::new(8 * GIB, 80 * GIB),
    }
}

/// One value of every kind in `DirectiveTag::ALL` order, the VM ones for attempt 1.
pub fn every_kind() -> Vec<DirectiveKind> {
    vec![
        DirectiveKind::PullImage { digest: digest() },
        DirectiveKind::MaterialiseImage {
            digest: digest(),
            base_name: base(),
            runner_version: "2.330.0".to_owned(),
        },
        DirectiveKind::EvictImage { base_name: base() },
        DirectiveKind::CloneVm {
            base_name: base(),
            vm_name: vm(1),
        },
        DirectiveKind::StartVm {
            vm_name: vm(1),
            run_options: run_options(),
        },
        DirectiveKind::VerifyGuest {
            vm_name: vm(1),
            nonce: Nonce::from_u128(1),
        },
        DirectiveKind::LaunchRunner {
            vm_name: vm(1),
            launch: SecretHandle {
                handle_id: SecretHandleId::from_u128(1),
            },
        },
        DirectiveKind::ExportDiagnostics { vm_name: vm(1) },
        DirectiveKind::StopVm {
            grace: Millis::new(1),
            proof: proof(1),
        },
        DirectiveKind::DeleteVm { proof: proof(1) },
        DirectiveKind::ReportInventory,
        DirectiveKind::ReportCapacity,
        DirectiveKind::ReportHostFacts,
    ]
}

/// A directive of the session for attempt 1 at generation 1.
pub fn directive(id: u128, epoch: Epoch, kind: DirectiveKind) -> Directive {
    Directive {
        directive_id: DirectiveId::from_u128(id),
        epoch,
        attempt: Some(AttemptRef {
            attempt_id: attempt(1),
            generation: Generation::FIRST,
        }),
        deadline: Deadline::at(Millis::new(60_000)),
        kind,
    }
}

pub fn sizing() -> Sizing {
    Sizing {
        image_disk_bytes: 50 * GIB,
        base_disk_bytes: 60 * GIB,
        clone_disk_bytes: 80 * GIB,
        vm_memory_bytes: 8 * GIB,
    }
}

pub fn capacity(vm_slots: u32, macos_guests: u32, memory_gib: u64, disk_gib: u64) -> Capacity {
    Capacity {
        vm_slots,
        macos_guests,
        resources: Resources::new(memory_gib * GIB, disk_gib * GIB),
    }
}

/// A host with room for everything: four slots, two macOS guests, 64 GiB, 1000 GiB.
pub fn roomy() -> HostProbe {
    HostProbe {
        totals: capacity(4, 2, 64, 1000),
        reserve: Capacity::default(),
        foreign: Capacity::default(),
        disk_low_water: false,
        memory_pressure: false,
        hypervisor_limit: false,
    }
}

pub fn ledger(probe: HostProbe) -> Ledger {
    Ledger::new(sizing(), probe)
}

/// A data plane whose session is open at [`EPOCH`] in `mode` over `ledger`.
pub fn open_with(mode: Mode, ledger: Ledger) -> DataPlane {
    let mut dp = DataPlane::start(host(), Epoch::new(4), ledger);
    dp.link(LinkStimulus::Hello);
    dp.link(LinkStimulus::Welcome(Welcome {
        epoch: EPOCH,
        mode,
        accepted_version: V1_0,
    }));
    let entered = dp.link(LinkStimulus::FloorRaised(EPOCH));
    assert_eq!(entered, LinkOutcome::Entered(EPOCH));
    dp
}

/// A data plane whose session is open at [`EPOCH`], normal, on a roomy host.
pub fn open() -> DataPlane {
    open_with(Mode::Normal, ledger(roomy()))
}

/// `CamelCase` to `snake_case`.
pub fn snake(text: &str) -> String {
    let mut out = String::new();
    for (i, c) in text.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The leading identifier of a `Debug` text, in `snake_case`.
pub fn variant(debug: &str) -> String {
    let end = debug
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(debug.len());
    snake(&debug[..end])
}

pub fn refusal(reason: &RefusalReason) -> String {
    match reason {
        RefusalReason::InsufficientResources { dimension } => {
            format!(
                "insufficient_resources({})",
                variant(&format!("{dimension:?}"))
            )
        }
        other => variant(&format!("{other:?}")),
    }
}

pub fn observation(kind: &ObservationKind) -> String {
    match kind {
        ObservationKind::DirectiveRefused { reason } => format!("refused {}", refusal(reason)),
        ObservationKind::OutcomeUnknown { reason } => {
            format!("unknown {}", variant(&format!("{reason:?}")))
        }
        ObservationKind::RunnerLaunchUnknown => "unknown runner_launch_unknown".to_owned(),
        other => format!("applied {}", variant(&format!("{other:?}"))),
    }
}

pub fn receipt(receipt: &Receipt) -> String {
    match receipt {
        Receipt::Discard(Discard::NotEntered) => "discard not_entered".to_owned(),
        Receipt::Discard(Discard::Version(refusal)) => {
            format!("discard version {}", variant(&format!("{refusal:?}")))
        }
        Receipt::Reply(reply) => {
            format!("reply e{} {}", reply.epoch.get(), observation(&reply.kind))
        }
        Receipt::Pending => "pending".to_owned(),
        Receipt::ReObserve => "reobserve".to_owned(),
        Receipt::Execute => "execute".to_owned(),
    }
}

/// The observation a successful effect of `kind` answers with.
pub fn applied(kind: &DirectiveKind) -> ObservationKind {
    match kind {
        DirectiveKind::PullImage { digest } => ObservationKind::ImagePulled {
            digest: digest.clone(),
        },
        DirectiveKind::MaterialiseImage {
            digest, base_name, ..
        } => ObservationKind::ImageMaterialised {
            digest: digest.clone(),
            base_name: base_name.clone(),
        },
        DirectiveKind::EvictImage { base_name } => ObservationKind::ImageEvicted {
            base_name: base_name.clone(),
        },
        DirectiveKind::CloneVm { .. } => ObservationKind::VmCloned {
            instance_id: ProviderInstanceId::sample(),
        },
        DirectiveKind::StartVm { .. } => ObservationKind::VmBooted,
        DirectiveKind::VerifyGuest { .. } => ObservationKind::GuestRejected {
            reason: mayfly_protocol::GuestRejection::ChallengeFailed,
        },
        DirectiveKind::LaunchRunner { .. } => ObservationKind::RunnerLaunched,
        DirectiveKind::ExportDiagnostics { .. } => ObservationKind::DiagnosticsExported {
            bytes: 1,
            sha256: [0; 32],
        },
        DirectiveKind::StopVm { .. } => ObservationKind::VmStopped,
        DirectiveKind::DeleteVm { .. } => ObservationKind::VmDeleted,
        DirectiveKind::ReportInventory => ObservationKind::Inventory { vms: Vec::new() },
        DirectiveKind::ReportCapacity => {
            ObservationKind::CapacityReported(mayfly_protocol::CapacityReport {
                report_id: 1,
                totals: Capacity::default(),
                used: Capacity::default(),
                free: Capacity::default(),
                foreign: Capacity::default(),
                reasons: std::collections::BTreeSet::new(),
            })
        }
        DirectiveKind::ReportHostFacts => ObservationKind::HostFacts(mayfly_protocol::HostFacts {
            disk_free_bytes: 0,
            disk_low_water: false,
            woke: false,
            tart_version: "2.30.0".to_owned(),
            external_guests: 0,
        }),
    }
}
