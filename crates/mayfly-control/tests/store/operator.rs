//! The operator's use cases (`mayfly_control::commands`, RT-03) over the in-memory journal: a
//! pool paused, resumed and drained, its breakers reset, a host drained and resumed, each
//! decided by its aggregate under the actor the caller names.

use mayfly_control::commands::{
    CommandError, HostAction, PoolAction, ResetError, host_action, pool_action, reset_breakers,
};
use mayfly_control::journal::Journal as _;
use mayfly_control::store::{decode_event, stream_of};
use mayfly_control_domain::Actor;
use mayfly_control_domain::attempt::GuestOs;
use mayfly_control_domain::host::Drain;
use mayfly_control_domain::pool::{Pool, PoolCommand, PoolEvent, PoolMode, TripReason};
use mayfly_domain::{Deadline, HostId, ImageDigest, Millis, PoolName, Sample as _};
use mayfly_testkit::block_on;
use mayfly_testkit::clock::FakeClock;
use mayfly_testkit::fleet::{EPOCH, PoolSpec, T0, World, report, store};
use mayfly_testkit::journal::MemoryJournal;

const OPERATOR: Actor = Actor::Operator { uid: 501 };

fn pool() -> PoolName {
    PoolName::new("mac").unwrap_or_else(|e| panic!("{e}"))
}

fn spec() -> PoolSpec {
    PoolSpec {
        name: pool(),
        os: GuestOs::Linux,
        memory_gib: 4,
        max_running: 2,
        min_prepared: 0,
        breaker: 1,
    }
}

fn other_digest() -> ImageDigest {
    ImageDigest::new(&format!("sha256:{}", "b".repeat(64))).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn should_pause_and_resume_a_pool_when_an_operator_asks() {
    // given
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let mut world = World::new(&store);
    world.configure_pool(spec());

    // when
    let paused = block_on(pool_action(
        &store,
        &pool(),
        PoolAction::Pause,
        OPERATOR,
        EPOCH,
    ));
    let mode_paused = world.pool(&pool()).mode;
    let resumed = block_on(pool_action(
        &store,
        &pool(),
        PoolAction::Resume,
        OPERATOR,
        EPOCH,
    ));
    let drained = block_on(pool_action(
        &store,
        &pool(),
        PoolAction::Drain,
        OPERATOR,
        EPOCH,
    ));

    // then
    assert_eq!(paused, Ok(()));
    assert_eq!(mode_paused, PoolMode::Paused);
    assert_eq!(resumed, Ok(()));
    assert_eq!(drained, Ok(()));
    assert_eq!(world.pool(&pool()).mode, PoolMode::Draining);
}

#[test]
fn should_answer_the_aggregates_code_when_the_pool_is_already_paused() {
    // given: a paused pool, and a pool nobody configured
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let mut world = World::new(&store);
    world.configure_pool(spec());
    let first = block_on(pool_action(
        &store,
        &pool(),
        PoolAction::Pause,
        OPERATOR,
        EPOCH,
    ));

    // when
    let again = block_on(pool_action(
        &store,
        &pool(),
        PoolAction::Pause,
        OPERATOR,
        EPOCH,
    ));
    let unknown = block_on(pool_action(
        &store,
        &PoolName::new("nope").unwrap_or_else(|e| panic!("{e}")),
        PoolAction::Drain,
        OPERATOR,
        EPOCH,
    ));

    // then
    assert_eq!(first, Ok(()));
    assert_eq!(again, Err(CommandError::Refused("already_paused")));
    assert_eq!(unknown, Err(CommandError::Refused("not_configured")));
}

#[test]
fn should_reset_every_tripped_breaker_when_no_digest_is_named() {
    // given: the pool's two digests tripped
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let mut world = World::new(&store);
    world.configure_pool(spec());
    for digest in [ImageDigest::sample(), other_digest()] {
        let tripped = world.command::<Pool>(
            pool().as_str(),
            Actor::Automation { policy: "breaker" },
            PoolCommand::TripBreaker {
                digest,
                reason: TripReason::Failures,
                at: T0,
            },
        );
        assert_eq!(tripped, Ok(()));
    }

    // when
    let reset = block_on(reset_breakers(&store, &pool(), None, OPERATOR, EPOCH));

    // then: both reset, each recording the operator's uid
    let mut expected = vec![ImageDigest::sample(), other_digest()];
    expected.sort();
    assert_eq!(reset, Ok(expected));
    let state = world.pool(&pool());
    assert!(
        state.breakers.values().all(|b| b.tripped_at.is_none()),
        "{state:?}"
    );
    let stream = stream_of::<Pool>(pool().as_str()).unwrap_or_else(|e| panic!("{e}"));
    let uids: Vec<u32> = block_on(journal.read_stream(&stream, None, None))
        .unwrap_or_else(|e| panic!("{e}"))
        .iter()
        .map(|e| decode_event::<PoolEvent>(e).unwrap_or_else(|e| panic!("{e:?}")))
        .filter_map(|e| match e {
            PoolEvent::PoolBreakerReset(r) => Some(r.operator_uid),
            _ => None,
        })
        .collect();
    assert_eq!(uids, [501, 501]);
}

#[test]
fn should_refuse_a_reset_when_no_breaker_is_tripped() {
    // given
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let mut world = World::new(&store);
    world.configure_pool(spec());

    // when
    let none = block_on(reset_breakers(&store, &pool(), None, OPERATOR, EPOCH));
    let named = block_on(reset_breakers(
        &store,
        &pool(),
        Some(&ImageDigest::sample()),
        OPERATOR,
        EPOCH,
    ));
    let by_policy = {
        let _ = world.command::<Pool>(
            pool().as_str(),
            Actor::Automation { policy: "breaker" },
            PoolCommand::TripBreaker {
                digest: ImageDigest::sample(),
                reason: TripReason::Failures,
                at: T0,
            },
        );
        block_on(reset_breakers(
            &store,
            &pool(),
            None,
            Actor::Automation {
                policy: "reconciler",
            },
            EPOCH,
        ))
    };

    // then
    let refused = |code| {
        Err(ResetError {
            reset: Vec::new(),
            error: CommandError::Refused(code),
        })
    };
    assert_eq!(none, refused("not_tripped"));
    assert_eq!(named, refused("not_tripped"));
    assert_eq!(by_policy, refused("operator_only"));
}

#[test]
fn should_drain_and_resume_a_host_when_an_operator_asks() {
    // given
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let mut world = World::new(&store);
    world.register_host("h1", report(64, 0, 4));
    let host = HostId::new("h1").unwrap_or_else(|e| panic!("{e}"));
    let within = Millis::new(3_600_000);
    let deadline = Deadline::at(Millis::new(T0.get() + 3_600_000));

    // when
    let drained = block_on(host_action(
        &store,
        &host,
        HostAction::Drain { within },
        OPERATOR,
        EPOCH,
    ));
    let gate = world.host(&host).gate.drain;
    let again = block_on(host_action(
        &store,
        &host,
        HostAction::Drain { within },
        OPERATOR,
        EPOCH,
    ));
    let resumed = block_on(host_action(
        &store,
        &host,
        HostAction::Resume,
        OPERATOR,
        EPOCH,
    ));

    // then
    assert_eq!(drained, Ok(()));
    assert_eq!(gate, Drain::Draining { deadline });
    assert_eq!(again, Err(CommandError::Refused("already_draining")));
    assert_eq!(resumed, Ok(()));
    assert_eq!(world.host(&host).gate.drain, Drain::Open);
}
