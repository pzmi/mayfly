//! Both planes of one host, wired as `mayflyd` will wire them (RT-02), each over its own fakes:
//! the control plane's store, saga and directive relay on a journal and its delivery ledger;
//! the data plane's executor on the fake provider; the in-process link between them, opened by
//! a real handshake.
//!
//! [`Cp::record`] hands an answer to the control plane's observation inbox
//! (`mayfly_control::inbox`), as the link's receiving half will; a test that loses a reply simply
//! does not record it.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::num::NonZeroUsize;

use mayfly_adapter_link::inproc::{self, ChannelRx, ChannelTx};
use mayfly_adapter_link::{
    DirectiveReceiver, DirectiveSender, ObservationReceiver, ObservationSender,
};
use mayfly_adapter_sqlite::{SqliteJournal, SqliteLedger, SqliteOptions};
use mayfly_control::delivery::{
    DeliveryConfig, DeliveryLedger, DeliveryWorker, DirectiveRelay, Kicks, LinkSink, SinkConfig,
};
use mayfly_control::inbox::Inbox;
use mayfly_control::journal::{Journal, StreamId};
use mayfly_control::saga::{AttemptSaga, SagaConfig};
use mayfly_control::sessions::{HostSession, HostSessions};
use mayfly_control::store::{AggregateStore, Clock, decode_event};
use mayfly_control_domain::ControlAggregates;
use mayfly_control_domain::attempt::{Attempt, AttemptEvent, AttemptState, DirectiveIssued};
use mayfly_data::{Config, Diagnostics, Executor, GuestAnswer, ImageListing, Ports};
use mayfly_data_domain::{HostProbe, Sizing};
use mayfly_domain::{Epoch, HostId, ImageDigest, Millis, Resources, Sample};
use mayfly_protocol::{
    BaseName, CURRENT, Capacity, Directive, GuestFacts, GuestOs, HandshakeResponder as _,
    HostFacts, Mode, Observation, ObservationSource as _, SUPPORTED, Welcome, negotiate,
};
use mayfly_testkit::attempts::{FixedDetails, GIB, instance, save, start};
use mayfly_testkit::clock::{FakeClock, SeededRng};
use mayfly_testkit::delivery::MemoryLedger;
use mayfly_testkit::journal::MemoryJournal;
use mayfly_testkit_data::{
    FakeClock as DpClock, FakeGuest, FakeHostProbe, FakeImageStore, FakeVmProvider, GuestScript,
    MemoryFloor, SequentialIds,
};

/// The session both planes enter.
pub const EPOCH: Epoch = Epoch::FIRST;
/// The control plane's clock when a test starts (Unix-epoch milliseconds, ADR 0020).
pub const CP_NOW: Millis = Millis::new(1_700_000_000_000);

/// Where a control plane keeps its journal and ledger.
pub trait Backend {
    /// The journal.
    type J: Journal + Clone + 'static;
    /// Its delivery ledger.
    type L: DeliveryLedger;
    /// A handle on the journal.
    fn journal(&self) -> Self::J;
    /// A new ledger handle on the journal's store, judging leases by `clock`: one per relay
    /// worker, as two processes would hold.
    fn ledger(&self, clock: &FakeClock) -> Self::L;
}

/// `MemoryJournal` and `MemoryLedger`.
pub struct Memory(MemoryJournal);

impl Backend for Memory {
    type J = MemoryJournal;
    type L = MemoryLedger<FakeClock>;

    fn journal(&self) -> MemoryJournal {
        self.0.clone()
    }

    fn ledger(&self, clock: &FakeClock) -> MemoryLedger<FakeClock> {
        MemoryLedger::new(&self.0, clock.clone())
    }
}

/// The SQLite journal in a tempdir, and its ledger.
pub struct Sqlite {
    journal: SqliteJournal,
    _dir: tempfile::TempDir,
}

impl Backend for Sqlite {
    type J = SqliteJournal;
    type L = SqliteLedger<FakeClock>;

    fn journal(&self) -> SqliteJournal {
        self.journal.clone()
    }

    fn ledger(&self, clock: &FakeClock) -> SqliteLedger<FakeClock> {
        self.journal.ledger(clock.clone())
    }
}

pub fn memory() -> Memory {
    Memory(MemoryJournal::new())
}

pub fn sqlite() -> Sqlite {
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let options = SqliteOptions {
        writers: NonZeroUsize::MIN,
        ..SqliteOptions::new(CP_NOW)
    };
    let journal = SqliteJournal::open(&dir.path().join("journal.db"), options)
        .unwrap_or_else(|e| panic!("{e}"));
    Sqlite { journal, _dir: dir }
}

pub type Store<B> = AggregateStore<<B as Backend>::J, FakeClock, SeededRng, Kicks>;
pub type Worker<B, T> = DeliveryWorker<<B as Backend>::J, <B as Backend>::L, SeededRng, T>;
pub type Relay<'s, B, L, T> = DirectiveRelay<
    LinkSink<'s, <B as Backend>::J, FakeClock, SeededRng, Kicks, L, T, FixedDetails>,
>;

/// A delivery timer that never fires: a worker under it is never cut at its handle budget,
/// so a test decides alone when a held `handle` ends (the clock tests use cuts on purpose; these
/// do not).
#[derive(Debug, Clone, Copy)]
pub struct NeverFires;

impl Clock for NeverFires {
    fn now(&self) -> Millis {
        Millis::new(0)
    }

    fn sleep(&self, _duration: Millis) -> impl Future<Output = ()> + Send {
        std::future::pending()
    }
}

/// The control plane of one host.
pub struct Cp<B: Backend> {
    pub backend: B,
    pub store: Store<B>,
    /// The control plane's clock: commands, deadlines and the saga's timers.
    pub clock: FakeClock,
    /// The delivery ledger's store clock: leases and backoffs.
    pub store_clock: FakeClock,
    pub sessions: HostSessions,
    pub config: DeliveryConfig,
}

impl<B: Backend> Cp<B> {
    pub fn new(backend: B) -> Self {
        let store = AggregateStore::new::<ControlAggregates>(
            backend.journal(),
            FakeClock::at(CP_NOW),
            SeededRng::new(1),
            Kicks::new(64),
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let clock = store.clock().clone();
        Self {
            backend,
            store,
            clock,
            store_clock: FakeClock::at(Millis::new(5_000)),
            sessions: HostSessions::new(),
            config: DeliveryConfig::default(),
        }
    }

    /// A delivery worker with its own ledger handle, paced by `timer`.
    pub fn worker<T: mayfly_control::delivery::DeliveryTimer>(&self, timer: T) -> Worker<B, T> {
        DeliveryWorker::new(
            self.backend.journal(),
            self.backend.ledger(&self.store_clock),
            SeededRng::new(7),
            timer,
            self.config,
        )
    }

    /// The saga, issuing under the host sessions.
    pub fn saga(&self) -> AttemptSaga<'_, B::J, FakeClock, SeededRng, Kicks> {
        AttemptSaga::new(&self.store, self.sessions.clone(), SagaConfig::default())
    }

    /// A directive relay into `link`, each send bounded by `timer`.
    pub fn relay<L: mayfly_protocol::DirectiveSink, T: mayfly_control::delivery::DeliveryTimer>(
        &self,
        link: L,
        timer: T,
    ) -> Relay<'_, B, L, T> {
        DirectiveRelay::new(LinkSink::new(
            &self.store,
            link,
            timer,
            FixedDetails::default(),
            self.sessions.clone(),
            SinkConfig::default(),
            &self.config,
        ))
    }

    pub async fn start(&self, n: u128) {
        save(&self.store, n, 0x5000 + n, EPOCH, start(n)).await;
    }

    pub async fn events(&self, n: u128) -> Vec<AttemptEvent> {
        let stream =
            StreamId::new(&format!("attempt-{}", instance(n))).unwrap_or_else(|e| panic!("{e}"));
        self.store
            .journal()
            .read_stream(&stream, None, None)
            .await
            .unwrap_or_else(|e| panic!("{e}"))
            .iter()
            .map(|e| decode_event::<AttemptEvent>(e).unwrap_or_else(|e| panic!("{e:?}")))
            .collect()
    }

    pub async fn directives(&self, n: u128) -> Vec<DirectiveIssued> {
        self.events(n)
            .await
            .into_iter()
            .filter_map(|e| match e {
                AttemptEvent::DirectiveIssued(d) => Some(d),
                _ => None,
            })
            .collect()
    }

    pub async fn state(&self, n: u128) -> AttemptState {
        self.store
            .load::<Attempt>(&instance(n))
            .await
            .unwrap_or_else(|e| panic!("{e}"))
    }

    /// The observation inbox records an answer (P8); `n` names the attempt the answer is
    /// expected to reach.
    pub async fn record(&self, n: u128, observation: &Observation) {
        assert_eq!(
            observation
                .answers
                .and_then(|a| a.attempt)
                .map(|a| a.attempt_id),
            Some(mayfly_domain::AttemptId::from_u128(n)),
            "an answer about attempt {n}"
        );
        let recorded = Inbox::new(&self.store, HostId::sample(), EPOCH)
            .record(observation)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(recorded.accepted, 1, "{recorded:?}");
    }
}

pub type Exec =
    Executor<FakeVmProvider, FakeImageStore, FakeGuest, FakeHostProbe, DpClock, SequentialIds>;

/// The data plane of the sample host, over the data-plane kit's fakes.
pub struct Dp {
    pub provider: FakeVmProvider,
    pub floor: MemoryFloor,
    pub exec: Exec,
}

fn sizing() -> Sizing {
    Sizing {
        image_disk_bytes: Some(50 * GIB),
        base_disk_bytes: Some(60 * GIB),
        clone_disk_bytes: Some(80 * GIB),
        vm_memory_bytes: 8 * GIB,
    }
}

fn probe() -> HostProbe {
    let capacity = |vm_slots, memory_gib: u64, disk_gib: u64| Capacity {
        vm_slots,
        macos_guests: 2,
        resources: Resources::new(memory_gib * GIB, disk_gib * GIB),
    };
    HostProbe {
        totals: capacity(4, 64, 1000),
        reserve: Capacity::default(),
        foreign: Capacity::default(),
        foreign_unsized: BTreeSet::new(),
        disk_low_water: false,
        memory_pressure: false,
        hypervisor_limit: false,
    }
}

impl Dp {
    pub async fn start() -> Self {
        let provider = FakeVmProvider::new(Some(GuestOs::Linux), Some(80 * GIB));
        let images = FakeImageStore::new(ImageListing {
            images: BTreeSet::from([ImageDigest::sample()]),
            bases: BTreeMap::from([(
                BaseName::new("mfb-base").unwrap_or_else(|e| panic!("{e}")),
                GuestOs::Linux,
            )]),
            ..ImageListing::default()
        });
        let guest = FakeGuest::new(GuestScript {
            verify: Ok(GuestAnswer::Verified(GuestFacts {
                os: GuestOs::Linux,
                uptime: Millis::new(5),
            })),
            launch: Ok(mayfly_data::LaunchAnswer::Delivered),
            status: Ok(mayfly_data::RunnerStatus::NotLaunched),
            export: Ok(Diagnostics::Exported {
                bytes: 10,
                sha256: [7; 32],
            }),
        });
        let host = FakeHostProbe::new(
            probe(),
            HostFacts {
                disk_free_bytes: 500 * GIB,
                disk_low_water: false,
                woke: false,
                tart_version: "2.0.0".to_owned(),
                external_guests: 0,
            },
        );
        let floor = MemoryFloor::new(Epoch::new(0));
        let config = Config {
            host_id: HostId::sample(),
            sizing: sizing(),
            concurrency: 2,
            observe_every: Millis::new(10_000),
            listing_timeout: Millis::new(5_000),
            boot_id: "boot-1".to_owned(),
            dp_version: "0.1.0".to_owned(),
        };
        let ports = Ports {
            provider: provider.clone(),
            images,
            guest,
            host,
            clock: DpClock::new(Millis::new(1_000)),
            ids: SequentialIds::default(),
        };
        let exec = Exec::start(config, ports, floor.floor())
            .await
            .unwrap_or_else(|e| panic!("{e:?}"));
        Self {
            provider,
            floor,
            exec,
        }
    }
}

/// The control plane's halves of an open session.
pub struct CpLink {
    pub directives: DirectiveSender<ChannelTx>,
    pub observations: ObservationReceiver<ChannelRx>,
}

/// The data plane's halves of an open session.
pub struct DpLink {
    pub directives: DirectiveReceiver<ChannelRx>,
    pub observations: ObservationSender<ChannelTx>,
}

/// Opens a session at [`EPOCH`] over a new in-process link: the data plane says hello, the
/// control plane welcomes it with its clock, and both record the session (the control plane's
/// in its host sessions, in `mode`).
pub async fn connect<B: Backend>(cp: &Cp<B>, dp: &mut Dp, mode: Mode) -> (CpLink, DpLink) {
    let (mut dp_link, mut cp_link) = inproc::link(
        inproc::DEFAULT_CAPACITY,
        SUPPORTED.to_vec(),
        SUPPORTED.to_vec(),
    );
    let respond = async {
        let hello = cp_link.next_hello().await.unwrap_or_else(|e| panic!("{e}"));
        let accepted_version =
            negotiate(&hello.supported, SUPPORTED).unwrap_or_else(|e| panic!("{e:?}"));
        cp_link
            .welcome(Welcome {
                epoch: EPOCH,
                mode: Mode::Normal,
                accepted_version,
                cp_clock: Some(cp.clock.now()),
            })
            .await
            .unwrap_or_else(|e| panic!("{e}"));
    };
    let (entered, ()) = tokio::join!(dp.exec.enter(&mut dp_link, &dp.floor), respond);
    assert_eq!(entered, Ok(EPOCH));
    assert_eq!(cp_link.session(), Some(CURRENT));
    cp.sessions
        .open(HostId::sample(), HostSession { epoch: EPOCH, mode });
    let (directives, observations) = cp_link.split().unwrap_or_else(|e| panic!("{e}"));
    let (dp_directives, dp_observations) = dp_link.split().unwrap_or_else(|e| panic!("{e}"));
    (
        CpLink {
            directives,
            observations,
        },
        DpLink {
            directives: dp_directives,
            observations: dp_observations,
        },
    )
}

impl CpLink {
    /// The next observation that answers a directive (capacity reports and other unsolicited
    /// facts are skipped).
    pub async fn next_answer(&mut self) -> Observation {
        loop {
            let observation = self
                .observations
                .recv()
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            if observation.answers.is_some() {
                return observation;
            }
        }
    }
}

/// Runs `test` on a current-thread runtime: both planes and the script are polled on one
/// task, nothing spawned.
pub fn run<F: Future>(test: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap_or_else(|e| panic!("{e}"))
        .block_on(test)
}

/// A link's sending half that records every frame it forwards. While its `gate` is shut a send
/// waits there first (a relay worker whose send is still on its way when its lease runs out);
/// with `hold_after_send` it never returns once the frame is on the link (a relay killed
/// between the send and the advance, once its future is dropped).
#[derive(Clone)]
pub struct Tapped<L> {
    pub inner: L,
    pub sent: std::sync::Arc<std::sync::Mutex<Vec<Directive>>>,
    pub gate: Option<tokio::sync::watch::Receiver<bool>>,
    pub hold_after_send: bool,
    /// Notified when a send reaches the tap (before its gate): the signal a test waits on
    /// to know a relay worker is inside its send.
    pub entered: std::sync::Arc<tokio::sync::Notify>,
}

impl<L> Tapped<L> {
    pub fn new(inner: L) -> Self {
        Self {
            inner,
            sent: std::sync::Arc::default(),
            gate: None,
            hold_after_send: false,
            entered: std::sync::Arc::default(),
        }
    }

    /// The record of what this tap forwards, kept by a test after the tap (and the link
    /// sender in it) went to a relay that is dropped to close the link.
    pub fn log(&self) -> Log {
        Log(std::sync::Arc::clone(&self.sent))
    }
}

/// What a [`Tapped`] forwarded, in order.
pub struct Log(std::sync::Arc<std::sync::Mutex<Vec<Directive>>>);

impl Log {
    pub fn sent(&self) -> Vec<Directive> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl<L: mayfly_protocol::DirectiveSink + Clone> mayfly_protocol::DirectiveSink for Tapped<L> {
    fn send(
        &self,
        directive: Directive,
    ) -> impl Future<Output = Result<(), mayfly_protocol::LinkError>> + Send {
        let this = self.clone();
        async move {
            this.entered.notify_one();
            if let Some(mut gate) = this.gate.clone() {
                let _ = gate.wait_for(|open| *open).await;
            }
            this.sent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(directive.clone());
            let sent = this.inner.send(directive).await;
            if this.hold_after_send {
                std::future::pending::<()>().await;
            }
            sent
        }
    }
}
