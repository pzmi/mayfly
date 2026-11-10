//! The scale set's `DETERMINISM_CASES` (ver §3.1): accepted cases along a session's life and one
//! refused case per [`ScaleSetRejection`] code. Histories are produced by `decide` itself from
//! command paths (each path is checked to be accepted step by step by
//! `should_accept_every_step_when_a_case_path_is_replayed`).

use std::collections::BTreeSet;

use mayfly_domain::{CommandId, Epoch, Millis, PoolName, Sample};

use super::events::ScaleSetEvent;
use super::state::{
    EnsureScaleSet, JobFact, LossCause, MessageId, MessageItem, RecordMessage, ScaleSetCommand,
    ScaleSetRejection, ScaleSetState, SessionId,
};
use super::{ScaleSet, decide, evolve};
use crate::aggregate::{DeterminismCase, Expect};
use crate::attempt::RunnerRequestId;
use crate::command::{Actor, Command};
use crate::demand::{Capacity, EventName, Offer, Statistics, WireMode};
use crate::pool::TrustTier;
use crate::registration::{RunnerId, ScaleSetId};

type Step = Command<ScaleSetCommand>;

const LISTENER: Actor = Actor::Automation {
    policy: "scale_set_listener",
};
const FIRST: SessionId = SessionId { high: 1, low: 1 };
const OTHER: SessionId = SessionId { high: 2, low: 2 };

fn at(run_epoch: u64, body: ScaleSetCommand) -> Step {
    Command::new(
        CommandId::from_u128(0x5C),
        Millis::new(1_000),
        Epoch::new(run_epoch),
        LISTENER,
        body,
    )
}

fn auto(body: ScaleSetCommand) -> Step {
    at(1, body)
}

fn ensure(github_id: u64) -> ScaleSetCommand {
    ScaleSetCommand::EnsureScaleSet(EnsureScaleSet {
        pool: PoolName::sample(),
        github_id: ScaleSetId(github_id),
        tier: TrustTier::Trusted,
    })
}

fn capacity(revision: u64, deliverable: u32, reported_max: u32, mode: WireMode) -> ScaleSetCommand {
    ScaleSetCommand::ApplyCapacity(Capacity {
        revision,
        deliverable,
        reported_max,
        mode,
    })
}

fn assigned(total_assigned_jobs: u32) -> Statistics {
    Statistics {
        total_assigned_jobs,
        ..Statistics::default()
    }
}

fn opened(session_id: SessionId) -> ScaleSetCommand {
    ScaleSetCommand::RecordSessionOpened {
        session_id,
        statistics: assigned(0),
    }
}

fn offer(id: u64, event_name: EventName) -> MessageItem {
    MessageItem::Offer(Offer {
        request_id: RunnerRequestId(id),
        event_name,
    })
}

fn message(
    session_id: SessionId,
    id: u64,
    statistics: Statistics,
    items: Vec<MessageItem>,
) -> ScaleSetCommand {
    ScaleSetCommand::RecordMessage(RecordMessage {
        session_id,
        message_id: MessageId(id),
        statistics: Some(statistics),
        items,
    })
}

fn offers() -> ScaleSetCommand {
    message(
        FIRST,
        1,
        assigned(0),
        vec![offer(1, EventName::Push), offer(2, EventName::Push)],
    )
}

fn result(requested: &[u64], acquired: &[u64]) -> ScaleSetCommand {
    ScaleSetCommand::RecordAcquireResult {
        requested: requested.iter().copied().map(RunnerRequestId).collect(),
        acquired: acquired
            .iter()
            .copied()
            .map(RunnerRequestId)
            .collect::<BTreeSet<_>>(),
    }
}

fn then(mut path: Vec<Step>, more: impl IntoIterator<Item = ScaleSetCommand>) -> Vec<Step> {
    path.extend(more.into_iter().map(auto));
    path
}

fn ensured() -> Vec<Step> {
    vec![auto(ensure(7))]
}

fn opening() -> Vec<Step> {
    then(
        ensured(),
        [
            capacity(1, 2, 2, WireMode::Open),
            ScaleSetCommand::BeginOpen,
        ],
    )
}

fn active() -> Vec<Step> {
    then(opening(), [opened(FIRST)])
}

fn with_offers() -> Vec<Step> {
    then(active(), [offers()])
}

fn acquired() -> Vec<Step> {
    then(with_offers(), [result(&[1, 2], &[1, 2])])
}

fn backoff() -> Vec<Step> {
    then(
        active(),
        [ScaleSetCommand::RecordSessionLoss {
            cause: LossCause::Expired,
        }],
    )
}

fn degraded() -> Vec<Step> {
    then(
        active(),
        [message(
            FIRST,
            1,
            assigned(0),
            vec![
                MessageItem::Unknown,
                MessageItem::Unknown,
                MessageItem::Unknown,
            ],
        )],
    )
}

fn closing() -> Vec<Step> {
    then(active(), [ScaleSetCommand::StartClosing])
}

fn deleted() -> Vec<Step> {
    then(
        backoff(),
        [ScaleSetCommand::RecordSessionDeleted { session_id: FIRST }],
    )
}

/// Messages 1 and 3 recorded in the first session, then a restart: the second session opens
/// with the floor at 3.
fn reopened_at_three() -> Vec<Step> {
    then(
        active(),
        [
            message(FIRST, 1, assigned(0), vec![]),
            message(FIRST, 3, assigned(0), vec![]),
            ScaleSetCommand::RecordSessionLoss {
                cause: LossCause::Restarted,
            },
            ScaleSetCommand::RecordSessionDeleted { session_id: FIRST },
            ScaleSetCommand::BeginOpen,
            opened(OTHER),
        ],
    )
}

/// Four creates in a row answered 409, the fifth in flight.
fn conflicted_four_times() -> Vec<Step> {
    let conflict = || {
        [
            ScaleSetCommand::RecordSessionLoss {
                cause: LossCause::Conflicted,
            },
            ScaleSetCommand::BeginOpen,
        ]
    };
    then(
        opening(),
        conflict()
            .into_iter()
            .chain(conflict())
            .chain(conflict())
            .chain(conflict()),
    )
}

fn at_epoch_two() -> Vec<Step> {
    let mut path = active();
    path.push(at(2, capacity(2, 2, 2, WireMode::Open)));
    path
}

/// Folds `path` through `decide` and `evolve`; the first refused step, if any.
fn replay_path(path: Vec<Step>) -> (Vec<ScaleSetEvent>, Option<(usize, ScaleSetRejection)>) {
    let mut state = ScaleSetState::initial();
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
) -> DeterminismCase<ScaleSet> {
    DeterminismCase {
        name,
        history: replay_path(path).0,
        command,
        expect,
    }
}

fn accepted(
    name: &'static str,
    path: Vec<Step>,
    body: ScaleSetCommand,
) -> DeterminismCase<ScaleSet> {
    case(name, path, auto(body), Expect::Accepted)
}

fn refused(
    name: &'static str,
    path: Vec<Step>,
    body: ScaleSetCommand,
) -> DeterminismCase<ScaleSet> {
    case(name, path, auto(body), Expect::Refused)
}

/// Every path a case starts from, for the replay check.
#[cfg(test)]
fn paths() -> Vec<Vec<Step>> {
    vec![
        ensured(),
        opening(),
        active(),
        with_offers(),
        acquired(),
        backoff(),
        degraded(),
        closing(),
        deleted(),
        reopened_at_three(),
        conflicted_four_times(),
        at_epoch_two(),
    ]
}

pub(super) fn cases() -> Vec<DeterminismCase<ScaleSet>> {
    vec![
        // accepted
        accepted("ensure_scale_set", Vec::new(), ensure(7)),
        accepted("same_scale_set_records_nothing", ensured(), ensure(7)),
        accepted("begin_open", ensured(), ScaleSetCommand::BeginOpen),
        accepted("session_opened", opening(), opened(FIRST)),
        accepted(
            "batch_planned_within_deliverable",
            active(),
            message(
                FIRST,
                1,
                assigned(0),
                vec![
                    offer(1, EventName::Push),
                    offer(2, EventName::PullRequest),
                    offer(3, EventName::Schedule),
                ],
            ),
        ),
        accepted(
            "untrusted_offer_declined",
            active(),
            message(
                FIRST,
                1,
                assigned(0),
                vec![offer(4, EventName::PullRequestTarget)],
            ),
        ),
        accepted(
            "unclassifiable_offer_declined",
            active(),
            message(FIRST, 1, assigned(0), vec![offer(5, EventName::Unknown)]),
        ),
        accepted(
            "degraded_session_plans_nothing",
            degraded(),
            message(FIRST, 2, assigned(0), vec![offer(6, EventName::Push)]),
        ),
        accepted(
            "message_below_the_floor_degrades_the_session",
            reopened_at_three(),
            message(OTHER, 1, assigned(1), vec![offer(7, EventName::Push)]),
        ),
        accepted(
            "fifth_conflict_in_a_row_escalates",
            conflicted_four_times(),
            ScaleSetCommand::RecordSessionLoss {
                cause: LossCause::Conflicted,
            },
        ),
        accepted(
            "older_message_keeps_statistics",
            then(active(), [message(FIRST, 5, assigned(2), vec![])]),
            message(FIRST, 3, assigned(0), vec![]),
        ),
        accepted(
            "redelivery_in_a_new_session_reserves_nothing_again",
            then(
                with_offers(),
                [
                    ScaleSetCommand::RecordSessionLoss {
                        cause: LossCause::Restarted,
                    },
                    ScaleSetCommand::RecordSessionDeleted { session_id: FIRST },
                    ScaleSetCommand::BeginOpen,
                    opened(OTHER),
                ],
            ),
            message(
                OTHER,
                1,
                assigned(0),
                vec![offer(1, EventName::Push), offer(2, EventName::Push)],
            ),
        ),
        accepted(
            "job_started_releases_the_reservation",
            acquired(),
            message(
                FIRST,
                2,
                assigned(2),
                vec![
                    MessageItem::Fact(JobFact::Assigned {
                        request_id: RunnerRequestId(1),
                    }),
                    MessageItem::Fact(JobFact::Started {
                        request_id: RunnerRequestId(1),
                        runner_id: Some(RunnerId(42)),
                        runner_name: None,
                    }),
                ],
            ),
        ),
        accepted(
            "unknown_types_degrade_the_session",
            active(),
            message(
                FIRST,
                1,
                assigned(0),
                vec![
                    MessageItem::Unknown,
                    MessageItem::Unknown,
                    MessageItem::Unknown,
                ],
            ),
        ),
        accepted("acquire_answered", with_offers(), result(&[1, 2], &[1])),
        accepted(
            "session_expired",
            active(),
            ScaleSetCommand::RecordSessionLoss {
                cause: LossCause::Expired,
            },
        ),
        accepted(
            "create_conflicted",
            opening(),
            ScaleSetCommand::RecordSessionLoss {
                cause: LossCause::Conflicted,
            },
        ),
        accepted(
            "stale_session_deleted",
            backoff(),
            ScaleSetCommand::RecordSessionDeleted { session_id: FIRST },
        ),
        accepted("reopen_after_delete", deleted(), ScaleSetCommand::BeginOpen),
        accepted(
            "drain_sends_zero",
            active(),
            capacity(2, 2, 1, WireMode::Draining),
        ),
        accepted(
            "degraded_session_recovered",
            degraded(),
            ScaleSetCommand::RecordSessionRecovered,
        ),
        accepted("start_closing", active(), ScaleSetCommand::StartClosing),
        accepted(
            "closing_session_deleted",
            closing(),
            ScaleSetCommand::RecordSessionDeleted { session_id: FIRST },
        ),
        case(
            "newer_epoch_is_recorded",
            active(),
            at(2, capacity(2, 1, 1, WireMode::Paused)),
            Expect::Accepted,
        ),
        // refused, one per code
        refused("stale_run", at_epoch_two(), ScaleSetCommand::StartClosing),
        refused("not_ensured", Vec::new(), ScaleSetCommand::BeginOpen),
        refused("scale_set_mismatch", ensured(), ensure(8)),
        refused(
            "stale_session_pending",
            backoff(),
            ScaleSetCommand::BeginOpen,
        ),
        refused("session_live", active(), ScaleSetCommand::BeginOpen),
        refused("closing", closing(), ScaleSetCommand::BeginOpen),
        refused("not_opening", ensured(), opened(FIRST)),
        refused("no_session", ensured(), offers()),
        refused(
            "session_mismatch",
            active(),
            message(OTHER, 1, assigned(0), vec![]),
        ),
        refused(
            "unknown_session",
            active(),
            ScaleSetCommand::RecordSessionDeleted { session_id: OTHER },
        ),
        refused("not_acquiring", active(), result(&[], &[])),
        refused("acquired_unrequested", with_offers(), result(&[1], &[1, 2])),
        refused(
            "reported_over_deliverable",
            active(),
            capacity(2, 2, 3, WireMode::Open),
        ),
        case(
            "stale_capacity",
            at_epoch_two(),
            at(2, capacity(1, 2, 2, WireMode::Open)),
            Expect::Refused,
        ),
        refused(
            "capacity_conflict",
            active(),
            capacity(1, 1, 1, WireMode::Open),
        ),
        refused(
            "not_degraded",
            active(),
            ScaleSetCommand::RecordSessionRecovered,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_accept_every_step_when_a_case_path_is_replayed() {
        // given
        let paths = paths();
        // when
        let refusals: Vec<(usize, Option<(usize, ScaleSetRejection)>)> = paths
            .into_iter()
            .map(|path| replay_path(path).1)
            .enumerate()
            .filter(|(_, refusal)| refusal.is_some())
            .collect();
        // then
        assert!(refusals.is_empty(), "{refusals:?}");
    }
}
