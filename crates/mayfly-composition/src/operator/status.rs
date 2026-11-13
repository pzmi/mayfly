//! What `status`, `pool list` and `vm list` answer: the wire's read models
//! ([`super::wire`]) from the control plane's views (`mayfly_control::queries::fleet`, the
//! `HostRun` state) and the start-up sizing. Pure functions of what was read, so each mapping
//! is pinned without a daemon.

use std::collections::BTreeMap;

use mayfly_control::queries::FleetView;
use mayfly_control_domain::Aggregate;
use mayfly_control_domain::host::Host;
use mayfly_control_domain::operations::hostrun::{HostRunState, RunPhase, SafeModeReason};
use mayfly_control_domain::pool::Pool;

use super::wire::{
    AttemptStatus, Desync, HostStatus, PoolOnHost, PoolStatus, RunStatus, SizingValue, Tripped, Vm,
    desync_note,
};
use crate::sizing::SizingReport;

/// The instance a projection row key of `A`'s streams names (`pool-mac` → `mac`).
fn instance_of<A: Aggregate>(key: &str) -> Option<&str> {
    key.strip_prefix(&format!("{}-", A::NAME.replace('_', "-")))
}

/// A `snake_case` serde form, without its quotes.
fn code<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(text)) => text,
        Ok(other) => other.to_string(),
        Err(_) => String::from("unknown"),
    }
}

/// The run's part of a status. Pinned by
/// `tests::should_name_the_phase_and_the_safe_mode_reason_when_a_run_is_mapped`.
#[must_use]
pub fn run(state: &HostRunState, ready: bool) -> RunStatus {
    let phase = match state.phase {
        RunPhase::NotStarted => "not_started",
        RunPhase::Starting => "starting",
        RunPhase::Recovering => "recovering",
        RunPhase::Operating => "operating",
        RunPhase::ShuttingDown => "shutting_down",
        RunPhase::Ended => "ended",
    };
    RunStatus {
        number: state.run_no,
        epoch: state.epoch.get(),
        phase: phase.to_owned(),
        ready,
        safe_mode: state.safe_mode.map(|safe| {
            match safe.reason {
                SafeModeReason::EpochBelowFloor => "epoch_below_floor",
                SafeModeReason::UnknownEventKind => "unknown_event_kind",
                SafeModeReason::VerificationFailed => "verification_failed",
                SafeModeReason::CrashLoop => "crash_loop",
            }
            .to_owned()
        }),
    }
}

/// The sizing values as the start-up report has them.
#[must_use]
pub fn sizing(report: &SizingReport) -> Vec<SizingValue> {
    report
        .values
        .iter()
        .map(|sized| SizingValue {
            key: sized.key.to_owned(),
            value: sized.value.to_string(),
            source: sized.source.as_str().to_owned(),
            reason: sized.reason.map(str::to_owned),
        })
        .collect()
}

/// Every pool of `view`, by name, each with its place on every host (VER-09's signal). Pinned
/// by `tests::should_show_a_pool_admitted_on_a_host_when_its_terms_are_open_and_its_base_is_materialised`.
#[must_use]
pub fn pools(view: &FleetView) -> Vec<PoolStatus> {
    let mut out: Vec<PoolStatus> = view
        .pools
        .iter()
        .filter_map(|(key, row)| {
            let name = instance_of::<Pool>(key)?;
            let hosts = view
                .hosts
                .iter()
                .filter_map(|(host_key, host)| {
                    let id = instance_of::<Host>(host_key)?;
                    let placement = row.image_pin.as_ref().and_then(|pin| {
                        view.placements
                            .values()
                            .find(|p| p.host_id.as_str() == id && &p.digest == pin)
                            .map(|p| p.phase.clone())
                    });
                    let leases = view.leases.get(host_key).map_or(0, |slots| {
                        slots.iter().filter(|s| s.pool.as_str() == name).count()
                    });
                    Some(PoolOnHost {
                        host: id.to_owned(),
                        terms_open: host.terms.iter().any(|t| t.pool.as_str() == name && t.open),
                        placement,
                        leases: u32::try_from(leases).unwrap_or(u32::MAX),
                    })
                })
                .collect();
            Some(PoolStatus {
                name: name.to_owned(),
                mode: row.mode.clone(),
                image: row.image_pin.as_ref().map(|d| d.as_str().to_owned()),
                max_running: row.max_running,
                min_prepared: row.min_prepared,
                tripped: row
                    .tripped
                    .iter()
                    .map(|t| Tripped {
                        digest: t.digest.as_str().to_owned(),
                        at: t.at.get(),
                    })
                    .collect(),
                hosts,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Every host of `view`, by id.
#[must_use]
pub fn hosts(view: &FleetView) -> Vec<HostStatus> {
    let mut out: Vec<HostStatus> = view
        .hosts
        .iter()
        .filter_map(|(key, row)| {
            Some(HostStatus {
                host: instance_of::<Host>(key)?.to_owned(),
                admission_open: row.admission_open,
                drain: row.drain.clone(),
                leases: u32::try_from(view.leases.get(key).map_or(0, Vec::len)).unwrap_or(u32::MAX),
            })
        })
        .collect();
    out.sort_by(|a, b| a.host.cmp(&b.host));
    out
}

/// The attempts' counts and desyncs; `unknown_effects` from the pending-effects query. Pinned
/// by `tests::should_name_a_desync_in_plain_words_when_a_vm_vanished`.
#[must_use]
pub fn attempts(view: &FleetView, unknown_effects: usize) -> AttemptStatus {
    let mut by_phase: BTreeMap<String, u32> = BTreeMap::new();
    let mut desync = Vec::new();
    for row in view.attempts.values() {
        *by_phase.entry(row.phase.clone()).or_default() += 1;
        if let Some(found) = row.desync {
            let discovered_by = code(&found);
            desync.push(Desync {
                attempt: row.attempt_id.to_string(),
                pool: row.pool.as_str().to_owned(),
                vm: row.vm_name.to_string(),
                note: desync_note(&discovered_by),
                discovered_by,
            });
        }
    }
    AttemptStatus {
        by_phase,
        unknown_effects: u32::try_from(unknown_effects).unwrap_or(u32::MAX),
        desync,
    }
}

/// The phases in which an attempt has no VM to list: never started, deleted, closed.
const NO_VM: &[&str] = &["not_started", "deleted", "closed"];

/// `vm list`: every attempt whose VM may exist or vanished, by attempt id.
#[must_use]
pub fn vms(view: &FleetView) -> Vec<Vm> {
    view.attempts
        .values()
        .filter(|row| !NO_VM.contains(&row.phase.as_str()))
        .map(|row| {
            let desync = row.desync.map(|found| code(&found));
            Vm {
                attempt: row.attempt_id.to_string(),
                pool: row.pool.as_str().to_owned(),
                host: row.host_id.as_str().to_owned(),
                vm: row.vm_name.to_string(),
                phase: row.phase.clone(),
                pending: row.pending.clone(),
                pending_unknown: row.pending_unknown,
                job: row.job.map(|job| job.0),
                note: desync.as_deref().map(desync_note),
                desync,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use mayfly_control::projection::views::{
        AttemptRow, HostRow, HostTermsRow, PlacementRow, PoolRow, SlotRow,
    };
    use mayfly_control_domain::attempt::{GuestOs, VanishDiscovery};
    use mayfly_control_domain::operations::hostrun::SafeMode;
    use mayfly_domain::{
        AttemptId, Epoch, Generation, HostId, ImageDigest, Millis, OwnedVmName, PoolName,
        Resources, Sample as _, Slot,
    };

    use super::*;

    fn pool_row(pin: Option<ImageDigest>) -> PoolRow {
        PoolRow {
            mode: "active".to_owned(),
            config_revision: Some(1),
            tier: None,
            os: Some(GuestOs::Linux),
            vm_resources: None,
            max_running: Some(2),
            min_prepared: Some(1),
            image_pin: pin,
            tripped: Vec::new(),
            revision: 3,
        }
    }

    fn host_row(open: bool) -> HostRow {
        HostRow {
            report_id: None,
            report: None,
            reserves: Resources::new(0, 0),
            admission_open: true,
            drain: "open".to_owned(),
            terms: vec![HostTermsRow {
                pool: PoolName::sample(),
                revision: 1,
                open,
                max_running: 2,
            }],
            revision: 4,
        }
    }

    fn placement(phase: &str) -> PlacementRow {
        PlacementRow {
            host_id: HostId::sample(),
            digest: ImageDigest::sample(),
            phase: phase.to_owned(),
            wanted: None,
            base: None,
            base_name: None,
            disk_bytes: None,
            pending: None,
            revision: 2,
        }
    }

    fn view(open: bool, phase: &str, leases: usize) -> FleetView {
        let host_key = format!("host-{}", HostId::sample());
        let slots = (0..leases)
            .map(|n| SlotRow {
                attempt_id: AttemptId::from_u128(u128::try_from(n).unwrap_or(0) + 1),
                generation: Generation::FIRST,
                pool: PoolName::sample(),
                os: GuestOs::Linux,
                resources: Resources::new(0, 0),
                phase: "reserved".to_owned(),
                holds: None,
            })
            .collect();
        FleetView {
            hosts: [(host_key.clone(), host_row(open))].into_iter().collect(),
            leases: [(host_key, slots)].into_iter().collect(),
            pools: [(
                format!("pool-{}", PoolName::sample()),
                pool_row(Some(ImageDigest::sample())),
            )]
            .into_iter()
            .collect(),
            placements: [("placement-1".to_owned(), placement(phase))]
                .into_iter()
                .collect(),
            ..FleetView::default()
        }
    }

    #[test]
    fn should_show_a_pool_admitted_on_a_host_when_its_terms_are_open_and_its_base_is_materialised()
    {
        // given
        let admitted = view(true, "materialised", 1);
        let pulling = view(false, "pulling", 0);

        // when
        let (admitted, pulling) = (pools(&admitted), pools(&pulling));

        // then
        let on_host = |terms_open, placement: &str, leases| PoolOnHost {
            host: HostId::sample().as_str().to_owned(),
            terms_open,
            placement: Some(placement.to_owned()),
            leases,
        };
        assert_eq!(admitted.len(), 1);
        assert_eq!(admitted[0].name, PoolName::sample().as_str());
        assert_eq!(admitted[0].hosts, [on_host(true, "materialised", 1)]);
        assert_eq!(pulling[0].hosts, [on_host(false, "pulling", 0)]);
    }

    #[test]
    fn should_name_a_desync_in_plain_words_when_a_vm_vanished() {
        // given
        let id = AttemptId::from_u128(7);
        let row = AttemptRow {
            attempt_id: id,
            pool: PoolName::sample(),
            host_id: HostId::sample(),
            slot: Slot::new(0),
            generation: Generation::FIRST,
            vm_name: OwnedVmName::sample(),
            image_digest: ImageDigest::sample(),
            phase: "lost".to_owned(),
            backoff_stage: None,
            backoff: None,
            quarantine: None,
            pending: None,
            pending_unknown: false,
            job: None,
            retire_reason: None,
            desync: Some(VanishDiscovery::StopVm),
            refused: None,
            vm_gone: None,
            capacity_refused_at: None,
            retirement_requested: None,
            prepared_at: None,
            revision: 5,
            updated_at: Millis::new(1),
        };
        let fleet = FleetView {
            attempts: [(id, row)].into_iter().collect(),
            ..FleetView::default()
        };

        // when
        let (status, listed) = (attempts(&fleet, 0), vms(&fleet));

        // then
        assert_eq!(status.by_phase.get("lost"), Some(&1));
        assert_eq!(status.desync.len(), 1);
        assert_eq!(status.desync[0].discovered_by, "stop_vm");
        assert!(
            status.desync[0]
                .note
                .starts_with("the VM vanished outside mayfly (found by stop_vm): a desync"),
            "{:?}",
            status.desync
        );
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].desync.as_deref(), Some("stop_vm"));
    }

    #[test]
    fn should_name_the_phase_and_the_safe_mode_reason_when_a_run_is_mapped() {
        // given
        let state = HostRunState {
            phase: RunPhase::Recovering,
            run_no: 2,
            epoch: Epoch::new(5),
            safe_mode: Some(SafeMode {
                reason: SafeModeReason::EpochBelowFloor,
                since: Millis::new(1),
                entered_run: 2,
                needs_plan: true,
            }),
            ..HostRunState::initial()
        };

        // when
        let mapped = run(&state, false);

        // then
        assert_eq!(
            mapped,
            RunStatus {
                number: 2,
                epoch: 5,
                phase: "recovering".to_owned(),
                ready: false,
                safe_mode: Some("epoch_below_floor".to_owned()),
            }
        );
    }
}
