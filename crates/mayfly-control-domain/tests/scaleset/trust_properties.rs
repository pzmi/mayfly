//! The invariants `formal/scaleset.qnt` adds for github.com's direct assignment (ADR 0090 with its
//! 2026-11-12 amendment), asserted on the Rust aggregate over random command sequences:
//! `assignedJobsClassified`, `vmOnlyForAJobClassifiedTrusted`, `cancelOnlyRefused`,
//! `sinkOnlyForARefusedJob` and the renumbered branch of `a1RefutationIsFlagged`. As in the model,
//! the ghost knows each job's true class (`TRUST`: jobs 1 and 2 trusted, 3 a
//! `pull_request_target`, 4 a fork's pull request) and which lookups failed, and never feeds a
//! command's guard. The two last tests are the negative controls: the state the mutants
//! `RUN_ASSIGNED` and `CANCEL_ANY` would leave is caught by the same check.

use std::collections::BTreeSet;

use mayfly_control_domain::demand::{EventName, JobKey, RunLookup, WireMode};
use mayfly_control_domain::registration::ScaleSetId;
use mayfly_control_domain::scaleset::{
    CancelAnswer, DegradedCause, LossCause, MessageItem, MessageOrigin, Phase, RefusalAction,
    ScaleSetCommand, ScaleSetEvent, ScaleSetState, SessionId,
};
use proptest::prelude::*;

use super::support::{
    REPOSITORY, apply, assign, assigned, auto, capacity, completed_job, deleted, direct, key, loss,
    message, opened, reach_from, run, started_job,
};

/// The model's `TRUST`: jobs 1 and 2 are trusted.
const fn truly_trusted(n: u64) -> bool {
    n <= 2
}

fn job_number(job: &JobKey) -> u64 {
    job.run_id.0 - 100
}

#[derive(Debug, Clone)]
enum Op {
    Assign(Vec<u64>),
    Release { n: u64, started: bool, named: bool },
    Reused,
    Lookup { n: u64, ok: bool },
    StartSink(u64),
    SinkEnded,
    Cancel { n: u64, accepted: bool },
    Mode(WireMode),
    Reopen,
    Renumber(Vec<u64>),
    Recreate,
}

fn job() -> impl Strategy<Value = u64> {
    1u64..=4
}

fn ops() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => prop::collection::vec(job(), 1..3).prop_map(Op::Assign),
        2 => (job(), any::<bool>(), any::<bool>())
            .prop_map(|(n, started, named)| Op::Release { n, started, named }),
        1 => Just(Op::Reused),
        4 => (job(), prop::bool::weighted(0.8)).prop_map(|(n, ok)| Op::Lookup { n, ok }),
        2 => job().prop_map(Op::StartSink),
        1 => Just(Op::SinkEnded),
        2 => (job(), any::<bool>()).prop_map(|(n, accepted)| Op::Cancel { n, accepted }),
        1 => prop::sample::select(vec![
            WireMode::Open, WireMode::Paused, WireMode::Draining, WireMode::SafeMode,
        ])
        .prop_map(Op::Mode),
        1 => Just(Op::Reopen),
        1 => prop::collection::vec(job(), 1..3).prop_map(Op::Renumber),
        1 => Just(Op::Recreate),
    ]
}

/// The model's ghost.
#[derive(Debug, Default)]
struct Ghost {
    next_id: u64,
    revision: u64,
    sessions: u64,
    /// Jobs whose last lookup failed.
    lookup_failed: BTreeSet<JobKey>,
    /// Jobs whose `JobAssigned` was recorded and no job fact released yet.
    assigned_seen: BTreeSet<JobKey>,
}

impl Ghost {
    /// A refusal of `job` is warranted: it is untrusted, or its lookup failed.
    fn refusable(&self, job: &JobKey) -> bool {
        !truly_trusted(job_number(job)) || self.lookup_failed.contains(job)
    }
}

fn lookup_of(n: u64, ok: bool) -> RunLookup {
    if !ok {
        return RunLookup::Failed;
    }
    match n {
        1 | 2 => run(EventName::Push, REPOSITORY),
        3 => run(EventName::PullRequestTarget, REPOSITORY),
        _ => run(EventName::PullRequest, "fork/app"),
    }
}

fn reopen(state: &ScaleSetState, ghost: &mut Ghost, cause: LossCause) -> Vec<ScaleSetCommand> {
    ghost.sessions += 1;
    let mut path = vec![loss(cause)];
    if let Some(session) = state.session {
        path.push(deleted(session));
    }
    path.extend([
        ScaleSetCommand::BeginOpen,
        opened(
            SessionId {
                high: ghost.sessions,
                low: ghost.sessions,
            },
            assigned(1),
        ),
    ]);
    path
}

fn commands(state: &ScaleSetState, op: &Op, ghost: &mut Ghost) -> Vec<ScaleSetCommand> {
    let session = state.session.unwrap_or(SessionId { high: 0, low: 0 });
    let mut fresh = |items: Vec<MessageItem>| {
        ghost.next_id += 1;
        message(session, ghost.next_id, Some(assigned(1)), items)
    };
    match op {
        Op::Assign(jobs) => vec![fresh(jobs.iter().map(|n| assign(*n)).collect())],
        Op::Release { n, started, named } => {
            let item = if *started && *named {
                started_job(*n)
            } else {
                completed_job(*n, *named)
            };
            vec![fresh(vec![item])]
        }
        Op::Reused => {
            let reused = state.cursor.map_or(0, |c| c.0.saturating_sub(1)).max(1);
            vec![message(session, reused, Some(assigned(1)), vec![assign(3)])]
        }
        Op::Lookup { n, ok } => vec![ScaleSetCommand::RecordRunLookup {
            job: key(*n),
            lookup: lookup_of(*n, *ok),
        }],
        Op::StartSink(n) => vec![ScaleSetCommand::StartSink { job: key(*n) }],
        Op::SinkEnded => vec![ScaleSetCommand::RecordSinkEnded],
        Op::Cancel { n, accepted } => vec![ScaleSetCommand::RecordCancelAnswer {
            job: key(*n),
            answer: if *accepted {
                CancelAnswer::Accepted
            } else {
                CancelAnswer::Refused
            },
        }],
        Op::Mode(mode) => {
            ghost.revision += 1;
            vec![capacity(ghost.revision, 2, 2, *mode)]
        }
        Op::Reopen => reopen(state, ghost, LossCause::Expired),
        Op::Renumber(jobs) => {
            let mut path = reopen(state, ghost, LossCause::Restarted);
            ghost.next_id = 1;
            let next = SessionId {
                high: ghost.sessions,
                low: ghost.sessions,
            };
            let items = jobs.iter().map(|n| assign(*n)).collect();
            path.push(message(next, 1, Some(assigned(1)), items));
            path
        }
        Op::Recreate => {
            let mut path = reopen(state, ghost, LossCause::Restarted);
            path.truncate(path.len() - 2);
            let old = state.github_id.unwrap_or(ScaleSetId(7));
            ghost.next_id = 0;
            path.extend([
                ScaleSetCommand::RecordScaleSetVanished { github_id: old },
                ScaleSetCommand::RecordScaleSetRecreated {
                    github_id: ScaleSetId(old.0 + 1),
                },
                ScaleSetCommand::BeginOpen,
                opened(
                    SessionId {
                        high: ghost.sessions,
                        low: ghost.sessions,
                    },
                    assigned(0),
                ),
            ]);
            path
        }
    }
}

/// The ghost after an accepted transition: what was assigned and released, which lookups failed.
fn observe(command: &ScaleSetCommand, events: &[ScaleSetEvent], ghost: &mut Ghost) {
    if let ScaleSetCommand::RecordRunLookup { job, lookup } = command {
        if *lookup == RunLookup::Failed {
            ghost.lookup_failed.insert(job.clone());
        } else {
            ghost.lookup_failed.remove(job);
        }
    }
    for event in events {
        match event {
            ScaleSetEvent::MessageRecorded(m) if m.origin == MessageOrigin::Fresh => {
                for fact in &m.job_facts {
                    match fact {
                        mayfly_control_domain::scaleset::JobFact::Assigned {
                            job: Some(job),
                            ..
                        } => {
                            ghost.assigned_seen.insert(job.key.clone());
                        }
                        mayfly_control_domain::scaleset::JobFact::Started {
                            job: Some(job),
                            ..
                        }
                        | mayfly_control_domain::scaleset::JobFact::Completed {
                            job: Some(job),
                            ..
                        } => {
                            ghost.assigned_seen.remove(job);
                        }
                        _ => {}
                    }
                }
            }
            ScaleSetEvent::ScaleSetRecreated(_) => ghost.assigned_seen.clear(),
            _ => {}
        }
    }
}

/// Which invariant the transition `before --command--> after` breaks, if any.
fn broken(
    before: &ScaleSetState,
    command: &ScaleSetCommand,
    events: &[ScaleSetEvent],
    after: &ScaleSetState,
    ghost: &Ghost,
) -> Option<String> {
    // assignedJobsClassified
    if after.trusted.keys().any(|j| !truly_trusted(job_number(j))) {
        return Some("assignedJobsClassified: an untrusted job classified trusted".to_owned());
    }
    let unclassified = ghost.assigned_seen.iter().find(|j| {
        !truly_trusted(job_number(j))
            && !after.refused.contains(*j)
            && !after.pending.contains_key(*j)
    });
    if let Some(job) = unclassified {
        return Some(format!(
            "assignedJobsClassified: {job:?} neither refused nor pending"
        ));
    }
    // vmOnlyForAJobClassifiedTrusted: the model's union `acquired ∪ trusted`, each job once
    let backed = after
        .trusted
        .iter()
        .filter(|(j, a)| {
            truly_trusted(job_number(j))
                && !ghost.lookup_failed.contains(*j)
                && !after.acquired.contains(&a.request_id)
        })
        .count()
        + after.acquired.len();
    let backed = u32::try_from(backed).unwrap_or(u32::MAX);
    if let Some(idle) = (0..=4).find(|idle| after.runner_may_start(*idle) && *idle >= backed) {
        return Some(format!(
            "vmOnlyForAJobClassifiedTrusted: {idle} idle, {backed} backed"
        ));
    }
    // cancelOnlyRefused
    for job in after.cancel_tried.difference(&before.cancel_tried) {
        if !before.cancels_refused_runs() || !ghost.refusable(job) {
            return Some(format!("cancelOnlyRefused: {job:?}"));
        }
    }
    if let Some(RefusalAction::CancelRun(job)) = after.refusal_action()
        && (!after.cancels_refused_runs() || !ghost.refusable(&job))
    {
        return Some(format!("cancelOnlyRefused: {job:?} due"));
    }
    // sinkOnlyForARefusedJob
    let started: Vec<&JobKey> = after.sink_tried.difference(&before.sink_tried).collect();
    for job in started {
        let quiet = before.pending.is_empty() && before.waiting() == 0 && !before.sink_live;
        if before.cancels_refused_runs() || !ghost.refusable(job) || !quiet {
            return Some(format!("sinkOnlyForARefusedJob: {job:?}"));
        }
    }
    if let Some(RefusalAction::StartSink(job)) = after.refusal_action()
        && (after.cancels_refused_runs() || !ghost.refusable(&job) || !after.pending.is_empty())
    {
        return Some(format!("sinkOnlyForARefusedJob: {job:?} due"));
    }
    // a1RefutationIsFlagged
    if let ScaleSetCommand::RecordMessage(m) = command {
        let first = !before.session_recorded;
        let restarts = first && before.session_floor.is_some_and(|f| m.message_id < f);
        let earlier = !restarts && before.cursor.is_some_and(|c| m.message_id <= c);
        let flagged = events
            .iter()
            .any(|e| matches!(e, ScaleSetEvent::MessageIdsRestarted(_)));
        let origin = events.iter().find_map(|e| match e {
            ScaleSetEvent::MessageRecorded(r) => Some(r.origin),
            _ => None,
        });
        if restarts && (!flagged || origin != Some(MessageOrigin::Fresh) || !after.renumbered) {
            return Some("a1RefutationIsFlagged: a restart not recorded".to_owned());
        }
        if earlier {
            let explained = first && before.session_floor == Some(m.message_id);
            let degraded = after.phase == Phase::Degraded(DegradedCause::MessageIdsRegressed);
            let live = matches!(before.phase, Phase::Active | Phase::Degraded(_));
            if origin == Some(MessageOrigin::Fresh) || flagged {
                return Some("a1RefutationIsFlagged: a reused id applied".to_owned());
            }
            if !explained && live && !degraded {
                return Some("a1RefutationIsFlagged: a reused id dropped silently".to_owned());
            }
        }
    }
    None
}

proptest! {
    #[test]
    fn should_hold_the_trust_invariants_when_assignments_are_random(
        cancel_refused_runs in any::<bool>(),
        ops in prop::collection::vec(ops(), 1..40),
    ) {
        // given
        let mut state = direct(cancel_refused_runs);
        let mut ghost = Ghost { revision: 2, sessions: 1, ..Ghost::default() };
        // when
        for op in &ops {
            for command in commands(&state, op, &mut ghost) {
                let Ok((after, events)) = apply(&state, auto(command.clone())) else {
                    continue;
                };
                observe(&command, &events, &mut ghost);
                // then
                let problem = broken(&state, &command, &events, &after, &ghost);
                prop_assert!(problem.is_none(), "{problem:?} after {command:?} from {state:?}");
                state = after;
            }
        }
    }
}

/// Negative control, mutant `RUN_ASSIGNED` (`mutantRunAssignedViolatesTest`): an assignment
/// accepted at once, unlooked-up, breaks `assignedJobsClassified` and, with a runner started for
/// it, `vmOnlyForAJobClassifiedTrusted`.
#[test]
fn should_flag_an_unlooked_up_trusted_assignment_when_the_mutant_accepts_it() {
    // given: the `pull_request_target` job 3 recorded
    let mut ghost = Ghost::default();
    let state = direct(false);
    let command = message(
        SessionId { high: 1, low: 1 },
        1,
        Some(assigned(1)),
        vec![assign(3)],
    );
    let (honest, events) = apply(&state, auto(command.clone())).expect("recorded");
    observe(&command, &events, &mut ghost);
    // when: the mutant's state, the job trusted at once
    let mut mutant = honest.clone();
    let assignment = mutant.pending.remove(&key(3)).expect("pending");
    mutant.trusted.insert(key(3), assignment.clone());
    // then
    assert_eq!(broken(&state, &command, &events, &honest, &ghost), None);
    let problem = broken(&state, &command, &events, &mutant, &ghost).expect("flagged");
    assert!(problem.starts_with("assignedJobsClassified"), "{problem}");
    // and a runner started for a job whose lookup failed, as if it were trusted
    mutant.trusted = [(key(1), assignment)].into_iter().collect();
    ghost.assigned_seen.clear();
    ghost.lookup_failed.insert(key(1));
    let problem = broken(&state, &command, &events, &mutant, &ghost).expect("flagged");
    assert!(
        problem.starts_with("vmOnlyForAJobClassifiedTrusted"),
        "{problem}"
    );
}

/// Negative control, mutant `CANCEL_ANY` (`mutantCancelAnyViolatesTest`): a trusted job's run
/// cancelled, with the option off, breaks `cancelOnlyRefused`.
#[test]
fn should_flag_a_cancelled_trusted_run_when_the_mutant_cancels_any() {
    // given: job 1 classified trusted, the option off
    let mut ghost = Ghost::default();
    let command = message(
        SessionId { high: 1, low: 1 },
        1,
        Some(assigned(1)),
        vec![assign(1)],
    );
    let before = reach_from(direct(false), [command.clone()]);
    let lookup = ScaleSetCommand::RecordRunLookup {
        job: key(1),
        lookup: lookup_of(1, true),
    };
    let (honest, events) = apply(&before, auto(lookup.clone())).expect("recorded");
    observe(&lookup, &events, &mut ghost);
    // when: the mutant's state, its run cancelled
    let mut mutant = honest.clone();
    mutant.cancel_tried.insert(key(1));
    // then
    assert_eq!(broken(&before, &lookup, &events, &honest, &ghost), None);
    let problem = broken(&before, &lookup, &events, &mutant, &ghost).expect("flagged");
    assert!(problem.starts_with("cancelOnlyRefused"), "{problem}");
}
