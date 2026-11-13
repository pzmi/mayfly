//! What the start, inbox, recovery and repair tests share: a control-plane store over a
//! scripted in-memory journal with the control projections, attempts driven to a phase through
//! the store, a data plane's handshake and inventories.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use mayfly_control::journal::{Journal, StorageError, StreamId};
use mayfly_control::projection::{BuildId, Projections, control};
use mayfly_control::recovery::{Opened, Recovery, Startup, StartupConfig, StructureCheck};
use mayfly_control::saga::{AttemptSaga, SagaConfig};
use mayfly_control::sessions::HostSessions;
use mayfly_control::store::{
    AggregateStore, Clock as _, NoNotifier, Outcome, SaveRequest, decode_event,
};
use mayfly_control_domain::attempt::{
    Attempt, AttemptCommand, AttemptEvent, AttemptState, Fact, GuestFacts, GuestOs, GuestRejection,
    Observation as AttemptObservation, RetireReason, RunnerRequestId,
};
use mayfly_control_domain::host::{ApplyPoolTerms, Host, HostCommand, HostState, ReserveSlot};
use mayfly_control_domain::operations::hostrun::{HostRun, HostRunState};
use mayfly_control_domain::pool::TrustTier;
use mayfly_control_domain::{Actor, Command, ControlAggregates};
use mayfly_domain::{
    AttemptId, CommandId, Deadline, DirectiveId, Epoch, EventId, Generation, HostId, Millis,
    ObservationId, OwnedVmName, PoolName, ProviderInstanceId, Resources, Sample, SecretHandleId,
    Slot,
};
use mayfly_protocol::{
    HandshakeRefused, HandshakeResponder, Hello, InventoryGap, LinkError, Observation,
    ObservationKind, SUPPORTED, VmObservation, Welcome,
};
use mayfly_testkit::attempts::start;
use mayfly_testkit::block_on;
use mayfly_testkit::clock::{FakeClock, SeededRng};
use mayfly_testkit::journal::MemoryJournal;

use crate::scripted::ScriptedJournal;

/// The control plane's clock when a test starts.
pub const NOW: Millis = Millis::new(1_700_000_000_000);
/// The epoch the attempts were started and driven under, before any restart.
pub const BEFORE: Epoch = Epoch::FIRST;

pub type Store = AggregateStore<ScriptedJournal, FakeClock, SeededRng, NoNotifier>;

/// A store and what it runs over.
pub struct Kit {
    pub memory: MemoryJournal,
    pub journal: ScriptedJournal,
    pub clock: FakeClock,
    pub store: Store,
    pub sessions: HostSessions,
}

/// Command ids unique across every kit of a test binary (two kits share a journal).
static NEXT: AtomicU64 = AtomicU64::new(1);

pub fn projections() -> Projections {
    control().unwrap_or_else(|e| panic!("{e}"))
}

pub fn build() -> BuildId {
    BuildId::new("0.1.0").unwrap_or_else(|e| panic!("{e}"))
}

/// A kit over a fresh journal, its store keeping the control projections.
pub fn kit() -> Kit {
    kit_over(&MemoryJournal::new())
}

/// A kit over `memory`: a second process on the same journal.
pub fn kit_over(memory: &MemoryJournal) -> Kit {
    let journal = ScriptedJournal::new(memory.clone());
    let clock = FakeClock::at(NOW);
    let store = AggregateStore::new::<ControlAggregates>(
        journal.clone(),
        clock.clone(),
        SeededRng::new(3),
        NoNotifier,
    )
    .unwrap_or_else(|e| panic!("{e}"))
    .with_projections(projections());
    Kit {
        memory: memory.clone(),
        journal,
        clock,
        store,
        sessions: HostSessions::new(),
    }
}

pub fn host() -> HostId {
    HostId::sample()
}

pub fn attempt(n: u128) -> AttemptId {
    AttemptId::from_u128(n)
}

pub fn vm_name(n: u128) -> OwnedVmName {
    OwnedVmName::new(
        PoolName::sample(),
        host(),
        Slot::new(u32::try_from(n).unwrap_or_else(|e| panic!("{e}"))),
        Generation::FIRST,
        attempt(n),
    )
}

pub fn instance() -> ProviderInstanceId {
    ProviderInstanceId::sample()
}

pub fn other_instance() -> ProviderInstanceId {
    ProviderInstanceId::new("0a:00:00:00:00:02").unwrap_or_else(|e| panic!("{e}"))
}

pub fn stream(name: &str) -> StreamId {
    StreamId::new(name).unwrap_or_else(|e| panic!("{e}"))
}

impl Kit {
    fn mint(&self) -> CommandId {
        CommandId::from_u128(0x7E57_0000_0000 + u128::from(NEXT.fetch_add(1, Ordering::SeqCst)))
    }

    /// Saves `body` on attempt `n` under `epoch` as the saga would, returning the outcome.
    pub fn command(
        &self,
        n: u128,
        epoch: Epoch,
        body: AttemptCommand,
    ) -> Outcome<mayfly_control_domain::attempt::AttemptRejection> {
        let command = Command::new(
            self.mint(),
            self.clock.now(),
            epoch,
            Actor::Automation {
                policy: "attempt_saga",
            },
            body,
        );
        block_on(
            self.store
                .execute::<Attempt>(SaveRequest::new(attempt(n), command)),
        )
        .unwrap_or_else(|e| panic!("{e}"))
    }

    /// Starts attempt `n` and drives it along `path` under [`BEFORE`].
    ///
    /// # Panics
    /// When a step is refused: the path is wrong.
    pub fn drive(&self, n: u128, path: Vec<AttemptCommand>) {
        self.lease(n);
        for (index, body) in std::iter::once(start(n)).chain(path).enumerate() {
            if let Outcome::Refused { rejection, .. } = self.command(n, BEFORE, body) {
                panic!("step {index} of attempt {n}'s path refused: {rejection:?}");
            }
        }
    }

    /// A `Host` command under [`BEFORE`], which must be accepted.
    pub fn host_command(&self, body: HostCommand) {
        let command = Command::new(
            self.mint(),
            self.clock.now(),
            BEFORE,
            Actor::Automation {
                policy: "reconciler",
            },
            body,
        );
        match block_on(
            self.store
                .execute::<Host>(SaveRequest::new(host().as_str(), command)),
        )
        .unwrap_or_else(|e| panic!("{e}"))
        {
            Outcome::Accepted { .. } => {}
            Outcome::Refused { rejection, .. } => panic!("host command refused: {rejection:?}"),
        }
    }

    /// Reserves attempt `n`'s slot on the host as the reconciler would, registering the host,
    /// its first report and the sample pool's terms first if they are missing.
    pub fn lease(&self, n: u128) {
        let state = self.host_state();
        if !state.registered {
            self.host_command(HostCommand::RegisterHost {
                reserves: Resources::new(0, 0),
            });
        }
        if state.capacity.is_none() {
            self.host_command(HostCommand::RecordCapacityReport {
                report_id: 1,
                capacity: mayfly_testkit::fleet::report(64, 0, 4),
            });
        }
        if state.pool_terms.is_empty() {
            self.host_command(HostCommand::ApplyPoolTerms(ApplyPoolTerms {
                pool: PoolName::sample(),
                revision: 1,
                open: true,
                max_running: 4,
                tier: TrustTier::Trusted,
            }));
        }
        self.host_command(HostCommand::ReserveSlot(ReserveSlot {
            attempt_id: attempt(n),
            pool: PoolName::sample(),
            os: GuestOs::Linux,
            resources: Resources::new(8 << 30, 50 << 30),
            terms_revision: self.terms_revision(),
        }));
    }

    pub fn state(&self, n: u128) -> AttemptState {
        block_on(self.store.load::<Attempt>(&attempt(n).to_string()))
            .unwrap_or_else(|e| panic!("{e}"))
    }

    pub fn events(&self, n: u128) -> Vec<AttemptEvent> {
        block_on(
            self.memory
                .read_stream(&stream(&format!("attempt-{}", attempt(n))), None, None),
        )
        .unwrap_or_else(|e| panic!("{e}"))
        .iter()
        .map(|e| decode_event::<AttemptEvent>(e).unwrap_or_else(|e| panic!("{e:?}")))
        .collect()
    }

    /// The sample pool's terms revision as the host applied it.
    pub fn terms_revision(&self) -> u64 {
        self.host_state()
            .pool_terms
            .get(&PoolName::sample())
            .map_or(0, |t| t.revision)
    }

    pub fn hostrun(&self) -> HostRunState {
        block_on(self.store.load::<HostRun>(host().as_str())).unwrap_or_else(|e| panic!("{e}"))
    }

    pub fn host_state(&self) -> HostState {
        block_on(self.store.load::<Host>(host().as_str())).unwrap_or_else(|e| panic!("{e}"))
    }

    pub fn startup(
        &self,
        crash_loop: bool,
    ) -> Startup<'_, ScriptedJournal, FakeClock, SeededRng, NoNotifier> {
        Startup::new(
            &self.store,
            self.sessions.clone(),
            StartupConfig {
                host: host(),
                boot_id: "boot-1".to_owned(),
                version: "0.1.0".to_owned(),
                reserves: Resources::new(0, 0),
                crash_loop,
            },
        )
    }

    /// A first run that recovered an empty host at epoch 1, so attempts driven after it belong
    /// to a trusted journal whose runs minted epoch 1.
    pub fn previous_run(&self) {
        let (opened, _) = self.open(0);
        assert_eq!(opened.epoch, BEFORE);
        let saga = AttemptSaga::new(&self.store, self.sessions.clone(), SagaConfig::default());
        let recovered = block_on(self.startup(false).recover(
            &inventory(0x1, BEFORE, Vec::new()),
            &BTreeMap::new(),
            &saga,
        ))
        .unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(recovered, Recovery::Completed(_)));
    }

    /// Start-up's first half with a data plane at `floor` and a store whose structure verifies.
    pub fn open(&self, floor: u64) -> (Opened, Responder) {
        let mut link = Responder::new(hello(floor));
        let opened = block_on(self.startup(false).open(
            &mut link,
            &Structure(true),
            (&self.memory, &projections(), &build()),
        ))
        .unwrap_or_else(|e| panic!("{e}"));
        (opened, link)
    }
}

/// The paths an attempt takes to a phase, as the saga and the inbox would drive it.
pub mod path {
    use super::{
        AttemptCommand, AttemptObservation, BEFORE, Deadline, DirectiveId, EventId, Fact,
        Generation, GuestFacts, GuestOs, GuestRejection, Millis, ObservationId, RetireReason,
        RunnerRequestId, SecretHandleId, instance,
    };

    pub fn directive(n: u128, k: u128) -> DirectiveId {
        DirectiveId::from_u128((n << 16) + k)
    }

    pub fn issue(n: u128, k: u128) -> AttemptCommand {
        AttemptCommand::IssueNext {
            directive_id: directive(n, k),
            deadline: Deadline::at(Millis::new(1_700_000_060_000)),
        }
    }

    pub fn sent(n: u128, k: u128) -> AttemptCommand {
        AttemptCommand::RecordDirectiveSent {
            directive_id: directive(n, k),
            deadline: Deadline::at(Millis::new(1_700_000_060_000)),
        }
    }

    pub fn observe(n: u128, k: u128, fact: Fact) -> AttemptCommand {
        AttemptCommand::RecordObservation(AttemptObservation {
            observation_id: ObservationId::from_u128((n << 16) + 0x8000 + k),
            epoch: BEFORE,
            generation: Generation::FIRST,
            fact,
        })
    }

    fn facts() -> GuestFacts {
        GuestFacts {
            os: GuestOs::Linux,
            uptime: Millis::new(30_000),
        }
    }

    pub fn cloning(n: u128) -> Vec<AttemptCommand> {
        vec![issue(n, 1)]
    }

    pub fn unknown_clone(n: u128) -> Vec<AttemptCommand> {
        let mut p = cloning(n);
        p.push(observe(
            n,
            1,
            Fact::OutcomeUnknown {
                directive_id: directive(n, 1),
            },
        ));
        p
    }

    pub fn booting(n: u128) -> Vec<AttemptCommand> {
        let mut p = cloning(n);
        p.push(observe(
            n,
            1,
            Fact::VmCloned {
                directive_id: directive(n, 1),
                instance_id: instance(),
            },
        ));
        p
    }

    pub fn verifying(n: u128) -> Vec<AttemptCommand> {
        let mut p = booting(n);
        p.push(issue(n, 2));
        p.push(observe(
            n,
            2,
            Fact::VmBooted {
                directive_id: directive(n, 2),
            },
        ));
        p
    }

    pub fn prepared(n: u128) -> Vec<AttemptCommand> {
        let mut p = verifying(n);
        p.push(issue(n, 3));
        p.push(observe(
            n,
            3,
            Fact::GuestVerified {
                directive_id: directive(n, 3),
                facts: facts(),
            },
        ));
        p
    }

    pub fn quarantined(n: u128) -> Vec<AttemptCommand> {
        let mut p = verifying(n);
        p.push(issue(n, 3));
        p.push(observe(
            n,
            3,
            Fact::GuestRejected {
                directive_id: directive(n, 3),
                reason: GuestRejection::ChallengeFailed,
            },
        ));
        p
    }

    pub fn activating(n: u128) -> Vec<AttemptCommand> {
        let mut p = prepared(n);
        p.push(AttemptCommand::BeginActivation {
            grant_ref: EventId::from_u128((n << 16) + 1),
        });
        p.push(issue(n, 4));
        p.push(observe(
            n,
            4,
            Fact::GuestVerified {
                directive_id: directive(n, 4),
                facts: facts(),
            },
        ));
        p
    }

    pub fn jit_issued(n: u128) -> Vec<AttemptCommand> {
        let mut p = activating(n);
        p.push(AttemptCommand::RecordJitHandle {
            handle_id: SecretHandleId::from_u128((n << 16) + 1),
            registration_ref: EventId::from_u128((n << 16) + 2),
        });
        p
    }

    pub fn listening(n: u128) -> Vec<AttemptCommand> {
        let mut p = jit_issued(n);
        p.push(issue(n, 5));
        p.push(observe(
            n,
            5,
            Fact::RunnerLaunched {
                directive_id: directive(n, 5),
            },
        ));
        p
    }

    pub fn busy(n: u128) -> Vec<AttemptCommand> {
        let mut p = listening(n);
        p.push(AttemptCommand::BindJob {
            request_id: RunnerRequestId(1),
        });
        p
    }

    pub fn stopped(n: u128) -> Vec<AttemptCommand> {
        let mut p = busy(n);
        p.push(observe(n, 6, Fact::RunnerExited { code: 0 }));
        p.push(AttemptCommand::RequestRetirement {
            reason: RetireReason::JobDone,
        });
        p.push(issue(n, 6));
        p.push(observe(
            n,
            7,
            Fact::DiagnosticsExported {
                directive_id: directive(n, 6),
                bytes: 4_096,
                sha256: [7; 32],
            },
        ));
        p.push(issue(n, 7));
        p.push(observe(
            n,
            8,
            Fact::VmStopped {
                directive_id: directive(n, 7),
            },
        ));
        p
    }

    pub fn deleting(n: u128) -> Vec<AttemptCommand> {
        let mut p = stopped(n);
        p.push(issue(n, 8));
        p
    }

    pub fn deleted(n: u128) -> Vec<AttemptCommand> {
        let mut p = deleting(n);
        p.push(observe(
            n,
            9,
            Fact::VmDeleted {
                directive_id: directive(n, 8),
            },
        ));
        p
    }
}

/// The hello of the sample host's data plane at `floor`.
pub fn hello(floor: u64) -> Hello {
    Hello {
        host_id: host(),
        boot_id: "dp-boot-1".to_owned(),
        dp_version: "0.1.0".to_owned(),
        supported: SUPPORTED.to_vec(),
        capabilities: BTreeSet::new(),
        epoch_floor: Epoch::new(floor),
    }
}

/// The control plane's end of a handshake: one queued hello, and what it was answered.
pub struct Responder {
    hello: Option<Hello>,
    pub welcomed: Arc<Mutex<Vec<Welcome>>>,
    pub refused: Arc<Mutex<Vec<HandshakeRefused>>>,
}

impl Responder {
    pub fn new(hello: Hello) -> Self {
        Self {
            hello: Some(hello),
            welcomed: Arc::default(),
            refused: Arc::default(),
        }
    }

    pub fn welcomes(&self) -> Vec<Welcome> {
        self.welcomed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl HandshakeResponder for Responder {
    async fn next_hello(&mut self) -> Result<Hello, LinkError> {
        self.hello.take().ok_or(LinkError::Closed)
    }

    async fn welcome(&mut self, welcome: Welcome) -> Result<(), LinkError> {
        self.welcomed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(welcome);
        Ok(())
    }

    async fn refuse(&mut self, refused: HandshakeRefused) -> Result<(), LinkError> {
        self.refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(refused);
        Ok(())
    }
}

/// A store whose structure verifies (or not).
pub struct Structure(pub bool);

impl StructureCheck for Structure {
    async fn verify(&self) -> Result<bool, StorageError> {
        Ok(self.0)
    }
}

/// Attempt `n`'s VM as a complete listing shows it.
pub fn listed(n: u128, instance_id: ProviderInstanceId, running: bool) -> VmObservation {
    named(&vm_name(n).to_string(), Some(instance_id), running)
}

pub fn named(name: &str, instance_id: Option<ProviderInstanceId>, running: bool) -> VmObservation {
    VmObservation {
        name: name.to_owned(),
        instance_id,
        running,
        guest: None,
        memory_bytes: None,
        disk_bytes: None,
    }
}

/// A complete inventory of `vms`, observation `id`, in `epoch`.
pub fn inventory(id: u128, epoch: Epoch, vms: Vec<VmObservation>) -> Observation {
    Observation {
        observation_id: ObservationId::from_u128(id),
        epoch,
        answers: None,
        observed_at_dp: Millis::new(5),
        kind: ObservationKind::Inventory {
            vms,
            image_store: None,
        },
    }
}

pub fn incomplete(id: u128, epoch: Epoch) -> Observation {
    Observation {
        observation_id: ObservationId::from_u128(id),
        epoch,
        answers: None,
        observed_at_dp: Millis::new(5),
        kind: ObservationKind::InventoryIncomplete {
            reason: InventoryGap::ListFailed,
        },
    }
}
