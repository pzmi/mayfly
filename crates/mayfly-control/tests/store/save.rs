//! `AggregateStore::save`, `with_revision_retry` and the start-up kind check, step by step
//! (`doc/event-store.md` §3.7, §6.3).

use mayfly_control::journal::{
    AggregateName, Appended, AppendedStream, ExpectedRevision, InboxKey, InboxSource, Journal,
    MessageId, NewEvent, Payload, ReadCondition, ReceiptOutcome, StorageError, StoredKind,
    StreamId, StreamRevision, SupportedEpochs, UnitOfWork,
};
use mayfly_control::store::{
    AggregateStore, Clock, Committed, KindCheck, NoNotifier, Outcome, SaveError, SaveRequest,
    UnknownStoredKind, check_stored_kinds, derive_event_id,
};
use mayfly_domain::{CommandId, EventId, Millis};
use mayfly_testkit::block_on;
use mayfly_testkit::clock::{FakeClock, Notification, RecordingNotifier, SeededRng};
use mayfly_testkit::contracts::journal::InjectedFault;
use mayfly_testkit::journal::MemoryJournal;

use crate::counter::{
    Counter, CounterBody, CounterLog, CounterRejection, Counters, NewerCounter, NewerCounters,
    command,
};
use crate::scripted::{BeforeReceipt, Script, ScriptedJournal};

type Store = AggregateStore<ScriptedJournal, FakeClock, SeededRng, RecordingNotifier>;

struct Harness {
    store: Store,
    journal: ScriptedJournal,
    clock: FakeClock,
    notifier: RecordingNotifier,
}

fn harness(memory: &MemoryJournal) -> Harness {
    let journal = ScriptedJournal::new(memory.clone());
    let clock = FakeClock::at(Millis::new(1_700_000_000_000));
    let notifier = RecordingNotifier::new();
    let store = AggregateStore::new::<Counters>(
        journal.clone(),
        clock.clone(),
        SeededRng::new(42),
        notifier.clone(),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    Harness {
        store,
        journal,
        clock,
        notifier,
    }
}

fn newer(
    memory: &MemoryJournal,
) -> AggregateStore<MemoryJournal, FakeClock, SeededRng, NoNotifier> {
    AggregateStore::new::<NewerCounters>(
        memory.clone(),
        FakeClock::at(Millis::new(1)),
        SeededRng::new(1),
        NoNotifier,
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

fn stream(name: &str) -> StreamId {
    StreamId::new(name).unwrap_or_else(|e| panic!("{e}"))
}

fn name(aggregate: &str) -> AggregateName {
    AggregateName::new(aggregate).unwrap_or_else(|e| panic!("{e}"))
}

fn at(n: u64) -> ExpectedRevision {
    ExpectedRevision::Exactly(StreamRevision::new(n))
}

fn inc(id: u128, by: u32, limit: u32) -> SaveRequest<CounterBody> {
    SaveRequest::new("1", command(id, CounterBody::Increment { by, limit }))
}

fn save(
    store: &Store,
    request: SaveRequest<CounterBody>,
) -> Result<Outcome<CounterRejection>, SaveError> {
    block_on(store.save::<Counter>(request))
}

fn accepted(
    stream_name: &str,
    first: u64,
    ids: &[(u128, u32)],
    replayed: bool,
) -> Outcome<CounterRejection> {
    let first = StreamRevision::new(first);
    let mut last = first;
    for _ in 1..ids.len() {
        last = last.next().unwrap_or(last);
    }
    Outcome::Accepted {
        appended: Appended {
            streams: vec![AppendedStream {
                stream: stream(stream_name),
                first,
                last,
                event_ids: ids
                    .iter()
                    .map(|(c, o)| derive_event_id(CommandId::from_u128(*c), *o))
                    .collect(),
            }],
        },
        replayed,
    }
}

fn refused(replayed: bool) -> Outcome<CounterRejection> {
    Outcome::Refused {
        rejection: CounterRejection::AtLimit,
        replayed,
    }
}

fn head(journal: &impl Journal, name: &str) -> Option<StreamRevision> {
    block_on(journal.head(&stream(name)))
        .unwrap_or_else(|e| panic!("{e}"))
        .map(|h| h.revision)
}

fn key(message: &str) -> InboxKey {
    InboxKey {
        source: InboxSource::new("scaleset-1").unwrap_or_else(|e| panic!("{e}")),
        message_id: MessageId::new(message).unwrap_or_else(|e| panic!("{e}")),
    }
}

#[test]
fn should_write_derived_ids_and_the_clock_instant_when_a_command_is_accepted() {
    // given
    let h = harness(&MemoryJournal::new());

    // when
    let outcome = save(&h.store, inc(1, 2, 9));

    // then
    assert_eq!(outcome, Ok(accepted("counter-1", 0, &[(1, 0)], false)));
    let events = block_on(h.journal.read_stream(&stream("counter-1"), None, None))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].payload.as_str(), "{\"by\":2}");
    assert_eq!(events[0].kind.event_type, "counter_incremented");
    assert_eq!(events[0].recorded_at, h.clock.now());
}

#[test]
fn should_return_its_receipt_when_a_command_is_retried() {
    // given
    let h = harness(&MemoryJournal::new());
    let first = save(&h.store, inc(1, 2, 9));
    save(&h.store, inc(2, 1, 9)).unwrap_or_else(|e| panic!("{e}"));
    let zero = save(&h.store, inc(3, 0, 9));

    // when
    let again = save(&h.store, inc(1, 2, 9));
    let zero_again = save(&h.store, inc(3, 0, 9));

    // then
    assert_eq!(first, Ok(accepted("counter-1", 0, &[(1, 0)], false)));
    assert_eq!(again, Ok(accepted("counter-1", 0, &[(1, 0)], true)));
    assert_eq!(head(&h.journal, "counter-1"), Some(StreamRevision::new(1)));
    assert_eq!(
        zero,
        Ok(Outcome::Accepted {
            appended: Appended::default(),
            replayed: false
        })
    );
    assert_eq!(
        zero_again,
        Ok(Outcome::Accepted {
            appended: Appended::default(),
            replayed: true
        })
    );
}

#[test]
fn should_replay_a_refusal_until_its_stream_moves_when_a_command_is_repeated() {
    // given
    let h = harness(&MemoryJournal::new());
    save(&h.store, inc(30, 2, 9)).unwrap_or_else(|e| panic!("{e}"));
    let tight = || inc(21, 3, 4);
    let first = save(&h.store, tight());
    let repeat = save(&h.store, tight());
    save(
        &h.store,
        SaveRequest::new("1", command(31, CounterBody::Reset)),
    )
    .unwrap_or_else(|e| panic!("{e}"));

    // when
    let after_move = save(&h.store, tight());
    let later = save(&h.store, tight());

    // then
    assert_eq!(first, Ok(refused(false)));
    assert_eq!(repeat, Ok(refused(true)));
    assert_eq!(after_move, Ok(accepted("counter-1", 2, &[(21, 0)], false)));
    assert_eq!(later, Ok(accepted("counter-1", 2, &[(21, 0)], true)));
}

#[test]
fn should_bind_a_refusal_to_every_stream_the_decision_read_when_it_is_recorded() {
    // given
    let h = harness(&MemoryJournal::new());
    let pool = stream("counter-pool");
    save(
        &h.store,
        SaveRequest::new(
            "pool",
            command(10, CounterBody::Increment { by: 1, limit: 9 }),
        ),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let over = |pool_at: u64| inc(20, 5, 4).with_read(pool.clone(), at(pool_at));
    let first = save(&h.store, over(0));
    let repeat = save(&h.store, over(0));
    save(
        &h.store,
        SaveRequest::new(
            "pool",
            command(11, CounterBody::Increment { by: 1, limit: 9 }),
        ),
    )
    .unwrap_or_else(|e| panic!("{e}"));

    // when
    let after_pool_moved = save(&h.store, over(1));

    // then
    assert_eq!(first, Ok(refused(false)));
    assert_eq!(repeat, Ok(refused(true)));
    assert_eq!(after_pool_moved, Ok(refused(false)));
    let receipt = block_on(h.journal.receipt(CommandId::from_u128(20)))
        .unwrap_or_else(|e| panic!("{e}"))
        .map(|r| r.outcome);
    assert!(
        matches!(
            &receipt,
            Some(ReceiptOutcome::Refused { bound, .. }) if *bound == vec![
                ReadCondition { stream: stream("counter-1"), expected: ExpectedRevision::NoStream },
                ReadCondition { stream: pool.clone(), expected: at(1) },
            ]
        ),
        "{receipt:?}"
    );
}

#[test]
fn should_refuse_a_reused_command_id_when_the_intent_differs() {
    // given
    let h = harness(&MemoryJournal::new());
    save(&h.store, inc(1, 1, 9)).unwrap_or_else(|e| panic!("{e}"));
    let elsewhere = SaveRequest::new("2", command(1, CounterBody::Increment { by: 1, limit: 9 }));

    // when
    let other_body = save(&h.store, inc(1, 2, 9));
    let other_stream = save(&h.store, elsewhere);

    // then
    let reused = Err(SaveError::CommandIdReused {
        command_id: CommandId::from_u128(1),
    });
    assert_eq!(other_body, reused);
    assert_eq!(other_stream, reused);
    assert_eq!(head(&h.journal, "counter-2"), None);
}

#[test]
fn should_refuse_a_newer_epoch_before_the_receipt_when_a_newer_build_wrote_the_stream() {
    // given
    let memory = MemoryJournal::new();
    let h = harness(&memory);
    save(&h.store, inc(1, 1, 9)).unwrap_or_else(|e| panic!("{e}"));
    block_on(
        newer(&memory).save::<NewerCounter>(SaveRequest::new("1", command(2, CounterBody::Seal))),
    )
    .unwrap_or_else(|e| panic!("{e}"));

    // when
    let retried = save(&h.store, inc(1, 1, 9));

    // then
    assert_eq!(
        retried,
        Err(SaveError::StreamEpochUnsupported {
            stream: stream("counter-1"),
            epoch: 2
        })
    );
    let readable = block_on(h.journal.read_stream(&stream("counter-1"), None, None))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(readable.len(), 2);
}

#[test]
fn should_refuse_the_replay_when_a_newer_build_recorded_the_receipt_after_the_epoch_check() {
    // given
    let memory = MemoryJournal::new();
    let h = harness(&memory);
    save(&h.store, inc(1, 1, 9)).unwrap_or_else(|e| panic!("{e}"));
    let newer_memory = memory.clone();
    h.journal.before_next_receipt(BeforeReceipt::new(move || {
        let newer = newer(&newer_memory);
        block_on(newer.save::<NewerCounter>(SaveRequest::new("1", command(2, CounterBody::Seal))))
            .unwrap_or_else(|e| panic!("{e}"));
        block_on(newer.save::<NewerCounter>(inc(3, 1, 9))).unwrap_or_else(|e| panic!("{e}"));
    }));

    // when
    let raced = save(&h.store, inc(3, 1, 9));

    // then
    assert_eq!(
        raced,
        Err(SaveError::StreamEpochUnsupported {
            stream: stream("counter-1"),
            epoch: 2
        })
    );
    assert_eq!(head(&h.journal, "counter-1"), Some(StreamRevision::new(2)));
}

#[test]
fn should_leave_a_lost_reply_unknown_when_a_newer_build_raised_the_stream_since() {
    // given
    let h = harness(&MemoryJournal::new());
    let raise = UnitOfWork::new(
        Millis::new(1),
        SupportedEpochs::new().with(name("counter"), 2),
    )
    .with_append(
        stream("counter-1"),
        name("counter"),
        at(0),
        vec![NewEvent {
            event_id: EventId::from_u128(0xFEED),
            kind: StoredKind {
                event_type: "sealed".to_owned(),
                schema_version: 1,
            },
            required_epoch: 2,
            payload: Payload::new("{}"),
        }],
    );
    h.journal.play(Script::LoseReplyThenRaise {
        raise: Box::new(raise),
    });

    // when
    let outcome = save(&h.store, inc(1, 1, 9));

    // then
    assert_eq!(
        outcome,
        Err(SaveError::OutcomeUnknown(StorageError::new(
            "scripted_lost_reply"
        )))
    );
    assert!(h.notifier.seen().is_empty());
}

#[test]
fn should_raise_safe_mode_when_a_stream_holds_an_unknown_kind() {
    // given
    let memory = MemoryJournal::new();
    let h = harness(&memory);
    block_on(
        newer(&memory).save::<NewerCounter>(SaveRequest::new("1", command(2, CounterBody::Note))),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let noted = StoredKind {
        event_type: "counter_noted".to_owned(),
        schema_version: 1,
    };

    // when
    let outcome = save(&h.store, inc(5, 1, 9));

    // then
    assert_eq!(
        outcome,
        Err(SaveError::UnknownKind {
            stream: stream("counter-1"),
            kind: noted.clone()
        })
    );
    assert_eq!(
        h.notifier.seen(),
        [Notification::UnknownKind {
            stream: stream("counter-1"),
            kind: noted
        }]
    );
    let receipt =
        block_on(h.journal.receipt(CommandId::from_u128(5))).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(receipt, None);
}

#[test]
fn should_start_in_safe_mode_when_a_stored_kind_is_unknown() {
    // given
    let memory = MemoryJournal::new();
    let empty = block_on(check_stored_kinds::<Counters, _>(&memory));
    let h = harness(&memory);
    save(&h.store, inc(1, 1, 9)).unwrap_or_else(|e| panic!("{e}"));
    for (instance, id) in [("3", 2), ("2", 3)] {
        block_on(
            newer(&memory)
                .save::<NewerCounter>(SaveRequest::new(instance, command(id, CounterBody::Note))),
        )
        .unwrap_or_else(|e| panic!("{e}"));
    }

    // when
    let this_build = block_on(check_stored_kinds::<Counters, _>(&memory));
    let newer_build = block_on(check_stored_kinds::<NewerCounters, _>(&memory));

    // then
    assert_eq!(empty, Ok(KindCheck::Known));
    assert_eq!(
        this_build,
        Ok(KindCheck::SafeMode {
            unknown: vec![UnknownStoredKind {
                kind: StoredKind {
                    event_type: "counter_noted".to_owned(),
                    schema_version: 1,
                },
                first_stream: Some(stream("counter-2")),
            }]
        })
    );
    assert_eq!(newer_build, Ok(KindCheck::Known));
}

#[test]
fn should_kick_once_after_commit_and_never_when_a_receipt_answers() {
    // given
    let h = harness(&MemoryJournal::new());

    // when
    save(&h.store, inc(1, 1, 9)).unwrap_or_else(|e| panic!("{e}"));
    save(&h.store, inc(1, 1, 9)).unwrap_or_else(|e| panic!("{e}"));
    save(&h.store, inc(2, 9, 1)).unwrap_or_else(|e| panic!("{e}"));
    save(&h.store, inc(3, 0, 9)).unwrap_or_else(|e| panic!("{e}"));

    // then
    let Ok(Outcome::Accepted { appended, .. }) = save(&h.store, inc(1, 1, 9)) else {
        panic!("the replay answers with the acceptance");
    };
    assert_eq!(
        h.notifier.seen(),
        [Notification::Committed(Committed {
            command_id: CommandId::from_u128(1),
            appended,
        })]
    );
}

#[test]
fn should_retry_only_conflicts_when_a_stream_moves_under_the_decision() {
    // given
    let h = harness(&MemoryJournal::new());
    let counter_1 = stream("counter-1");

    // when
    h.journal.play(Script::Race {
        stream: counter_1.clone(),
        n: 1,
    });
    let once = save(&h.store, inc(1, 1, 9));
    h.journal.play(Script::Race {
        stream: counter_1.clone(),
        n: 1,
    });
    let retried = block_on(h.store.execute::<Counter>(inc(2, 1, 9)));
    let sleeps_after_one_retry = h.clock.sleeps().len();
    h.journal.play(Script::Race {
        stream: counter_1.clone(),
        n: 3,
    });
    let exhausted = block_on(h.store.execute::<Counter>(inc(3, 1, 9)));
    let sleeps_after_exhaustion = h.clock.sleeps().len();
    h.journal.inner.inject(InjectedFault::Busy);
    let busy = block_on(h.store.execute::<Counter>(inc(4, 1, 9)));

    // then
    assert!(
        matches!(&once, Err(e @ SaveError::Conflict { .. }) if e.is_retryable()),
        "{once:?}"
    );
    assert_eq!(retried, Ok(accepted("counter-1", 2, &[(2, 0)], false)));
    assert_eq!(sleeps_after_one_retry, 1);
    assert!(
        matches!(exhausted, Err(SaveError::Conflict { .. })),
        "{exhausted:?}"
    );
    assert_eq!(sleeps_after_exhaustion, 3);
    assert!(h.clock.sleeps().iter().all(|s| s.get() <= 20));
    assert_eq!(busy, Err(SaveError::Busy));
    assert_eq!(h.clock.sleeps().len(), 3);
}

#[test]
fn should_resolve_a_lost_reply_by_its_receipt_when_the_commit_landed_or_not() {
    // given
    let h = harness(&MemoryJournal::new());

    // when
    h.journal.inner.inject(InjectedFault::ReplyLostAfterCommit);
    let landed = save(&h.store, inc(1, 1, 9));
    h.journal.inner.inject(InjectedFault::ReplyLostBeforeCommit);
    let lost = save(&h.store, inc(2, 1, 9));

    // then
    assert_eq!(landed, Ok(accepted("counter-1", 0, &[(1, 0)], false)));
    assert_eq!(lost, Ok(accepted("counter-1", 1, &[(2, 0)], false)));
    assert_eq!(head(&h.journal, "counter-1"), Some(StreamRevision::new(1)));
    assert_eq!(h.notifier.seen().len(), 2);
}

#[test]
fn should_answer_from_the_twin_receipt_when_the_same_command_commits_first() {
    // given
    let h = harness(&MemoryJournal::new());
    h.journal.play(Script::Twin);

    // when
    let outcome = save(&h.store, inc(1, 1, 9));

    // then
    assert_eq!(outcome, Ok(accepted("counter-1", 0, &[(1, 0)], true)));
    assert!(
        h.notifier.seen().is_empty(),
        "the twin's commit is not this call's"
    );
}

#[test]
fn should_confirm_a_lost_reply_by_lookup_when_the_id_sits_where_the_unit_wrote_it() {
    // given
    let h = harness(&MemoryJournal::new());
    h.journal.play(Script::LoseReplyWithoutReceipt);

    // when
    let outcome = save(&h.store, inc(1, 1, 9));

    // then
    assert_eq!(outcome, Ok(accepted("counter-1", 0, &[(1, 0)], false)));
    assert_eq!(head(&h.journal, "counter-1"), Some(StreamRevision::FIRST));
}

#[test]
fn should_report_a_collision_when_a_derived_id_lives_elsewhere() {
    // given
    let h = harness(&MemoryJournal::new());
    h.journal.play(Script::LoseReplyAfterPlanting {
        elsewhere: stream("counter-other"),
    });

    // when
    let outcome = save(&h.store, inc(1, 1, 9));

    // then
    let Err(SaveError::EventIdCollision { event_id, location }) = outcome else {
        panic!("a collision, got {outcome:?}");
    };
    assert_eq!(event_id, derive_event_id(CommandId::from_u128(1), 0));
    assert_eq!(location.map(|l| l.stream), Some(stream("counter-other")));
    assert_eq!(head(&h.journal, "counter-1"), None);
    assert!(h.notifier.seen().is_empty());
}

#[test]
fn should_carry_every_registered_epoch_when_built_from_a_registry() {
    // given
    let memory = MemoryJournal::new();

    // when
    let this_build = harness(&memory).store.supported_epochs().clone();
    let newer_build = newer(&memory).supported_epochs().clone();

    // then
    assert_eq!(
        this_build,
        SupportedEpochs::new()
            .with(name("counter"), 1)
            .with(name("counter_log"), 1)
    );
    assert_eq!(newer_build, SupportedEpochs::new().with(name("counter"), 2));
}

#[test]
fn should_refuse_when_the_stream_belongs_to_another_aggregate() {
    // given
    let h = harness(&MemoryJournal::new());
    block_on(h.store.save::<CounterLog>(inc(1, 1, 9))).unwrap_or_else(|e| panic!("{e}"));
    let overlapping = SaveRequest::new(
        "log-1",
        command(2, CounterBody::Increment { by: 1, limit: 9 }),
    );

    // when
    let outcome = save(&h.store, overlapping);

    // then
    assert_eq!(
        outcome,
        Err(SaveError::AggregateMismatch {
            stream: stream("counter-log-1"),
            recorded: name("counter_log"),
        })
    );
}

#[test]
fn should_refuse_a_redelivered_message_when_its_inbox_key_was_consumed() {
    // given
    let h = harness(&MemoryJournal::new());
    save(&h.store, inc(1, 1, 9).with_inbox(key("7"))).unwrap_or_else(|e| panic!("{e}"));
    let refusal = save(&h.store, inc(3, 9, 1).with_inbox(key("8")));

    // when
    let redelivered = save(&h.store, inc(2, 1, 9).with_inbox(key("7")));

    // then
    assert_eq!(redelivered, Err(SaveError::DuplicateMessage(key("7"))));
    assert_eq!(refusal, Ok(refused(false)));
    let consumed = block_on(h.journal.inbox_contains(&key("8"))).unwrap_or_else(|e| panic!("{e}"));
    assert!(!consumed, "a refusal does not consume its message");
}

#[test]
fn should_settle_a_lost_refusal_reply_as_its_own_commit_when_the_receipt_landed() {
    // given
    let h = harness(&MemoryJournal::new());
    save(&h.store, inc(1, 1, 9)).unwrap_or_else(|e| panic!("{e}"));
    h.journal.inner.inject(InjectedFault::ReplyLostAfterCommit);

    // when
    let outcome = save(&h.store, inc(2, 9, 1));

    // then
    assert_eq!(outcome, Ok(refused(false)));
    assert_eq!(h.notifier.seen().len(), 1, "a refusal kicks nothing");
}

#[test]
fn should_not_retry_when_the_policy_allows_one_decision() {
    // given
    let h = harness(&MemoryJournal::new());
    let store = h
        .store
        .with_retry_policy(mayfly_control::store::RetryPolicy {
            attempts: 1,
            base: Millis::new(10),
            cap: Millis::new(10),
        });
    h.journal.play(Script::Race {
        stream: stream("counter-1"),
        n: 1,
    });

    // when
    let outcome = block_on(store.execute::<Counter>(inc(1, 1, 9)));

    // then
    assert!(
        matches!(outcome, Err(SaveError::Conflict { .. })),
        "{outcome:?}"
    );
    assert!(h.clock.sleeps().is_empty());
    assert_eq!(
        head(store.journal(), "counter-1"),
        Some(StreamRevision::FIRST)
    );
}

#[test]
fn should_refuse_reads_in_execute_when_a_retry_could_not_rebuild_them() {
    // given
    let h = harness(&MemoryJournal::new());
    let pool = stream("counter-pool");
    let request = inc(1, 1, 9).with_read(pool.clone(), ExpectedRevision::NoStream);

    // when
    let outcome = block_on(h.store.execute::<Counter>(request));

    // then
    assert_eq!(outcome, Err(SaveError::ReadsOutsideRetry));
    assert_eq!(head(&h.journal, "counter-1"), None);
    let receipt =
        block_on(h.journal.receipt(CommandId::from_u128(1))).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(receipt, None, "refused before any write");
}

#[test]
fn should_refuse_as_a_conflict_when_the_own_stream_is_not_where_expected() {
    // given a stream at revision 0, and a request that requires it at none
    let h = harness(&MemoryJournal::new());
    save(&h.store, inc(1, 1, 9)).unwrap_or_else(|e| panic!("{e}"));

    // when
    let moved = save(
        &h.store,
        inc(2, 1, 9).with_expected(ExpectedRevision::NoStream),
    );
    let held = save(&h.store, inc(3, 1, 9).with_expected(at(0)));
    let executed = block_on(
        h.store
            .execute::<Counter>(inc(4, 1, 9).with_expected(at(1))),
    );

    // then
    assert_eq!(
        moved,
        Err(SaveError::Conflict {
            stream: stream("counter-1"),
            expected: ExpectedRevision::NoStream,
            actual: Some(StreamRevision::new(0)),
        }),
        "never decided on a stream found elsewhere"
    );
    let receipt =
        block_on(h.journal.receipt(CommandId::from_u128(2))).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(receipt, None, "no receipt: nothing was decided");
    assert_eq!(held, Ok(accepted("counter-1", 1, &[(3, 0)], false)));
    assert_eq!(executed, Err(SaveError::ReadsOutsideRetry));
}

#[test]
fn should_refuse_the_commit_when_the_own_stream_moves_after_the_expected_load() {
    // given a competitor that commits between the load and the append
    let h = harness(&MemoryJournal::new());
    save(&h.store, inc(1, 1, 9)).unwrap_or_else(|e| panic!("{e}"));
    h.journal.play(Script::Race {
        stream: stream("counter-1"),
        n: 1,
    });

    // when
    let outcome = save(&h.store, inc(2, 1, 9).with_expected(at(0)));

    // then
    assert!(
        matches!(outcome, Err(SaveError::Conflict { .. })),
        "{outcome:?}"
    );
    assert_eq!(head(&h.journal, "counter-1"), Some(StreamRevision::new(1)));
}

#[test]
fn should_rebuild_reads_in_the_closure_when_a_read_stream_moves() {
    // given
    let h = harness(&MemoryJournal::new());
    let pool = stream("counter-pool");
    save(
        &h.store,
        SaveRequest::new(
            "pool",
            command(10, CounterBody::Increment { by: 1, limit: 9 }),
        ),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    h.journal.play(Script::Race {
        stream: pool.clone(),
        n: 1,
    });
    let store = &h.store;

    // when
    let outcome = block_on(store.with_revision_retry(|| {
        let pool = pool.clone();
        async move {
            let read = store
                .journal()
                .head(&pool)
                .await
                .map_err(SaveError::Storage)?
                .map(|h| h.revision);
            store
                .save::<Counter>(inc(2, 1, 9).with_read(pool, ExpectedRevision::of(read)))
                .await
        }
    }));

    // then
    assert_eq!(outcome, Ok(accepted("counter-1", 0, &[(2, 0)], false)));
    assert_eq!(h.clock.sleeps().len(), 1, "one conflict, one retry");
    assert_eq!(
        head(&h.journal, "counter-pool"),
        Some(StreamRevision::new(1))
    );
}

#[test]
fn should_name_what_failed_when_each_save_error_is_displayed() {
    // given
    let s = stream("counter-1");
    let errors = [
        SaveError::InvalidName(mayfly_control::journal::JournalValueError::InvalidStreamId),
        SaveError::StreamEpochUnsupported {
            stream: s.clone(),
            epoch: 2,
        },
        SaveError::AggregateMismatch {
            stream: s.clone(),
            recorded: name("counter_log"),
        },
        SaveError::CommandIdReused {
            command_id: CommandId::from_u128(1),
        },
        SaveError::UnknownKind {
            stream: s.clone(),
            kind: StoredKind {
                event_type: "counter_noted".to_owned(),
                schema_version: 1,
            },
        },
        SaveError::Codec {
            stream: s.clone(),
            what: "event_payload",
        },
        SaveError::Conflict {
            stream: s.clone(),
            expected: at(0),
            actual: Some(StreamRevision::new(1)),
        },
        SaveError::Conflict {
            stream: s.clone(),
            expected: at(0),
            actual: None,
        },
        SaveError::DuplicateMessage(key("7")),
        SaveError::EventIdCollision {
            event_id: derive_event_id(CommandId::from_u128(1), 0),
            location: None,
        },
        SaveError::Malformed(mayfly_control::journal::MalformedUnit::Empty),
        SaveError::ReadsOutsideRetry,
        SaveError::EpochAboveBuild { stream: s.clone() },
        SaveError::Busy,
        SaveError::Storage(mayfly_control::journal::StorageError::new("io")),
        SaveError::OutcomeUnknown(mayfly_control::journal::StorageError::new("lost")),
    ];

    // when
    let texts: Vec<String> = errors.iter().map(ToString::to_string).collect();

    // then
    for (text, want) in texts.iter().zip([
        "invalid stream id",
        "at epoch 2",
        "belongs to aggregate counter_log",
        "used for another intent",
        "counter_noted/v1; safe mode",
        "did not convert: event_payload",
        "moved to revision 1",
        "has no event",
        "already consumed",
        "where this command did not write it",
        "refused the unit",
        "rebuild other streams' reads",
        "needs a higher epoch",
        "busy",
        "io; nothing was committed",
        "outcome is unknown",
    ]) {
        assert!(text.contains(want), "{want:?} not in {text:?}");
    }
    assert!(errors.iter().filter(|e| e.is_retryable()).count() == 2);
}
