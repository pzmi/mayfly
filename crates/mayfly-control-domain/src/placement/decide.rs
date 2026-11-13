//! `decide` and `evolve` for the placement: every phase against every command, as explicit
//! match arms with no wildcard (`ARCH-exhaustive-transition`); the outcome of every pair is
//! pinned by `tests/placement/matrix.rs::should_define_every_pair_when_each_phase_meets_each_command`.

use mayfly_domain::{Deadline, DirectiveId, Epoch, HostId, ImageDigest, Millis};

use super::events::{
    BaseRefreshRequested, BaseRolledBack, EvictionRequested, ImageDriftDetected, ImageEvicted,
    ImageMaterialised, ImagePulled, PlacementDirectiveIssued, PlacementDirectiveNotApplied,
    PlacementDirectiveRefused, PlacementDirectiveSent, PlacementEpochAdvanced, PlacementEvent,
    PlacementFailed, PlacementOutcomeUnknown, PlacementRequested,
};
use super::state::{
    EvictionScope, Pending, PendingOutcome, PlacementCommand, PlacementDirective, PlacementFailure,
    PlacementPhase, PlacementRejection, PlacementState,
};
use crate::attempt::RefusalReason;
use crate::command::Command;
use crate::image::RunnerVersion;

/// What `decide` returns.
pub type Decision = Result<Vec<PlacementEvent>, PlacementRejection>;

/// Decides `command` against `state`. An older run epoch is refused (I18); an accepted command
/// under a newer one records `PlacementEpochAdvanced` ahead of its events.
///
/// # Errors
/// The [`PlacementRejection`] that names why the command is not allowed.
pub fn decide(state: &PlacementState, command: Command<PlacementCommand>) -> Decision {
    let Command {
        run_epoch,
        issued_at,
        body,
        ..
    } = command;
    if run_epoch < state.epoch {
        return Err(PlacementRejection::StaleRun);
    }
    let events = match body {
        PlacementCommand::RequestPlacement {
            host_id,
            digest,
            runner_version,
        } => decide_request(state, host_id, digest, runner_version)?,
        PlacementCommand::IssueDirective {
            directive_id,
            deadline,
        } => decide_issue(state, directive_id, deadline)?,
        PlacementCommand::RecordSent {
            directive_id,
            deadline,
        } => decide_sent(state, directive_id, deadline, run_epoch)?,
        PlacementCommand::RecordPulled { directive_id } => decide_pulled(state, directive_id)?,
        PlacementCommand::RecordDrift {
            directive_id,
            observed,
        } => decide_drift(state, directive_id, observed)?,
        PlacementCommand::RecordMaterialised {
            directive_id,
            disk_bytes,
        } => decide_materialised(state, directive_id, disk_bytes)?,
        PlacementCommand::RecordEvicted { directive_id } => decide_evicted(state, directive_id)?,
        PlacementCommand::RecordRefused {
            directive_id,
            reason,
        } => decide_refused(state, directive_id, reason)?,
        PlacementCommand::RecordOutcomeUnknown { directive_id } => {
            decide_unknown(state, directive_id, issued_at)?
        }
        PlacementCommand::RecordNotApplied { directive_id } => {
            decide_not_applied(state, directive_id)?
        }
        PlacementCommand::RequestRefresh { runner_version } => {
            decide_refresh(state, runner_version)?
        }
        PlacementCommand::RollBackBase => decide_rollback(state)?,
        PlacementCommand::RequestEviction { scope, references } => {
            decide_eviction(state, scope, references)?
        }
    };
    Ok(with_epoch(state.epoch, run_epoch, events))
}

fn with_epoch(stored: Epoch, run_epoch: Epoch, events: Vec<PlacementEvent>) -> Vec<PlacementEvent> {
    if events.is_empty() || run_epoch <= stored {
        return events;
    }
    let mut all = Vec::with_capacity(events.len() + 1);
    all.push(PlacementEvent::PlacementEpochAdvanced(
        PlacementEpochAdvanced { epoch: run_epoch },
    ));
    all.extend(events);
    all
}

/// The stream is one host and one digest: a request for another is refused. A request while
/// the placement is under way records nothing, so a second pin of the digest never starts a
/// second pull; after an eviction or a failure it starts again.
fn decide_request(
    state: &PlacementState,
    host_id: HostId,
    digest: ImageDigest,
    runner_version: RunnerVersion,
) -> Decision {
    let other_host = state.host_id.as_ref().is_some_and(|h| *h != host_id);
    let other_digest = state.digest.as_ref().is_some_and(|d| *d != digest);
    if other_host || other_digest {
        return Err(PlacementRejection::PlacementDiffers);
    }
    match state.phase {
        PlacementPhase::Unrequested | PlacementPhase::Evicted | PlacementPhase::Failed => {
            Ok(vec![PlacementEvent::PlacementRequested(
                PlacementRequested {
                    host_id,
                    digest,
                    runner_version,
                },
            )])
        }
        PlacementPhase::Wanted
        | PlacementPhase::Pulling
        | PlacementPhase::Pulled
        | PlacementPhase::Materialised
        | PlacementPhase::Evicting => Ok(vec![]),
    }
}

/// The directive due in `state`, if any.
fn due(state: &PlacementState) -> Result<PlacementDirective, PlacementRejection> {
    match state.phase {
        PlacementPhase::Unrequested => Err(PlacementRejection::NotRequested),
        PlacementPhase::Wanted => Ok(PlacementDirective::PullImage),
        PlacementPhase::Pulled | PlacementPhase::Materialised => match state.wanted {
            Some(runner_version) if state.base != Some(runner_version) => {
                Ok(PlacementDirective::MaterialiseImage { runner_version })
            }
            Some(_) | None => Err(PlacementRejection::NothingToIssue),
        },
        PlacementPhase::Evicting => {
            let whole = state.eviction == Some(EvictionScope::Placement);
            let target = state.previous.or(if whole { state.base } else { None });
            target
                .map(|runner_version| PlacementDirective::EvictImage { runner_version })
                .ok_or(PlacementRejection::NothingToIssue)
        }
        PlacementPhase::Pulling | PlacementPhase::Evicted | PlacementPhase::Failed => {
            Err(PlacementRejection::NothingToIssue)
        }
    }
}

/// One directive at a time: while one is pending, in flight or unknown, nothing else is issued,
/// so a host pulls a digest at most once at a time. Pinned by
/// `tests/placement/rules.rs::should_pull_once_when_a_pull_is_pending_or_unknown`.
fn decide_issue(state: &PlacementState, directive_id: DirectiveId, deadline: Deadline) -> Decision {
    if state.phase == PlacementPhase::Unrequested {
        return Err(PlacementRejection::NotRequested);
    }
    if state.pending.is_some() {
        return Err(PlacementRejection::DirectivePending);
    }
    let directive = due(state)?;
    Ok(vec![PlacementEvent::PlacementDirectiveIssued(
        PlacementDirectiveIssued {
            directive_id,
            directive,
            deadline,
        },
    )])
}

/// The pending directive an answer names.
fn pending_for(
    state: &PlacementState,
    directive_id: DirectiveId,
) -> Result<Pending, PlacementRejection> {
    if state.phase == PlacementPhase::Unrequested {
        return Err(PlacementRejection::NotRequested);
    }
    state
        .pending
        .filter(|p| p.directive_id == directive_id)
        .ok_or(PlacementRejection::NotPending)
}

/// The relay sent the pending directive: recorded once, while its outcome is in flight, with
/// the run epoch of the session that carried it (the command's).
fn decide_sent(
    state: &PlacementState,
    directive_id: DirectiveId,
    deadline: Deadline,
    epoch: Epoch,
) -> Decision {
    let pending = pending_for(state, directive_id)?;
    match pending.outcome {
        PendingOutcome::InFlight if !pending.sent => {
            Ok(vec![PlacementEvent::PlacementDirectiveSent(
                PlacementDirectiveSent {
                    directive_id,
                    deadline,
                    epoch: Some(epoch),
                },
            )])
        }
        PendingOutcome::InFlight | PendingOutcome::Unknown { .. } => Ok(vec![]),
    }
}

fn pending_pull(
    state: &PlacementState,
    directive_id: DirectiveId,
) -> Result<(), PlacementRejection> {
    match pending_for(state, directive_id)?.directive {
        PlacementDirective::PullImage => Ok(()),
        PlacementDirective::MaterialiseImage { .. } | PlacementDirective::EvictImage { .. } => {
            Err(PlacementRejection::NotPending)
        }
    }
}

fn decide_pulled(state: &PlacementState, directive_id: DirectiveId) -> Decision {
    pending_pull(state, directive_id)?;
    Ok(vec![PlacementEvent::ImagePulled(ImagePulled {
        directive_id,
    })])
}

fn decide_drift(
    state: &PlacementState,
    directive_id: DirectiveId,
    observed: ImageDigest,
) -> Decision {
    pending_pull(state, directive_id)?;
    let Some(expected) = state.digest.clone() else {
        return Err(PlacementRejection::NotRequested);
    };
    if observed == expected {
        return Err(PlacementRejection::NoDrift);
    }
    Ok(vec![
        PlacementEvent::ImageDriftDetected(ImageDriftDetected {
            directive_id,
            expected,
            observed,
        }),
        PlacementEvent::PlacementFailed(PlacementFailed {}),
    ])
}

fn decide_materialised(
    state: &PlacementState,
    directive_id: DirectiveId,
    disk_bytes: Option<u64>,
) -> Decision {
    match pending_for(state, directive_id)?.directive {
        PlacementDirective::MaterialiseImage { runner_version } => {
            Ok(vec![PlacementEvent::ImageMaterialised(ImageMaterialised {
                directive_id,
                runner_version,
                disk_bytes,
            })])
        }
        PlacementDirective::PullImage | PlacementDirective::EvictImage { .. } => {
            Err(PlacementRejection::NotPending)
        }
    }
}

fn decide_evicted(state: &PlacementState, directive_id: DirectiveId) -> Decision {
    match pending_for(state, directive_id)?.directive {
        PlacementDirective::EvictImage { runner_version } => {
            Ok(vec![PlacementEvent::ImageEvicted(ImageEvicted {
                directive_id,
                runner_version,
            })])
        }
        PlacementDirective::PullImage | PlacementDirective::MaterialiseImage { .. } => {
            Err(PlacementRejection::NotPending)
        }
    }
}

/// A refused pull or first materialisation fails the placement; a refused refresh or eviction
/// leaves the materialised base as it was.
fn decide_refused(
    state: &PlacementState,
    directive_id: DirectiveId,
    reason: RefusalReason,
) -> Decision {
    let pending = pending_for(state, directive_id)?;
    let fails = match pending.directive {
        PlacementDirective::PullImage => true,
        PlacementDirective::MaterialiseImage { .. } => state.base.is_none(),
        PlacementDirective::EvictImage { .. } => false,
    };
    let mut events = vec![PlacementEvent::PlacementDirectiveRefused(
        PlacementDirectiveRefused {
            directive_id,
            reason,
        },
    )];
    if fails {
        events.push(PlacementEvent::PlacementFailed(PlacementFailed {}));
    }
    Ok(events)
}

/// I10: only a sent directive whose deadline passed has an unknown outcome (one held unsent did
/// nothing, ADR 0025), and only an observation (an answer or a complete listing) makes it known
/// again.
fn decide_unknown(state: &PlacementState, directive_id: DirectiveId, now: Millis) -> Decision {
    let pending = pending_for(state, directive_id)?;
    match pending.outcome {
        PendingOutcome::Unknown { .. } => Ok(vec![]),
        PendingOutcome::InFlight if !pending.sent => Err(PlacementRejection::NotSent),
        PendingOutcome::InFlight if !pending.deadline.is_expired(now) => {
            Err(PlacementRejection::DeadlineNotPassed)
        }
        PendingOutcome::InFlight => Ok(vec![PlacementEvent::PlacementOutcomeUnknown(
            PlacementOutcomeUnknown {
                directive_id,
                since: now,
            },
        )]),
    }
}

fn decide_not_applied(state: &PlacementState, directive_id: DirectiveId) -> Decision {
    match pending_for(state, directive_id)?.outcome {
        PendingOutcome::Unknown { .. } => Ok(vec![PlacementEvent::PlacementDirectiveNotApplied(
            PlacementDirectiveNotApplied { directive_id },
        )]),
        PendingOutcome::InFlight => Err(PlacementRejection::OutcomeNotUnknown),
    }
}

/// Refresh, rollback and eviction start only from a materialised base with nothing pending.
fn materialised_idle(state: &PlacementState) -> Result<(), PlacementRejection> {
    match state.phase {
        PlacementPhase::Unrequested => Err(PlacementRejection::NotRequested),
        PlacementPhase::Materialised if state.pending.is_some() => {
            Err(PlacementRejection::DirectivePending)
        }
        PlacementPhase::Materialised => Ok(()),
        PlacementPhase::Wanted
        | PlacementPhase::Pulling
        | PlacementPhase::Pulled
        | PlacementPhase::Evicting
        | PlacementPhase::Evicted
        | PlacementPhase::Failed => Err(PlacementRejection::NotMaterialised),
    }
}

/// A placement holds at most two bases, the pointed-at one and the previous one (ADR 0039: "one
/// base per pinned digest plus the previous one"): a refresh while the previous is kept is
/// refused until it is evicted.
fn decide_refresh(state: &PlacementState, runner_version: RunnerVersion) -> Decision {
    materialised_idle(state)?;
    if state.base == Some(runner_version) || state.wanted == Some(runner_version) {
        return Ok(vec![]);
    }
    if state.previous.is_some() {
        return Err(PlacementRejection::PreviousBaseHeld);
    }
    Ok(vec![PlacementEvent::BaseRefreshRequested(
        BaseRefreshRequested { runner_version },
    )])
}

/// The base pointer moves back to the previous base, and the base it leaves becomes the
/// previous one. Pinned by
/// `tests/placement/rules.rs::should_point_back_at_the_previous_base_when_rolled_back`.
fn decide_rollback(state: &PlacementState) -> Decision {
    materialised_idle(state)?;
    let (Some(to), Some(from)) = (state.previous, state.base) else {
        return Err(PlacementRejection::NoPreviousBase);
    };
    Ok(vec![PlacementEvent::BaseRolledBack(BaseRolledBack {
        to,
        from,
    })])
}

/// P17: eviction only from `Materialised`, with nothing pending and zero references (the count
/// is the policy's; the data plane re-checks it against its base set). Pinned by
/// `tests/placement/rules.rs::should_refuse_eviction_when_the_base_is_referenced`.
fn decide_eviction(state: &PlacementState, scope: EvictionScope, references: u32) -> Decision {
    if state.phase == PlacementPhase::Unrequested {
        return Err(PlacementRejection::NotRequested);
    }
    let evicting = state.phase == PlacementPhase::Evicting && state.eviction == Some(scope);
    if references > 0 && (evicting || state.phase == PlacementPhase::Materialised) {
        return Err(PlacementRejection::BaseReferenced);
    }
    if evicting {
        return Ok(vec![]);
    }
    materialised_idle(state)?;
    match scope {
        EvictionScope::Previous if state.previous.is_none() => {
            Err(PlacementRejection::NoPreviousBase)
        }
        EvictionScope::Previous | EvictionScope::Placement => {
            Ok(vec![PlacementEvent::EvictionRequested(EvictionRequested {
                scope,
            })])
        }
    }
}

/// Applies one recorded event.
#[must_use]
pub fn evolve(mut state: PlacementState, event: &PlacementEvent) -> PlacementState {
    match event {
        PlacementEvent::PlacementEpochAdvanced(e) => state.epoch = e.epoch,
        PlacementEvent::PlacementRequested(e) => {
            state.phase = PlacementPhase::Wanted;
            state.host_id = Some(e.host_id.clone());
            state.digest = Some(e.digest.clone());
            state.wanted = Some(e.runner_version);
            state.failure = None;
        }
        PlacementEvent::PlacementDirectiveIssued(e) => {
            state.pending = Some(Pending {
                directive_id: e.directive_id,
                directive: e.directive,
                deadline: e.deadline,
                sent: false,
                sent_epoch: None,
                outcome: PendingOutcome::InFlight,
            });
            state.phase = evolve_issued_phase(state.phase, e.directive);
        }
        PlacementEvent::PlacementDirectiveSent(e) => {
            if let Some(pending) = state.pending.as_mut() {
                pending.sent = true;
                pending.deadline = e.deadline;
                pending.sent_epoch = e.epoch;
            }
        }
        PlacementEvent::PlacementOutcomeUnknown(e) => {
            if let Some(pending) = state.pending.as_mut() {
                pending.outcome = PendingOutcome::Unknown { since: e.since };
            }
        }
        PlacementEvent::PlacementDirectiveNotApplied(_) => {
            state.pending = None;
            state.phase = evolve_not_applied_phase(state.phase);
        }
        PlacementEvent::PlacementDirectiveRefused(e) => {
            state.pending = None;
            state = evolve_refused(state, e.reason);
        }
        PlacementEvent::ImagePulled(_) => {
            state.pending = None;
            state.phase = PlacementPhase::Pulled;
        }
        PlacementEvent::ImageDriftDetected(_) => {
            state.pending = None;
            state.failure = Some(PlacementFailure::Drift);
        }
        PlacementEvent::PlacementFailed(_) => state.phase = PlacementPhase::Failed,
        PlacementEvent::ImageMaterialised(e) => {
            state.pending = None;
            if state.base != Some(e.runner_version) {
                state.previous = state.base;
            }
            state.base = Some(e.runner_version);
            state.wanted = Some(e.runner_version);
            state.phase = PlacementPhase::Materialised;
            if e.disk_bytes.is_some() {
                state.disk_bytes = e.disk_bytes;
            }
        }
        PlacementEvent::BaseRefreshRequested(e) => state.wanted = Some(e.runner_version),
        PlacementEvent::BaseRolledBack(e) => {
            state.base = Some(e.to);
            state.previous = Some(e.from);
            state.wanted = Some(e.to);
        }
        PlacementEvent::EvictionRequested(e) => {
            state.phase = PlacementPhase::Evicting;
            state.eviction = Some(e.scope);
            state.wanted = state.base;
        }
        PlacementEvent::ImageEvicted(e) => {
            state.pending = None;
            state = evolve_evicted(state, e.runner_version);
        }
    }
    state
}

const fn evolve_issued_phase(
    phase: PlacementPhase,
    directive: PlacementDirective,
) -> PlacementPhase {
    match directive {
        PlacementDirective::PullImage => PlacementPhase::Pulling,
        PlacementDirective::MaterialiseImage { .. } | PlacementDirective::EvictImage { .. } => {
            phase
        }
    }
}

const fn evolve_not_applied_phase(phase: PlacementPhase) -> PlacementPhase {
    match phase {
        PlacementPhase::Pulling => PlacementPhase::Wanted,
        PlacementPhase::Unrequested
        | PlacementPhase::Wanted
        | PlacementPhase::Pulled
        | PlacementPhase::Materialised
        | PlacementPhase::Evicting
        | PlacementPhase::Evicted
        | PlacementPhase::Failed => phase,
    }
}

/// A refused pull or first materialisation records its reason (`PlacementFailed` follows); a
/// refused refresh keeps the base pointer; a refused eviction returns to `Materialised`.
fn evolve_refused(mut state: PlacementState, reason: RefusalReason) -> PlacementState {
    match state.phase {
        PlacementPhase::Pulling | PlacementPhase::Pulled => {
            state.failure = Some(PlacementFailure::Refused(reason));
        }
        PlacementPhase::Materialised => state.wanted = state.base,
        PlacementPhase::Evicting => {
            state.phase = PlacementPhase::Materialised;
            state.eviction = None;
        }
        PlacementPhase::Unrequested
        | PlacementPhase::Wanted
        | PlacementPhase::Evicted
        | PlacementPhase::Failed => {}
    }
    state
}

/// Removes the evicted base; with none left the placement is `Evicted`, and an eviction of the
/// previous base alone returns to `Materialised`.
fn evolve_evicted(mut state: PlacementState, runner_version: RunnerVersion) -> PlacementState {
    if state.previous == Some(runner_version) {
        state.previous = None;
    } else if state.base == Some(runner_version) {
        state.base = None;
        state.wanted = None;
    }
    if state.base.is_none() && state.previous.is_none() {
        state.phase = PlacementPhase::Evicted;
        state.eviction = None;
    } else if state.eviction == Some(EvictionScope::Previous) {
        state.phase = PlacementPhase::Materialised;
        state.eviction = None;
    }
    state
}
