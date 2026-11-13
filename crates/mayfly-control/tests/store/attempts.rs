//! A harness of real attempts for the saga's and the sink's component tests: an
//! `AggregateStore` of the control aggregates over `MemoryJournal`, a delivery worker on its
//! ledger, and the sample host's session at epoch 1.

use mayfly_control::delivery::{DeliveryConfig, DeliveryWorker, Kicks};
use mayfly_control::journal::{Journal, StreamId};
use mayfly_control::projection::control;
use mayfly_control::saga::sent_of;
use mayfly_control::sessions::{HostSession, HostSessions};
use mayfly_control::store::{AggregateStore, decode_event};
use mayfly_control_domain::ControlAggregates;
use mayfly_control_domain::attempt::{AttemptCommand, AttemptEvent, AttemptState, DirectiveIssued};
use mayfly_domain::{Epoch, HostId, Millis, Sample};
use mayfly_protocol::Mode;
use mayfly_testkit::attempts::{instance, save, start};
use mayfly_testkit::block_on;
use mayfly_testkit::clock::{FakeClock, SeededRng};
use mayfly_testkit::delivery::MemoryLedger;
use mayfly_testkit::journal::MemoryJournal;

pub const EPOCH: Epoch = Epoch::FIRST;
pub const NOW: Millis = Millis::new(1_700_000_000_000);

pub type Store = AggregateStore<MemoryJournal, FakeClock, SeededRng, Kicks>;
pub type Worker = DeliveryWorker<MemoryJournal, MemoryLedger<FakeClock>, SeededRng, FakeClock>;

pub struct Harness {
    pub journal: MemoryJournal,
    pub store: Store,
    pub clock: FakeClock,
    pub worker: Worker,
    pub sessions: HostSessions,
}

pub fn harness() -> Harness {
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(NOW);
    let store = AggregateStore::new::<ControlAggregates>(
        journal.clone(),
        clock.clone(),
        SeededRng::new(1),
        Kicks::new(64),
    )
    .unwrap_or_else(|e| panic!("{e}"))
    // the registration manager's pass reads the control projections (RT-07)
    .with_projections(control().unwrap_or_else(|e| panic!("{e}")));
    let store_clock = FakeClock::at(Millis::new(5_000));
    let worker = DeliveryWorker::new(
        journal.clone(),
        MemoryLedger::new(&journal, store_clock.clone()),
        SeededRng::new(3),
        store_clock.clone(),
        DeliveryConfig::default(),
    );
    let sessions = HostSessions::new();
    sessions.open(
        HostId::sample(),
        HostSession {
            epoch: EPOCH,
            mode: Mode::Normal,
        },
    );
    Harness {
        journal,
        store,
        clock,
        worker,
        sessions,
    }
}

pub fn stream(n: u128) -> StreamId {
    StreamId::new(&format!("attempt-{}", instance(n))).unwrap_or_else(|e| panic!("{e}"))
}

impl Harness {
    /// Starts attempt `n` under the session's epoch.
    pub fn start(&self, n: u128) {
        block_on(save(&self.store, n, 0x5000 + n, EPOCH, start(n)));
    }

    /// Records that the relay sent `directive` of attempt `n` with its issued deadline, under
    /// the relay's own command id.
    pub fn sent(&self, n: u128, directive: &DirectiveIssued) {
        block_on(save(
            &self.store,
            n,
            sent_of(directive.directive_id).as_u128(),
            EPOCH,
            AttemptCommand::RecordDirectiveSent {
                directive_id: directive.directive_id,
                deadline: directive.deadline,
            },
        ));
    }

    pub fn events(&self, n: u128) -> Vec<AttemptEvent> {
        block_on(self.journal.read_stream(&stream(n), None, None))
            .unwrap_or_else(|e| panic!("{e}"))
            .iter()
            .map(|e| decode_event::<AttemptEvent>(e).unwrap_or_else(|e| panic!("{e:?}")))
            .collect()
    }

    pub fn directives(&self, n: u128) -> Vec<DirectiveIssued> {
        self.events(n)
            .into_iter()
            .filter_map(|e| match e {
                AttemptEvent::DirectiveIssued(d) => Some(d),
                _ => None,
            })
            .collect()
    }

    pub fn state(&self, n: u128) -> AttemptState {
        block_on(
            self.store
                .load::<mayfly_control_domain::attempt::Attempt>(&instance(n)),
        )
        .unwrap_or_else(|e| panic!("{e}"))
    }
}
