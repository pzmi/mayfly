//! Builders and names shared by the data-plane tests.

use mayfly_data_domain::{
    DataPlane, Discard, HostProbe, Inventory, Ledger, LinkOutcome, LinkStimulus, Receipt, Sizing,
};
use mayfly_domain::{
    AttemptId, Deadline, DirectiveId, Epoch, Generation, HostId, ImageDigest, Millis, OwnedVmName,
    PoolName, ProviderInstanceId, Resources, Sample, SecretHandleId, Slot,
};
use mayfly_protocol::{
    AttemptRef, BaseName, Capacity, Directive, DirectiveKind, Mode, Nonce, ObservationKind,
    OwnershipProof, ProtocolVersion, RefusalReason, RunOptions, SecretHandle, VmObservation,
    Welcome,
};

pub const GIB: u64 = 1 << 30;
/// The session every test opens.
pub const EPOCH: Epoch = Epoch::new(5);
/// The clock reading every directive is received at, well before its deadline.
pub const NOW: Millis = Millis::new(1_000);
pub const V1_0: ProtocolVersion = ProtocolVersion::new(1, 0);
/// The minor that carries what the data plane cannot determine (ADR 0052, ADR 0053).
pub const V1_1: ProtocolVersion = ProtocolVersion::new(1, 1);
/// The minor that carries the refusal `target_absent` (ADR 0066).
pub const V1_2: ProtocolVersion = ProtocolVersion::new(1, 2);

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

/// `name` as a listing shows it, undescribed: with `instance_id` when the provider reports one.
pub fn listed_vm(
    name: &str,
    instance_id: Option<ProviderInstanceId>,
    running: bool,
) -> VmObservation {
    VmObservation {
        name: name.to_owned(),
        instance_id,
        running,
        guest: None,
        memory_bytes: None,
        disk_bytes: None,
    }
}

/// A complete listing of exactly `vms`.
pub fn listing(vms: Vec<VmObservation>) -> Inventory {
    Inventory::Complete { vms }
}

/// A complete listing of `name` alone, running, with `instance_id`.
pub fn seen(name: &OwnedVmName, instance_id: ProviderInstanceId) -> Inventory {
    listing(vec![listed_vm(&name.to_string(), Some(instance_id), true)])
}

/// A complete listing that shows exactly [`proof`]`(n)`'s VM, running.
pub fn seen_as_proved(n: u128) -> Inventory {
    seen(&vm(n), ProviderInstanceId::sample())
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
            instance_id: None,
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
        image_disk_bytes: Some(50 * GIB),
        base_disk_bytes: Some(60 * GIB),
        clone_disk_bytes: Some(80 * GIB),
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
        foreign_unsized: std::collections::BTreeSet::new(),
        disk_low_water: false,
        memory_pressure: false,
        hypervisor_limit: false,
    }
}

/// A ledger over `probe` that has read a complete inventory and the image store, both empty:
/// everything it counts is known.
pub fn ledger(probe: HostProbe) -> Ledger {
    let mut ledger = unlisted(probe);
    ledger.apply_inventory(&mayfly_data_domain::Inventory::Complete { vms: Vec::new() });
    ledger.apply_images(
        &std::collections::BTreeSet::new(),
        &std::collections::BTreeMap::new(),
        &mayfly_data_domain::MeasuredDisks::default(),
    );
    ledger
}

/// A ledger over `probe` as a start leaves it: nothing listed yet (ADR 0052).
pub fn unlisted(probe: HostProbe) -> Ledger {
    Ledger::new(sizing(), probe)
}

/// A data plane whose session is open at [`EPOCH`] in `mode` over `ledger`, at v1.0.
pub fn open_with(mode: Mode, ledger: Ledger) -> DataPlane {
    open_at(V1_0, mode, ledger)
}

/// A data plane whose session is open at [`EPOCH`] and `version` in `mode` over `ledger`.
pub fn open_at(version: ProtocolVersion, mode: Mode, ledger: Ledger) -> DataPlane {
    let mut dp = DataPlane::start(host(), Epoch::new(4), ledger);
    dp.link(LinkStimulus::Hello);
    dp.link(LinkStimulus::Welcome(Welcome {
        epoch: EPOCH,
        mode,
        accepted_version: version,
        cp_clock: None,
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
        RefusalReason::CapacityUnknown { dimension } => {
            format!("capacity_unknown({})", variant(&format!("{dimension:?}")))
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
                unknown: Vec::new(),
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

/// The listing a re-observation of `kind` finds when its effect is to run: a clone's name
/// absent, a start's VM listed stopped, a stop's or delete's target as proved.
pub fn effect_listing(kind: &DirectiveKind) -> Inventory {
    match kind {
        DirectiveKind::StartVm { vm_name, .. } => listing(vec![listed_vm(
            &vm_name.to_string(),
            Some(ProviderInstanceId::sample()),
            false,
        )]),
        DirectiveKind::StopVm { proof, .. } | DirectiveKind::DeleteVm { proof } => {
            listing(vec![listed_vm(
                &proof.vm_name.to_string(),
                Some(proof.instance_id.clone()),
                true,
            )])
        }
        DirectiveKind::PullImage { .. }
        | DirectiveKind::MaterialiseImage { .. }
        | DirectiveKind::EvictImage { .. }
        | DirectiveKind::CloneVm { .. }
        | DirectiveKind::VerifyGuest { .. }
        | DirectiveKind::LaunchRunner { .. }
        | DirectiveKind::ExportDiagnostics { .. }
        | DirectiveKind::ReportInventory
        | DirectiveKind::ReportCapacity
        | DirectiveKind::ReportHostFacts => listing(Vec::new()),
    }
}

/// Receives `d` at [`NOW`] and, when the domain re-observes it first, reports the
/// [`effect_listing`] so its effect may run: `Execute` then stands for "the effect runs".
pub fn receive_through(dp: &mut DataPlane, d: &Directive) -> Receipt {
    let got = dp.receive(V1_0, d, NOW);
    if got != Receipt::ReObserve {
        return got;
    }
    match dp.reobserved(d.epoch, d.directive_id, &effect_listing(&d.kind)) {
        mayfly_data_domain::Progress::Execute => Receipt::Execute,
        mayfly_data_domain::Progress::Reply(reply) => Receipt::Reply(reply),
        other => panic!("a re-observation for the effect did not let it run: {other:?}"),
    }
}
