//! The terms policy (P4; CP-08) at component level, over a real `AggregateStore` and the
//! control projections on the in-memory journal: every change that bumps a pool's terms
//! revision reaches every host as the pool's current terms, a stale revision is refused, terms
//! read before a newer pool event are never applied, a tripped breaker closes the terms, a repeat
//! applies nothing, safe mode applies nothing, and a host registered later is brought up to date.

use std::sync::{Arc, Mutex, PoisonError};

use mayfly_control::delivery::Kicks;
use mayfly_control::journal::Journal as _;
use mayfly_control::projection::control;
use mayfly_control::queries::QueryError;
use mayfly_control::store::{AggregateStore, Clock as _, decode_event};
use mayfly_control::terms::{TermsConfig, TermsError, TermsPolicy, TermsReport};
use mayfly_control_domain::attempt::{GuestOs, Stage};
use mayfly_control_domain::host::{
    ApplyPoolTerms, Host, HostCommand, HostEvent, HostPoolTerms, ReserveSlot,
};
use mayfly_control_domain::pool::{
    ConfigurePool, ImagePinned, Pool, PoolCommand, PoolConfigured, PoolEpochAdvanced, TrustTier,
    terms,
};
use mayfly_control_domain::{
    Actor, Aggregate, AggregateRegistry, AggregateVisitor, Command, ControlAggregates,
    DeterminismCase, DeterminismCases, DomainEvent, Rejection, event_set,
};
use mayfly_domain::{
    AttemptId, CommandId, Epoch, EventId, HostId, ImageDigest, Millis, PoolName, Resources, Sample,
};
use mayfly_testkit::block_on;
use mayfly_testkit::clock::{FakeClock, Notification, RecordingNotifier, SeededRng};
use mayfly_testkit::contracts::journal::InjectedFault;
use mayfly_testkit::fleet::{EPOCH, report, store};
use mayfly_testkit::journal::MemoryJournal;
use serde::{Deserialize, Serialize};

use crate::counter::{CounterBody, CounterRejection, command as counter_command};
use crate::recovery_kit::{Kit, Store, kit, stream};
use crate::scripted::BeforeAppend;

type Policy<'s> = TermsPolicy<
    's,
    crate::scripted::ScriptedJournal,
    FakeClock,
    mayfly_testkit::clock::SeededRng,
    mayfly_control::store::NoNotifier,
    MemoryJournal,
>;

fn pool() -> PoolName {
    PoolName::new("lin").unwrap_or_else(|e| panic!("{e}"))
}

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap_or_else(|e| panic!("{e}"))
}

/// Saves `body` on `instance` of `A` through `store` under command id `id`.
fn command<A: Aggregate>(
    store: &Store,
    id: u128,
    instance: &str,
    actor: Actor,
    body: A::Body,
) -> Result<(), &'static str>
where
    A::Body: Clone,
{
    let command = Command::new(
        CommandId::from_u128(0x7E57_0000 + id),
        store.clock().now(),
        EPOCH,
        actor,
        body,
    );
    match block_on(store.execute::<A>(mayfly_control::store::SaveRequest::new(instance, command)))
        .unwrap_or_else(|e| panic!("{e}"))
    {
        mayfly_control::store::Outcome::Accepted { .. } => Ok(()),
        mayfly_control::store::Outcome::Refused { rejection, .. } => Err(rejection.code()),
    }
}

fn register(kit: &Kit, id: u128, name: &str) {
    command::<Host>(
        &kit.store,
        id,
        name,
        Actor::Startup,
        HostCommand::RegisterHost {
            reserves: Resources::new(0, 0),
        },
    )
    .unwrap_or_else(|code| panic!("{code}"));
    command::<Host>(
        &kit.store,
        id + 1,
        name,
        Actor::Startup,
        HostCommand::RecordCapacityReport {
            report_id: 1,
            capacity: report(64, 0, 8),
        },
    )
    .unwrap_or_else(|code| panic!("{code}"));
}

fn configure(revision: u64, max_running: u32) -> PoolCommand {
    PoolCommand::ConfigurePool(ConfigurePool {
        revision,
        tier: TrustTier::Trusted,
        os: GuestOs::Linux,
        vm_resources: Resources::new(4 << 30, 50 << 30),
        max_running,
        min_prepared: None,
        breaker_threshold: 1,
        breaker_window: Millis::new(3_600_000),
    })
}

fn pin() -> PoolCommand {
    PoolCommand::PinImage {
        digest: ImageDigest::sample(),
        verified_ref: Some(EventId::from_u128(7)),
    }
}

fn on_pool(kit: &Kit, id: u128, actor: Actor, body: PoolCommand) {
    command::<Pool>(&kit.store, id, pool().as_str(), actor, body)
        .unwrap_or_else(|code| panic!("{code}"));
}

const POOL_ACTOR: Actor = Actor::Startup;

fn policy(kit: &Kit) -> Policy<'_> {
    TermsPolicy::new(
        &kit.store,
        &kit.memory,
        TermsConfig::new(EPOCH),
        Kicks::new(8),
    )
}

fn pass(policy: &Policy<'_>) -> TermsReport {
    block_on(policy.run_pass()).unwrap_or_else(|e| panic!("{e}"))
}

/// The pool's terms as `name` holds them.
fn held(kit: &Kit, name: &str) -> Option<HostPoolTerms> {
    block_on(kit.store.load::<Host>(name))
        .unwrap_or_else(|e| panic!("{e}"))
        .pool_terms
        .get(&pool())
        .copied()
}

/// The pool's own terms now, as a host would hold them.
fn wanted(kit: &Kit) -> HostPoolTerms {
    let state = block_on(kit.store.load::<Pool>(pool().as_str())).unwrap_or_else(|e| panic!("{e}"));
    let t = terms(&state).unwrap_or_else(|| panic!("the pool is configured"));
    HostPoolTerms {
        revision: t.revision,
        open: t.open,
        max_running: t.max_running,
        tier: t.tier,
    }
}

/// The pool's terms `name` recorded, in order, as `(revision, open)`.
fn applied_events(kit: &Kit, name: &str) -> Vec<(u64, bool)> {
    block_on(
        kit.memory
            .read_stream(&stream(&format!("host-{name}")), None, None),
    )
    .unwrap_or_else(|e| panic!("{e}"))
    .iter()
    .filter_map(
        |e| match decode_event::<HostEvent>(e).unwrap_or_else(|e| panic!("{e:?}")) {
            HostEvent::PoolTermsApplied(t) if t.pool == pool() => Some((t.revision, t.open)),
            _ => None,
        },
    )
    .collect()
}

#[test]
fn should_apply_every_bump_on_every_host_when_the_pool_changes() {
    // given: two registered hosts and the pool's terms-bumping commands, one at a time
    let kit = kit();
    register(&kit, 1, "h1");
    register(&kit, 3, "h2");
    let policy = policy(&kit);
    let operator = Actor::Operator { uid: 501 };
    let failure = PoolCommand::CountAttemptFailure {
        attempt_id: AttemptId::from_u128(0xF1),
        digest: ImageDigest::sample(),
        stage: Stage::Verify,
        at: Millis::new(1_700_000_000_000),
    };
    let reset = PoolCommand::ResetBreaker {
        digest: ImageDigest::sample(),
    };
    let steps: Vec<(&str, Actor, PoolCommand, bool)> = vec![
        ("configure", POOL_ACTOR, configure(1, 2), false),
        ("pin", POOL_ACTOR, pin(), true),
        ("pause", POOL_ACTOR, PoolCommand::PausePool, false),
        ("resume", POOL_ACTOR, PoolCommand::ResumePool, true),
        ("reconfigure", POOL_ACTOR, configure(2, 3), true),
        ("trip", POOL_ACTOR, failure, false),
        ("reset", operator, reset, true),
        ("drain", POOL_ACTOR, PoolCommand::StartPoolDrain, false),
    ];

    for (n, (step, actor, body, open)) in (10_u128..).zip(steps) {
        // when
        on_pool(&kit, n, actor, body);
        let report = pass(&policy);

        // then: each host holds the pool's terms at its new revision, open or closed
        let want = wanted(&kit);
        assert_eq!(want.open, open, "{step}");
        assert_eq!(report.accepted.len(), 2, "{step}: {report:?}");
        for name in ["h1", "h2"] {
            assert_eq!(held(&kit, name), Some(want), "{step} on {name}");
        }
    }
}

#[test]
fn should_close_the_terms_on_every_host_when_the_breaker_of_the_pin_trips() {
    // given: the pool pinned and open on both hosts
    let kit = kit();
    register(&kit, 1, "h1");
    register(&kit, 3, "h2");
    on_pool(&kit, 10, POOL_ACTOR, configure(1, 2));
    on_pool(&kit, 11, POOL_ACTOR, pin());
    let policy = policy(&kit);
    let _ = pass(&policy);
    assert!(held(&kit, "h1").is_some_and(|t| t.open));

    // when: one stage failure trips the breaker of the pin (threshold 1)
    on_pool(
        &kit,
        12,
        Actor::Automation {
            policy: "attempt_saga",
        },
        PoolCommand::CountAttemptFailure {
            attempt_id: AttemptId::from_u128(0xF1),
            digest: ImageDigest::sample(),
            stage: Stage::Clone,
            at: Millis::new(1_700_000_000_000),
        },
    );
    let _ = pass(&policy);

    // then: closed on both, and a reservation under the applied revision is refused
    let closed = held(&kit, "h1").unwrap_or_else(|| panic!("terms on h1"));
    assert!(!closed.open);
    assert_eq!(held(&kit, "h2"), Some(closed));
    let reserved = command::<Host>(
        &kit.store,
        20,
        "h1",
        Actor::Automation {
            policy: "reconciler",
        },
        HostCommand::ReserveSlot(ReserveSlot {
            attempt_id: AttemptId::from_u128(0xA1),
            pool: pool(),
            os: GuestOs::Linux,
            resources: Resources::new(4 << 30, 50 << 30),
            terms_revision: closed.revision,
        }),
    );
    assert_eq!(reserved, Err("terms_closed"));
}

#[test]
fn should_refuse_a_stale_revision_when_the_host_holds_a_newer_one() {
    // given: the pool's terms at the pin's revision applied on the host
    let kit = kit();
    register(&kit, 1, "h1");
    on_pool(&kit, 10, POOL_ACTOR, configure(1, 2));
    let older = wanted(&kit);
    on_pool(&kit, 11, POOL_ACTOR, pin());
    let _ = pass(&policy(&kit));
    let applied = held(&kit, "h1");

    // when: the configuration's older terms arrive late
    let late = command::<Host>(
        &kit.store,
        20,
        "h1",
        Actor::Automation {
            policy: "terms_policy",
        },
        HostCommand::ApplyPoolTerms(ApplyPoolTerms {
            pool: pool(),
            revision: older.revision,
            open: older.open,
            max_running: older.max_running,
            tier: older.tier,
        }),
    );

    // then
    assert_eq!(late, Err("stale_terms"));
    assert_eq!(held(&kit, "h1"), applied);
    assert_eq!(applied, Some(wanted(&kit)));
}

#[test]
fn should_apply_nothing_superseded_when_the_pool_moves_between_the_read_and_the_save() {
    // given: the pool open when the pass reads it, and paused right before its save commits
    let kit = kit();
    register(&kit, 1, "h1");
    on_pool(&kit, 10, POOL_ACTOR, configure(1, 2));
    on_pool(&kit, 11, POOL_ACTOR, pin());
    let memory = kit.memory.clone();
    kit.journal.before_next_append(BeforeAppend::new(
        stream(&format!("pool-{}", pool())),
        move || {
            let other = store(&memory, &FakeClock::at(Millis::new(1_700_000_000_000)));
            let command = Command::new(
                CommandId::from_u128(0x9A05E),
                Millis::new(1_700_000_000_000),
                EPOCH,
                POOL_ACTOR,
                PoolCommand::PausePool,
            );
            let saved = block_on(
                other.execute::<Pool>(mayfly_control::store::SaveRequest::new("lin", command)),
            );
            assert!(saved.is_ok(), "{saved:?}");
        },
    ));
    let policy = policy(&kit);

    // when
    let raced = pass(&policy);

    // then: the open terms the pass read were refused at the commit, and its retry read the
    // pool again and applied the pause's closed terms; the host never held the open ones
    assert!(raced.complete(), "{raced:?}");
    assert_eq!(raced.accepted.len(), 1, "{raced:?}");
    let want = wanted(&kit);
    assert!(!want.open);
    assert_eq!(applied_events(&kit, "h1"), [(want.revision, false)]);
    assert_eq!(held(&kit, "h1"), Some(want));
}

#[test]
fn should_apply_the_terms_in_the_same_pass_when_the_host_moves_between_the_read_and_the_save() {
    // given: a capacity report commits on the host right before the terms' save
    let kit = kit();
    register(&kit, 1, "h1");
    on_pool(&kit, 10, POOL_ACTOR, configure(1, 2));
    on_pool(&kit, 11, POOL_ACTOR, pin());
    let memory = kit.memory.clone();
    kit.journal
        .before_next_append(BeforeAppend::appending(stream("host-h1"), move || {
            let other = store(&memory, &FakeClock::at(Millis::new(1_700_000_000_000)));
            let command = Command::new(
                CommandId::from_u128(0x9A05F),
                Millis::new(1_700_000_000_000),
                EPOCH,
                Actor::Startup,
                HostCommand::RecordCapacityReport {
                    report_id: 2,
                    capacity: report(64, 0, 8),
                },
            );
            let saved = block_on(
                other.execute::<Host>(mayfly_control::store::SaveRequest::new("h1", command)),
            );
            assert!(saved.is_ok(), "{saved:?}");
        }));
    let policy = policy(&kit);

    // when
    let raced = pass(&policy);

    // then: retried in the pass, not left to the next one
    assert!(raced.complete(), "{raced:?}");
    assert_eq!(raced.accepted.len(), 1, "{raced:?}");
    assert_eq!(held(&kit, "h1"), Some(wanted(&kit)));
}

#[test]
fn should_record_the_terms_once_when_two_passes_apply_the_same_terms() {
    // given: a second policy over the same journal applies the same terms right before this
    // one's save commits
    let kit = kit();
    register(&kit, 1, "h1");
    on_pool(&kit, 10, POOL_ACTOR, configure(1, 2));
    on_pool(&kit, 11, POOL_ACTOR, pin());
    let memory = kit.memory.clone();
    let other_report = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&other_report);
    kit.journal
        .before_next_append(BeforeAppend::appending(stream("host-h1"), move || {
            let other = store(&memory, &FakeClock::at(Millis::new(1_700_000_000_000)));
            let second = TermsPolicy::new(&other, &memory, TermsConfig::new(EPOCH), Kicks::new(8));
            let report = block_on(second.run_pass()).unwrap_or_else(|e| panic!("{e}"));
            *seen.lock().unwrap_or_else(PoisonError::into_inner) = Some(report);
        }));
    let policy = policy(&kit);

    // when
    let first = pass(&policy);

    // then: both answered accepted, the later one by the receipt, and one event recorded
    let second = other_report
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
        .unwrap_or_else(|| panic!("the second pass ran"));
    assert_eq!(second.accepted.len(), 1, "{second:?}");
    assert_eq!(first.accepted.len(), 1, "{first:?}");
    assert!(first.complete(), "{first:?}");
    let want = wanted(&kit);
    assert_eq!(applied_events(&kit, "h1"), [(want.revision, true)]);
}

#[test]
fn should_apply_nothing_twice_when_a_pass_is_repeated() {
    // given
    let kit = kit();
    register(&kit, 1, "h1");
    on_pool(&kit, 10, POOL_ACTOR, configure(1, 2));
    on_pool(&kit, 11, POOL_ACTOR, pin());
    let policy = policy(&kit);
    let first = pass(&policy);
    let (_, head) =
        block_on(kit.store.load_with_head::<Host>("h1")).unwrap_or_else(|e| panic!("{e}"));

    // when
    let second = pass(&policy);

    // then
    assert_eq!(first.accepted.len(), 1);
    assert!(second.due.is_empty(), "{second:?}");
    let (_, after) =
        block_on(kit.store.load_with_head::<Host>("h1")).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(after, head);
}

#[test]
fn should_apply_nothing_when_the_policy_is_in_safe_mode() {
    // given: terms due on the host
    let kit = kit();
    register(&kit, 1, "h1");
    on_pool(&kit, 10, POOL_ACTOR, configure(1, 2));
    on_pool(&kit, 11, POOL_ACTOR, pin());
    let policy = policy(&kit);
    policy.set_safe_mode(true);

    // when
    let safe = pass(&policy);
    let during = held(&kit, "h1");
    policy.set_safe_mode(false);
    let normal = pass(&policy);

    // then: held in safe mode, applied once it is left
    assert_eq!(safe.held.len(), 1, "{safe:?}");
    assert!(safe.accepted.is_empty() && safe.failed.is_empty() && safe.refused.is_empty());
    assert_eq!(during, None);
    assert_eq!(normal.accepted.len(), 1, "{normal:?}");
    assert_eq!(held(&kit, "h1"), Some(wanted(&kit)));
}

#[test]
fn should_apply_the_current_terms_when_a_host_registers_after_the_pool_changed() {
    // given: the pool's terms applied on h1, then h2 registered
    let kit = kit();
    register(&kit, 1, "h1");
    on_pool(&kit, 10, POOL_ACTOR, configure(1, 2));
    on_pool(&kit, 11, POOL_ACTOR, pin());
    let policy = policy(&kit);
    let _ = pass(&policy);
    register(&kit, 3, "h2");

    // when
    let report = pass(&policy);

    // then: only h2 lagged
    assert_eq!(report.accepted.len(), 1, "{report:?}");
    assert_eq!(report.accepted[0].host_id, host("h2"));
    assert_eq!(held(&kit, "h2"), Some(wanted(&kit)));
}

#[test]
fn should_pass_at_most_once_an_interval_when_no_pool_or_host_is_kicked() {
    // given
    let kit = kit();
    register(&kit, 1, "h1");
    let kicks = Kicks::new(8);
    let policy = TermsPolicy::new(
        &kit.store,
        &kit.memory,
        TermsConfig::new(EPOCH),
        kicks.clone(),
    );

    // when
    let first = block_on(policy.tick()).unwrap_or_else(|e| panic!("{e}"));
    kicks.kick(&stream("scaleset-lin"));
    let unrelated = block_on(policy.tick()).unwrap_or_else(|e| panic!("{e}"));
    kicks.kick(&stream("pool-lin"));
    let kicked = block_on(policy.tick()).unwrap_or_else(|e| panic!("{e}"));
    kit.clock.advance(Millis::new(60_000));
    let timed = block_on(policy.tick()).unwrap_or_else(|e| panic!("{e}"));

    // then
    assert!(first.is_some());
    assert!(unrelated.is_none());
    assert!(kicked.is_some());
    assert!(timed.is_some());
}

#[test]
fn should_complete_in_safe_mode_when_a_pool_stream_holds_an_unknown_kind() {
    // given: the pool's stream ends in a kind this build does not decode (a newer build's,
    // left after a rollback), read by a store that reports unknown kinds
    let kit = kit();
    register(&kit, 1, "h1");
    on_pool(&kit, 10, POOL_ACTOR, configure(1, 2));
    on_pool(&kit, 11, POOL_ACTOR, pin());
    let pool_stream = stream(&format!("pool-{}", pool()));
    let newer = AggregateStore::new::<NewerPools>(
        kit.memory.clone(),
        FakeClock::at(Millis::new(1_700_000_000_000)),
        SeededRng::new(6),
        mayfly_control::store::NoNotifier,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let noted = block_on(
        newer.save::<NewerPool>(mayfly_control::store::SaveRequest::new(
            pool().as_str(),
            counter_command(0x7E57_F07E, CounterBody::Note),
        )),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert!(
        matches!(noted, mayfly_control::store::Outcome::Accepted { .. }),
        "{noted:?}"
    );
    let notifier = RecordingNotifier::new();
    let reporting = AggregateStore::new::<ControlAggregates>(
        kit.memory.clone(),
        FakeClock::at(Millis::new(1_700_000_000_000)),
        SeededRng::new(5),
        notifier.clone(),
    )
    .unwrap_or_else(|e| panic!("{e}"))
    .with_projections(control().unwrap_or_else(|e| panic!("{e}")));
    let policy = TermsPolicy::new(
        &reporting,
        &kit.memory,
        TermsConfig::new(EPOCH),
        Kicks::new(8),
    );

    // when: a normal pass meets the kind, which raises safe mode (the daemon then sets the
    // policy's), and the next pass runs in safe mode
    let normal = block_on(policy.run_pass()).unwrap_or_else(|e| panic!("{e}"));
    let raised = notifier
        .seen()
        .iter()
        .any(|n| matches!(n, Notification::UnknownKind { stream, .. } if *stream == pool_stream));
    policy.set_safe_mode(true);
    let safe = block_on(policy.run_pass()).unwrap_or_else(|e| panic!("{e}"));

    // then: incomplete in normal mode, complete in safe mode, nothing applied either way
    assert!(raised, "{:?}", notifier.seen());
    assert_eq!(normal.unread, [pool()]);
    assert!(!normal.complete());
    assert_eq!(safe.unread, [pool()]);
    assert!(safe.complete(), "{safe:?}");
    assert_eq!(held(&kit, "h1"), None);
}

/// A newer build's pool event this build does not know.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PoolNoted {}
impl DomainEvent for PoolNoted {
    const EVENT_TYPE: &'static str = "pool_noted_by_a_newer_build";
    const SCHEMA_VERSION: u32 = 1;
}

event_set! {
    /// The newer build's pool events: those this test's pool stream holds, and one more.
    pub enum NewerPoolEvent {
        /// The run epoch moved.
        PoolEpochAdvanced(PoolEpochAdvanced),
        /// Configured.
        PoolConfigured(PoolConfigured),
        /// Pinned.
        ImagePinned(ImagePinned),
        /// Noted.
        PoolNoted(PoolNoted),
    }
}

/// A newer build of `Pool` that reads this test's pool stream and writes a kind this build
/// cannot decode.
pub struct NewerPool;
impl Aggregate for NewerPool {
    const NAME: &'static str = "pool";
    type State = ();
    type Body = CounterBody;
    type Event = NewerPoolEvent;
    type Rejection = CounterRejection;

    fn initial() {}

    fn decide(
        (): &(),
        _command: Command<CounterBody>,
    ) -> Result<Vec<NewerPoolEvent>, CounterRejection> {
        Ok(vec![NewerPoolEvent::PoolNoted(PoolNoted {})])
    }

    fn evolve((): (), _event: &NewerPoolEvent) {}
}
impl DeterminismCases for NewerPool {
    fn determinism_cases() -> Vec<DeterminismCase<Self>> {
        Vec::new()
    }
}

/// The newer build's aggregates.
pub struct NewerPools;
impl AggregateRegistry for NewerPools {
    fn accept<V: AggregateVisitor>(visitor: &mut V) {
        visitor.visit::<NewerPool>();
    }
}

#[test]
fn should_report_a_refusal_as_decided_when_the_host_is_at_a_newer_run() {
    // given: the host took a command of a newer run than the policy's
    let kit = kit();
    register(&kit, 1, "h1");
    on_pool(&kit, 10, POOL_ACTOR, configure(1, 2));
    on_pool(&kit, 11, POOL_ACTOR, pin());
    let newer = Command::new(
        CommandId::from_u128(0x7E57_0E90),
        Millis::new(1_700_000_000_000),
        Epoch::new(EPOCH.get() + 1),
        Actor::Startup,
        HostCommand::RecordCapacityReport {
            report_id: 2,
            capacity: report(64, 0, 8),
        },
    );
    let saved = block_on(
        kit.store
            .execute::<Host>(mayfly_control::store::SaveRequest::new("h1", newer)),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert!(
        matches!(saved, mayfly_control::store::Outcome::Accepted { .. }),
        "{saved:?}"
    );

    // when
    let report = pass(&policy(&kit));

    // then: refused by `Host` (I18), which decides the item: the pass is complete
    assert_eq!(report.refused.len(), 1, "{report:?}");
    assert_eq!(report.refused[0].1, "stale_run");
    assert!(report.accepted.is_empty());
    assert!(report.complete());
    assert_eq!(held(&kit, "h1"), None);
}

#[test]
fn should_report_the_pass_incomplete_when_the_store_fails_the_save() {
    // given: the store fails the next append before it commits
    let kit = kit();
    register(&kit, 1, "h1");
    on_pool(&kit, 10, POOL_ACTOR, configure(1, 2));
    on_pool(&kit, 11, POOL_ACTOR, pin());
    assert!(kit.memory.inject(InjectedFault::StorageBeforeCommit));
    let policy = policy(&kit);

    // when
    let failed = pass(&policy);
    let next = pass(&policy);

    // then: left to the next pass, which applies it
    assert_eq!(failed.failed.len(), 1, "{failed:?}");
    assert_eq!(failed.failed[0].1, "save_failed");
    assert!(!failed.complete());
    assert_eq!(next.accepted.len(), 1, "{next:?}");
    assert_eq!(held(&kit, "h1"), Some(wanted(&kit)));
}

#[test]
fn should_name_the_policy_and_the_failure_when_formatted() {
    // given
    let kit = kit();
    let policy = policy(&kit);
    let error = TermsError::Query(QueryError::Row {
        projection: "pools",
    });

    // when
    let (debug, display) = (format!("{policy:?}"), error.to_string());

    // then
    assert!(debug.starts_with("TermsPolicy"), "{debug}");
    assert_eq!(
        display,
        "terms pass not run: query failed: a pools row is unreadable"
    );
}
