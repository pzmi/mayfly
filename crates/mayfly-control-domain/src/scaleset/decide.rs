//! `decide` and `evolve` for the scale set: every phase against every command, as explicit match
//! arms with no wildcard (`ARCH-exhaustive-transition`); the outcome of every pair is pinned by
//! `tests/scaleset/matrix.rs::should_define_every_pair_when_each_phase_meets_each_command`.

use std::collections::BTreeSet;

use mayfly_domain::Epoch;

use super::events::{
    AcquireObserved, AssignedJobRefused, AssignedJobTrusted, CancelAnswered, CapacityApplied,
    ClosingStarted, MessageIdsRestarted, MessageRecorded, ScaleSetEnsured, ScaleSetEpochAdvanced,
    ScaleSetEvent, ScaleSetRecreated, ScaleSetVanished, SessionClosed, SessionConflictEscalated,
    SessionDegraded, SessionDeleted, SessionLost, SessionOpenRequested, SessionOpened,
    SessionRecovered, SinkEnded, SinkStarted, TrustPolicyApplied,
};
use super::state::{
    Assignment, CancelAnswer, DegradedCause, EnsureScaleSet, JobFact, LossCause, MessageId,
    MessageItem, MessageOrigin, PROTOCOL_DRIFT_THRESHOLD, Phase, RecordMessage,
    SESSION_CONFLICT_ESCALATION, ScaleSetCommand, ScaleSetRejection, ScaleSetState, SessionId,
};
use crate::attempt::RunnerRequestId;
use crate::command::Command;
use crate::demand::{
    self, Budget, Capacity, Classification, Declined, JobKey, OfferDecision, RunLookup, Statistics,
    TrustPolicy,
};
use crate::registration::ScaleSetId;

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
        ScaleSetCommand::ApplyTrustPolicy(policy) => Ok(decide_trust_policy(state, policy)),
        ScaleSetCommand::RecordRunLookup { job, lookup } => decide_lookup(state, job, &lookup),
        ScaleSetCommand::StartSink { job } => decide_start_sink(state, job),
        ScaleSetCommand::RecordSinkEnded => decide_sink_ended(state),
        ScaleSetCommand::RecordCancelAnswer { job, answer } => {
            decide_cancel_answer(state, job, answer)
        }
        ScaleSetCommand::RecordScaleSetVanished { github_id } => decide_vanished(state, github_id),
        ScaleSetCommand::RecordScaleSetRecreated { github_id } => {
            decide_recreated(state, github_id)
        }
    }
}

/// One scale set per pool (ADR 0040): ensured once; the same scale set again records nothing,
/// another pool, id or tier is refused. A vanished one is re-created, not ensured anew
/// (`RecordScaleSetRecreated` resets what the old set's numbering and jobs left).
fn decide_ensure(state: &ScaleSetState, ensure: EnsureScaleSet) -> Decision {
    match state.phase {
        Phase::Unconfigured if state.vanished() => Err(ScaleSetRejection::ScaleSetVanished),
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

/// Where a message stands (A1, `formal/scaleset.qnt`'s `record`).
enum Standing {
    /// Above the cursor: applies in full.
    Fresh,
    /// The session's first message, strictly below its floor: a new numbering
    /// (`MessageIdsRestarted`), applied as new.
    Restarts(MessageId),
    /// At or below the cursor otherwise: applies nothing.
    Earlier(MessageOrigin),
}

/// Where `message_id` stands: a session's first message strictly below its floor cannot be a
/// redelivery (redelivery is oldest first), so it starts a new numbering; any other id at or
/// below the cursor applies nothing, and unless it is the floor as the session's first message
/// (the stream's last record redelivered after a lost ack) it is a reused id that degrades the
/// session, receipt or not (only the cursor's id can be a duplicate). Pinned by
/// `tests/scaleset/rules.rs::should_apply_nothing_when_a_message_from_an_earlier_session_is_redelivered`,
/// `should_restart_the_numbering_when_a_new_session_delivers_an_id_below_the_floor` and
/// `should_degrade_the_session_when_an_id_at_or_below_the_cursor_comes_later_in_a_session`.
fn standing(state: &ScaleSetState, message_id: MessageId) -> Standing {
    let first = !state.session_recorded;
    if let Some(floor) = state.session_floor
        && first
        && message_id < floor
    {
        return Standing::Restarts(floor);
    }
    match state.cursor {
        Some(cursor) if message_id <= cursor => {
            if first && state.session_floor == Some(message_id) {
                Standing::Earlier(MessageOrigin::Redelivered)
            } else {
                Standing::Earlier(MessageOrigin::IdRegressed)
            }
        }
        Some(_) | None => Standing::Fresh,
    }
}

/// A message at or below the cursor: recorded so it can be acked, applying nothing; one A1
/// does not explain degrades a live session (once: a session already degraded for it stays so).
fn earlier_message(
    state: &ScaleSetState,
    message: RecordMessage,
    origin: MessageOrigin,
    degradable: bool,
) -> Vec<ScaleSetEvent> {
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
    events
}

/// `RecordMessage` (plan §7.2 step 3), in one unit. A message at or below the cursor
/// ([`standing`] `Earlier`) is recorded so the listener can ack it and applies nothing: no
/// statistics, no plan, no release, no classification, no unknown types (its effects were
/// journaled when it was first recorded); one A1 does not explain degrades the session, never
/// silently. A session's first message below its floor records `MessageIdsRestarted` first and
/// then applies as new. A fresh message applies its statistics only when it is newer than the
/// latest statistics (A1: a newer message's, or a session's opening; a message emitted before
/// the open but recorded after it cannot be told apart and applies, A6); each offer is planned
/// by `PlanAcquire` against the budget left by the items before it, only while the session is
/// active (a degraded or closing session declines it as held, P21); `JobStarted` and
/// `JobCompleted` release the job's reservation and, when they name its key, its
/// classification; a `JobAssigned` with a key that mayfly did not acquire waits for its run's
/// lookup (`pending`, folded by `evolve` from the job facts; a new assignment replaces an earlier
/// classification); unknown types are counted, and the session degraded once they reach the
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
    let (state, mut events) = match standing(state, message.message_id) {
        Standing::Earlier(origin) => {
            return Ok(earlier_message(state, message, origin, degradable));
        }
        Standing::Restarts(floor) => {
            let restarted = ScaleSetEvent::MessageIdsRestarted(MessageIdsRestarted {
                message_id: message.message_id,
                floor,
            });
            (evolve(state.clone(), &restarted), vec![restarted])
        }
        Standing::Fresh => (state.clone(), Vec::new()),
    };
    let state = &state;
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
    events.push(ScaleSetEvent::MessageRecorded(MessageRecorded {
        session_id: message.session_id,
        message_id: message.message_id,
        origin: MessageOrigin::Fresh,
        statistics: message.statistics,
        statistics_applied,
        released: batch.released,
        acquire: batch.acquire,
        declined: batch.declined,
        job_facts: batch.job_facts,
        unknown_types: batch.unknown_types,
        desired: latest.map_or(0, |s| demand::desired(&s, state.capacity.deliverable)),
    }));
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
        wire_max: demand::wire_max(&capacity, state.sink_pin()),
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

/// The pool's trust policy: the same one again records nothing. A changed one sends every
/// trusted job back to `pending`, to be looked up and classified again (fail closed: a revoked
/// repository or event stops backing runners; the model's `TRUST` is fixed, so this is the
/// aggregate's own rule). A refused job stays refused: it holds nothing, and a new assignment
/// classifies it afresh. Pinned by
/// `tests/scaleset/trust.rs::should_classify_trusted_jobs_again_when_the_trust_policy_changes`.
fn decide_trust_policy(state: &ScaleSetState, policy: TrustPolicy) -> Vec<ScaleSetEvent> {
    if state.policy.as_ref() == Some(&policy) {
        return Vec::new();
    }
    vec![ScaleSetEvent::TrustPolicyApplied(TrustPolicyApplied {
        policy,
    })]
}

/// A pending assignment's run lookup classifies it through [`demand::classify_assigned`]:
/// trusted, or refused (a failed lookup refuses). Only a pending job: a job classified, released
/// or never assigned is refused `not_pending` (`formal/scaleset.qnt`'s `lookupRun`; pinned by
/// `tests/scaleset/trust.rs::should_refuse_an_assignment_when_its_lookup_fails`).
fn decide_lookup(state: &ScaleSetState, job: JobKey, lookup: &RunLookup) -> Decision {
    let Some(assignment) = state.pending.get(&job) else {
        return Err(ScaleSetRejection::NotPending);
    };
    Ok(
        match demand::classify_assigned(state.policy.as_ref(), &assignment.job, lookup) {
            Classification::Trusted => {
                vec![ScaleSetEvent::AssignedJobTrusted(AssignedJobTrusted {
                    job,
                })]
            }
            Classification::Refused(reason) => {
                vec![ScaleSetEvent::AssignedJobRefused(AssignedJobRefused {
                    job,
                    reason,
                })]
            }
        },
    )
}

/// A refusal sink for a refused job: once per refusal, one at a time, without the cancel option,
/// only while open and active and no trusted or pending assignment is known
/// ([`ScaleSetState::sink_may_start`], the model's `startSink`; pinned by
/// `tests/scaleset/trust.rs::should_start_a_sink_only_when_no_trusted_or_pending_job_is_known`).
fn decide_start_sink(state: &ScaleSetState, job: JobKey) -> Decision {
    if !state.refused.contains(&job) {
        return Err(ScaleSetRejection::NotRefused);
    }
    if !state.sink_may_start() || state.sink_tried.contains(&job) {
        return Err(ScaleSetRejection::SinkNotDue);
    }
    Ok(vec![ScaleSetEvent::SinkStarted(SinkStarted { job })])
}

fn decide_sink_ended(state: &ScaleSetState) -> Decision {
    if state.sink_live {
        Ok(vec![ScaleSetEvent::SinkEnded(SinkEnded {})])
    } else {
        Err(ScaleSetRejection::NoSink)
    }
}

/// A cancel's answer, recorded once, only for a refused job with the option on (the model's
/// `cancelRun` guard, `cancelOnlyRefused`; pinned by
/// `tests/scaleset/trust.rs::should_cancel_only_a_refused_run_when_the_option_is_on`). A lost
/// answer is never recorded, so the cancel is asked again.
fn decide_cancel_answer(state: &ScaleSetState, job: JobKey, answer: CancelAnswer) -> Decision {
    let due = state.cancels_refused_runs()
        && state.refused.contains(&job)
        && !state.cancel_tried.contains(&job);
    if !due {
        return Err(ScaleSetRejection::CancelNotDue);
    }
    Ok(vec![ScaleSetEvent::CancelAnswered(CancelAnswered {
        job,
        answer,
    })])
}

/// A 404 for the journaled scale set while no session is live (a create's answer, or a read):
/// GitHub deleted it as dormant, and the stream returns to `Unconfigured` until it is re-created
/// (`doc/github-scaleset.md` §3.1). A stale session is deleted first (its delete answers 404
/// then, which completes it).
fn decide_vanished(state: &ScaleSetState, github_id: ScaleSetId) -> Decision {
    match state.phase {
        Phase::Unconfigured if state.vanished() => {
            return if state.github_id == Some(github_id) {
                Ok(vec![])
            } else {
                Err(ScaleSetRejection::ScaleSetMismatch)
            };
        }
        Phase::Unconfigured => return Err(ScaleSetRejection::NotEnsured),
        Phase::Active | Phase::Degraded(_) | Phase::Closing => {
            return Err(ScaleSetRejection::SessionLive);
        }
        Phase::Ensured | Phase::Opening | Phase::Backoff(_) | Phase::Closed => {}
    }
    if state.github_id != Some(github_id) {
        return Err(ScaleSetRejection::ScaleSetMismatch);
    }
    if state.stale_session.is_some() {
        return Err(ScaleSetRejection::StaleSessionPending);
    }
    Ok(vec![ScaleSetEvent::ScaleSetVanished(ScaleSetVanished {
        github_id,
    })])
}

/// The vanished scale set re-created under a new id (`formal/scaleset.qnt`'s `recreateScaleSet`;
/// pinned by `tests/scaleset/trust.rs::should_reset_the_numbering_and_drop_reservations_when_the_scale_set_is_recreated`).
fn decide_recreated(state: &ScaleSetState, github_id: ScaleSetId) -> Decision {
    if !state.vanished() {
        return Err(match state.phase {
            Phase::Unconfigured => ScaleSetRejection::NotEnsured,
            Phase::Ensured
            | Phase::Opening
            | Phase::Active
            | Phase::Backoff(_)
            | Phase::Degraded(_)
            | Phase::Closing
            | Phase::Closed => ScaleSetRejection::NotVanished,
        });
    }
    if state.github_id == Some(github_id) {
        return Err(ScaleSetRejection::ScaleSetMismatch);
    }
    Ok(vec![ScaleSetEvent::ScaleSetRecreated(ScaleSetRecreated {
        github_id,
    })])
}

/// Folds a fresh message's job facts into the classification sets, in arrival order
/// (`formal/scaleset.qnt`'s `assignedStep` and `itemStep`): a `JobAssigned` with a key that
/// mayfly did not acquire is pending again (its earlier class, if any, replaced); a `JobStarted`
/// or `JobCompleted` naming a key releases it from every set. A sink tried for a job that is no
/// longer refused is forgotten.
fn fold_assignments(state: &mut ScaleSetState, facts: &[JobFact]) {
    let mut held = state.acquired.clone();
    for fact in facts {
        match fact {
            JobFact::Assigned { request_id, job } => {
                let Some(job) = job else { continue };
                if held.contains(request_id) {
                    continue;
                }
                state.trusted.remove(&job.key);
                state.refused.remove(&job.key);
                let assignment = Assignment {
                    request_id: *request_id,
                    job: job.clone(),
                };
                state.pending.insert(job.key.clone(), assignment);
            }
            JobFact::Started {
                request_id, job, ..
            }
            | JobFact::Completed {
                request_id, job, ..
            } => {
                held.remove(request_id);
                if let Some(key) = job {
                    state.pending.remove(key);
                    state.trusted.remove(key);
                    state.refused.remove(key);
                }
            }
        }
    }
    let refused = &state.refused;
    state.sink_tried.retain(|job| refused.contains(job));
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
            fold_assignments(&mut state, &e.job_facts);
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
            // an assignment the acquire now names is an acquired job: counted once (the model's
            // union `acquired ∪ trusted`)
            let acquired = &e.acquired;
            state
                .pending
                .retain(|_, a| !acquired.contains(&a.request_id));
            state
                .trusted
                .retain(|_, a| !acquired.contains(&a.request_id));
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
        ScaleSetEvent::MessageIdsRestarted(_) => {
            state.cursor = None;
            state.session_floor = None;
            state.statistics_after = None;
            state.renumbered = true;
        }
        ScaleSetEvent::TrustPolicyApplied(e) => {
            state.policy = Some(e.policy.clone());
            // fail closed: a changed policy classifies every trusted job again, so a revoked
            // repository or event stops backing runners
            let trusted = std::mem::take(&mut state.trusted);
            state.pending.extend(trusted);
        }
        ScaleSetEvent::AssignedJobTrusted(e) => {
            if let Some(assignment) = state.pending.remove(&e.job) {
                state.trusted.insert(e.job.clone(), assignment);
            }
        }
        ScaleSetEvent::AssignedJobRefused(e) => {
            state.pending.remove(&e.job);
            state.refused.insert(e.job.clone());
        }
        ScaleSetEvent::SinkStarted(e) => {
            state.sink_live = true;
            state.sink_tried.insert(e.job.clone());
        }
        ScaleSetEvent::SinkEnded(_) => state.sink_live = false,
        ScaleSetEvent::CancelAnswered(e) => {
            state.cancel_tried.insert(e.job.clone());
        }
        ScaleSetEvent::ScaleSetVanished(_) => state.phase = Phase::Unconfigured,
        ScaleSetEvent::ScaleSetRecreated(e) => {
            state.phase = Phase::Ensured;
            state.github_id = Some(e.github_id);
            state.cursor = None;
            state.session_floor = None;
            state.session_recorded = false;
            state.statistics_after = None;
            state.statistics = None;
            state.desired = 0;
            state.acquiring.clear();
            state.acquired.clear();
            state.pending.clear();
            state.trusted.clear();
            state.refused.clear();
            state.cancel_tried.clear();
            state.sink_tried.clear();
        }
    }
    state
}
