//! `decide` for the attempt: every phase against every stimulus, as explicit match arms with
//! no wildcard (plan §6 rule 8, `ARCH-exhaustive-transition`). The outcome of every pair is
//! pinned by
//! `tests/attempt/matrix.rs::should_define_every_pair_when_each_phase_meets_each_stimulus`.

use mayfly_domain::{
    Deadline, DirectiveId, Epoch, EventId, Millis, OwnedVmName, ProviderInstanceId, SecretHandleId,
};

use super::command::{
    AttemptCommand, DiagnosticsLoss, Fact, GuestFacts, Observation, RefusalReason, StartAttempt,
};
use super::events::{
    ActivationStarted, AttemptAdopted, AttemptBackedOff, AttemptClosed, AttemptEvent, AttemptLost,
    AttemptPrepared, AttemptQuarantined, AttemptRetired, AttemptStarted, DiagnosticsExported,
    DiagnosticsLost, DirectiveIssued, DirectiveObservedNotApplied, DirectiveOutcomeUnknown,
    DirectiveRefused, DirectiveSent, GuestRejected, GuestVerified, JitHandleIssued,
    JitHandleOrphaned, JitHandleTaken, JobBound, LostEvidence, RetirementRequested,
    RetirementWithdrawn, RunnerExited, RunnerLaunchUnknown, RunnerLaunched,
    StaleObservationIgnored, Staleness, StopDeregistrationConfirmed, StopDeregistrationRefusedBusy,
    StopDeregistrationRequested, VmBooted, VmCloned, VmDeleted, VmObservedStopped, VmStopped,
    VmVanished,
};
use super::rejection::AttemptRejection;
use super::state::{
    AttemptState, Diagnostics, DirectiveKind, DirectiveTag, Jit, Outcome, OwnershipProof, Pending,
    Phase, QuarantineReason, RetireReason, RunnerRequestId, Stage, StopDeregistration,
    VanishDiscovery, VmCondition,
};
use crate::command::{Actor, Command};

/// What `decide` returns.
pub type Decision = Result<Vec<AttemptEvent>, AttemptRejection>;

/// Decides `command` against `state`. Refusals are rejections, never events (§6.0).
///
/// # Errors
/// The [`AttemptRejection`] that names why the command is not allowed.
pub fn decide(state: &AttemptState, command: Command<AttemptCommand>) -> Decision {
    let Command {
        issued_at,
        run_epoch,
        actor,
        body,
        ..
    } = command;
    match state.phase {
        Phase::NotStarted => decide_not_started(run_epoch, body),
        Phase::Closed => Err(AttemptRejection::Closed),
        Phase::Reserved
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
        | Phase::Quarantined { .. } => decide_started(state, issued_at, run_epoch, actor, body),
    }
}

fn decide_not_started(run_epoch: Epoch, body: AttemptCommand) -> Decision {
    match body {
        AttemptCommand::StartAttempt(start) => Ok(vec![started(start, run_epoch)]),
        AttemptCommand::IssueNext { .. }
        | AttemptCommand::RecordObservation(..)
        | AttemptCommand::RecordDirectiveTimeout { .. }
        | AttemptCommand::RecordDirectiveSent { .. }
        | AttemptCommand::BeginActivation { .. }
        | AttemptCommand::RecordJitHandle { .. }
        | AttemptCommand::BindJob { .. }
        | AttemptCommand::RequestRetirement { .. }
        | AttemptCommand::ConfirmRetirement
        | AttemptCommand::WithdrawRetirement
        | AttemptCommand::Quarantine { .. }
        | AttemptCommand::Adopt { .. }
        | AttemptCommand::ResolveQuarantine
        | AttemptCommand::Close => Err(AttemptRejection::NotStarted),
    }
}

/// The VM name is derived from the reservation's values, never taken from elsewhere (I30).
fn started(start: StartAttempt, epoch: Epoch) -> AttemptEvent {
    let vm_name = OwnedVmName::new(
        start.pool.clone(),
        start.host_id.clone(),
        start.slot,
        start.generation,
        start.attempt_id,
    );
    AttemptEvent::AttemptStarted(AttemptStarted {
        attempt_id: start.attempt_id,
        host_id: start.host_id,
        slot: start.slot,
        generation: start.generation,
        vm_name,
        pool: start.pool,
        image_digest: start.image_digest,
        retry_budget: start.retry_budget.max(1),
        epoch,
    })
}

/// The envelope rules, in order: a started stream refuses `StartAttempt`; an older run epoch
/// is refused (I18); `Adopt` and observations may carry a newer epoch, any other command must
/// wait for the adoption; with an outcome `Unknown`, nothing but an observation or an adoption
/// is decided (I10); with a directive in flight, no command is.
fn decide_started(
    state: &AttemptState,
    issued_at: Millis,
    run_epoch: Epoch,
    actor: Actor,
    body: AttemptCommand,
) -> Decision {
    if run_epoch < state.epoch {
        return match body {
            AttemptCommand::StartAttempt(..) => Err(AttemptRejection::AlreadyStarted),
            AttemptCommand::IssueNext { .. }
            | AttemptCommand::RecordObservation(..)
            | AttemptCommand::RecordDirectiveTimeout { .. }
            | AttemptCommand::RecordDirectiveSent { .. }
            | AttemptCommand::BeginActivation { .. }
            | AttemptCommand::RecordJitHandle { .. }
            | AttemptCommand::BindJob { .. }
            | AttemptCommand::RequestRetirement { .. }
            | AttemptCommand::ConfirmRetirement
            | AttemptCommand::WithdrawRetirement
            | AttemptCommand::Quarantine { .. }
            | AttemptCommand::Adopt { .. }
            | AttemptCommand::ResolveQuarantine
            | AttemptCommand::Close => Err(AttemptRejection::StaleRun),
        };
    }
    match body {
        AttemptCommand::StartAttempt(..) => Err(AttemptRejection::AlreadyStarted),
        AttemptCommand::Adopt { inventory_ref } => decide_adopt(state, run_epoch, inventory_ref),
        AttemptCommand::RecordObservation(observation) => {
            decide_observation(state, issued_at, &observation)
        }
        AttemptCommand::RecordDirectiveTimeout { directive_id } => {
            if run_epoch > state.epoch {
                return Err(AttemptRejection::NotAdopted);
            }
            decide_timeout(state, issued_at, directive_id)
        }
        AttemptCommand::RecordDirectiveSent {
            directive_id,
            deadline,
        } => {
            if run_epoch > state.epoch {
                return Err(AttemptRejection::NotAdopted);
            }
            decide_sent(state, directive_id, deadline)
        }
        command @ (AttemptCommand::IssueNext { .. }
        | AttemptCommand::BeginActivation { .. }
        | AttemptCommand::RecordJitHandle { .. }
        | AttemptCommand::BindJob { .. }
        | AttemptCommand::RequestRetirement { .. }
        | AttemptCommand::ConfirmRetirement
        | AttemptCommand::WithdrawRetirement
        | AttemptCommand::Quarantine { .. }
        | AttemptCommand::ResolveQuarantine
        | AttemptCommand::Close) => {
            if run_epoch > state.epoch {
                return Err(AttemptRejection::NotAdopted);
            }
            match state.pending {
                Some(Pending {
                    outcome: Outcome::Unknown { .. },
                    ..
                }) => Err(AttemptRejection::OutcomeUnknown),
                Some(Pending {
                    outcome: Outcome::InFlight,
                    ..
                }) => Err(AttemptRejection::DirectivePending),
                None => decide_command(state, actor, command),
            }
        }
    }
}

fn decide_command(state: &AttemptState, actor: Actor, body: AttemptCommand) -> Decision {
    match body {
        AttemptCommand::IssueNext {
            directive_id,
            deadline,
        } => decide_issue_next(state, directive_id, deadline),
        AttemptCommand::BeginActivation { grant_ref } => decide_begin_activation(state, grant_ref),
        AttemptCommand::RecordJitHandle {
            handle_id,
            registration_ref,
        } => decide_record_jit_handle(state, handle_id, registration_ref),
        AttemptCommand::BindJob { request_id } => decide_bind_job(state, request_id),
        AttemptCommand::RequestRetirement { reason } => decide_request_retirement(state, reason),
        AttemptCommand::ConfirmRetirement => decide_confirm_retirement(state),
        AttemptCommand::WithdrawRetirement => decide_withdraw_retirement(state),
        AttemptCommand::Quarantine { reason } => decide_quarantine(state, reason),
        AttemptCommand::ResolveQuarantine => decide_resolve_quarantine(state, actor),
        AttemptCommand::Close => decide_close(state),
        AttemptCommand::StartAttempt(..) => Err(AttemptRejection::AlreadyStarted),
        AttemptCommand::Adopt { .. }
        | AttemptCommand::RecordObservation(..)
        | AttemptCommand::RecordDirectiveTimeout { .. }
        | AttemptCommand::RecordDirectiveSent { .. } => Err(AttemptRejection::ObservationMismatch),
    }
}

// ---- directives ------------------------------------------------------------------------------

/// `IssueNext`: the stage's directive. Retries are issued from `Backoff` with the same name and
/// generation (I13): the name is the stream's, never the command's.
fn decide_issue_next(
    state: &AttemptState,
    directive_id: DirectiveId,
    deadline: Deadline,
) -> Decision {
    issuable(state)?
        .map(|kind| {
            vec![AttemptEvent::DirectiveIssued(DirectiveIssued {
                directive_id,
                kind,
                deadline,
            })]
        })
        .ok_or(AttemptRejection::NothingToIssue)
}

/// The directive `IssueNext` would issue in `state`'s phase, pending overlay aside: `None` when
/// the phase has none (`nothing_to_issue`), an error when the stream cannot issue at all. The
/// saga reads it to issue only what would be accepted (pinned by
/// `tests/attempt/saga.rs::should_issue_exactly_when_issue_next_would_be_accepted`).
///
/// # Errors
/// [`AttemptRejection::NotStarted`], [`AttemptRejection::Closed`], or
/// [`AttemptRejection::NoProof`] for a stop or delete without a recorded instance id.
pub fn issuable(state: &AttemptState) -> Result<Option<DirectiveKind>, AttemptRejection> {
    Ok(match state.phase {
        Phase::Reserved | Phase::Cloning => Some(DirectiveKind::CloneVm),
        Phase::Booting => Some(DirectiveKind::StartVm),
        Phase::Verifying => Some(DirectiveKind::VerifyGuest),
        // a retirement waiting for its deregistration does not retry the stage (KER-04G)
        Phase::Backoff { stage, .. } => match state.retirement_requested {
            Some(..) => None,
            None => Some(stage_directive(stage)),
        },
        Phase::Activating => activation_directive(state),
        Phase::Retiring => cleanup_directive(state)?,
        Phase::Deleting => {
            if within_budget(state) {
                Some(DirectiveKind::DeleteVm {
                    proof: proof(state)?,
                })
            } else {
                None
            }
        }
        Phase::Quarantined { .. } => {
            // stopped with proof unless a job is live, never deleted; a VM without a recorded
            // instance id (ownership doubt) cannot be proven ours and is not touched; a runner
            // that may be registered in it is deregistered first (ADR 0076)
            let stoppable = state.vm_running
                && !job_live(state)
                && state.instance_id.is_some()
                && stop_cleared(state)
                && within_budget(state);
            if stoppable {
                Some(DirectiveKind::StopVm {
                    proof: proof(state)?,
                })
            } else {
                None
            }
        }
        Phase::Prepared
        | Phase::Listening
        | Phase::Busy
        | Phase::Finished
        | Phase::Deleted
        | Phase::Lost => None,
        Phase::NotStarted => return Err(AttemptRejection::NotStarted),
        Phase::Closed => return Err(AttemptRejection::Closed),
    })
}

/// `RecordDirectiveTimeout`: the pending directive was sent and its deadline passed with no
/// answer, so its outcome is `Unknown` (P7): the effect happened or not, and nothing is read as
/// success. A timeout of a directive never sent is refused (held unsent, nothing happened), one
/// before the deadline is refused, a second one changes nothing, and one for a directive no
/// longer pending is refused (the answer came first). Pinned by
/// `tests/attempt/lifecycle.rs::should_make_the_outcome_unknown_never_success_when_a_deadline_passes`.
fn decide_timeout(state: &AttemptState, issued_at: Millis, directive_id: DirectiveId) -> Decision {
    match state.pending {
        Some(pending) if pending.directive_id == directive_id => match pending.outcome {
            Outcome::InFlight if !pending.sent => Err(AttemptRejection::DirectiveNotSent),
            Outcome::InFlight => {
                if pending.deadline.is_expired(issued_at) {
                    Ok(vec![AttemptEvent::DirectiveOutcomeUnknown(
                        DirectiveOutcomeUnknown {
                            directive_id,
                            since: issued_at,
                        },
                    )])
                } else {
                    Err(AttemptRejection::DeadlineNotReached)
                }
            }
            Outcome::Unknown { .. } => Ok(Vec::new()),
        },
        Some(..) | None => Err(AttemptRejection::DirectiveNotPending),
    }
}

/// `RecordDirectiveSent`: the relay put the pending directive on the link; its deadline runs
/// from the one the frame carried. A repeat (another relay worker's send) changes nothing, and
/// one for a directive no longer pending (answered first) is refused. Pinned by
/// `tests/attempt/lifecycle.rs::should_time_out_only_a_sent_directive_when_its_deadline_passes`.
fn decide_sent(state: &AttemptState, directive_id: DirectiveId, deadline: Deadline) -> Decision {
    match state.pending {
        Some(pending) if pending.directive_id == directive_id => {
            if pending.sent || pending.outcome != Outcome::InFlight {
                Ok(Vec::new())
            } else {
                Ok(vec![AttemptEvent::DirectiveSent(DirectiveSent {
                    directive_id,
                    deadline,
                })])
            }
        }
        Some(..) | None => Err(AttemptRejection::DirectiveNotPending),
    }
}

const fn stage_directive(stage: Stage) -> DirectiveKind {
    match stage {
        Stage::Clone => DirectiveKind::CloneVm,
        Stage::Boot => DirectiveKind::StartVm,
        Stage::Verify | Stage::Activate => DirectiveKind::VerifyGuest,
    }
}

/// Activation: a fresh verification first, then the one launch of the one handle (I22).
fn activation_directive(state: &AttemptState) -> Option<DirectiveKind> {
    if !state.fresh_verification {
        return Some(DirectiveKind::VerifyGuest);
    }
    match state.jit {
        Jit::Issued(handle_id) => Some(DirectiveKind::LaunchRunner { handle_id }),
        Jit::None | Jit::Taken(..) | Jit::Orphaned(..) => None,
    }
}

/// Whether another stop or delete may be issued: they count against the retry budget like the
/// stage directives, so cleanup the data plane keeps refusing ends (pinned by
/// `tests/attempt/lifecycle.rs::should_quarantine_when_a_delete_is_refused_again_and_again`).
fn within_budget(state: &AttemptState) -> bool {
    state.tries
        < state
            .identity
            .as_ref()
            .map_or(1, |identity| identity.retry_budget)
}

/// Cleanup: diagnostics while the VM runs, then the stop, then the delete, each with proof and
/// within the retry budget.
fn cleanup_directive(state: &AttemptState) -> Result<Option<DirectiveKind>, AttemptRejection> {
    let diagnostics_pending = match state.diagnostics {
        Diagnostics::NotTaken => true,
        Diagnostics::Exported | Diagnostics::Lost => false,
    };
    if state.vm_running && diagnostics_pending {
        return Ok(Some(DirectiveKind::ExportDiagnostics));
    }
    if !within_budget(state) {
        return Ok(None);
    }
    if state.vm_running {
        return Ok(Some(DirectiveKind::StopVm {
            proof: proof(state)?,
        }));
    }
    match state.instance_id {
        Some(..) => Ok(Some(DirectiveKind::DeleteVm {
            proof: proof(state)?,
        })),
        None => Ok(None),
    }
}

/// The ownership proof of a stop or delete, built from this stream's `AttemptStarted` and
/// `VmCloned` only (I04 at the control plane; pinned by
/// `tests/attempt/lifecycle.rs::should_carry_the_recorded_instance_id_when_a_delete_is_issued`).
fn proof(state: &AttemptState) -> Result<OwnershipProof, AttemptRejection> {
    match (&state.identity, &state.instance_id) {
        (Some(identity), Some(instance_id)) => Ok(OwnershipProof {
            attempt_id: identity.attempt_id,
            host_id: identity.host_id.clone(),
            slot: identity.slot,
            generation: identity.generation,
            vm_name: identity.vm_name.clone(),
            instance_id: instance_id.clone(),
        }),
        (None, Some(..)) | (Some(..), None) | (None, None) => Err(AttemptRejection::NoProof),
    }
}

/// A job is bound and its runner has not exited: the VM is not stopped under it.
const fn job_live(state: &AttemptState) -> bool {
    state.job_binding.is_some() && !state.runner_exited
}

/// Whether the automatic stop of a quarantined VM would leave a runner of the attempt's
/// registration registered and idle on GitHub (ADR 0076, the model's `stopNeedsDeregistration`):
/// a JIT was issued, the runner was not seen exiting, no retirement passed the deregistration
/// before (a retiring attempt had P11's answer or had nothing to wait for), and the stop would
/// be issued at all (the VM runs, its instance id is recorded, no job is live). Read on entering
/// quarantine only.
const fn stop_needs_deregistration(state: &AttemptState) -> bool {
    let registered = match state.jit {
        Jit::None => false,
        Jit::Issued(..) | Jit::Taken(..) | Jit::Orphaned(..) => true,
    };
    let retired = match state.phase {
        Phase::Retiring | Phase::Deleting => true,
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
        | Phase::Deleted
        | Phase::Lost
        | Phase::Backoff { .. }
        | Phase::Quarantined { .. }
        | Phase::Closed => false,
    };
    registered
        && !state.runner_exited
        && !retired
        && state.vm_running
        && state.instance_id.is_some()
        && !job_live(state)
}

/// Whether the quarantine stop waits for nothing more: no deregistration was needed, P11
/// confirmed it, or the runner has exited since (an exited runner takes no job, I21). Pinned
/// by `tests/attempt/lifecycle.rs::should_stop_a_quarantined_vm_only_after_the_deregistration_when_a_runner_may_be_registered`.
const fn stop_cleared(state: &AttemptState) -> bool {
    match state.stop_deregistration {
        StopDeregistration::NotNeeded | StopDeregistration::Confirmed => true,
        StopDeregistration::Requested | StopDeregistration::RefusedBusy => state.runner_exited,
    }
}

const fn condition(state: &AttemptState) -> VmCondition {
    if state.vm_running {
        VmCondition::Running
    } else {
        VmCondition::Stopped
    }
}

// ---- activation and the job ------------------------------------------------------------------

fn decide_begin_activation(state: &AttemptState, grant_ref: EventId) -> Decision {
    match state.phase {
        Phase::Prepared => Ok(vec![AttemptEvent::ActivationStarted(ActivationStarted {
            grant_ref,
        })]),
        Phase::Reserved
        | Phase::Cloning
        | Phase::Booting
        | Phase::Verifying
        | Phase::Activating
        | Phase::Listening
        | Phase::Busy
        | Phase::Finished
        | Phase::Retiring
        | Phase::Deleting
        | Phase::Deleted
        | Phase::Lost
        | Phase::Backoff { .. }
        | Phase::Quarantined { .. } => Err(AttemptRejection::NotPrepared),
        Phase::NotStarted => Err(AttemptRejection::NotStarted),
        Phase::Closed => Err(AttemptRejection::Closed),
    }
}

/// The JIT handle needs the grant and a fresh verification of this attempt in the current
/// epoch, and there is only ever one (I22; pinned by
/// `tests/attempt/lifecycle.rs::should_refuse_a_jit_handle_when_activation_lacks_a_grant_or_a_fresh_verification`).
fn decide_record_jit_handle(
    state: &AttemptState,
    handle_id: SecretHandleId,
    registration_ref: EventId,
) -> Decision {
    match state.phase {
        Phase::Activating => {
            if state.grant_ref.is_none() {
                return Err(AttemptRejection::NoGrant);
            }
            if !state.fresh_verification {
                return Err(AttemptRejection::NoFreshVerification);
            }
            match state.jit {
                Jit::None => Ok(vec![AttemptEvent::JitHandleIssued(JitHandleIssued {
                    handle_id,
                    registration_ref,
                })]),
                Jit::Issued(..) | Jit::Taken(..) | Jit::Orphaned(..) => {
                    Err(AttemptRejection::JitAlreadyIssued)
                }
            }
        }
        Phase::Prepared => Err(AttemptRejection::NoGrant),
        Phase::Reserved
        | Phase::Cloning
        | Phase::Booting
        | Phase::Verifying
        | Phase::Listening
        | Phase::Busy
        | Phase::Finished
        | Phase::Retiring
        | Phase::Deleting
        | Phase::Deleted
        | Phase::Lost
        | Phase::Backoff { .. }
        | Phase::Quarantined { .. } => Err(AttemptRejection::NotActivating),
        Phase::NotStarted => Err(AttemptRejection::NotStarted),
        Phase::Closed => Err(AttemptRejection::Closed),
    }
}

/// One VM serves at most one job (I14); a binding withdraws a pending retirement (the 422 race
/// of I21).
fn decide_bind_job(state: &AttemptState, request_id: RunnerRequestId) -> Decision {
    match state.phase {
        Phase::Listening => match (state.job_binding, state.jit) {
            (Some(..), Jit::None | Jit::Issued(..) | Jit::Taken(..) | Jit::Orphaned(..)) => {
                Err(AttemptRejection::JobAlreadyBound)
            }
            (None, Jit::Orphaned(..)) => Err(AttemptRejection::JitPresumedLeaked),
            (None, Jit::None | Jit::Issued(..) | Jit::Taken(..)) => {
                let bound = AttemptEvent::JobBound(JobBound { request_id });
                match state.retirement_requested {
                    Some(..) => Ok(vec![
                        AttemptEvent::RetirementWithdrawn(RetirementWithdrawn {}),
                        bound,
                    ]),
                    None => Ok(vec![bound]),
                }
            }
        },
        Phase::Busy | Phase::Finished => match state.job_binding {
            Some(bound) if bound == request_id => Ok(Vec::new()),
            Some(..) | None => Err(AttemptRejection::JobAlreadyBound),
        },
        Phase::Reserved
        | Phase::Cloning
        | Phase::Booting
        | Phase::Verifying
        | Phase::Prepared
        | Phase::Activating
        | Phase::Retiring
        | Phase::Deleting
        | Phase::Deleted
        | Phase::Lost
        | Phase::Backoff { .. }
        | Phase::Quarantined { .. } => Err(AttemptRejection::NotListening),
        Phase::NotStarted => Err(AttemptRejection::NotStarted),
        Phase::Closed => Err(AttemptRejection::Closed),
    }
}

// ---- retirement, quarantine, adoption, close ------------------------------------------------

/// A runner that holds a registration (a JIT was issued) waits for its deregistration before
/// its VM is stopped (I21), whether activating, listening or backing off from a failed fresh
/// verification; anything else retires at once. A request while one waits is refused wherever
/// it waits: retiring at once there would skip that deregistration (KER-04G; pinned by
/// `tests/attempt/lifecycle.rs::should_wait_for_the_deregistration_when_a_retirement_is_requested_in_backoff_holding_a_registration`
/// and `should_refuse_a_second_request_when_a_retirement_waits_in_backoff`). A busy attempt is
/// never retired.
fn decide_request_retirement(state: &AttemptState, reason: RetireReason) -> Decision {
    let retired = AttemptEvent::AttemptRetired(AttemptRetired { reason });
    if state.retirement_requested.is_some() {
        return Err(AttemptRejection::AlreadyRetiring);
    }
    match state.phase {
        Phase::Reserved
        | Phase::Cloning
        | Phase::Booting
        | Phase::Verifying
        | Phase::Prepared
        | Phase::Finished => Ok(vec![retired]),
        Phase::Activating | Phase::Listening | Phase::Backoff { .. } => match state.jit {
            Jit::None => Ok(vec![retired]),
            Jit::Issued(..) | Jit::Taken(..) | Jit::Orphaned(..) => {
                Ok(vec![AttemptEvent::RetirementRequested(
                    RetirementRequested { reason },
                )])
            }
        },
        Phase::Busy => Err(AttemptRejection::Busy),
        Phase::Retiring | Phase::Deleting => Err(AttemptRejection::AlreadyRetiring),
        Phase::Deleted | Phase::Lost => Err(AttemptRejection::Terminal),
        Phase::Quarantined { .. } => Err(AttemptRejection::Quarantined),
        Phase::NotStarted => Err(AttemptRejection::NotStarted),
        Phase::Closed => Err(AttemptRejection::Closed),
    }
}

/// `ConfirmRetirement` after the deregistration (I21, P11). A request made while activating or
/// listening outlives the phase: a failed fresh verification backs off and a runner that exits
/// finishes with it still set, and the deregistration's confirmation retires the attempt there
/// too (VER-01F; pinned by
/// `tests/attempt/lifecycle.rs::should_retire_when_a_retirement_is_confirmed_after_the_attempt_backed_off`
/// and `should_retire_when_a_retirement_is_confirmed_after_the_runner_exited`). A lost VM ends
/// the attempt: nothing is confirmed after it. Inside quarantine it answers the quarantine
/// stop's deregistration instead (ADR 0076): `StopDeregistrationConfirmed`, and the attempt
/// stays quarantined (pinned by
/// `should_stop_a_quarantined_vm_only_after_the_deregistration_when_a_runner_may_be_registered`).
fn decide_confirm_retirement(state: &AttemptState) -> Decision {
    match state.phase {
        Phase::Activating | Phase::Listening | Phase::Backoff { .. } | Phase::Finished => {
            match state.retirement_requested {
                Some(reason) => Ok(vec![AttemptEvent::AttemptRetired(AttemptRetired {
                    reason,
                })]),
                None => Err(AttemptRejection::NoRetirementRequested),
            }
        }
        Phase::Reserved
        | Phase::Cloning
        | Phase::Booting
        | Phase::Verifying
        | Phase::Prepared
        | Phase::Busy
        | Phase::Retiring
        | Phase::Deleting
        | Phase::Deleted
        | Phase::Lost => Err(AttemptRejection::NoRetirementRequested),
        // P11's answer to the quarantine stop's deregistration: the stop may go (ADR 0076)
        Phase::Quarantined { .. } => match state.stop_deregistration {
            StopDeregistration::Requested => Ok(vec![AttemptEvent::StopDeregistrationConfirmed(
                StopDeregistrationConfirmed {},
            )]),
            StopDeregistration::NotNeeded
            | StopDeregistration::Confirmed
            | StopDeregistration::RefusedBusy => Err(AttemptRejection::NoRetirementRequested),
        },
        Phase::NotStarted => Err(AttemptRejection::NotStarted),
        Phase::Closed => Err(AttemptRejection::Closed),
    }
}

/// `WithdrawRetirement` after a busy answer: wherever a request can be confirmed, it can be
/// withdrawn (VER-01F; pinned by
/// `tests/attempt/lifecycle.rs::should_withdraw_when_a_busy_answer_arrives_after_the_attempt_backed_off`).
/// Inside quarantine a busy answer to the stop's deregistration keeps the VM running until its
/// runner is seen exiting (ADR 0076; pinned by
/// `should_keep_a_quarantined_vm_running_when_github_answers_busy_for_its_deregistration`).
fn decide_withdraw_retirement(state: &AttemptState) -> Decision {
    match state.phase {
        Phase::Activating | Phase::Listening | Phase::Backoff { .. } | Phase::Finished => {
            match state.retirement_requested {
                Some(..) => Ok(vec![AttemptEvent::RetirementWithdrawn(
                    RetirementWithdrawn {},
                )]),
                None => Err(AttemptRejection::NoRetirementRequested),
            }
        }
        Phase::Reserved
        | Phase::Cloning
        | Phase::Booting
        | Phase::Verifying
        | Phase::Prepared
        | Phase::Busy
        | Phase::Retiring
        | Phase::Deleting
        | Phase::Deleted
        | Phase::Lost => Err(AttemptRejection::NoRetirementRequested),
        // a job runs on the runner: the quarantined VM is not stopped under it (ADR 0076)
        Phase::Quarantined { .. } => match state.stop_deregistration {
            StopDeregistration::Requested => Ok(vec![AttemptEvent::StopDeregistrationRefusedBusy(
                StopDeregistrationRefusedBusy {},
            )]),
            StopDeregistration::NotNeeded
            | StopDeregistration::Confirmed
            | StopDeregistration::RefusedBusy => Err(AttemptRejection::NoRetirementRequested),
        },
        Phase::NotStarted => Err(AttemptRejection::NotStarted),
        Phase::Closed => Err(AttemptRejection::Closed),
    }
}

fn decide_quarantine(state: &AttemptState, reason: QuarantineReason) -> Decision {
    match state.phase {
        Phase::Reserved
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
        | Phase::Backoff { .. } => Ok(quarantined(state, reason)),
        Phase::Quarantined { .. } => Err(AttemptRejection::AlreadyQuarantined),
        Phase::Deleted | Phase::Lost => Err(AttemptRejection::Terminal),
        Phase::NotStarted => Err(AttemptRejection::NotStarted),
        Phase::Closed => Err(AttemptRejection::Closed),
    }
}

/// Quarantine for `reason`. Entering it with a runner that may be registered and idle in the
/// running VM also requests that runner's deregistration, which the automatic stop waits for
/// (ADR 0076; pinned by
/// `tests/attempt/lifecycle.rs::should_stop_a_quarantined_vm_only_after_the_deregistration_when_a_runner_may_be_registered`);
/// staying in it requests nothing again.
fn quarantined(state: &AttemptState, reason: QuarantineReason) -> Vec<AttemptEvent> {
    let entered = AttemptEvent::AttemptQuarantined(AttemptQuarantined {
        reason,
        vm: condition(state),
    });
    let entering = match state.phase {
        Phase::Quarantined { .. } => false,
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
        | Phase::Closed => true,
    };
    if entering && stop_needs_deregistration(state) {
        vec![
            entered,
            AttemptEvent::StopDeregistrationRequested(StopDeregistrationRequested {}),
        ]
    } else {
        vec![entered]
    }
}

/// Adoption under a newer run epoch launches nothing; a handle issued and not taken is
/// orphaned (the broker is empty after a restart, U4), and the next JIT needs a fresh
/// verification in the new epoch.
fn decide_adopt(state: &AttemptState, run_epoch: Epoch, inventory_ref: EventId) -> Decision {
    match state.phase {
        Phase::Reserved
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
        | Phase::Quarantined { .. } => {
            if run_epoch == state.epoch {
                return Err(AttemptRejection::AlreadyCurrent);
            }
            let mut events = vec![AttemptEvent::AttemptAdopted(AttemptAdopted {
                epoch: run_epoch,
                inventory_ref,
            })];
            match state.jit {
                Jit::Issued(handle_id) => {
                    events.push(AttemptEvent::JitHandleOrphaned(JitHandleOrphaned {
                        handle_id,
                    }));
                }
                Jit::None | Jit::Taken(..) | Jit::Orphaned(..) => {}
            }
            Ok(events)
        }
        Phase::Deleted | Phase::Lost => Err(AttemptRejection::Terminal),
        Phase::NotStarted => Err(AttemptRejection::NotStarted),
        Phase::Closed => Err(AttemptRejection::Closed),
    }
}

/// Only an operator takes an attempt out of quarantine, and only with a proof the data plane
/// re-checks (I28, ADR 0025).
fn decide_resolve_quarantine(state: &AttemptState, actor: Actor) -> Decision {
    match state.phase {
        Phase::Quarantined { .. } => match actor {
            Actor::Operator { .. } => match state.instance_id {
                Some(..) => Ok(vec![AttemptEvent::AttemptRetired(AttemptRetired {
                    reason: RetireReason::QuarantineResolved,
                })]),
                None => Err(AttemptRejection::NoProof),
            },
            Actor::Automation { .. } | Actor::Startup => Err(AttemptRejection::OperatorOnly),
        },
        Phase::Reserved
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
        | Phase::Backoff { .. } => Err(AttemptRejection::NotQuarantined),
        Phase::NotStarted => Err(AttemptRejection::NotStarted),
        Phase::Closed => Err(AttemptRejection::Closed),
    }
}

fn decide_close(state: &AttemptState) -> Decision {
    match state.phase {
        Phase::Deleted | Phase::Lost => Ok(vec![AttemptEvent::AttemptClosed(AttemptClosed {})]),
        Phase::Reserved
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
        | Phase::Quarantined { .. } => Err(AttemptRejection::NotTerminal),
        Phase::NotStarted => Err(AttemptRejection::NotStarted),
        Phase::Closed => Err(AttemptRejection::Closed),
    }
}

// ---- observations ----------------------------------------------------------------------------

/// An observation about another generation, from an older data-plane epoch, or answering a
/// directive that is not the pending one, is recorded as ignored; an answer moves the attempt;
/// an inventory fact is read against the phase.
fn decide_observation(
    state: &AttemptState,
    issued_at: Millis,
    observation: &Observation,
) -> Decision {
    let stale = |staleness| {
        Ok(vec![AttemptEvent::StaleObservationIgnored(
            StaleObservationIgnored {
                observation_id: observation.observation_id,
                staleness,
            },
        )])
    };
    let Some(identity) = &state.identity else {
        return Err(AttemptRejection::NotStarted);
    };
    if observation.generation != identity.generation {
        return stale(Staleness::Generation {
            observed: observation.generation,
        });
    }
    if observation.epoch < state.epoch {
        return stale(Staleness::Epoch {
            observed: observation.epoch,
        });
    }
    match observation.fact.directive_id() {
        Some(directive_id) => match state.pending {
            Some(pending) if pending.directive_id == directive_id => {
                decide_answer(state, pending, &observation.fact, issued_at)
            }
            Some(..) | None => stale(Staleness::Directive { directive_id }),
        },
        None => decide_inventory(state, &observation.fact),
    }
}

fn expect_tag(pending: Pending, tag: DirectiveTag) -> Result<(), AttemptRejection> {
    if pending.tag == tag {
        Ok(())
    } else {
        Err(AttemptRejection::ObservationMismatch)
    }
}

/// The answer to the pending directive. Only answers and inventory facts clear the pending
/// overlay, and an `Unknown` outcome only through them (I10; pinned by
/// `tests/attempt/lifecycle.rs::should_leave_unknown_only_on_an_observation_when_every_command_is_tried`).
fn decide_answer(
    state: &AttemptState,
    pending: Pending,
    fact: &Fact,
    issued_at: Millis,
) -> Decision {
    let directive_id = pending.directive_id;
    match fact {
        Fact::VmCloned { instance_id, .. } => {
            expect_tag(pending, DirectiveTag::CloneVm)?;
            Ok(vec![AttemptEvent::VmCloned(VmCloned {
                directive_id,
                instance_id: instance_id.clone(),
            })])
        }
        Fact::VmBooted { .. } => {
            expect_tag(pending, DirectiveTag::StartVm)?;
            Ok(vec![AttemptEvent::VmBooted(VmBooted { directive_id })])
        }
        Fact::GuestVerified { facts, .. } => {
            expect_tag(pending, DirectiveTag::VerifyGuest)?;
            decide_guest_verified(state, directive_id, *facts)
        }
        Fact::GuestRejected { reason, .. } => {
            expect_tag(pending, DirectiveTag::VerifyGuest)?;
            let mut events = vec![AttemptEvent::GuestRejected(GuestRejected {
                directive_id,
                reason: *reason,
            })];
            events.extend(quarantined(state, QuarantineReason::GuestIdentityRefused));
            Ok(events)
        }
        Fact::DirectiveRefused { reason, .. } => Ok(refused(state, pending, *reason)),
        Fact::OutcomeUnknown { .. } => match pending.outcome {
            Outcome::InFlight => Ok(vec![AttemptEvent::DirectiveOutcomeUnknown(
                DirectiveOutcomeUnknown {
                    directive_id,
                    since: issued_at,
                },
            )]),
            Outcome::Unknown { .. } => Ok(Vec::new()),
        },
        Fact::RunnerLaunched { .. } => {
            expect_tag(pending, DirectiveTag::LaunchRunner)?;
            match state.jit {
                Jit::Issued(handle_id) => Ok(vec![
                    AttemptEvent::JitHandleTaken(JitHandleTaken { handle_id }),
                    AttemptEvent::RunnerLaunched(RunnerLaunched { directive_id }),
                ]),
                // a restart orphaned the handle while the launch was in flight: the runner is
                // recorded, and never gets a job (`jit_presumed_leaked`)
                Jit::Orphaned(..) => Ok(vec![AttemptEvent::RunnerLaunched(RunnerLaunched {
                    directive_id,
                })]),
                Jit::None | Jit::Taken(..) => Err(AttemptRejection::ObservationMismatch),
            }
        }
        Fact::RunnerLaunchUnknown { .. } => {
            expect_tag(pending, DirectiveTag::LaunchRunner)?;
            Ok(vec![AttemptEvent::RunnerLaunchUnknown(
                RunnerLaunchUnknown { directive_id },
            )])
        }
        Fact::DiagnosticsExported { bytes, sha256, .. } => {
            expect_tag(pending, DirectiveTag::ExportDiagnostics)?;
            Ok(vec![AttemptEvent::DiagnosticsExported(
                DiagnosticsExported {
                    directive_id,
                    bytes: *bytes,
                    sha256: *sha256,
                },
            )])
        }
        Fact::DiagnosticsLost { reason, .. } => {
            expect_tag(pending, DirectiveTag::ExportDiagnostics)?;
            Ok(vec![AttemptEvent::DiagnosticsLost(DiagnosticsLost {
                directive_id,
                reason: *reason,
            })])
        }
        Fact::VmStopped { .. } => {
            expect_tag(pending, DirectiveTag::StopVm)?;
            Ok(vec![AttemptEvent::VmStopped(VmStopped { directive_id })])
        }
        Fact::VmDeleted { .. } => {
            expect_tag(pending, DirectiveTag::DeleteVm)?;
            Ok(vec![AttemptEvent::VmDeleted(VmDeleted { directive_id })])
        }
        Fact::RunnerExited { .. }
        | Fact::VmObservedStopped
        | Fact::VmPresent { .. }
        | Fact::VmAbsent => Err(AttemptRejection::ObservationMismatch),
    }
}

/// A verification while preparing prepares the attempt; one while activating is the fresh
/// verification the JIT handle needs (I22). The guest's facts are recorded, never read into an
/// ownership field (I30).
fn decide_guest_verified(
    state: &AttemptState,
    nonce_ref: DirectiveId,
    facts: GuestFacts,
) -> Decision {
    let verified = AttemptEvent::GuestVerified(GuestVerified { nonce_ref, facts });
    match state.phase {
        Phase::Verifying => Ok(vec![
            verified,
            AttemptEvent::AttemptPrepared(AttemptPrepared {}),
        ]),
        Phase::Activating => Ok(vec![verified]),
        Phase::Reserved
        | Phase::Cloning
        | Phase::Booting
        | Phase::Prepared
        | Phase::Listening
        | Phase::Busy
        | Phase::Finished
        | Phase::Retiring
        | Phase::Deleting
        | Phase::Deleted
        | Phase::Lost
        | Phase::Backoff { .. }
        | Phase::Quarantined { .. } => Err(AttemptRejection::ObservationMismatch),
        Phase::NotStarted => Err(AttemptRejection::NotStarted),
        Phase::Closed => Err(AttemptRejection::Closed),
    }
}

/// A refusal means nothing happened. A proof the data plane refused for a stop, a delete, a
/// launch or a start quarantines the attempt (ADR 0026, ADR 0038; a start's refusal is the
/// name it re-observed listed but unproven, or, from a data plane older than protocol 1.2, its
/// VM absent, ADR 0066); inside quarantine it is one more refused stop, so it cannot loop. A
/// stop's or start's target a complete listing shows absent (`target_absent`) is the VM this
/// attempt recorded, gone without our delete: the desync is recorded (`VmVanished`) and the
/// attempt closed, inside quarantine too, since quarantine holds only a VM that is present
/// (ADR 0066 as amended 2026-11-11, DEC-07). Any other refusal is a stage failure or a bounded
/// retry. Pinned by
/// `tests/attempt/lifecycle.rs::should_close_as_vanished_when_a_stop_or_start_finds_its_vm_absent`.
fn refused(state: &AttemptState, pending: Pending, reason: RefusalReason) -> Vec<AttemptEvent> {
    let refusal = AttemptEvent::DirectiveRefused(DirectiveRefused {
        directive_id: pending.directive_id,
        reason,
    });
    let proof_checked = match pending.tag {
        DirectiveTag::StopVm
        | DirectiveTag::DeleteVm
        | DirectiveTag::LaunchRunner
        | DirectiveTag::StartVm => true,
        DirectiveTag::CloneVm | DirectiveTag::VerifyGuest | DirectiveTag::ExportDiagnostics => {
            false
        }
    };
    let in_quarantine = match state.phase {
        Phase::Quarantined { .. } => true,
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
        | Phase::Closed => false,
    };
    // only a stop or a start re-observes a VM the attempt recorded cloning by its absence; the
    // data plane answers no other kind so, and one that did (or one with no recorded VM) is a
    // plain refusal
    let reads_absence = match pending.tag {
        DirectiveTag::StopVm => Some(VanishDiscovery::StopVm),
        DirectiveTag::StartVm => Some(VanishDiscovery::StartVm),
        DirectiveTag::CloneVm
        | DirectiveTag::DeleteVm
        | DirectiveTag::LaunchRunner
        | DirectiveTag::VerifyGuest
        | DirectiveTag::ExportDiagnostics => None,
    };
    let (proof_refused, vanished) = match reason {
        RefusalReason::ProofMismatch => (proof_checked && !in_quarantine, None),
        RefusalReason::TargetAbsent => (false, reads_absence),
        RefusalReason::InsufficientResources
        | RefusalReason::SafeMode
        | RefusalReason::StaleEpoch
        | RefusalReason::EffectFailed => (false, None),
    };
    if let Some(event) = vanished.and_then(|by| vanished_event(state, by)) {
        return vec![refusal, event];
    }
    if proof_refused {
        let mut events = vec![refusal];
        events.extend(quarantined(
            state,
            QuarantineReason::ProviderIdentityMismatch,
        ));
        return events;
    }
    let mut events = vec![refusal];
    events.extend(not_applied(state, pending));
    events
}

/// `VmVanished` for the VM this attempt recorded, found absent by `discovered_by`; `None` when
/// no VM was recorded (nothing of ours can vanish then).
fn vanished_event(state: &AttemptState, discovered_by: VanishDiscovery) -> Option<AttemptEvent> {
    state.instance_id.as_ref().map(|instance_id| {
        AttemptEvent::VmVanished(VmVanished {
            discovered_by,
            instance_id: instance_id.clone(),
            vm: condition(state),
        })
    })
}

/// What follows a directive that definitely did not take effect.
fn not_applied(state: &AttemptState, pending: Pending) -> Vec<AttemptEvent> {
    match pending.tag {
        DirectiveTag::CloneVm => stage_failed(state, Stage::Clone),
        DirectiveTag::StartVm => stage_failed(state, Stage::Boot),
        DirectiveTag::VerifyGuest => match state.phase {
            Phase::Activating => stage_failed(state, Stage::Activate),
            Phase::NotStarted
            | Phase::Reserved
            | Phase::Cloning
            | Phase::Booting
            | Phase::Verifying
            | Phase::Prepared
            | Phase::Listening
            | Phase::Busy
            | Phase::Finished
            | Phase::Retiring
            | Phase::Deleting
            | Phase::Deleted
            | Phase::Lost
            | Phase::Backoff { .. }
            | Phase::Quarantined { .. }
            | Phase::Closed => stage_failed(state, Stage::Verify),
        },
        DirectiveTag::LaunchRunner => vec![AttemptEvent::AttemptRetired(AttemptRetired {
            reason: RetireReason::LaunchRefused,
        })],
        DirectiveTag::ExportDiagnostics => vec![AttemptEvent::DiagnosticsLost(DiagnosticsLost {
            directive_id: pending.directive_id,
            reason: DiagnosticsLoss::Refused,
        })],
        DirectiveTag::StopVm | DirectiveTag::DeleteVm => cleanup_refused(state),
    }
}

/// A stop or delete that did not happen is retried within the budget; spent, cleanup ends in
/// quarantine (`delete_refused_repeatedly`), and inside quarantine the stops simply end.
fn cleanup_refused(state: &AttemptState) -> Vec<AttemptEvent> {
    match state.phase {
        Phase::Quarantined { .. } => Vec::new(),
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
        | Phase::Closed => {
            if within_budget(state) {
                Vec::new()
            } else {
                quarantined(state, QuarantineReason::DeleteRefusedRepeatedly)
            }
        }
    }
}

/// A stage that failed backs off with the same name and generation while its budget lasts,
/// then retires the attempt (U2). An attempt that holds a registration (a JIT was issued: the
/// activate stage) does not retire at once when its budget is spent: it backs off and requests
/// its retirement, which waits for the deregistration like a policy's request (I21, KER-04G;
/// pinned by
/// `tests/attempt/lifecycle.rs::should_request_the_retirement_when_the_activation_budget_is_spent_holding_a_registration`).
fn stage_failed(state: &AttemptState, stage: Stage) -> Vec<AttemptEvent> {
    let budget = state
        .identity
        .as_ref()
        .map_or(1, |identity| identity.retry_budget);
    let backed_off = AttemptEvent::AttemptBackedOff(AttemptBackedOff {
        stage,
        n: state.tries,
    });
    if state.tries < budget {
        return vec![backed_off];
    }
    match state.jit {
        Jit::None => vec![AttemptEvent::AttemptRetired(AttemptRetired {
            reason: RetireReason::StageFailed,
        })],
        Jit::Issued(..) | Jit::Taken(..) | Jit::Orphaned(..) => match state.retirement_requested {
            Some(..) => vec![backed_off],
            None => vec![
                backed_off,
                AttemptEvent::RetirementRequested(RetirementRequested {
                    reason: RetireReason::StageFailed,
                }),
            ],
        },
    }
}

fn decide_inventory(state: &AttemptState, fact: &Fact) -> Decision {
    match fact {
        Fact::RunnerExited { code } => decide_runner_exited(state, *code),
        Fact::VmObservedStopped => decide_vm_observed_stopped(state),
        Fact::VmPresent {
            instance_id,
            running,
        } => decide_vm_present(state, instance_id, *running),
        Fact::VmAbsent => decide_vm_absent(state),
        Fact::VmCloned { .. }
        | Fact::VmBooted { .. }
        | Fact::GuestVerified { .. }
        | Fact::GuestRejected { .. }
        | Fact::DirectiveRefused { .. }
        | Fact::OutcomeUnknown { .. }
        | Fact::RunnerLaunched { .. }
        | Fact::RunnerLaunchUnknown { .. }
        | Fact::DiagnosticsExported { .. }
        | Fact::DiagnosticsLost { .. }
        | Fact::VmStopped { .. }
        | Fact::VmDeleted { .. } => Err(AttemptRejection::ObservationMismatch),
    }
}

fn decide_runner_exited(state: &AttemptState, code: i32) -> Decision {
    let exited = || Ok(vec![AttemptEvent::RunnerExited(RunnerExited { code })]);
    match state.phase {
        Phase::Listening | Phase::Busy => exited(),
        Phase::Activating => match state.jit {
            Jit::Taken(..) => exited(),
            Jit::None | Jit::Issued(..) | Jit::Orphaned(..) => {
                Err(AttemptRejection::ObservationMismatch)
            }
        },
        Phase::Finished | Phase::Retiring | Phase::Deleting | Phase::Quarantined { .. } => {
            if state.runner_exited {
                Ok(Vec::new())
            } else {
                exited()
            }
        }
        Phase::Deleted | Phase::Lost => Ok(Vec::new()),
        Phase::Reserved
        | Phase::Cloning
        | Phase::Booting
        | Phase::Verifying
        | Phase::Prepared
        | Phase::Backoff { .. } => Err(AttemptRejection::ObservationMismatch),
        Phase::NotStarted => Err(AttemptRejection::NotStarted),
        Phase::Closed => Err(AttemptRejection::Closed),
    }
}

/// The VM stopped without our directive: a serving attempt is retired; with a directive
/// pending, the fact is only recorded and the answer decides.
fn decide_vm_observed_stopped(state: &AttemptState) -> Decision {
    let stopped = AttemptEvent::VmObservedStopped(VmObservedStopped {});
    let retire = |reason| {
        Ok(vec![
            stopped.clone(),
            AttemptEvent::AttemptRetired(AttemptRetired { reason }),
        ])
    };
    if state.pending.is_some() {
        return match state.phase {
            Phase::Deleted | Phase::Lost => Ok(Vec::new()),
            Phase::NotStarted => Err(AttemptRejection::NotStarted),
            Phase::Closed => Err(AttemptRejection::Closed),
            Phase::Reserved
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
            | Phase::Quarantined { .. } => Ok(vec![stopped]),
        };
    }
    match state.phase {
        Phase::Verifying | Phase::Prepared | Phase::Activating | Phase::Listening => {
            retire(RetireReason::VmStopped)
        }
        Phase::Busy => retire(RetireReason::GuestDied),
        Phase::Reserved
        | Phase::Cloning
        | Phase::Booting
        | Phase::Finished
        | Phase::Retiring
        | Phase::Deleting
        | Phase::Backoff { .. }
        | Phase::Quarantined { .. } => Ok(vec![stopped]),
        Phase::Deleted | Phase::Lost => Ok(Vec::new()),
        Phase::NotStarted => Err(AttemptRejection::NotStarted),
        Phase::Closed => Err(AttemptRejection::Closed),
    }
}

/// A complete inventory lists the VM. Another instance id than the recorded one is ownership
/// doubt (ADR 0026); an `Unknown` effect is resolved by what the listing shows (U1); a VM of
/// our exact name that we never recorded cloning is quarantined, never adopted or deleted.
fn decide_vm_present(
    state: &AttemptState,
    instance_id: &ProviderInstanceId,
    running: bool,
) -> Decision {
    // Quarantined is read like the others: an Unknown stop is resolved, and another VM under
    // our name is recorded as an identity mismatch (once, while nothing is pending)
    let already_mismatched = match state.phase {
        Phase::Deleted | Phase::Lost => return Ok(Vec::new()),
        Phase::NotStarted => return Err(AttemptRejection::NotStarted),
        Phase::Closed => return Err(AttemptRejection::Closed),
        Phase::Quarantined { reason, .. } => match reason {
            QuarantineReason::ProviderIdentityMismatch => state.pending.is_none(),
            // the case is open for review already; a VM that reappears is its evidence
            QuarantineReason::VmVanishedExternally => true,
            QuarantineReason::OwnershipDoubt
            | QuarantineReason::GuestIdentityRefused
            | QuarantineReason::JitPresumedLeaked
            | QuarantineReason::DeleteRefusedRepeatedly
            | QuarantineReason::RedactionViolation
            | QuarantineReason::ForensicRetention
            | QuarantineReason::OutcomeUnprovable => false,
        },
        Phase::Reserved
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
        | Phase::Backoff { .. } => false,
    };
    if let Some(recorded) = &state.instance_id
        && recorded != instance_id
    {
        return Ok(if already_mismatched {
            Vec::new()
        } else {
            quarantined(state, QuarantineReason::ProviderIdentityMismatch)
        });
    }
    match state.pending {
        Some(
            pending @ Pending {
                outcome: Outcome::Unknown { .. },
                ..
            },
        ) => Ok(resolve_present(state, pending, instance_id, running)),
        Some(Pending {
            outcome: Outcome::InFlight,
            ..
        }) => Ok(Vec::new()),
        None => {
            let before_clone = match state.phase {
                Phase::Reserved | Phase::Backoff { .. } => true,
                Phase::NotStarted
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
                | Phase::Closed => false,
            };
            let never_cloned = state.instance_id.is_none() && before_clone;
            if !never_cloned {
                return Ok(Vec::new());
            }
            let vm = if running {
                VmCondition::Running
            } else {
                VmCondition::Stopped
            };
            Ok(vec![AttemptEvent::AttemptQuarantined(AttemptQuarantined {
                reason: QuarantineReason::OwnershipDoubt,
                vm,
            })])
        }
    }
}

/// An `Unknown` effect resolved by a listing that shows the VM.
fn resolve_present(
    state: &AttemptState,
    pending: Pending,
    instance_id: &ProviderInstanceId,
    running: bool,
) -> Vec<AttemptEvent> {
    let directive_id = pending.directive_id;
    let not_applied_events = || {
        let mut events = vec![AttemptEvent::DirectiveObservedNotApplied(
            DirectiveObservedNotApplied { directive_id },
        )];
        events.extend(not_applied(state, pending));
        events
    };
    match pending.tag {
        DirectiveTag::CloneVm => vec![AttemptEvent::VmCloned(VmCloned {
            directive_id,
            instance_id: instance_id.clone(),
        })],
        DirectiveTag::StartVm => {
            if running {
                vec![AttemptEvent::VmBooted(VmBooted { directive_id })]
            } else {
                not_applied_events()
            }
        }
        DirectiveTag::StopVm => {
            if running {
                not_applied_events()
            } else {
                vec![AttemptEvent::VmStopped(VmStopped { directive_id })]
            }
        }
        DirectiveTag::DeleteVm => not_applied_events(),
        DirectiveTag::VerifyGuest
        | DirectiveTag::LaunchRunner
        | DirectiveTag::ExportDiagnostics => Vec::new(),
    }
}

/// A complete inventory does not list the VM: our delete took effect; an `Unknown` clone did
/// not; a VM this attempt recorded and did not delete vanished, quarantined or not, the same
/// desync a stop's or start's `target_absent` records (DEC-07; pinned by
/// `tests/attempt/lifecycle.rs::should_close_as_vanished_when_the_inventory_shows_a_recorded_vm_absent`);
/// a quarantined VM of our name we never recorded is released, lost (I25: an incomplete
/// inventory never arrives here).
fn decide_vm_absent(state: &AttemptState) -> Decision {
    match state.phase {
        Phase::Deleted | Phase::Lost => return Ok(Vec::new()),
        Phase::NotStarted => return Err(AttemptRejection::NotStarted),
        Phase::Closed => return Err(AttemptRejection::Closed),
        Phase::Reserved
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
        | Phase::Quarantined { .. } => {}
    }
    // a VM we recorded vanished; a quarantined VM of our name we never recorded cloning
    // (ownership doubt) is lost: its absence releases it, and it was never known to be ours
    let quarantined_now = match state.phase {
        Phase::Quarantined { .. } => true,
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
        | Phase::Closed => false,
    };
    let lost = || {
        if let Some(event) = vanished_event(state, VanishDiscovery::Inventory) {
            Ok(vec![event])
        } else if quarantined_now {
            Ok(vec![AttemptEvent::AttemptLost(AttemptLost {
                evidence: LostEvidence::InventoryAbsent,
            })])
        } else {
            Ok(Vec::new())
        }
    };
    match state.pending {
        Some(Pending {
            tag: DirectiveTag::DeleteVm,
            directive_id,
            ..
        }) => Ok(vec![AttemptEvent::VmDeleted(VmDeleted { directive_id })]),
        Some(
            pending @ Pending {
                tag: DirectiveTag::CloneVm,
                outcome: Outcome::Unknown { .. },
                ..
            },
        ) => {
            let mut events = vec![AttemptEvent::DirectiveObservedNotApplied(
                DirectiveObservedNotApplied {
                    directive_id: pending.directive_id,
                },
            )];
            events.extend(not_applied(state, pending));
            Ok(events)
        }
        Some(Pending {
            tag: DirectiveTag::CloneVm,
            outcome: Outcome::InFlight,
            ..
        }) => Ok(Vec::new()),
        Some(Pending {
            tag:
                DirectiveTag::StartVm
                | DirectiveTag::VerifyGuest
                | DirectiveTag::LaunchRunner
                | DirectiveTag::ExportDiagnostics
                | DirectiveTag::StopVm,
            ..
        })
        | None => lost(),
    }
}
