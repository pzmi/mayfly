//! The invariants of `formal/scaleset.qnt`, asserted on the Rust aggregate over random command
//! sequences from an open session: a check of the code, where the model's invariants check the
//! model. "Latest statistics" is tracked here independently of the aggregate, in message-id
//! order (a fresh message's key is `2 * id`, a session open's `2 * highest id recorded + 1`, A1):
//! the order the aggregate claims. Whether that order is GitHub's own is the model's question
//! (`desiredFromLatestStatistics` compares with GitHub's jobs; A6). A message at or below the
//! cursor must apply nothing, and one A1 does not explain must degrade the session, unless it is
//! a session's first message below its floor, which restarts the numbering
//! (`MessageIdsRestarted`, the order then restarting from it).

use std::collections::BTreeSet;

use mayfly_control_domain::attempt::RunnerRequestId;
use mayfly_control_domain::demand::{self, EventName, Offer, Statistics, TrustClass, WireMode};
use mayfly_control_domain::scaleset::{
    DegradedCause, LossCause, MessageItem, MessageOrigin, Phase, ScaleSetCommand, ScaleSetEvent,
    ScaleSetState, SessionId,
};
use proptest::prelude::*;

use super::support::{
    active, apply, assigned, auto, capacity, completed, deleted, loss, message, opened, result,
    started,
};

#[derive(Debug, Clone)]
enum Op {
    Message {
        id: u64,
        stats: Option<u32>,
        items: Vec<(u8, u64, EventName)>,
    },
    Answer {
        acquired: Vec<bool>,
    },
    Capacity {
        deliverable: u32,
        reported: u32,
        mode: WireMode,
    },
    Reopen {
        stats: u32,
    },
}

fn event_names() -> impl Strategy<Value = EventName> {
    prop::sample::select(vec![
        EventName::Push,
        EventName::PullRequest,
        EventName::PullRequestTarget,
        EventName::Unknown,
    ])
}

fn ops() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (
            1u64..10,
            prop::option::weighted(0.9, 0u32..5),
            prop::collection::vec((0u8..4, 1u64..7, event_names()), 0..6),
        )
            .prop_map(|(id, stats, items)| Op::Message { id, stats, items }),
        2 => prop::collection::vec(any::<bool>(), 0..6).prop_map(|acquired| Op::Answer { acquired }),
        1 => (0u32..4, 0u32..4, prop::sample::select(vec![
            WireMode::Open, WireMode::Paused, WireMode::Draining, WireMode::SafeMode,
        ]))
            .prop_map(|(deliverable, reported, mode)| Op::Capacity {
                deliverable,
                reported: reported.min(deliverable),
                mode,
            }),
        1 => (0u32..5).prop_map(|stats| Op::Reopen { stats }),
    ]
}

fn item(kind: u8, id: u64, event_name: EventName) -> MessageItem {
    match kind {
        0 | 1 => MessageItem::Offer(Offer {
            request_id: RunnerRequestId(id),
            event_name,
        }),
        2 => started(id),
        _ => completed(id),
    }
}

/// The model's ghost: the newest statistics by key, and every id ever offered trusted.
struct Tracker {
    latest: Option<(u64, Statistics)>,
    highest: u64,
    trusted_offers: BTreeSet<RunnerRequestId>,
    revision: u64,
    sessions: u64,
}

fn commands(state: &ScaleSetState, op: &Op, tracker: &mut Tracker) -> Vec<ScaleSetCommand> {
    let session = state.session.unwrap_or(SessionId { high: 0, low: 0 });
    match op {
        Op::Message { id, stats, items } => {
            let items: Vec<MessageItem> = items.iter().map(|(k, i, e)| item(*k, *i, *e)).collect();
            vec![message(session, *id, stats.map(assigned), items)]
        }
        Op::Answer { acquired } => {
            let requested: Vec<u64> = state.acquiring.iter().map(|r| r.0).collect();
            let got: Vec<u64> = requested
                .iter()
                .zip(acquired.iter().chain(std::iter::repeat(&false)))
                .filter(|(_, a)| **a)
                .map(|(r, _)| *r)
                .collect();
            vec![result(&requested, &got)]
        }
        Op::Capacity {
            deliverable,
            reported,
            mode,
        } => {
            tracker.revision += 1;
            vec![capacity(tracker.revision, *deliverable, *reported, *mode)]
        }
        Op::Reopen { stats } => {
            tracker.sessions += 1;
            let next = SessionId {
                high: tracker.sessions,
                low: tracker.sessions,
            };
            vec![
                loss(LossCause::Expired),
                deleted(session),
                ScaleSetCommand::BeginOpen,
                opened(next, assigned(*stats)),
            ]
        }
    }
}

/// Why the transition `before --command--> after` breaks an invariant, if it does.
fn broken(
    before: &ScaleSetState,
    command: &ScaleSetCommand,
    events: &[ScaleSetEvent],
    after: &ScaleSetState,
    tracker: &mut Tracker,
) -> Option<String> {
    if let ScaleSetCommand::RecordMessage(m) = command {
        for it in &m.items {
            if let MessageItem::Offer(o) = it
                && demand::trust(o) == TrustClass::Trusted
            {
                tracker.trusted_offers.insert(o.request_id);
            }
        }
    }
    let restarted = events
        .iter()
        .any(|e| matches!(e, ScaleSetEvent::MessageIdsRestarted(_)));
    for event in events {
        match event {
            ScaleSetEvent::MessageIdsRestarted(_) => {
                tracker.highest = 0;
                tracker.latest = tracker.latest.map(|(_, s)| (0, s));
            }
            ScaleSetEvent::MessageRecorded(m) if m.origin != MessageOrigin::Fresh => {
                if before.cursor.is_none_or(|c| m.message_id > c) {
                    return Some(
                        "a message above the cursor read as from an earlier session".into(),
                    );
                }
                let unchanged = !m.statistics_applied
                    && m.acquire.is_empty()
                    && m.released.is_empty()
                    && m.declined.is_empty()
                    && m.unknown_types == 0
                    && after.desired == before.desired
                    && after.drift == before.drift
                    && after.reserved() == before.reserved()
                    && after.statistics == before.statistics;
                if !unchanged {
                    return Some("redeliveryAppliesNothing".to_owned());
                }
                let live = matches!(before.phase, Phase::Active | Phase::Degraded(_));
                let flagged = after.phase == Phase::Degraded(DegradedCause::MessageIdsRegressed);
                if m.origin == MessageOrigin::IdRegressed && live && !flagged {
                    return Some("a1RefutationIsFlagged".to_owned());
                }
            }
            ScaleSetEvent::MessageRecorded(m) => {
                if !restarted && before.cursor.is_some_and(|c| m.message_id <= c) {
                    return Some("a message at or below the cursor applied as fresh".to_owned());
                }
                if let Some(stats) = m.statistics {
                    let key = 2 * m.message_id.0;
                    if tracker.latest.is_none_or(|(k, _)| key > k) {
                        tracker.latest = Some((key, stats));
                    }
                }
                tracker.highest = tracker.highest.max(m.message_id.0);
                let reserved_before: BTreeSet<RunnerRequestId> = before
                    .reserved()
                    .into_iter()
                    .filter(|id| !m.released.contains(id))
                    .collect();
                if m.acquire.iter().any(|id| reserved_before.contains(id)) {
                    return Some("noDoubleReservationPerRequest".to_owned());
                }
                if !m.acquire.is_empty() {
                    let assigned = after.statistics.map_or(u32::MAX, |s| s.total_assigned_jobs);
                    let open = u32::try_from(after.reserved().len()).unwrap_or(u32::MAX);
                    if assigned.saturating_add(open) > after.capacity.deliverable {
                        return Some(format!("I23: {assigned} + {open} reserved"));
                    }
                    if after.capacity.mode != WireMode::Open {
                        return Some("ADR 0029: acquired while held".to_owned());
                    }
                    if before.phase != Phase::Active {
                        return Some("P21: planned while degraded or closing".to_owned());
                    }
                }
            }
            ScaleSetEvent::SessionOpened(o) => {
                tracker.latest = Some((2 * tracker.highest + 1, o.statistics));
            }
            _ => {}
        }
    }
    if !after.acquiring.is_disjoint(&after.acquired) {
        return Some("a request both pending and acquired".to_owned());
    }
    if !after.reserved().is_subset(&tracker.trusted_offers) {
        return Some("I32: a reservation no trusted offer made".to_owned());
    }
    let expected = tracker
        .latest
        .map_or(0, |(_, s)| demand::desired(&s, after.capacity.deliverable));
    if after.desired != expected {
        return Some(format!(
            "desiredFromLatestStatistics: {} != {expected}",
            after.desired
        ));
    }
    let wire = match after.capacity.mode {
        WireMode::Open | WireMode::Paused => after.capacity.reported_max,
        WireMode::Draining | WireMode::SafeMode => 0,
    };
    if after.wire_max() != wire || wire > after.capacity.deliverable {
        return Some("I07: wire max".to_owned());
    }
    if matches!(after.phase, Phase::Backoff(_)) && after.session.is_some() {
        return Some("a backoff holding a session".to_owned());
    }
    None
}

proptest! {
    #[test]
    fn should_hold_the_reservation_invariants_when_messages_are_random(
        ops in prop::collection::vec(ops(), 1..40),
    ) {
        // given
        let mut state = active();
        let mut tracker = Tracker {
            latest: Some((1, assigned(0))),
            highest: 0,
            trusted_offers: BTreeSet::new(),
            revision: 1,
            sessions: 1,
        };
        // when
        for op in &ops {
            for command in commands(&state, op, &mut tracker) {
                let Ok((after, events)) = apply(&state, auto(command.clone())) else {
                    continue;
                };
                // then
                let problem = broken(&state, &command, &events, &after, &mut tracker);
                prop_assert!(problem.is_none(), "{problem:?} after {command:?} from {state:?}");
                state = after;
            }
        }
    }
}
