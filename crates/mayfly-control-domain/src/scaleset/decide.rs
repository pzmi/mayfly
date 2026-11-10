//! `decide` and `evolve` for the scale set: every phase against every command, as explicit match
//! arms with no wildcard (`ARCH-exhaustive-transition`); the outcome of every pair is pinned by
//! `tests/scaleset/matrix.rs::should_define_every_pair_when_each_phase_meets_each_command`.

use std::collections::BTreeSet;

use mayfly_domain::Epoch;

use super::events::{
    AcquireObserved, CapacityApplied, ClosingStarted, MessageRecorded, ScaleSetEnsured,
    ScaleSetEpochAdvanced, ScaleSetEvent, SessionClosed, SessionConflictEscalated, SessionDegraded,
    SessionDeleted, SessionLost, SessionOpenRequested, SessionOpened, SessionRecovered,
};
use super::state::{
    DegradedCause, EnsureScaleSet, JobFact, LossCause, MessageId, MessageItem, MessageOrigin,
    PROTOCOL_DRIFT_THRESHOLD, Phase, RecordMessage, SESSION_CONFLICT_ESCALATION, ScaleSetCommand,
    ScaleSetRejection, ScaleSetState, SessionId,
};
use crate::attempt::RunnerRequestId;
use crate::command::Command;
use crate::demand::{self, Budget, Capacity, Declined, OfferDecision, Statistics};

/// What `decide` returns.
pub type Decision = Result<Vec<ScaleSetEvent>, ScaleSetRejection>;

/// Decides `command` against `state`. An older run epoch is refused (I18); an accepted command
/// under a newer one records `ScaleSetEpochAdvanced` ahead of its events.
///
/// # Errors
/// The [`ScaleSetRejection`] that names why the command is not allowed.
pub fn decide(state: &ScaleSetState, command: Command<ScaleSetCommand>) -> Decision {
    let Command {
        run_epoch, body, ..
    } = command;
    if run_epoch < state.epoch {
        return Err(ScaleSetRejection::StaleRun);
    }
    let events = decide_body(state, body)?;
    Ok(with_epoch(state.epoch, run_epoch, events))
}

fn with_epoch(stored: Epoch, run_epoch: Epoch, events: Vec<ScaleSetEvent>) -> Vec<ScaleSetEvent> {
    if events.is_empty() || run_epoch <= stored {
        return events;
    }
    let mut all = Vec::with_capacity(events.len() + 1);
    all.push(ScaleSetEvent::ScaleSetEpochAdvanced(
        ScaleSetEpochAdvanced { epoch: run_epoch },
    ));
    all.extend(events);
    all
}

fn decide_body(state: &ScaleSetState, body: ScaleSetCommand) -> Decision {
    match body {
        ScaleSetCommand::EnsureScaleSet(ensure) => decide_ensure(state, ensure),
        ScaleSetCommand::BeginOpen => decide_begin_open(state),
        ScaleSetCommand::RecordSessionOpened {
            session_id,
            statistics,
        } => decide_opened(state, session_id, statistics),
        ScaleSetCommand::RecordSessionLoss { cause } => decide_loss(state, cause),
        ScaleSetCommand::RecordSessionDeleted { session_id } => decide_deleted(state, session_id),
        ScaleSetCommand::RecordMessage(message) => decide_message(state, message),
        ScaleSetCommand::RecordAcquireResult {
            requested,
            acquired,
        } => decide_acquire_result(state, &requested, &acquired),
        ScaleSetCommand::ApplyCapacity(capacity) => decide_capacity(state, capacity),
        ScaleSetCommand::RecordSessionRecovered => decide_recovered(state),
        ScaleSetCommand::StartClosing => decide_start_closing(state),
    }
}

/// One scale set per pool (ADR 0040): ensured once; the same scale set again records nothing,
/// another pool, id or tier is refused.
fn decide_ensure(state: &ScaleSetState, ensure: EnsureScaleSet) -> Decision {
    match state.phase {
        Phase::Unconfigured => Ok(vec![ScaleSetEvent::ScaleSetEnsured(ScaleSetEnsured {
            pool: ensure.pool,
            github_id: ensure.github_id,
            tier: ensure.tier,
        })]),
        Phase::Ensured
        | Phase::Opening
        | Phase::Active
        | Phase::Backoff(_)
        | Phase::Degraded(_)
        | Phase::Closing
        | Phase::Closed => {
            let same = state.pool.as_ref() == Some(&ensure.pool)
                && state.github_id == Some(ensure.github_id)
                && state.tier == Some(ensure.tier);
            if same {
                Ok(vec![])
            } else {
                Err(ScaleSetRejection::ScaleSetMismatch)
            }
        }
    }
}

/// A create is sent only once the journaled stale session is deleted (§3.2, A3: mayfly never
/// steals a session; pinned by
/// `tests/scaleset/rules.rs::should_refuse_a_create_when_a_journaled_session_is_not_deleted`).
fn decide_begin_open(state: &ScaleSetState) -> Decision {
    match state.phase {
        Phase::Unconfigured => Err(ScaleSetRejection::NotEnsured),
        Phase::Ensured | Phase::Backoff(_) | Phase::Closed => {
            if state.stale_session.is_some() {
                Err(ScaleSetRejection::StaleSessionPending)
            } else {
                Ok(vec![ScaleSetEvent::SessionOpenRequested(
                    SessionOpenRequested {},
                )])
            }
        }
        Phase::Opening => Ok(vec![]),
        Phase::Active | Phase::Degraded(_) => Err(ScaleSetRejection::SessionLive),
        Phase::Closing => Err(ScaleSetRejection::Closing),
    }
}

/// The initial statistics are authoritative: desired is recomputed from them without any
/// message (plan §7.2 step 5), and they heal a crash between `AcquireJobs` and its record.
/// The cursor is kept (A1). Pinned by
/// `tests/scaleset/rules.rs::should_recompute_desired_from_initial_statistics_when_a_session_opens`.
fn decide_opened(state: &ScaleSetState, session_id: SessionId, statistics: Statistics) -> Decision {
    match state.phase {
        Phase::Opening => Ok(vec![ScaleSetEvent::SessionOpened(SessionOpened {
            session_id,
            statistics,
            desired: demand::desired(&statistics, state.capacity.deliverable),
        })]),
        Phase::Active | Phase::Degraded(_) | Phase::Closing => {
            if state.session == Some(session_id) {
                Ok(vec![])
            } else {
                Err(ScaleSetRejection::SessionLive)
            }
        }
        Phase::Unconfigured => Err(ScaleSetRejection::NotEnsured),
        Phase::Ensured | Phase::Backoff(_) | Phase::Closed => Err(ScaleSetRejection::NotOpening),
    }
}

/// A lost session is journaled as stale, to be deleted before the next create; its open
/// reservations stay until an observation names them (A4). The
/// [`SESSION_CONFLICT_ESCALATION`]th create in a row answered 409 is escalated, once, and the
/// creates go on (A3; pinned by
/// `tests/scaleset/rules.rs::should_escalate_once_when_creates_keep_answering_conflict`).
fn decide_loss(state: &ScaleSetState, cause: LossCause) -> Decision {
    match state.phase {
        Phase::Opening => {
            let mut events = vec![ScaleSetEvent::SessionLost(SessionLost {
                cause,
                stale_session: None,
            })];
            let conflicts = state.open_conflicts.saturating_add(1);
            if cause == LossCause::Conflicted && conflicts == SESSION_CONFLICT_ESCALATION {
                events.push(ScaleSetEvent::SessionConflictEscalated(
                    SessionConflictEscalated { conflicts },
                ));
            }
            Ok(events)
        }
        Phase::Active | Phase::Degraded(_) | Phase::Closing => {
            Ok(vec![ScaleSetEvent::SessionLost(SessionLost {
                cause,
                stale_session: state.session,
            })])
        }
        Phase::Unconfigured => Err(ScaleSetRejection::NotEnsured),
        Phase::Ensured | Phase::Backoff(_) | Phase::Closed => Err(ScaleSetRejection::NoSession),
    }
}

/// Only a session mayfly journaled is deleted: the stale one, or the live one it is closing (A3,
/// pinned by `tests/scaleset/rules.rs::should_refuse_to_delete_a_session_when_mayfly_holds_no_record_of_it`).
fn decide_deleted(state: &ScaleSetState, session_id: SessionId) -> Decision {
    match state.phase {
        Phase::Unconfigured => return Err(ScaleSetRejection::NotEnsured),
        Phase::Closing => {
            if state.session == Some(session_id) {
                return Ok(vec![ScaleSetEvent::SessionClosed(SessionClosed {
                    session_id,
                })]);
            }
        }
        Phase::Ensured
        | Phase::Opening
        | Phase::Active
        | Phase::Backoff(_)
        | Phase::Degraded(_)
        | Phase::Closed => {}
    }
    if state.stale_session == Some(session_id) {
        Ok(vec![ScaleSetEvent::SessionDeleted(SessionDeleted {
            session_id,
        })])
    } else {
        Err(ScaleSetRejection::UnknownSession)
    }
}

/// What a batch does, folded in arrival order (`formal/scaleset.qnt`'s `itemStep`).
struct Batch {
    reserved: BTreeSet<RunnerRequestId>,
    released: Vec<RunnerRequestId>,
    acquire: Vec<RunnerRequestId>,
    declined: Vec<Declined>,
    job_facts: Vec<JobFact>,
    unknown_types: u32,
}

/// Where `message_id` stands against the session's floor (A1): above it, a message the stream
/// has not recorded; the floor itself as the session's first record, the stream's last record
/// redelivered after a lost ack; anything else at or below it, a message A1 does not explain
/// (ids restarted or went back). Pinned by
/// `tests/scaleset/rules.rs::should_apply_nothing_when_a_message_from_an_earlier_session_is_redelivered`
/// and `should_degrade_the_session_when_a_new_session_delivers_an_id_below_the_cursor`.
fn origin(state: &ScaleSetState, message_id: MessageId) -> MessageOrigin {
    match state.session_floor {
        Some(floor) if message_id <= floor => {
            if message_id == floor && !state.session_recorded {
                MessageOrigin::Redelivered
            } else {
                MessageOrigin::IdRegressed
            }
        }
        Some(_) | None => MessageOrigin::Fresh,
    }
}

/// `RecordMessage` (plan §7.2 step 3), in one unit. A message from before the session
/// ([`origin`] not `Fresh`) is recorded so the listener can ack it and applies nothing: no
/// statistics, no plan, no release, no unknown types (its effects were journaled when it was
/// first recorded); one A1 does not explain degrades the session, never silently. A fresh
/// message applies its statistics only when it is newer than the latest statistics (A1: a newer
/// message's, or a session's opening; a message emitted before the open but recorded after it
/// cannot be told apart and applies, A6); each offer is planned by `PlanAcquire` against the
/// budget left by the items before it, only while the session is active (a degraded or closing
/// session declines it as held, P21); `JobStarted` and `JobCompleted` release the job's
/// reservation; unknown types are counted, and the session degraded once they reach the
/// threshold (never safe mode, R7). The listener acks only after this record commits (I12) and
/// acquires exactly the event's `acquire` list.
fn decide_message(state: &ScaleSetState, message: RecordMessage) -> Decision {
    let degradable = match state.phase {
        Phase::Unconfigured => return Err(ScaleSetRejection::NotEnsured),
        Phase::Active | Phase::Degraded(_) => true,
        Phase::Closing => false,
        Phase::Ensured | Phase::Opening | Phase::Backoff(_) | Phase::Closed => {
            return Err(ScaleSetRejection::NoSession);
        }
    };
    if state.session != Some(message.session_id) {
        return Err(ScaleSetRejection::SessionMismatch);
    }
    let Some(tier) = state.tier else {
        return Err(ScaleSetRejection::NotEnsured);
    };
    let origin = origin(state, message.message_id);
    if origin != MessageOrigin::Fresh {
        let mut events = vec![ScaleSetEvent::MessageRecorded(MessageRecorded {
            session_id: message.session_id,
            message_id: message.message_id,
            origin,
            statistics: message.statistics,
            statistics_applied: false,
            released: Vec::new(),
            acquire: Vec::new(),
            declined: Vec::new(),
            job_facts: Vec::new(),
            unknown_types: 0,
            desired: state.desired,
        })];
        let already = state.phase == Phase::Degraded(DegradedCause::MessageIdsRegressed);
        if origin == MessageOrigin::IdRegressed && degradable && !already {
            events.push(ScaleSetEvent::SessionDegraded(SessionDegraded {
                cause: DegradedCause::MessageIdsRegressed,
            }));
        }
        return Ok(events);
    }
    let newer = state
        .statistics_after
        .is_none_or(|after| message.message_id > after);
    let statistics_applied = newer && message.statistics.is_some();
    let latest = if statistics_applied {
        message.statistics
    } else {
        state.statistics
    };
    let before = state.reserved();
    let mut batch = Batch {
        reserved: before.clone(),
        released: Vec::new(),
        acquire: Vec::new(),
        declined: Vec::new(),
        job_facts: Vec::new(),
        unknown_types: 0,
    };
    for item in message.items {
        match item {
            MessageItem::Offer(offer) => {
                let budget = Budget {
                    tier,
                    capacity: state.capacity,
                    frozen: !state.plans_offers(),
                    assigned: latest.map(|s| s.total_assigned_jobs),
                    reserved: batch.reserved.clone(),
                };
                match demand::decide_offer(&budget, &offer) {
                    OfferDecision::Acquire => {
                        batch.reserved.insert(offer.request_id);
                        batch.acquire.push(offer.request_id);
                    }
                    OfferDecision::Decline(reason) => batch.declined.push(Declined {
                        request_id: offer.request_id,
                        reason,
                    }),
                }
            }
            MessageItem::Fact(fact) => {
                let request_id = fact.request_id();
                if fact.releases() && batch.reserved.remove(&request_id) {
                    batch.acquire.retain(|id| *id != request_id);
                    if before.contains(&request_id) && !batch.released.contains(&request_id) {
                        batch.released.push(request_id);
                    }
                }
                batch.job_facts.push(fact);
            }
            MessageItem::Unknown => batch.unknown_types = batch.unknown_types.saturating_add(1),
        }
    }
    let drift = state.drift.saturating_add(batch.unknown_types);
    let mut events = vec![ScaleSetEvent::MessageRecorded(MessageRecorded {
        session_id: message.session_id,
        message_id: message.message_id,
        origin,
        statistics: message.statistics,
        statistics_applied,
        released: batch.released,
        acquire: batch.acquire,
        declined: batch.declined,
        job_facts: batch.job_facts,
        unknown_types: batch.unknown_types,
        desired: latest.map_or(0, |s| demand::desired(&s, state.capacity.deliverable)),
    })];
    let degrades = state.phase == Phase::Active;
    if degrades && batch.unknown_types > 0 && drift >= PROTOCOL_DRIFT_THRESHOLD {
        events.push(ScaleSetEvent::SessionDegraded(SessionDegraded {
            cause: DegradedCause::ProtocolDrift,
        }));
    }
    Ok(events)
}

/// `AcquireJobs` answered (A4): the pending ids it names leave `acquiring` (acquired, or
/// released); an id no longer pending (a job fact resolved it first) is ignored. Pending ids
/// leave only on such an observation (pinned by
/// `tests/scaleset/rules.rs::should_keep_acquisitions_pending_when_no_observation_names_them`).
fn decide_acquire_result(
    state: &ScaleSetState,
    requested: &BTreeSet<RunnerRequestId>,
    acquired: &BTreeSet<RunnerRequestId>,
) -> Decision {
    if requested.is_empty() {
        return Err(ScaleSetRejection::NotAcquiring);
    }
    if !acquired.is_subset(requested) {
        return Err(ScaleSetRejection::AcquiredUnrequested);
    }
    let pending: BTreeSet<RunnerRequestId> =
        requested.intersection(&state.acquiring).copied().collect();
    if pending.is_empty() {
        return Ok(vec![]);
    }
    Ok(vec![ScaleSetEvent::AcquireObserved(AcquireObserved {
        acquired: pending.intersection(acquired).copied().collect(),
        released: pending.difference(acquired).copied().collect(),
    })])
}

/// The capacity policy's input, ordered by its revision; the reported max never exceeds the
/// deliverable (I07, I24). Desired is recomputed from the latest statistics.
fn decide_capacity(state: &ScaleSetState, capacity: Capacity) -> Decision {
    if capacity.reported_max > capacity.deliverable {
        return Err(ScaleSetRejection::ReportedOverDeliverable);
    }
    if capacity.revision < state.capacity.revision {
        return Err(ScaleSetRejection::StaleCapacity);
    }
    if capacity.revision == state.capacity.revision {
        return if capacity == state.capacity {
            Ok(vec![])
        } else {
            Err(ScaleSetRejection::CapacityConflict)
        };
    }
    Ok(vec![ScaleSetEvent::CapacityApplied(CapacityApplied {
        capacity,
        wire_max: demand::wire_max(&capacity),
        desired: state
            .statistics
            .map_or(0, |s| demand::desired(&s, capacity.deliverable)),
    })])
}

fn decide_recovered(state: &ScaleSetState) -> Decision {
    match state.phase {
        Phase::Degraded(_) => Ok(vec![ScaleSetEvent::SessionRecovered(SessionRecovered {})]),
        Phase::Unconfigured => Err(ScaleSetRejection::NotEnsured),
        Phase::Ensured
        | Phase::Opening
        | Phase::Active
        | Phase::Backoff(_)
        | Phase::Closing
        | Phase::Closed => Err(ScaleSetRejection::NotDegraded),
    }
}

fn decide_start_closing(state: &ScaleSetState) -> Decision {
    match state.phase {
        Phase::Active | Phase::Degraded(_) => {
            Ok(vec![ScaleSetEvent::ClosingStarted(ClosingStarted {})])
        }
        Phase::Closing => Ok(vec![]),
        Phase::Unconfigured => Err(ScaleSetRejection::NotEnsured),
        Phase::Ensured | Phase::Opening | Phase::Backoff(_) | Phase::Closed => {
            Err(ScaleSetRejection::NoSession)
        }
    }
}

/// Applies one recorded event.
#[must_use]
pub fn evolve(mut state: ScaleSetState, event: &ScaleSetEvent) -> ScaleSetState {
    match event {
        ScaleSetEvent::ScaleSetEpochAdvanced(e) => state.epoch = e.epoch,
        ScaleSetEvent::ScaleSetEnsured(e) => {
            state.phase = Phase::Ensured;
            state.pool = Some(e.pool.clone());
            state.github_id = Some(e.github_id);
            state.tier = Some(e.tier);
        }
        ScaleSetEvent::SessionOpenRequested(_) => state.phase = Phase::Opening,
        ScaleSetEvent::SessionOpened(e) => {
            state.phase = Phase::Active;
            state.session = Some(e.session_id);
            state.statistics = Some(e.statistics);
            state.statistics_after = state.cursor;
            state.session_floor = state.cursor;
            state.session_recorded = false;
            state.open_conflicts = 0;
            state.desired = e.desired;
            state.drift = 0;
        }
        ScaleSetEvent::SessionLost(e) => {
            state.phase = Phase::Backoff(e.cause);
            state.session = None;
            state.stale_session = e.stale_session;
            if e.cause == LossCause::Conflicted {
                state.open_conflicts = state.open_conflicts.saturating_add(1);
            }
        }
        ScaleSetEvent::SessionConflictEscalated(_) => {}
        ScaleSetEvent::SessionDeleted(_) => state.stale_session = None,
        ScaleSetEvent::MessageRecorded(e) => {
            if e.statistics_applied {
                state.statistics = e.statistics;
                state.statistics_after = Some(e.message_id);
            }
            state.cursor = Some(state.cursor.map_or(e.message_id, |c| c.max(e.message_id)));
            state.session_recorded = true;
            for id in &e.released {
                state.acquiring.remove(id);
                state.acquired.remove(id);
            }
            state.acquiring.extend(e.acquire.iter().copied());
            state.drift = state.drift.saturating_add(e.unknown_types);
            state.desired = e.desired;
        }
        ScaleSetEvent::AcquireObserved(e) => {
            for id in e.acquired.iter().chain(&e.released) {
                state.acquiring.remove(id);
            }
            state.acquired.extend(e.acquired.iter().copied());
        }
        ScaleSetEvent::CapacityApplied(e) => {
            state.capacity = e.capacity;
            state.desired = e.desired;
        }
        ScaleSetEvent::SessionDegraded(e) => state.phase = Phase::Degraded(e.cause),
        ScaleSetEvent::SessionRecovered(_) => {
            state.phase = Phase::Active;
            state.drift = 0;
        }
        ScaleSetEvent::ClosingStarted(_) => state.phase = Phase::Closing,
        ScaleSetEvent::SessionClosed(_) => {
            state.phase = Phase::Closed;
            state.session = None;
        }
    }
    state
}
