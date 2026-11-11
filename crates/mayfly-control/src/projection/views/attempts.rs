//! `attempts` and `pending_effects`: the `Attempts` and `PendingEffects` read models of
//! `doc/domain-model.md` §8, kept inline from each attempt stream.

use mayfly_control_domain::EventKind;
use mayfly_control_domain::EventSet;
use mayfly_control_domain::attempt::{
    Attempt, AttemptBackedOff, AttemptEvent, AttemptLost, AttemptQuarantined, AttemptRetired,
    AttemptState, DiagnosticsExported, DiagnosticsLost, DirectiveIssued,
    DirectiveObservedNotApplied, DirectiveOutcomeUnknown, DirectiveRefused, DirectiveSent,
    DirectiveTag, GuestRejected, GuestVerified, Outcome, Phase, QuarantineReason, RefusalReason,
    RetireReason, RunnerLaunchUnknown, RunnerLaunched, RunnerRequestId, Stage, VanishDiscovery,
    VmBooted, VmCloned, VmDeleted, VmStopped, VmVanished,
};
use mayfly_control_domain::host::ReleaseEvidence;
use mayfly_domain::{
    AttemptId, Deadline, DirectiveId, Generation, HostId, ImageDigest, Millis, OwnedVmName,
    PoolName, Slot,
};
use serde::{Deserialize, Serialize};

use crate::projection::spec::{EventMeta, ProjectionSpec, RowChange, Rows, sync_rows};

/// One attempt as an operator sees it (`mayfly vm inspect`): where it runs, its phase, the
/// directive it waits on and why it ended. Key: the attempt's stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptRow {
    /// The attempt.
    pub attempt_id: AttemptId,
    /// Its pool.
    pub pool: PoolName,
    /// Its host.
    pub host_id: HostId,
    /// Its slot on the host.
    pub slot: Slot,
    /// The slot's generation it holds.
    pub generation: Generation,
    /// Its VM's name.
    pub vm_name: OwnedVmName,
    /// The image it runs.
    pub image_digest: ImageDigest,
    /// The phase, `snake_case` (`cloning`, `backoff`, `quarantined`, …).
    pub phase: String,
    /// The stage a backoff retries.
    pub backoff_stage: Option<Stage>,
    /// The backoff's count.
    pub backoff: Option<u32>,
    /// Why it is quarantined.
    pub quarantine: Option<QuarantineReason>,
    /// The directive it waits on, `snake_case`.
    pub pending: Option<String>,
    /// Whether that directive's outcome is unknown (I10).
    pub pending_unknown: bool,
    /// The job bound to it.
    pub job: Option<RunnerRequestId>,
    /// Why it retired.
    pub retire_reason: Option<RetireReason>,
    /// The desync it recorded: its VM vanished outside mayfly, and how that was found
    /// (`VmVanished`, DEC-07); the phase is then `lost`. What `mayfly status` and the
    /// diagnostics bundle surface. Absent from rows written by version 1, read as none.
    #[serde(default)]
    pub desync: Option<VanishDiscovery>,
    /// Why the latest directive was refused, while no newer one was issued: the reconciler
    /// reads `insufficient_resources` as the data plane's definite no (ADR 0024, CP-03).
    /// Absent from rows written before version 3, read as none.
    #[serde(default)]
    pub refused: Option<RefusalReason>,
    /// How the VM went: `vm_deleted` once the attempt observed its delete, `attempt_lost` once
    /// it was found lost; kept after the stream closes, so the reconciler releases the lease on
    /// that evidence (CP-03). Absent from rows written before version 3, read as none.
    #[serde(default)]
    pub vm_gone: Option<ReleaseEvidence>,
    /// The stream revision this row is the fold of.
    pub revision: u64,
    /// When that event was recorded (display only).
    pub updated_at: Millis,
}

/// The `attempts` projection: one row per started attempt.
#[derive(Debug, Clone, Copy, Default)]
pub struct Attempts;

impl ProjectionSpec for Attempts {
    const NAME: &'static str = "attempts";
    /// 2: the row's `desync` (DEC-07); 3: `refused` and `vm_gone` (CP-03); compatible
    /// additions.
    const VERSION: u32 = 3;
    const SUBSCRIBES: &'static [EventKind] = <AttemptEvent as EventSet>::KINDS;
    type Source = Attempt;
    type Row = AttemptRow;

    fn apply(
        rows: &Rows<AttemptRow>,
        event: &AttemptEvent,
        after: &AttemptState,
        at: EventMeta,
    ) -> Vec<RowChange<AttemptRow>> {
        let before = rows.get("");
        let desired = attempt_row(after, at)
            .map(|mut row| {
                row.refused = refused(before, event);
                row.vm_gone = vm_gone(before, after.phase);
                row
            })
            .map(|row| (String::new(), row))
            .into_iter()
            .collect();
        sync_rows(rows, desired)
    }
}

/// The refusal the row shows after `event`: set by a refusal, cleared by the next directive,
/// carried otherwise. Pinned by
/// `tests/store/projections.rs::should_show_the_refusal_until_the_next_directive_when_a_clone_is_refused`.
fn refused(before: Option<&AttemptRow>, event: &AttemptEvent) -> Option<RefusalReason> {
    match event {
        AttemptEvent::DirectiveRefused(DirectiveRefused { reason, .. }) => Some(*reason),
        AttemptEvent::DirectiveIssued(..) => None,
        _ => before.and_then(|row| row.refused),
    }
}

/// The evidence that the VM is gone: from the phase that observed it, then carried. Pinned by
/// `tests/store/projections.rs::should_keep_the_evidence_on_the_row_when_a_lost_attempt_closes`.
fn vm_gone(before: Option<&AttemptRow>, phase: Phase) -> Option<ReleaseEvidence> {
    match phase {
        Phase::Deleted => Some(ReleaseEvidence::VmDeleted),
        Phase::Lost => Some(ReleaseEvidence::AttemptLost),
        Phase::NotStarted
        | Phase::Reserved
        | Phase::Cloning
        | Phase::Booting
        | Phase::Verifying
        | Phase::Prepared
        | Phase::Activating
        | Phase::Listening
        | Phase::Busy
        | Phase::Finished
        | Phase::Retiring
        | Phase::Deleting
        | Phase::Backoff { .. }
        | Phase::Quarantined { .. }
        | Phase::Closed => before.and_then(|row| row.vm_gone),
    }
}

fn attempt_row(state: &AttemptState, at: EventMeta) -> Option<AttemptRow> {
    let identity = state.identity.as_ref()?;
    let (backoff_stage, backoff) = match state.phase {
        Phase::Backoff { stage, n } => (Some(stage), Some(n)),
        Phase::NotStarted
        | Phase::Reserved
        | Phase::Cloning
        | Phase::Booting
        | Phase::Verifying
        | Phase::Prepared
        | Phase::Activating
        | Phase::Listening
        | Phase::Busy
        | Phase::Finished
        | Phase::Retiring
        | Phase::Deleting
        | Phase::Deleted
        | Phase::Lost
        | Phase::Quarantined { .. }
        | Phase::Closed => (None, None),
    };
    let quarantine = match state.phase {
        Phase::Quarantined { reason, .. } => Some(reason),
        Phase::NotStarted
        | Phase::Reserved
        | Phase::Cloning
        | Phase::Booting
        | Phase::Verifying
        | Phase::Prepared
        | Phase::Activating
        | Phase::Listening
        | Phase::Busy
        | Phase::Finished
        | Phase::Retiring
        | Phase::Deleting
        | Phase::Deleted
        | Phase::Lost
        | Phase::Backoff { .. }
        | Phase::Closed => None,
    };
    Some(AttemptRow {
        attempt_id: identity.attempt_id,
        pool: identity.pool.clone(),
        host_id: identity.host_id.clone(),
        slot: identity.slot,
        generation: identity.generation,
        vm_name: identity.vm_name.clone(),
        image_digest: identity.image_digest.clone(),
        phase: phase_code(state.phase).to_owned(),
        backoff_stage,
        backoff,
        quarantine,
        pending: state.pending.map(|p| tag_code(p.tag).to_owned()),
        pending_unknown: state
            .pending
            .is_some_and(|p| matches!(p.outcome, Outcome::Unknown { .. })),
        job: state.job_binding,
        retire_reason: state.retire_reason,
        desync: state.vanished,
        refused: None,
        vm_gone: None,
        revision: at.revision.get(),
        updated_at: at.recorded_at,
    })
}

/// The phase's `snake_case` code: what the row stores, so the state's Rust shape can change
/// without changing the row.
const fn phase_code(phase: Phase) -> &'static str {
    match phase {
        Phase::NotStarted => "not_started",
        Phase::Reserved => "reserved",
        Phase::Cloning => "cloning",
        Phase::Booting => "booting",
        Phase::Verifying => "verifying",
        Phase::Prepared => "prepared",
        Phase::Activating => "activating",
        Phase::Listening => "listening",
        Phase::Busy => "busy",
        Phase::Finished => "finished",
        Phase::Retiring => "retiring",
        Phase::Deleting => "deleting",
        Phase::Deleted => "deleted",
        Phase::Lost => "lost",
        Phase::Backoff { .. } => "backoff",
        Phase::Quarantined { .. } => "quarantined",
        Phase::Closed => "closed",
    }
}

/// The directive's `snake_case` code.
const fn tag_code(tag: DirectiveTag) -> &'static str {
    match tag {
        DirectiveTag::CloneVm => "clone_vm",
        DirectiveTag::StartVm => "start_vm",
        DirectiveTag::VerifyGuest => "verify_guest",
        DirectiveTag::LaunchRunner => "launch_runner",
        DirectiveTag::ExportDiagnostics => "export_diagnostics",
        DirectiveTag::StopVm => "stop_vm",
        DirectiveTag::DeleteVm => "delete_vm",
    }
}

/// One directive in flight or of unknown outcome (`PendingEffects`, the P19 alert source).
/// Key: `<attempt stream>/<directive id>`; deleted when the directive resolves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingEffectRow {
    /// The attempt.
    pub attempt_id: AttemptId,
    /// Its pool.
    pub pool: PoolName,
    /// Its host.
    pub host_id: HostId,
    /// The directive.
    pub directive_id: DirectiveId,
    /// What it asks, `snake_case`.
    pub directive: String,
    /// Whether the relay sent it.
    pub sent: bool,
    /// Since when its outcome is unknown, if it is (I10).
    pub unknown_since: Option<Millis>,
    /// Its deadline.
    pub deadline: Deadline,
}

/// The `pending_effects` projection: at most one row per attempt, the directive it waits on.
#[derive(Debug, Clone, Copy, Default)]
pub struct PendingEffects;

impl ProjectionSpec for PendingEffects {
    const NAME: &'static str = "pending_effects";
    /// 2: `VmVanished` clears the pending directive (DEC-07), a compatible addition.
    const VERSION: u32 = 2;
    /// Every kind that sets, changes or clears the pending directive (`evolve`).
    const SUBSCRIBES: &'static [EventKind] = &[
        EventKind::of::<DirectiveIssued>(),
        EventKind::of::<DirectiveSent>(),
        EventKind::of::<DirectiveRefused>(),
        EventKind::of::<DirectiveOutcomeUnknown>(),
        EventKind::of::<DirectiveObservedNotApplied>(),
        EventKind::of::<VmCloned>(),
        EventKind::of::<VmBooted>(),
        EventKind::of::<GuestVerified>(),
        EventKind::of::<GuestRejected>(),
        EventKind::of::<RunnerLaunched>(),
        EventKind::of::<RunnerLaunchUnknown>(),
        EventKind::of::<AttemptBackedOff>(),
        EventKind::of::<AttemptRetired>(),
        EventKind::of::<DiagnosticsExported>(),
        EventKind::of::<DiagnosticsLost>(),
        EventKind::of::<VmStopped>(),
        EventKind::of::<VmDeleted>(),
        EventKind::of::<AttemptLost>(),
        EventKind::of::<VmVanished>(),
        EventKind::of::<AttemptQuarantined>(),
    ];
    type Source = Attempt;
    type Row = PendingEffectRow;

    fn apply(
        rows: &Rows<PendingEffectRow>,
        _event: &AttemptEvent,
        after: &AttemptState,
        _at: EventMeta,
    ) -> Vec<RowChange<PendingEffectRow>> {
        let desired = match (&after.identity, after.pending) {
            (Some(identity), Some(pending)) => {
                let row = PendingEffectRow {
                    attempt_id: identity.attempt_id,
                    pool: identity.pool.clone(),
                    host_id: identity.host_id.clone(),
                    directive_id: pending.directive_id,
                    directive: tag_code(pending.tag).to_owned(),
                    sent: pending.sent,
                    unknown_since: match pending.outcome {
                        Outcome::Unknown { since } => Some(since),
                        Outcome::InFlight => None,
                    },
                    deadline: pending.deadline,
                };
                [(pending.directive_id.to_string(), row)]
                    .into_iter()
                    .collect()
            }
            _ => Rows::new(),
        };
        sync_rows(rows, desired)
    }
}
