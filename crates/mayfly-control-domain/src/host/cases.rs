//! The host's `DETERMINISM_CASES` (ver §3.1): accepted cases along a host's life and one
//! refused case per [`HostRejection`] code. Histories are produced by `decide` itself from
//! command paths (each path is checked to be accepted step by step by
//! `should_accept_every_step_when_a_case_path_is_replayed`).

use mayfly_domain::{AttemptId, CommandId, Deadline, Epoch, Millis, PoolName, Resources, Sample};

use super::events::HostEvent;
use super::state::{
    AdmissionCause, ApplyPoolTerms, ForceScope, HostCommand, HostRejection, HostState,
    ReleaseEvidence, ReserveSlot, ResourceThreshold,
};
use super::{Host, decide, evolve};
use crate::aggregate::{DeterminismCase, Expect};
use crate::attempt::GuestOs;
use crate::capacity::ReportedCapacity;
use crate::command::{Actor, Command};
use crate::pool::TrustTier;

type Step = Command<HostCommand>;

const RECONCILER: Actor = Actor::Automation {
    policy: "reconciler",
};
const OPERATOR: Actor = Actor::Operator { uid: 501 };

const GIB: u64 = 1 << 30;

fn at(run_epoch: u64, actor: Actor, body: HostCommand) -> Step {
    Command::new(
        CommandId::from_u128(0xC2),
        Millis::new(1_000),
        Epoch::new(run_epoch),
        actor,
        body,
    )
}

fn auto(body: HostCommand) -> Step {
    at(1, RECONCILER, body)
}

fn attempt(n: u128) -> AttemptId {
    AttemptId::from_u128(0xA000 + n)
}

/// A host of 32 GiB and 200 GiB, four VM slots, two macOS guests.
pub(super) fn report() -> ReportedCapacity {
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

fn record(report_id: u64, capacity: ReportedCapacity) -> HostCommand {
    HostCommand::RecordCapacityReport {
        report_id,
        capacity,
    }
}

fn terms(revision: u64, open: bool, max_running: u32) -> HostCommand {
    HostCommand::ApplyPoolTerms(ApplyPoolTerms {
        pool: PoolName::sample(),
        revision,
        open,
        max_running,
        tier: TrustTier::Trusted,
    })
}

fn reserve(n: u128, memory_gib: u64) -> HostCommand {
    HostCommand::ReserveSlot(ReserveSlot {
        attempt_id: attempt(n),
        pool: PoolName::sample(),
        os: GuestOs::Macos,
        resources: Resources::new(memory_gib * GIB, 60 * GIB),
        terms_revision: 1,
    })
}

fn grant(n: u128) -> HostCommand {
    HostCommand::GrantActivation {
        attempt_id: attempt(n),
    }
}

fn quarantine(n: u128, vm_running: bool) -> HostCommand {
    HostCommand::QuarantineLease {
        attempt_id: attempt(n),
        vm_running,
    }
}

fn release(n: u128, evidence: ReleaseEvidence) -> HostCommand {
    HostCommand::ReleaseSlot {
        attempt_id: attempt(n),
        evidence,
    }
}

fn drain() -> HostCommand {
    HostCommand::StartHostDrain {
        deadline: Deadline::at(Millis::new(3_600_000)),
    }
}

fn then(mut path: Vec<Step>, more: impl IntoIterator<Item = HostCommand>) -> Vec<Step> {
    path.extend(more.into_iter().map(auto));
    path
}

fn registered() -> Vec<Step> {
    vec![auto(HostCommand::RegisterHost {
        reserves: Resources::new(4 * GIB, 20 * GIB),
    })]
}

fn reported() -> Vec<Step> {
    then(registered(), [record(1, report())])
}

fn open_terms() -> Vec<Step> {
    then(reported(), [terms(1, true, 2)])
}

fn closed_terms() -> Vec<Step> {
    then(reported(), [terms(1, false, 2)])
}

fn one_lease() -> Vec<Step> {
    then(open_terms(), [reserve(1, 8)])
}

fn activated() -> Vec<Step> {
    then(one_lease(), [grant(1)])
}

fn quarantined() -> Vec<Step> {
    then(one_lease(), [quarantine(1, true)])
}

fn two_leases() -> Vec<Step> {
    then(one_lease(), [reserve(2, 8)])
}

fn two_quarantined() -> Vec<Step> {
    then(two_leases(), [quarantine(1, false), quarantine(2, false)])
}

fn draining() -> Vec<Step> {
    then(one_lease(), [drain()])
}

fn empty_draining() -> Vec<Step> {
    then(open_terms(), [drain()])
}

fn drained() -> Vec<Step> {
    then(empty_draining(), [HostCommand::CompleteHostDrain])
}

fn disk_low() -> Vec<Step> {
    then(
        one_lease(),
        [HostCommand::RecordResourceThreshold {
            threshold: ResourceThreshold::DiskLow,
        }],
    )
}

fn recovering() -> Vec<Step> {
    then(
        one_lease(),
        [HostCommand::CloseAdmission {
            cause: AdmissionCause::Recovering,
        }],
    )
}

fn at_epoch_two() -> Vec<Step> {
    let mut path = open_terms();
    path.push(at(2, RECONCILER, reserve(1, 8)));
    path
}

/// A newer report in which foreign VMs take every VM slot.
fn no_slots() -> Vec<Step> {
    then(
        open_terms(),
        [record(
            2,
            ReportedCapacity {
                foreign_vms: 4,
                ..report()
            },
        )],
    )
}

/// A newer report in which two foreign macOS guests run (G3 leaves none).
fn foreign_macos() -> Vec<Step> {
    then(
        open_terms(),
        [record(
            2,
            ReportedCapacity {
                foreign_vms: 2,
                foreign_macos_guests: 2,
                ..report()
            },
        )],
    )
}

/// Folds `path` through `decide` and `evolve`; the first refused step, if any.
fn replay_path(path: Vec<Step>) -> (Vec<HostEvent>, Option<(usize, HostRejection)>) {
    let mut state = HostState::initial();
    let mut history = Vec::new();
    for (index, step) in path.into_iter().enumerate() {
        match decide(&state, step) {
            Ok(events) => {
                for event in &events {
                    state = evolve(state, event);
                }
                history.extend(events);
            }
            Err(rejection) => return (history, Some((index, rejection))),
        }
    }
    (history, None)
}

fn case(
    name: &'static str,
    path: Vec<Step>,
    command: Step,
    expect: Expect,
) -> DeterminismCase<Host> {
    DeterminismCase {
        name,
        history: replay_path(path).0,
        command,
        expect,
    }
}

fn accepted(name: &'static str, path: Vec<Step>, body: HostCommand) -> DeterminismCase<Host> {
    case(name, path, auto(body), Expect::Accepted)
}

fn refused(name: &'static str, path: Vec<Step>, body: HostCommand) -> DeterminismCase<Host> {
    case(name, path, auto(body), Expect::Refused)
}

/// Every path a case starts from, for the replay check.
#[cfg(test)]
fn paths() -> Vec<Vec<Step>> {
    vec![
        registered(),
        reported(),
        open_terms(),
        closed_terms(),
        one_lease(),
        activated(),
        quarantined(),
        two_leases(),
        two_quarantined(),
        draining(),
        empty_draining(),
        drained(),
        disk_low(),
        recovering(),
        at_epoch_two(),
        no_slots(),
        foreign_macos(),
    ]
}

pub(super) fn cases() -> Vec<DeterminismCase<Host>> {
    vec![
        // accepted
        accepted(
            "register_host",
            Vec::new(),
            HostCommand::RegisterHost {
                reserves: Resources::new(4 * GIB, 20 * GIB),
            },
        ),
        accepted("record_report", registered(), record(1, report())),
        accepted("stale_report_ignored", no_slots(), record(1, report())),
        accepted("apply_terms", reported(), terms(1, true, 2)),
        accepted("reserve_slot", open_terms(), reserve(1, 8)),
        accepted("reserve_next_slot", one_lease(), reserve(2, 8)),
        accepted("grant_activation", one_lease(), grant(1)),
        accepted("quarantine_lease", activated(), quarantine(1, true)),
        accepted(
            "quarantined_vm_stopped",
            quarantined(),
            quarantine(1, false),
        ),
        accepted(
            "same_holds_record_nothing",
            quarantined(),
            quarantine(1, true),
        ),
        accepted(
            "release_on_deletion",
            activated(),
            release(1, ReleaseEvidence::VmDeleted),
        ),
        accepted(
            "release_quarantined_on_absence",
            quarantined(),
            release(1, ReleaseEvidence::AttemptLost),
        ),
        accepted(
            "release_never_started",
            one_lease(),
            release(1, ReleaseEvidence::AttemptNeverStarted),
        ),
        accepted(
            "disk_low_closes",
            one_lease(),
            HostCommand::RecordResourceThreshold {
                threshold: ResourceThreshold::DiskLow,
            },
        ),
        accepted(
            "disk_recovers",
            disk_low(),
            HostCommand::RecordResourceThreshold {
                threshold: ResourceThreshold::DiskOk,
            },
        ),
        accepted(
            "close_admission",
            one_lease(),
            HostCommand::CloseAdmission {
                cause: AdmissionCause::Woke,
            },
        ),
        accepted(
            "open_admission",
            recovering(),
            HostCommand::OpenAdmission {
                cause: AdmissionCause::Recovering,
            },
        ),
        accepted("start_drain", one_lease(), drain()),
        accepted("cancel_drain", draining(), HostCommand::CancelHostDrain),
        accepted("reopen_drained", drained(), HostCommand::CancelHostDrain),
        accepted(
            "complete_drain",
            empty_draining(),
            HostCommand::CompleteHostDrain,
        ),
        case(
            "operator_forces_drain",
            draining(),
            at(
                1,
                OPERATOR,
                HostCommand::ForceDrain {
                    scope: ForceScope::Host,
                },
            ),
            Expect::Accepted,
        ),
        case(
            "newer_epoch_is_recorded",
            open_terms(),
            at(2, RECONCILER, reserve(1, 8)),
            Expect::Accepted,
        ),
        // refused, one per code
        refused("not_registered", Vec::new(), record(1, report())),
        refused(
            "already_registered",
            registered(),
            HostCommand::RegisterHost {
                reserves: Resources::new(0, 0),
            },
        ),
        refused("stale_run", at_epoch_two(), grant(1)),
        refused("stale_terms", open_terms(), terms(1, false, 2)),
        refused("unknown_pool", reported(), reserve(1, 8)),
        refused(
            "terms_changed",
            then(open_terms(), [terms(2, true, 2)]),
            reserve(1, 8),
        ),
        refused("terms_closed", closed_terms(), reserve(1, 8)),
        case(
            "terms_open",
            open_terms(),
            at(
                1,
                OPERATOR,
                HostCommand::ForceDrain {
                    scope: ForceScope::Pool {
                        pool: PoolName::sample(),
                    },
                },
            ),
            Expect::Refused,
        ),
        refused("admission_closed", recovering(), reserve(2, 8)),
        refused("draining", draining(), grant(1)),
        refused("no_capacity_report", registered(), reserve(1, 8)),
        refused("already_leased", one_lease(), reserve(1, 8)),
        refused("pool_full", two_leases(), reserve(3, 1)),
        refused("quarantine_held", two_quarantined(), reserve(3, 1)),
        refused(
            "no_free_slot",
            no_slots(),
            HostCommand::ReserveSlot(ReserveSlot {
                os: GuestOs::Linux,
                ..reserve_body(1, 1)
            }),
        ),
        refused("macos_guests", foreign_macos(), reserve(1, 1)),
        refused("insufficient_memory", open_terms(), reserve(1, 30)),
        refused(
            "insufficient_disk",
            open_terms(),
            HostCommand::ReserveSlot(ReserveSlot {
                resources: Resources::new(GIB, 150 * GIB),
                ..reserve_body(1, 1)
            }),
        ),
        refused("no_lease", open_terms(), grant(1)),
        refused("already_activated", activated(), grant(1)),
        refused("quarantined", quarantined(), grant(1)),
        refused("already_draining", draining(), drain()),
        refused("not_draining", one_lease(), HostCommand::CompleteHostDrain),
        refused(
            "leases_remaining",
            draining(),
            HostCommand::CompleteHostDrain,
        ),
        refused(
            "operator_only",
            draining(),
            HostCommand::ForceDrain {
                scope: ForceScope::Host,
            },
        ),
    ]
}

fn reserve_body(n: u128, memory_gib: u64) -> ReserveSlot {
    ReserveSlot {
        attempt_id: attempt(n),
        pool: PoolName::sample(),
        os: GuestOs::Macos,
        resources: Resources::new(memory_gib * GIB, 60 * GIB),
        terms_revision: 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_accept_every_step_when_a_case_path_is_replayed() {
        // given
        let paths = paths();
        // when
        let refusals: Vec<(usize, Option<(usize, HostRejection)>)> = paths
            .into_iter()
            .map(|path| replay_path(path).1)
            .enumerate()
            .filter(|(_, refusal)| refusal.is_some())
            .collect();
        // then
        assert!(refusals.is_empty(), "{refusals:?}");
    }
}
