//! Paths through a host's life, driven through its public `decide` and `evolve`.

use mayfly_control_domain::attempt::GuestOs;
use mayfly_control_domain::capacity::ReportedCapacity;
use mayfly_control_domain::host::{
    AdmissionCause, ApplyPoolTerms, ForceScope, Host, HostCommand, HostEvent, HostRejection,
    HostState, ReleaseEvidence, ReserveSlot, ResourceThreshold,
};
use mayfly_control_domain::pool::TrustTier;
use mayfly_control_domain::{Actor, Aggregate, Command};
use mayfly_domain::{AttemptId, CommandId, Deadline, Epoch, Millis, PoolName, Resources};

pub type Step = Command<HostCommand>;

pub const GIB: u64 = 1 << 30;

pub const RECONCILER: Actor = Actor::Automation {
    policy: "reconciler",
};
pub const OPERATOR: Actor = Actor::Operator { uid: 501 };

pub fn at(run_epoch: u64, actor: Actor, body: HostCommand) -> Step {
    Command::new(
        CommandId::from_u128(0xC2),
        Millis::new(1_000),
        Epoch::new(run_epoch),
        actor,
        body,
    )
}

pub fn auto(body: HostCommand) -> Step {
    at(1, RECONCILER, body)
}

pub fn attempt(n: u128) -> AttemptId {
    AttemptId::from_u128(0xA000 + n)
}

pub fn pool(name: &str) -> PoolName {
    PoolName::new(name).unwrap_or_else(|e| panic!("{e}"))
}

/// 32 GiB and 200 GiB (40 of them image bases), four VM slots, two macOS guests, nothing
/// foreign.
pub fn report() -> ReportedCapacity {
    ReportedCapacity {
        total: Resources::new(32 * GIB, 200 * GIB),
        foreign: Resources::new(0, 0),
        base: Resources::new(0, 40 * GIB),
        vm_slots: 4,
        foreign_vms: 0,
        macos_guests: 2,
        foreign_macos_guests: 0,
    }
}

pub fn record(report_id: u64, capacity: ReportedCapacity) -> HostCommand {
    HostCommand::RecordCapacityReport {
        report_id,
        capacity,
    }
}

pub fn terms(name: &str, revision: u64, open: bool, max_running: u32) -> HostCommand {
    HostCommand::ApplyPoolTerms(ApplyPoolTerms {
        pool: pool(name),
        revision,
        open,
        max_running,
        tier: TrustTier::Trusted,
    })
}

/// A reservation of `memory_gib` and 20 GiB of disk in pool `name`, at terms revision 1.
pub fn reserve_in(name: &str, n: u128, os: GuestOs, memory_gib: u64) -> HostCommand {
    HostCommand::ReserveSlot(ReserveSlot {
        attempt_id: attempt(n),
        pool: pool(name),
        os,
        resources: Resources::new(memory_gib * GIB, 20 * GIB),
        terms_revision: 1,
    })
}

pub fn reserve(n: u128, os: GuestOs, memory_gib: u64) -> HostCommand {
    reserve_in("pool0", n, os, memory_gib)
}

pub fn grant(n: u128) -> HostCommand {
    HostCommand::GrantActivation {
        attempt_id: attempt(n),
    }
}

pub fn quarantine(n: u128, vm_running: bool) -> HostCommand {
    HostCommand::QuarantineLease {
        attempt_id: attempt(n),
        vm_running,
    }
}

pub fn release(n: u128, evidence: ReleaseEvidence) -> HostCommand {
    HostCommand::ReleaseSlot {
        attempt_id: attempt(n),
        evidence,
    }
}

pub fn drain() -> HostCommand {
    HostCommand::StartHostDrain {
        deadline: Deadline::at(Millis::new(3_600_000)),
    }
}

pub fn register() -> HostCommand {
    HostCommand::RegisterHost {
        reserves: Resources::new(4 * GIB, 20 * GIB),
    }
}

/// Decides `step` and folds what it accepted.
pub fn apply(state: &HostState, step: Step) -> Result<(HostState, Vec<HostEvent>), HostRejection> {
    let events = Host::decide(state, step)?;
    let next = events.iter().fold(state.clone(), Host::evolve);
    Ok((next, events))
}

/// The state after `bodies`, every one accepted.
///
/// # Panics
/// When a step is refused: the path is wrong, not the aggregate under test.
pub fn reach(bodies: impl IntoIterator<Item = HostCommand>) -> HostState {
    bodies
        .into_iter()
        .enumerate()
        .fold(Host::initial(), |state, (index, body)| {
            apply(&state, auto(body))
                .unwrap_or_else(|r| panic!("step {index} of the path refused: {r:?}"))
                .0
        })
}

/// A registered host with the standard report and pool `pool0` open (max 2).
pub fn open_host() -> Vec<HostCommand> {
    vec![register(), record(1, report()), terms("pool0", 1, true, 2)]
}

pub fn with(
    mut path: Vec<HostCommand>,
    more: impl IntoIterator<Item = HostCommand>,
) -> Vec<HostCommand> {
    path.extend(more);
    path
}

/// The refusal code of deciding `body` on `state`, or `"accepted"`.
pub fn outcome(state: &HostState, step: Step) -> String {
    match Host::decide(state, step) {
        Ok(_) => "accepted".to_owned(),
        Err(rejection) => mayfly_control_domain::Rejection::code(&rejection).to_owned(),
    }
}

/// Every closing cause.
pub const CAUSES: [AdmissionCause; 4] = [
    AdmissionCause::TartUnsupported,
    AdmissionCause::Woke,
    AdmissionCause::Recovering,
    AdmissionCause::InventoryIncomplete,
];

/// Named states for the matrix.
pub fn matrix_states() -> Vec<(&'static str, HostState)> {
    let lease = with(open_host(), [reserve(1, GuestOs::Macos, 8)]);
    vec![
        ("unregistered", Host::initial()),
        ("registered", reach([register()])),
        ("reported", reach([register(), record(1, report())])),
        ("open", reach(open_host())),
        ("reserved", reach(lease.clone())),
        ("activated", reach(with(lease.clone(), [grant(1)]))),
        (
            "quarantined_running",
            reach(with(lease.clone(), [quarantine(1, true)])),
        ),
        (
            "quarantined_stopped",
            reach(with(lease.clone(), [quarantine(1, false)])),
        ),
        (
            "pool_full",
            reach(with(lease.clone(), [reserve(2, GuestOs::Linux, 8)])),
        ),
        (
            "disk_low",
            reach(with(
                lease.clone(),
                [HostCommand::RecordResourceThreshold {
                    threshold: ResourceThreshold::DiskLow,
                }],
            )),
        ),
        (
            "recovering",
            reach(with(
                lease.clone(),
                [HostCommand::CloseAdmission {
                    cause: AdmissionCause::Recovering,
                }],
            )),
        ),
        ("draining", reach(with(lease, [drain()]))),
        (
            "drained",
            reach(with(open_host(), [drain(), HostCommand::CompleteHostDrain])),
        ),
        (
            "terms_closed",
            reach(with(
                open_host(),
                [reserve(1, GuestOs::Linux, 8), terms("pool0", 2, false, 2)],
            )),
        ),
    ]
}

/// One stimulus of every command type (and the variants that decide differently).
pub fn stimuli() -> Vec<(&'static str, Step)> {
    vec![
        ("register_host", auto(register())),
        ("record_newer_report", auto(record(9, report()))),
        ("record_stale_report", auto(record(0, report()))),
        ("apply_newer_terms", auto(terms("pool0", 9, true, 2))),
        ("reserve_macos", auto(reserve(3, GuestOs::Macos, 8))),
        ("reserve_linux", auto(reserve(3, GuestOs::Linux, 8))),
        ("grant_activation", auto(grant(1))),
        ("quarantine_running", auto(quarantine(1, true))),
        ("quarantine_stopped", auto(quarantine(1, false))),
        (
            "release_vm_deleted",
            auto(release(1, ReleaseEvidence::VmDeleted)),
        ),
        (
            "release_never_started",
            auto(release(1, ReleaseEvidence::AttemptNeverStarted)),
        ),
        (
            "disk_low",
            auto(HostCommand::RecordResourceThreshold {
                threshold: ResourceThreshold::DiskLow,
            }),
        ),
        (
            "disk_ok",
            auto(HostCommand::RecordResourceThreshold {
                threshold: ResourceThreshold::DiskOk,
            }),
        ),
        (
            "close_recovering",
            auto(HostCommand::CloseAdmission {
                cause: AdmissionCause::Recovering,
            }),
        ),
        (
            "open_recovering",
            auto(HostCommand::OpenAdmission {
                cause: AdmissionCause::Recovering,
            }),
        ),
        ("start_drain", auto(drain())),
        ("cancel_drain", auto(HostCommand::CancelHostDrain)),
        ("complete_drain", auto(HostCommand::CompleteHostDrain)),
        (
            "force_host",
            at(
                1,
                OPERATOR,
                HostCommand::ForceDrain {
                    scope: ForceScope::Host,
                },
            ),
        ),
        (
            "force_pool",
            at(
                1,
                OPERATOR,
                HostCommand::ForceDrain {
                    scope: ForceScope::Pool {
                        pool: pool("pool0"),
                    },
                },
            ),
        ),
        ("stale_run", at(0, RECONCILER, grant(1))),
    ]
}
