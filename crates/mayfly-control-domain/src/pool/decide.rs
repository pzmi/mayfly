//! `decide` and `evolve` for the pool: every mode against every command, as explicit match
//! arms with no wildcard (`ARCH-exhaustive-transition`); the outcome of every pair is pinned by
//! `tests/pool/matrix.rs::should_define_every_pair_when_each_mode_meets_each_command`.

use mayfly_domain::{AttemptId, Epoch, EventId, ImageDigest, Millis};

use super::events::{
    AttemptFailureCounted, ImagePinned, PoolBreakerReset, PoolBreakerTripped, PoolConfigured,
    PoolDecommissioned, PoolDrainCompleted, PoolDrainStarted, PoolEpochAdvanced, PoolEvent,
    PoolPaused, PoolResumed,
};
use super::state::{
    Breaker, ConfigurePool, Failure, MIN_PREPARED_DEFAULT, PoolCommand, PoolConfig, PoolMode,
    PoolRejection, PoolState, TripReason,
};
use crate::attempt::Stage;
use crate::command::{Actor, Command};

/// What `decide` returns.
pub type Decision = Result<Vec<PoolEvent>, PoolRejection>;

/// Decides `command` against `state`. An older run epoch is refused (I18); an accepted command
/// under a newer one records `PoolEpochAdvanced` ahead of its events.
///
/// # Errors
/// The [`PoolRejection`] that names why the command is not allowed.
pub fn decide(state: &PoolState, command: Command<PoolCommand>) -> Decision {
    let Command {
        run_epoch,
        actor,
        body,
        ..
    } = command;
    let events = match state.mode {
        PoolMode::Unconfigured => decide_unconfigured(body)?,
        PoolMode::Decommissioned => return Err(PoolRejection::Decommissioned),
        PoolMode::Active | PoolMode::Paused | PoolMode::Draining | PoolMode::Drained => {
            if run_epoch < state.epoch {
                return Err(PoolRejection::StaleRun);
            }
            decide_configured(state, actor, body)?
        }
    };
    Ok(with_epoch(state.epoch, run_epoch, events))
}

fn with_epoch(stored: Epoch, run_epoch: Epoch, events: Vec<PoolEvent>) -> Vec<PoolEvent> {
    if events.is_empty() || run_epoch <= stored {
        return events;
    }
    let mut all = Vec::with_capacity(events.len() + 1);
    all.push(PoolEvent::PoolEpochAdvanced(PoolEpochAdvanced {
        epoch: run_epoch,
    }));
    all.extend(events);
    all
}

fn decide_unconfigured(body: PoolCommand) -> Decision {
    match body {
        PoolCommand::ConfigurePool(c) => Ok(vec![configured(c)]),
        PoolCommand::PinImage { .. }
        | PoolCommand::PausePool
        | PoolCommand::ResumePool
        | PoolCommand::StartPoolDrain
        | PoolCommand::CompletePoolDrain
        | PoolCommand::CountAttemptFailure { .. }
        | PoolCommand::TripBreaker { .. }
        | PoolCommand::ResetBreaker { .. }
        | PoolCommand::Decommission => Err(PoolRejection::NotConfigured),
    }
}

/// `min_prepared` defaults to 0 (ADR 0028); a breaker threshold of 0 is read as 1.
fn configured(c: ConfigurePool) -> PoolEvent {
    PoolEvent::PoolConfigured(PoolConfigured {
        revision: c.revision,
        tier: c.tier,
        os: c.os,
        vm_resources: c.vm_resources,
        max_running: c.max_running,
        min_prepared: c.min_prepared.unwrap_or(MIN_PREPARED_DEFAULT),
        breaker_threshold: c.breaker_threshold.max(1),
        breaker_window: c.breaker_window,
    })
}

fn decide_configured(state: &PoolState, actor: Actor, body: PoolCommand) -> Decision {
    match body {
        PoolCommand::ConfigurePool(c) => match state.config {
            Some(config) if c.revision <= config.revision => Err(PoolRejection::StaleRevision),
            Some(_) | None => Ok(vec![configured(c)]),
        },
        PoolCommand::PinImage {
            digest,
            verified_ref,
        } => decide_pin(state, digest, verified_ref),
        PoolCommand::PausePool => match state.mode {
            PoolMode::Active => Ok(vec![PoolEvent::PoolPaused(PoolPaused {})]),
            PoolMode::Paused => Err(PoolRejection::AlreadyPaused),
            PoolMode::Draining | PoolMode::Drained => Err(PoolRejection::AlreadyDraining),
            PoolMode::Unconfigured => Err(PoolRejection::NotConfigured),
            PoolMode::Decommissioned => Err(PoolRejection::Decommissioned),
        },
        PoolCommand::ResumePool => match state.mode {
            PoolMode::Active => Err(PoolRejection::NotPaused),
            PoolMode::Paused | PoolMode::Draining | PoolMode::Drained => {
                Ok(vec![PoolEvent::PoolResumed(PoolResumed {})])
            }
            PoolMode::Unconfigured => Err(PoolRejection::NotConfigured),
            PoolMode::Decommissioned => Err(PoolRejection::Decommissioned),
        },
        PoolCommand::StartPoolDrain => match state.mode {
            PoolMode::Active | PoolMode::Paused => {
                Ok(vec![PoolEvent::PoolDrainStarted(PoolDrainStarted {})])
            }
            PoolMode::Draining | PoolMode::Drained => Err(PoolRejection::AlreadyDraining),
            PoolMode::Unconfigured => Err(PoolRejection::NotConfigured),
            PoolMode::Decommissioned => Err(PoolRejection::Decommissioned),
        },
        PoolCommand::CompletePoolDrain => match state.mode {
            PoolMode::Draining => Ok(vec![PoolEvent::PoolDrainCompleted(PoolDrainCompleted {})]),
            PoolMode::Active | PoolMode::Paused | PoolMode::Drained => {
                Err(PoolRejection::NotDraining)
            }
            PoolMode::Unconfigured => Err(PoolRejection::NotConfigured),
            PoolMode::Decommissioned => Err(PoolRejection::Decommissioned),
        },
        PoolCommand::CountAttemptFailure {
            attempt_id,
            digest,
            stage,
            at,
        } => Ok(decide_count_failure(state, attempt_id, digest, stage, at)),
        PoolCommand::TripBreaker { digest, reason, at } => {
            Ok(decide_trip(state, digest, reason, at))
        }
        PoolCommand::ResetBreaker { digest } => decide_reset(state, actor, digest),
        PoolCommand::Decommission => match state.mode {
            PoolMode::Drained => Ok(vec![PoolEvent::PoolDecommissioned(PoolDecommissioned {})]),
            PoolMode::Active | PoolMode::Paused | PoolMode::Draining => {
                Err(PoolRejection::NotDrained)
            }
            PoolMode::Unconfigured => Err(PoolRejection::NotConfigured),
            PoolMode::Decommissioned => Err(PoolRejection::Decommissioned),
        },
    }
}

/// A pin needs the image's verification, passed by the image policy as the event it read; the
/// same pin again records nothing.
fn decide_pin(state: &PoolState, digest: ImageDigest, verified_ref: Option<EventId>) -> Decision {
    let Some(verified_ref) = verified_ref else {
        return Err(PoolRejection::ImageNotVerified);
    };
    if state.image_pin.as_ref() == Some(&digest) {
        return Ok(vec![]);
    }
    Ok(vec![PoolEvent::ImagePinned(ImagePinned {
        digest,
        previous: state.image_pin.clone(),
        verified_ref,
    })])
}

/// Failures inside the window, oldest first, as of `at`.
fn in_window(breaker: &Breaker, window: Millis, at: Millis) -> usize {
    breaker
        .failures
        .iter()
        .filter(|f| at.get().saturating_sub(f.at.get()) < window.get())
        .count()
}

/// I27: a failure is counted once per attempt and only while the breaker is closed (so the
/// count is bounded by the threshold); the one that reaches the threshold inside the window
/// trips it. Pinned by `tests/pool/rules.rs::should_trip_at_the_threshold_and_count_nothing_more_when_failures_repeat`.
fn decide_count_failure(
    state: &PoolState,
    attempt_id: AttemptId,
    digest: ImageDigest,
    stage: Stage,
    at: Millis,
) -> Vec<PoolEvent> {
    let Some(config) = state.config else {
        return vec![];
    };
    let empty = Breaker::default();
    let breaker = state.breakers.get(&digest).unwrap_or(&empty);
    if breaker.tripped_at.is_some() || breaker.failures.iter().any(|f| f.attempt_id == attempt_id) {
        return vec![];
    }
    let failures = u32::try_from(in_window(breaker, config.breaker_window, at))
        .unwrap_or(u32::MAX)
        .saturating_add(1);
    let mut events = vec![PoolEvent::AttemptFailureCounted(AttemptFailureCounted {
        attempt_id,
        digest: digest.clone(),
        stage,
        at,
    })];
    if failures >= config.breaker_threshold {
        events.push(PoolEvent::PoolBreakerTripped(PoolBreakerTripped {
            digest,
            failures,
            at,
            reason: Some(TripReason::Failures),
        }));
    }
    events
}

/// P18: the breaker of `digest` trips for `reason` whatever its count (the failures in the
/// window are recorded as they stand); one already tripped records nothing, so a repeated
/// expiry is idempotent. **An operator's reset is final for that expiry**: the placement
/// manager issues the trip under a command id derived from the pool, the digest and the
/// expiry's instant (`mayfly_control::placement`), so once an operator reset the breaker, the
/// manager's next passes, which still read the expiry and plan the trip again, are answered by
/// the first trip's receipt and record nothing; only a newer expiry (another instant) trips it
/// again. Pinned by
/// `tests/pool/rules.rs::should_close_the_terms_when_the_runner_version_trips_the_breaker` and
/// `mayfly-control`'s
/// `tests/store/placement.rs::should_leave_the_breaker_reset_when_an_operator_resets_it_after_the_expiry`.
fn decide_trip(
    state: &PoolState,
    digest: ImageDigest,
    reason: TripReason,
    at: Millis,
) -> Vec<PoolEvent> {
    let empty = Breaker::default();
    let breaker = state.breakers.get(&digest).unwrap_or(&empty);
    if breaker.tripped_at.is_some() {
        return vec![];
    }
    let window = state.config.map_or(Millis::new(0), |c| c.breaker_window);
    let failures = u32::try_from(in_window(breaker, window, at)).unwrap_or(u32::MAX);
    vec![PoolEvent::PoolBreakerTripped(PoolBreakerTripped {
        digest,
        failures,
        at,
        reason: Some(reason),
    })]
}

fn decide_reset(state: &PoolState, actor: Actor, digest: ImageDigest) -> Decision {
    let uid = match actor {
        Actor::Operator { uid } => uid,
        Actor::Automation { .. } | Actor::Startup => return Err(PoolRejection::OperatorOnly),
    };
    let tripped = state
        .breakers
        .get(&digest)
        .is_some_and(|b| b.tripped_at.is_some());
    if !tripped {
        return Err(PoolRejection::NotTripped);
    }
    Ok(vec![PoolEvent::PoolBreakerReset(PoolBreakerReset {
        digest,
        operator_uid: uid,
    })])
}

/// Applies one recorded event. Every change that affects admission bumps the terms revision.
#[must_use]
pub fn evolve(mut state: PoolState, event: &PoolEvent) -> PoolState {
    match event {
        PoolEvent::PoolEpochAdvanced(e) => {
            state.epoch = e.epoch;
            return state;
        }
        PoolEvent::PoolConfigured(e) => {
            state.config = Some(PoolConfig {
                revision: e.revision,
                tier: e.tier,
                os: e.os,
                vm_resources: e.vm_resources,
                max_running: e.max_running,
                min_prepared: e.min_prepared,
                breaker_threshold: e.breaker_threshold,
                breaker_window: e.breaker_window,
            });
            state.mode = evolve_configured_mode(state.mode);
        }
        PoolEvent::ImagePinned(e) => state.image_pin = Some(e.digest.clone()),
        PoolEvent::PoolPaused(_) => state.mode = PoolMode::Paused,
        PoolEvent::PoolResumed(_) => state.mode = PoolMode::Active,
        PoolEvent::PoolDrainStarted(_) => state.mode = PoolMode::Draining,
        PoolEvent::PoolDrainCompleted(_) => state.mode = PoolMode::Drained,
        PoolEvent::PoolDecommissioned(_) => state.mode = PoolMode::Decommissioned,
        PoolEvent::AttemptFailureCounted(e) => {
            let window = state.config.map_or(Millis::new(0), |c| c.breaker_window);
            let breaker = state.breakers.entry(e.digest.clone()).or_default();
            breaker
                .failures
                .retain(|f| e.at.get().saturating_sub(f.at.get()) < window.get());
            breaker.failures.push(Failure {
                attempt_id: e.attempt_id,
                at: e.at,
            });
            return state;
        }
        PoolEvent::PoolBreakerTripped(e) => {
            state
                .breakers
                .entry(e.digest.clone())
                .or_default()
                .tripped_at = Some(e.at);
        }
        PoolEvent::PoolBreakerReset(e) => {
            state.breakers.remove(&e.digest);
        }
    }
    state.terms_revision = state.terms_revision.saturating_add(1);
    state
}

const fn evolve_configured_mode(mode: PoolMode) -> PoolMode {
    match mode {
        PoolMode::Unconfigured => PoolMode::Active,
        PoolMode::Active
        | PoolMode::Paused
        | PoolMode::Draining
        | PoolMode::Drained
        | PoolMode::Decommissioned => mode,
    }
}
