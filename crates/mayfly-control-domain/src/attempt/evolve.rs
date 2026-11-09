//! `evolve` for the attempt: total, since a recorded event happened. Every match is explicit
//! (`ARCH-exhaustive-transition`).

use super::events::AttemptEvent;
use super::state::{
    AttemptState, Diagnostics, DirectiveTag, Identity, Jit, Outcome, Pending, Phase, Stage,
    VmCondition,
};

/// Applies one event to the state.
#[must_use]
pub fn evolve(state: AttemptState, event: &AttemptEvent) -> AttemptState {
    let mut s = state;
    match event {
        AttemptEvent::AttemptStarted(e) => {
            s.phase = Phase::Reserved;
            s.identity = Some(Identity {
                attempt_id: e.attempt_id,
                host_id: e.host_id.clone(),
                slot: e.slot,
                generation: e.generation,
                vm_name: e.vm_name.clone(),
                pool: e.pool.clone(),
                image_digest: e.image_digest.clone(),
                retry_budget: e.retry_budget,
            });
            s.epoch = e.epoch;
        }
        AttemptEvent::DirectiveIssued(e) => {
            let tag = e.kind.tag();
            s.pending = Some(Pending {
                directive_id: e.directive_id,
                tag,
                outcome: Outcome::InFlight,
                deadline: e.deadline,
            });
            if matches!(
                tag,
                DirectiveTag::CloneVm | DirectiveTag::StartVm | DirectiveTag::VerifyGuest
            ) {
                s.tries = s.tries.saturating_add(1);
            }
            s.phase = evolve_issued_phase(s.phase, tag);
        }
        AttemptEvent::DirectiveRefused(..) | AttemptEvent::DirectiveObservedNotApplied(..) => {
            s.pending = None;
        }
        AttemptEvent::DirectiveOutcomeUnknown(e) => {
            s.pending = s.pending.map(|pending| Pending {
                outcome: Outcome::Unknown { since: e.since },
                ..pending
            });
        }
        AttemptEvent::VmCloned(e) => {
            s.pending = None;
            s.instance_id = Some(e.instance_id.clone());
            s.phase = Phase::Booting;
            s.tries = 0;
        }
        AttemptEvent::VmBooted(..) => {
            s.pending = None;
            s.vm_running = true;
            s.phase = Phase::Verifying;
            s.tries = 0;
        }
        AttemptEvent::GuestVerified(..) => {
            s.pending = None;
            s.fresh_verification = s.phase == Phase::Activating;
            s.tries = 0;
        }
        AttemptEvent::GuestRejected(..) => s.pending = None,
        AttemptEvent::RunnerLaunched(..) => {
            s.pending = None;
            s.phase = Phase::Listening;
        }
        AttemptEvent::StaleObservationIgnored(..) => {}
        AttemptEvent::AttemptPrepared(..) => {
            s.phase = Phase::Prepared;
            s.tries = 0;
        }
        AttemptEvent::ActivationStarted(e) => {
            s.phase = Phase::Activating;
            s.grant_ref = Some(e.grant_ref);
            s.fresh_verification = false;
            s.tries = 0;
        }
        AttemptEvent::JitHandleIssued(e) => s.jit = Jit::Issued(e.handle_id),
        AttemptEvent::JitHandleTaken(e) => s.jit = Jit::Taken(e.handle_id),
        AttemptEvent::JitHandleOrphaned(e) => s.jit = Jit::Orphaned(e.handle_id),
        AttemptEvent::RunnerLaunchUnknown(..) => {
            s.pending = None;
            s.jit = evolve_presumed_taken(s.jit);
        }
        AttemptEvent::JobBound(e) => {
            s.job_binding = Some(e.request_id);
            s.retirement_requested = None;
            s.phase = Phase::Busy;
        }
        AttemptEvent::RunnerExited(..) => {
            s.runner_exited = true;
            s.phase = evolve_exited_phase(s.phase);
        }
        AttemptEvent::VmObservedStopped(..) => {
            s.vm_running = false;
            s.phase = evolve_stopped_phase(s.phase);
        }
        AttemptEvent::AttemptBackedOff(e) => {
            s.pending = None;
            s.phase = Phase::Backoff {
                stage: e.stage,
                n: e.n,
            };
        }
        AttemptEvent::RetirementRequested(e) => s.retirement_requested = Some(e.reason),
        AttemptEvent::RetirementWithdrawn(..) => s.retirement_requested = None,
        AttemptEvent::AttemptRetired(e) => {
            s.retire_reason = Some(e.reason);
            s.retirement_requested = None;
            s.pending = None;
            s.phase = match s.instance_id {
                Some(..) => Phase::Retiring,
                None => Phase::Deleted,
            };
        }
        AttemptEvent::DiagnosticsExported(..) => {
            s.pending = None;
            s.diagnostics = Diagnostics::Exported;
        }
        AttemptEvent::DiagnosticsLost(..) => {
            s.pending = None;
            s.diagnostics = Diagnostics::Lost;
        }
        AttemptEvent::VmStopped(..) => {
            s.pending = None;
            s.vm_running = false;
            s.phase = evolve_stopped_phase(s.phase);
        }
        AttemptEvent::VmDeleted(..) => {
            s.pending = None;
            s.vm_running = false;
            s.phase = Phase::Deleted;
        }
        AttemptEvent::AttemptLost(..) => {
            s.pending = None;
            s.vm_running = false;
            s.phase = Phase::Lost;
        }
        AttemptEvent::AttemptQuarantined(e) => {
            s.pending = None;
            s.retirement_requested = None;
            s.phase = Phase::Quarantined {
                reason: e.reason,
                vm: e.vm,
            };
        }
        AttemptEvent::AttemptAdopted(e) => {
            s.epoch = e.epoch;
            s.fresh_verification = false;
        }
        AttemptEvent::AttemptClosed(..) => s.phase = Phase::Closed,
    }
    s
}

/// The phase a directive moves the attempt into: a clone leaves `Reserved`, a retry leaves
/// `Backoff` for its stage, a delete moves cleanup to `Deleting`.
const fn evolve_issued_phase(phase: Phase, tag: DirectiveTag) -> Phase {
    match phase {
        Phase::Reserved => Phase::Cloning,
        Phase::Backoff { stage, .. } => match stage {
            Stage::Clone => Phase::Cloning,
            Stage::Boot => Phase::Booting,
            Stage::Verify => Phase::Verifying,
            Stage::Activate => Phase::Activating,
        },
        Phase::Retiring => match tag {
            DirectiveTag::DeleteVm => Phase::Deleting,
            DirectiveTag::CloneVm
            | DirectiveTag::StartVm
            | DirectiveTag::VerifyGuest
            | DirectiveTag::LaunchRunner
            | DirectiveTag::ExportDiagnostics
            | DirectiveTag::StopVm => Phase::Retiring,
        },
        Phase::NotStarted
        | Phase::Cloning
        | Phase::Booting
        | Phase::Verifying
        | Phase::Prepared
        | Phase::Activating
        | Phase::Listening
        | Phase::Busy
        | Phase::Finished
        | Phase::Deleting
        | Phase::Deleted
        | Phase::Lost
        | Phase::Quarantined { .. }
        | Phase::Closed => phase,
    }
}

/// A runner that exits ends its service: Listening, Busy and a launch of unknown outcome are
/// `Finished`; elsewhere the exit is only recorded.
const fn evolve_exited_phase(phase: Phase) -> Phase {
    match phase {
        Phase::Listening | Phase::Busy | Phase::Activating => Phase::Finished,
        Phase::NotStarted
        | Phase::Reserved
        | Phase::Cloning
        | Phase::Booting
        | Phase::Verifying
        | Phase::Prepared
        | Phase::Finished
        | Phase::Retiring
        | Phase::Deleting
        | Phase::Deleted
        | Phase::Lost
        | Phase::Backoff { .. }
        | Phase::Quarantined { .. }
        | Phase::Closed => phase,
    }
}

/// A quarantined VM that stops is recorded as stopped; no other phase changes on a stop.
const fn evolve_stopped_phase(phase: Phase) -> Phase {
    match phase {
        Phase::Quarantined { reason, .. } => Phase::Quarantined {
            reason,
            vm: VmCondition::Stopped,
        },
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
        | Phase::Closed => phase,
    }
}

/// A launch whose outcome is unknown counts as taken: the handle is never launched again
/// (I22).
const fn evolve_presumed_taken(jit: Jit) -> Jit {
    match jit {
        Jit::Issued(handle_id) => Jit::Taken(handle_id),
        Jit::None | Jit::Taken(..) | Jit::Orphaned(..) => jit,
    }
}
