//! The data plane's tables, every pair defined (plan §6 rule 8). Each is pinned in
//! `tests/fixtures/matrix/*.txt`, regenerated deliberately from the test's failure text
//! (ver §4.2), so a pair that changes shows in review. Each fixture's header says which states
//! it covers and why the rest are not reachable.

use std::fmt::Write as _;

use mayfly_data_domain::{
    DataPlane, Entry, Link, LinkStimulus, Phase, Seen, Shim, Stage, Stimulus, VmFact, VmFacts,
    executor, link, supervisor,
};
use mayfly_domain::{DirectiveId, Epoch, Millis};
use mayfly_protocol::{
    Directive, DirectiveKind, DirectiveTag, GuestOs, Mode, ObservationKind, ProtocolVersion,
    RefusalReason, UnknownReason, Welcome,
};

use super::support::{
    EPOCH, NOW, V1_0, applied, capacity, directive, every_kind, host, ledger, observation,
    open_with, other_host, proof, receipt, refusal, roomy, variant, vm, vm_on,
};

fn check(golden: &str, text: &str, file: &str) {
    assert!(
        golden == text,
        "the table differs from tests/fixtures/matrix/{file}; if these outcomes are intended, commit it as:\n{text}"
    );
}

fn link_state(link: &Link) -> String {
    match link {
        Link::Idle => "idle".to_owned(),
        Link::Awaiting => "awaiting".to_owned(),
        Link::Entering(w) => format!("entering(e{})", w.epoch.get()),
        Link::Open(s) => format!("open(e{})", s.epoch.get()),
    }
}

fn welcome(epoch: u64, major: u16, minor: u16) -> Welcome {
    Welcome {
        epoch: Epoch::new(epoch),
        mode: Mode::Normal,
        accepted_version: ProtocolVersion::new(major, minor),
        cp_clock: None,
    }
}

/// Each session state at its floor: idle and awaiting at 4, entering e5 at 4, open at 5.
fn link_states() -> Vec<(Link, Epoch)> {
    let open = super::support::open().link;
    vec![
        (Link::Idle, Epoch::new(4)),
        (Link::Awaiting, Epoch::new(4)),
        (Link::Entering(welcome(5, 1, 0)), Epoch::new(4)),
        (open, EPOCH),
    ]
}

#[test]
fn should_define_every_pair_when_each_session_state_meets_each_stimulus() {
    // given
    let stimuli = [
        ("hello", LinkStimulus::Hello),
        ("welcome_e4", LinkStimulus::Welcome(welcome(4, 1, 0))),
        ("welcome_e5", LinkStimulus::Welcome(welcome(5, 1, 0))),
        ("welcome_e6", LinkStimulus::Welcome(welcome(6, 1, 0))),
        ("welcome_e6_v1.1", LinkStimulus::Welcome(welcome(6, 1, 1))),
        ("welcome_e6_v1.2", LinkStimulus::Welcome(welcome(6, 1, 2))),
        ("welcome_e6_v1.3", LinkStimulus::Welcome(welcome(6, 1, 3))),
        ("welcome_e6_v1.4", LinkStimulus::Welcome(welcome(6, 1, 4))),
        ("welcome_e6_v2.0", LinkStimulus::Welcome(welcome(6, 2, 0))),
        ("floor_raised_e5", LinkStimulus::FloorRaised(Epoch::new(5))),
        ("floor_raised_e6", LinkStimulus::FloorRaised(Epoch::new(6))),
    ];
    let mut text = String::from(
        "# link: every session state (at its floor) against every stimulus: the next state and \
         the outcome.\n# Floors: idle, awaiting and entering(e5) at 4, open(e5) at 5. A restart \
         is a new data plane at its persisted floor, so it has no row.\n",
    );

    // when
    for (state, floor) in link_states() {
        for (name, stimulus) in &stimuli {
            let (next, outcome) = link::step(&state, floor, stimulus.clone());
            let _ = writeln!(
                text,
                "{} floor {} x {name} -> {} {outcome:?}",
                link_state(&state),
                floor.get(),
                link_state(&next)
            );
        }
    }

    // then
    check(
        include_str!("../fixtures/matrix/link.txt"),
        &text,
        "link.txt",
    );
}

#[test]
fn should_define_every_pair_when_each_directive_phase_meets_each_stimulus() {
    // given
    let phases = [
        ("received", Phase::Received),
        ("reobserving", Phase::Executing(Stage::ReObserve)),
        ("executing", Phase::Executing(Stage::Effect)),
        (
            "done",
            Phase::Done(ObservationKind::VmCloned {
                instance_id: mayfly_domain::Sample::sample(),
            }),
        ),
        ("refused", Phase::Refused(RefusalReason::SafeMode)),
        ("unknown", Phase::Unknown(UnknownReason::Deadline)),
    ];
    let stimuli = [
        ("admit_reobserve", Stimulus::Admit(Stage::ReObserve)),
        ("admit_effect", Stimulus::Admit(Stage::Effect)),
        ("deny_safe_mode", Stimulus::Deny(RefusalReason::SafeMode)),
        ("reobserved_match", Stimulus::ReObserved { matches: true }),
        (
            "reobserved_mismatch",
            Stimulus::ReObserved { matches: false },
        ),
        ("reobserve_failed", Stimulus::ReObserveFailed),
        ("target_absent", Stimulus::TargetAbsent),
        (
            "satisfied_vm_deleted",
            Stimulus::Satisfied(ObservationKind::VmDeleted),
        ),
        (
            "satisfied_a_refusal",
            Stimulus::Satisfied(ObservationKind::DirectiveRefused {
                reason: RefusalReason::ProofMismatch,
            }),
        ),
        (
            "applied_vm_cloned",
            Stimulus::Applied(ObservationKind::VmCloned {
                instance_id: mayfly_domain::Sample::sample(),
            }),
        ),
        (
            "applied_a_refusal",
            Stimulus::Applied(ObservationKind::DirectiveRefused {
                reason: RefusalReason::EffectFailed,
            }),
        ),
        (
            "applied_an_unknown",
            Stimulus::Applied(ObservationKind::RunnerLaunchUnknown),
        ),
        (
            "applied_a_fact",
            Stimulus::Applied(ObservationKind::HypervisorLimitHit),
        ),
        ("failed", Stimulus::Failed),
        ("identity_refused", Stimulus::IdentityRefused),
        (
            "ambiguous_deadline",
            Stimulus::Ambiguous(UnknownReason::Deadline),
        ),
        (
            "ambiguous_process_lost",
            Stimulus::Ambiguous(UnknownReason::ProcessLost),
        ),
        (
            "ambiguous_host_slept",
            Stimulus::Ambiguous(UnknownReason::HostSlept),
        ),
        (
            "ambiguous_stale_epoch",
            Stimulus::Ambiguous(UnknownReason::StaleEpoch),
        ),
        (
            "ambiguous_listing_incomplete",
            Stimulus::Ambiguous(UnknownReason::ListingIncomplete),
        ),
        ("expired", Stimulus::Expired),
    ];
    let mut text = String::from(
        "# directive: every phase against every stimulus: the next phase, or the misfit that \
         leaves it unchanged.\n# Every phase is reachable: received on every frame admitted to \
         the table, reobserving for CloneVm/StartVm/StopVm/DeleteVm, the rest from those.\n",
    );

    // when
    for (phase_name, phase) in &phases {
        for (stimulus_name, stimulus) in &stimuli {
            let outcome = match executor::step(phase, stimulus.clone()) {
                Ok(Phase::Done(kind)) => format!("done ({})", observation(&kind)),
                Ok(Phase::Refused(reason)) => format!("refused {}", refusal(&reason)),
                Ok(Phase::Unknown(reason)) => {
                    format!("unknown {}", variant(&format!("{reason:?}")))
                }
                Ok(Phase::Executing(stage)) => {
                    format!("executing {}", variant(&format!("{stage:?}")))
                }
                Ok(Phase::Received) => "received".to_owned(),
                Err(misfit) => format!("misfit {}", variant(&format!("{misfit:?}"))),
            };
            let _ = writeln!(text, "{phase_name} x {stimulus_name} -> {outcome}");
        }
    }

    // then
    check(
        include_str!("../fixtures/matrix/directive.txt"),
        &text,
        "directive.txt",
    );
}

#[test]
fn should_fence_every_frame_when_it_meets_each_session_state_and_table_entry() {
    // given
    let id = DirectiveId::from_u128(7);
    let frame = |epoch: u64| directive(7, Epoch::new(epoch), DirectiveKind::ReportInventory);
    let entries = [
        ("absent", None),
        ("executing", Some(Phase::Executing(Stage::Effect))),
        ("reobserving", Some(Phase::Executing(Stage::ReObserve))),
        (
            "done",
            Some(Phase::Done(ObservationKind::Inventory { vms: Vec::new() })),
        ),
        ("refused", Some(Phase::Refused(RefusalReason::SafeMode))),
        ("unknown", Some(Phase::Unknown(UnknownReason::Deadline))),
    ];
    let versions = [V1_0, ProtocolVersion::new(1, 1), ProtocolVersion::new(2, 0)];
    let mut text = String::from(
        "# fencing: a frame (epoch, version) of directive 7 against each session state and the \
         table's entry for 7: the receipt.\n# Before a session opens there is no table, so \
         only the absent entry is shown there; the open session is at e5, v1.0.\n",
    );

    // when
    for state in ["idle", "awaiting", "entering(e5)"] {
        let mut dp = DataPlane::start(host(), Epoch::new(4), ledger(roomy()));
        if state != "idle" {
            dp.link(LinkStimulus::Hello);
        }
        if state == "entering(e5)" {
            dp.link(LinkStimulus::Welcome(welcome(5, 1, 0)));
        }
        for epoch in [4, 5, 6] {
            let got = dp.receive(V1_0, &frame(epoch), NOW);
            let _ = writeln!(text, "{state} x e{epoch} v1.0 absent -> {}", receipt(&got));
        }
    }
    for (entry_name, entry) in &entries {
        for epoch in [4, 5, 6] {
            for version in versions {
                let mut dp = super::support::open();
                if let (Some(phase), Link::Open(session)) = (entry, &mut dp.link) {
                    session.table.insert(
                        id,
                        Entry {
                            directive: frame(5),
                            phase: phase.clone(),
                        },
                    );
                }
                let got = dp.receive(version, &frame(epoch), NOW);
                let _ = writeln!(
                    text,
                    "open(e5) x e{epoch} v{}.{} {entry_name} -> {}",
                    version.major,
                    version.minor,
                    receipt(&got)
                );
            }
        }
    }

    // then
    check(
        include_str!("../fixtures/matrix/fencing.txt"),
        &text,
        "fencing.txt",
    );
}

/// The directive of `tag` under `condition`, and the mode and ledger it meets.
fn admission_case(
    kind: &DirectiveKind,
    condition: &str,
) -> (Mode, mayfly_data_domain::Ledger, Directive) {
    let mut probe = roomy();
    let mut mode = Mode::Normal;
    let mut d = directive(1, EPOCH, kind.clone());
    match condition {
        "normal" => {}
        "safe" => mode = Mode::Safe,
        "expired" => d.deadline = mayfly_domain::Deadline::at(Millis::new(NOW.get())),
        "no_room" => probe.totals = capacity(0, 0, 0, 0),
        "other_host" => {
            d.kind = match d.kind {
                DirectiveKind::CloneVm { base_name, .. } => DirectiveKind::CloneVm {
                    base_name,
                    vm_name: vm_on(other_host(), 1),
                },
                DirectiveKind::StartVm { run_options, .. } => DirectiveKind::StartVm {
                    vm_name: vm_on(other_host(), 1),
                    run_options,
                },
                DirectiveKind::DeleteVm { mut proof } => {
                    proof.host_id = other_host();
                    proof.vm_name = vm_on(other_host(), 1);
                    DirectiveKind::DeleteVm { proof }
                }
                DirectiveKind::StopVm { grace, mut proof } => {
                    proof.host_id = other_host();
                    DirectiveKind::StopVm { grace, proof }
                }
                DirectiveKind::VerifyGuest { nonce, .. } => DirectiveKind::VerifyGuest {
                    vm_name: vm_on(other_host(), 1),
                    nonce,
                },
                DirectiveKind::LaunchRunner {
                    launch,
                    instance_id,
                    ..
                } => DirectiveKind::LaunchRunner {
                    vm_name: vm_on(other_host(), 1),
                    launch,
                    instance_id,
                },
                DirectiveKind::ExportDiagnostics { .. } => DirectiveKind::ExportDiagnostics {
                    vm_name: vm_on(other_host(), 1),
                },
                other => other,
            };
        }
        "foreign_proof" => {
            d.kind = match d.kind {
                DirectiveKind::DeleteVm { .. } => DirectiveKind::DeleteVm { proof: proof(2) },
                DirectiveKind::StopVm { grace, mut proof } => {
                    proof.generation = mayfly_domain::Generation::new(2).expect("nonzero");
                    DirectiveKind::StopVm { grace, proof }
                }
                other => other,
            };
        }
        "bad_runner_version" => {
            if let DirectiveKind::MaterialiseImage { runner_version, .. } = &mut d.kind {
                *runner_version = "2.330.0; rm -rf /".to_owned();
            }
        }
        other => unreachable!("condition {other}"),
    }
    let mut ledger = ledger(probe);
    if kind.tag() != DirectiveTag::CloneVm {
        ledger.vms.insert(
            vm(1),
            mayfly_data_domain::VmUse {
                guest: Some(GuestOs::Macos),
                running: false,
                memory_bytes: None,
                disk_bytes: Some(0),
            },
        );
    }
    (mode, ledger, d)
}

#[test]
fn should_admit_or_refuse_every_kind_when_it_meets_each_condition() {
    // given
    let conditions = [
        "normal",
        "safe",
        "expired",
        "no_room",
        "other_host",
        "foreign_proof",
        "bad_runner_version",
    ];
    let mut text = String::from(
        "# admission: every directive kind (attempt 1's VM: cloned and stopped, not yet cloned for \
         the clone) against each \
         condition, received in a fresh session: the receipt.\n# no_room: a host with nothing \
         (mutant 24: refused whatever the control plane allocated); other_host: the VM or proof \
         names host9; foreign_proof: the proof names attempt 2's VM (delete) or generation 2 \
         (stop) for attempt 1's directive; a condition that does not apply to a kind leaves it \
         as normal.\n",
    );

    // when
    for kind in every_kind() {
        let tag = kind.tag();
        for condition in conditions {
            let (mode, ledger, d) = admission_case(&kind, condition);
            let mut dp = open_with(mode, ledger);
            let got = dp.receive(V1_0, &d, NOW);
            let _ = writeln!(
                text,
                "{} x {condition} -> {}",
                variant(&format!("{tag:?}")),
                receipt(&got)
            );
        }
    }

    // then
    check(
        include_str!("../fixtures/matrix/admission.txt"),
        &text,
        "admission.txt",
    );
}

#[test]
fn should_fit_or_name_the_short_dimension_when_each_creating_kind_meets_each_host() {
    // given
    let kinds: Vec<DirectiveKind> = every_kind()
        .into_iter()
        .filter(DirectiveKind::needs_resource_fit)
        .collect();
    let hosts = [
        "roomy",
        "no_slot",
        "slots_taken_by_foreign_vms",
        "macos_guests_taken_by_foreign_vms",
        "hypervisor_limit",
        "no_memory",
        "memory_pressure",
        "no_disk",
        "disk_low_water",
        "all_reserved",
        "target_already_there",
        "nothing_listed_yet",
        "images_not_listed_yet",
        "a_running_vm_of_unknown_memory",
        "a_running_vm_of_unknown_guest",
        "a_vm_of_unknown_disk",
        "target_of_unknown_guest",
        "foreign_memory_unsized",
    ];
    let mut text = String::from(
        "# ledger: each kind that needs a fit against each host: its demand (slots, macOS \
         guests, memory GiB, disk GiB) and the fit.\n# Attempt 1's VM is cloned and stopped \
         (macOS); a clone targets attempt 2's new VM unless the target is already there.\n# \
         ADR 0052: a host that does not know a fact refuses what needs it capacity_unknown; \
         the VM of unknown facts is attempt 9's, listed by a complete inventory.\n",
    );

    // when
    for kind in &kinds {
        for host_name in hosts {
            let mut probe = roomy();
            match host_name {
                "no_slot" => probe.totals.vm_slots = 1,
                "slots_taken_by_foreign_vms" => probe.foreign.vm_slots = 3,
                "macos_guests_taken_by_foreign_vms" => probe.foreign.macos_guests = 2,
                "hypervisor_limit" => probe.hypervisor_limit = true,
                "no_memory" => probe.totals.resources.memory_bytes = super::support::GIB,
                "memory_pressure" => probe.memory_pressure = true,
                "no_disk" => probe.totals.resources.disk_bytes = super::support::GIB,
                "disk_low_water" => probe.disk_low_water = true,
                "all_reserved" => probe.reserve = probe.totals,
                "foreign_memory_unsized" => {
                    probe
                        .foreign_unsized
                        .insert(mayfly_protocol::ResourceDimension::Memory);
                }
                _ => {}
            }
            let mut ledger = match host_name {
                "nothing_listed_yet" => super::support::unlisted(probe),
                "images_not_listed_yet" => {
                    let mut l = super::support::unlisted(probe);
                    l.apply_inventory(&mayfly_data_domain::Inventory::Complete { vms: Vec::new() });
                    l
                }
                _ => ledger(probe),
            };
            let unknown_vm = |running: bool,
                              guest: Option<GuestOs>,
                              memory_bytes: Option<u64>,
                              disk_bytes: Option<u64>| {
                mayfly_data_domain::VmUse {
                    guest,
                    running,
                    memory_bytes,
                    disk_bytes,
                }
            };
            let gib = super::support::GIB;
            match host_name {
                "a_running_vm_of_unknown_memory" => {
                    ledger.vms.insert(
                        vm(9),
                        unknown_vm(true, Some(GuestOs::Linux), None, Some(80 * gib)),
                    );
                }
                "a_running_vm_of_unknown_guest" => {
                    ledger
                        .vms
                        .insert(vm(9), unknown_vm(true, None, Some(8 * gib), Some(80 * gib)));
                }
                "a_vm_of_unknown_disk" => {
                    ledger
                        .vms
                        .insert(vm(9), unknown_vm(false, Some(GuestOs::Linux), None, None));
                }
                _ => {}
            }
            ledger.vms.insert(
                vm(1),
                mayfly_data_domain::VmUse {
                    guest: Some(GuestOs::Macos),
                    running: false,
                    memory_bytes: None,
                    disk_bytes: Some(0),
                },
            );
            let mut kind = kind.clone();
            if let DirectiveKind::CloneVm { vm_name, .. } = &mut kind {
                *vm_name = vm(2);
            }
            if host_name == "target_already_there" {
                match &mut kind {
                    DirectiveKind::PullImage { digest } => {
                        ledger.images.insert(digest.clone(), Some(1));
                    }
                    DirectiveKind::MaterialiseImage { base_name, .. } => {
                        ledger.bases.insert(
                            base_name.clone(),
                            mayfly_data_domain::BaseUse {
                                guest: Some(GuestOs::Linux),
                                disk_bytes: Some(1),
                            },
                        );
                    }
                    DirectiveKind::CloneVm { vm_name, .. } => *vm_name = vm(1),
                    DirectiveKind::StartVm { vm_name, .. } => {
                        if let Some(vm) = ledger.vms.get_mut(vm_name) {
                            vm.running = true;
                        }
                    }
                    _ => {}
                }
            }
            if host_name == "target_of_unknown_guest"
                && let Some(vm) = ledger.vms.get_mut(&vm(1))
            {
                vm.guest = None;
            }
            let line = match ledger.demand(&kind) {
                Ok(demand) => {
                    let fit = match ledger.fits(demand) {
                        Ok(()) => "fits".to_owned(),
                        Err(RefusalReason::InsufficientResources { dimension }) => {
                            format!("short of {}", variant(&format!("{dimension:?}")))
                        }
                        Err(other) => format!("refused {}", refusal(&other)),
                    };
                    format!(
                        "demand ({}, {}, {}, {}) {fit}",
                        demand.vm_slots,
                        demand.macos_guests,
                        demand.resources.memory_bytes / gib,
                        demand.resources.disk_bytes / gib,
                    )
                }
                Err(dimension) => {
                    format!("demand unknown in {}", variant(&format!("{dimension:?}")))
                }
            };
            let _ = writeln!(
                text,
                "{} x {host_name} -> {line}",
                variant(&format!("{:?}", kind.tag())),
            );
        }
    }

    // then
    assert_eq!(
        kinds.iter().map(DirectiveKind::tag).collect::<Vec<_>>(),
        [
            DirectiveTag::PullImage,
            DirectiveTag::MaterialiseImage,
            DirectiveTag::CloneVm,
            DirectiveTag::StartVm
        ]
    );
    check(
        include_str!("../fixtures/matrix/ledger.txt"),
        &text,
        "ledger.txt",
    );
}

fn facts_name(facts: VmFacts) -> String {
    let seen = facts
        .seen
        .map_or("unlisted_yet".to_owned(), |s| variant(&format!("{s:?}")));
    format!("{seen}/{}", variant(&format!("{:?}", facts.shim)))
}

#[test]
fn should_derive_every_state_and_signal_when_each_fact_meets_each_observation() {
    // given
    let seen = [
        None,
        Some(Seen::Absent),
        Some(Seen::Stopped),
        Some(Seen::Running),
    ];
    let shims = [Shim::NeverStarted, Shim::Alive, Shim::Exited, Shim::Lost];
    let facts: Vec<VmFacts> = seen
        .iter()
        .flat_map(|s| {
            shims.iter().map(move |shim| VmFacts {
                seen: *s,
                shim: *shim,
            })
        })
        .collect();
    let observations = [
        VmFact::Inventory(Seen::Absent),
        VmFact::Inventory(Seen::Stopped),
        VmFact::Inventory(Seen::Running),
        VmFact::InventoryIncomplete,
        VmFact::Shim(Shim::NeverStarted),
        VmFact::Shim(Shim::Alive),
        VmFact::Shim(Shim::Exited),
        VmFact::Shim(Shim::Lost),
    ];
    let mut text = String::from(
        "# supervisor: every combination of listing (from the last complete inventory) and \
         shim files, its state; then each against every observation: the new facts, the new \
         state and the signal.\n# Every combination is reachable: the listing and the shim \
         are observed independently, in any order, also across a restart.\n",
    );

    // when
    for f in &facts {
        let _ = writeln!(
            text,
            "{} => {}",
            facts_name(*f),
            variant(&format!("{:?}", supervisor::state(*f)))
        );
    }
    for f in &facts {
        for o in observations {
            let next = supervisor::step(*f, o);
            let before = supervisor::state(*f);
            let after = supervisor::state(next);
            let _ = writeln!(
                text,
                "{} x {o:?} -> {} {} {:?}",
                facts_name(*f),
                facts_name(next),
                variant(&format!("{after:?}")),
                supervisor::signal(before, after)
            );
        }
    }

    // then
    check(
        include_str!("../fixtures/matrix/supervisor.txt"),
        &text,
        "supervisor.txt",
    );
}

#[test]
fn should_answer_every_effect_when_it_is_applied_in_its_session() {
    // given
    let mut text = String::new();

    // when
    for kind in every_kind() {
        let mut dp = super::support::open();
        if kind.tag() != DirectiveTag::CloneVm {
            dp.ledger.vms.insert(
                vm(1),
                mayfly_data_domain::VmUse {
                    guest: Some(GuestOs::Macos),
                    running: false,
                    memory_bytes: None,
                    disk_bytes: Some(0),
                },
            );
        }
        let d = directive(1, EPOCH, kind.clone());
        let first = dp.receive(V1_0, &d, NOW);
        if first == mayfly_data_domain::Receipt::ReObserve {
            dp.reobserved(
                EPOCH,
                d.directive_id,
                &super::support::effect_listing(&kind),
            );
        }
        let done = dp.finish(EPOCH, d.directive_id, Stimulus::Applied(applied(&kind)));
        let _ = writeln!(text, "{:?} {first:?} {done:?}", kind.tag());
    }

    // then
    assert!(!text.contains("Misfit"), "{text}");
    assert!(!text.contains("Refused"), "{text}");
}
