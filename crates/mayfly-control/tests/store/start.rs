//! P2: an attempt starts from its host's lease under a read of the host stream at the revision
//! the lease was read at (ADR 0041, CP-03F).

use mayfly_control::start::{StartRequest, Started, start_attempt};
use mayfly_control::store::{Outcome, SaveRequest};
use mayfly_control_domain::attempt::{GuestOs, Phase};
use mayfly_control_domain::host::{Host, HostCommand, ReleaseEvidence, ReserveSlot};
use mayfly_control_domain::{Actor, Command};
use mayfly_domain::CommandId;
use mayfly_domain::{PoolName, Resources, Sample};
use mayfly_testkit::block_on;
use mayfly_testkit::clock::FakeClock;
use mayfly_testkit::fleet::{self, GIB, PoolSpec, World, report};
use mayfly_testkit::journal::MemoryJournal;

use crate::recovery_kit::{BEFORE, Kit, NOW, attempt, host, kit, stream};
use crate::scripted::BeforeAppend;

const POOL_GIB: u64 = 8;

/// The sample host with the sample pool's terms and attempt 1's slot reserved.
fn leased() -> Kit {
    let kit = kit();
    let store = fleet::store(&kit.memory, &FakeClock::at(NOW));
    let mut world = World::new(&store);
    world.configure_pool(PoolSpec {
        name: PoolName::sample(),
        os: GuestOs::Linux,
        memory_gib: POOL_GIB,
        max_running: 4,
        min_prepared: 0,
        breaker: 3,
    });
    world.register_host(host().as_str(), report(64, 0, 4));
    world
        .command::<Host>(
            host().as_str(),
            Actor::Automation {
                policy: "reconciler",
            },
            HostCommand::ReserveSlot(ReserveSlot {
                attempt_id: attempt(1),
                pool: PoolName::sample(),
                os: GuestOs::Linux,
                resources: Resources::new(POOL_GIB * GIB, 50 * GIB),
                terms_revision: kit.terms_revision(),
            }),
        )
        .unwrap_or_else(|e| panic!("{e}"));
    kit
}

/// A second writer on the same journal saving `body` on the host.
fn host_command_on(memory: &MemoryJournal, id: u128, body: HostCommand) {
    let store = fleet::store(memory, &FakeClock::at(NOW));
    let command = Command::new(
        CommandId::from_u128(id),
        NOW,
        BEFORE,
        Actor::Automation {
            policy: "reconciler",
        },
        body,
    );
    match block_on(store.execute::<Host>(SaveRequest::new(host().as_str(), command)))
        .unwrap_or_else(|e| panic!("{e}"))
    {
        Outcome::Accepted { .. } => {}
        Outcome::Refused { rejection, .. } => panic!("refused: {rejection:?}"),
    }
}

fn request() -> StartRequest {
    StartRequest {
        host: host(),
        attempt: attempt(1),
        retry_budget: 2,
        run_epoch: BEFORE,
    }
}

#[test]
fn should_start_the_attempt_from_its_lease_when_the_host_holds_it() {
    // given
    let kit = leased();

    // when
    let started = block_on(start_attempt(&kit.store, &request())).unwrap_or_else(|e| panic!("{e}"));

    // then
    assert!(matches!(
        started,
        Started::Started {
            replayed: false,
            ..
        }
    ));
    assert_eq!(kit.state(1).phase, Phase::Reserved);
}

#[test]
fn should_refuse_the_start_at_commit_when_the_host_stream_moves_after_the_read() {
    // given the lease is released between the start's read and its commit
    let kit = leased();
    let memory = kit.memory.clone();
    kit.journal.before_next_append(BeforeAppend::new(
        stream(&format!("host-{}", host())),
        move || {
            host_command_on(
                &memory,
                0x5E1,
                HostCommand::ReleaseSlot {
                    attempt_id: attempt(1),
                    evidence: ReleaseEvidence::AttemptNeverStarted,
                },
            );
        },
    ));

    // when
    let started = block_on(start_attempt(&kit.store, &request())).unwrap_or_else(|e| panic!("{e}"));

    // then the unit read the moved host stream and was refused; the retry read no lease
    assert_eq!(started, Started::NoLease);
    assert!(
        kit.events(1).is_empty(),
        "no attempt for a lease the host no longer holds"
    );
    assert_eq!(kit.clock.sleeps().len(), 1, "one conflict, one retry");
}

#[test]
fn should_start_from_the_lease_read_again_when_another_write_moves_the_host_stream() {
    // given a newer capacity report lands between the start's read and its commit
    let kit = leased();
    let memory = kit.memory.clone();
    kit.journal.before_next_append(BeforeAppend::new(
        stream(&format!("host-{}", host())),
        move || {
            host_command_on(
                &memory,
                0x5E2,
                HostCommand::RecordCapacityReport {
                    report_id: 100,
                    capacity: report(48, 0, 4),
                },
            );
        },
    ));

    // when
    let started = block_on(start_attempt(&kit.store, &request())).unwrap_or_else(|e| panic!("{e}"));

    // then the retry read the host again and started from the lease it still holds
    assert!(matches!(started, Started::Started { .. }));
    assert_eq!(kit.clock.sleeps().len(), 1, "one conflict, one retry");
}
