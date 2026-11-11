//! A fleet in a real `AggregateStore` over `MemoryJournal`, for the reconciler's component tests
//! and its seeded simulation (CP-03): hosts with capacity reports, pools with their scale sets
//! and terms, and a world that plays the other policies (P2, P4, P6, P15, P16), the data plane's
//! answers and GitHub's statistics, so a reconciler pass can be run against it step by step.
//!
//! [`simulate`] runs a seeded sequence of world steps and reconciler passes and checks, after
//! each step, the invariants the reconciler must keep (`doc/domain-model.md` §5.4): no slot
//! generation reused (I02), never allocated beyond the applied report (I03), the reconciler
//! touches no VM and only attempts and leases the journal records (I04), the reported max fits
//! the deliverable and the carve-outs fit each host together (I07, I24), nothing admitted on a
//! draining host (I08), acquisition within the deliverable (I23), nothing admitted while the gate
//! or the pool's terms are closed (I26), nothing admitted for a tripped breaker (I27), and the
//! `Host` never refuses a reservation the pass planned for capacity: the plan and the report
//! agree.

use std::collections::{BTreeMap, BTreeSet};

use mayfly_control::delivery::Kicks;
use mayfly_control::projection::control;
use mayfly_control::reconciler::{PassReport, Reconciler, ReconcilerConfig};
use mayfly_control::store::{AggregateStore, Clock, NoNotifier, Outcome, Rng, SaveRequest};
use mayfly_control_domain::attempt::RunnerRequestId;
use mayfly_control_domain::attempt::{
    Attempt, AttemptCommand, AttemptState, DirectiveTag, Fact, GuestFacts, GuestOs, Observation,
    Phase, QuarantineReason, RefusalReason, RetireReason, SagaStep, Stage, StartAttempt, next_step,
};
use mayfly_control_domain::capacity::{Footprint, ReportedCapacity};
use mayfly_control_domain::demand::Statistics;
use mayfly_control_domain::demand::{EventName, Offer};
use mayfly_control_domain::host::{
    AdmissionCause, ApplyPoolTerms, Drain, Host, HostCommand, HostState,
};
use mayfly_control_domain::pool::{ConfigurePool, Pool, PoolCommand, PoolState, TrustTier, terms};
use mayfly_control_domain::reconcile::{PassCommand, PolicyOf};
use mayfly_control_domain::registration::{JobResult, ScaleSetId};
use mayfly_control_domain::scaleset::{
    EnsureScaleSet, JobFact, MessageId, MessageItem, RecordMessage, ScaleSet, ScaleSetCommand,
    ScaleSetState, SessionId,
};
use mayfly_control_domain::{Actor, Aggregate, Command, Rejection};
use mayfly_domain::{
    AttemptId, CommandId, Deadline, DirectiveId, Epoch, EventId, Generation, HostId, ImageDigest,
    Millis, ObservationId, PoolName, ProviderInstanceId, Resources, Sample, Slot,
};

use crate::block_on;
use crate::clock::{FakeClock, SeededRng};
use crate::journal::MemoryJournal;

/// The store a fleet lives in.
pub type FleetStore = AggregateStore<MemoryJournal, FakeClock, SeededRng, NoNotifier>;

/// A GiB.
pub const GIB: u64 = 1 << 30;

/// The fleet's run epoch.
pub const EPOCH: Epoch = Epoch::new(1);

/// The instant the fleet's clock starts at.
pub const T0: Millis = Millis::new(1_700_000_000_000);

/// A store over `journal` with the control projections, reading `clock`.
///
/// # Panics
/// When the store cannot be built: the kit is wrong.
#[must_use]
pub fn store(journal: &MemoryJournal, clock: &FakeClock) -> FleetStore {
    AggregateStore::new::<mayfly_control_domain::ControlAggregates>(
        journal.clone(),
        clock.clone(),
        SeededRng::new(11),
        NoNotifier,
    )
    .unwrap_or_else(|e| panic!("{e}"))
    .with_projections(control().unwrap_or_else(|e| panic!("{e}")))
}

/// A report of `memory_gib` memory, 1 TiB of disk, `slots` VM slots and two macOS guests, with
/// `foreign_gib` of foreign memory.
#[must_use]
pub const fn report(memory_gib: u64, foreign_gib: u64, slots: u32) -> ReportedCapacity {
    ReportedCapacity {
        total: Resources::new(memory_gib * GIB, 1024 * GIB),
        foreign: Resources::new(foreign_gib * GIB, 0),
        base: Resources::new(0, 0),
        vm_slots: slots,
        foreign_vms: 0,
        macos_guests: 2,
        foreign_macos_guests: 0,
    }
}

/// A pool's shape in the fleet.
#[derive(Debug, Clone)]
pub struct PoolSpec {
    /// The pool.
    pub name: PoolName,
    /// Its guest OS.
    pub os: GuestOs,
    /// Memory per VM, GiB.
    pub memory_gib: u64,
    /// Its `max_running`.
    pub max_running: u32,
    /// Its `min_prepared`.
    pub min_prepared: u32,
    /// Its breaker threshold.
    pub breaker: u32,
}

/// The fleet's world: every command a policy other than the reconciler, the data plane or
/// GitHub would cause, issued through the store.
#[derive(Debug)]
pub struct World<'s> {
    store: &'s FleetStore,
    next: u128,
    hosts: Vec<HostId>,
    pools: Vec<PoolSpec>,
    message: u64,
    request: u64,
    report_id: BTreeMap<HostId, u64>,
}

fn pool_name(name: &str) -> PoolName {
    PoolName::new(name).unwrap_or_else(|e| panic!("{e}"))
}

fn session() -> SessionId {
    SessionId { high: 1, low: 1 }
}

impl<'s> World<'s> {
    /// An empty world over `store`.
    #[must_use]
    pub const fn new(store: &'s FleetStore) -> Self {
        Self {
            store,
            next: 1,
            hosts: Vec::new(),
            pools: Vec::new(),
            message: 0,
            request: 0,
            report_id: BTreeMap::new(),
        }
    }

    /// The hosts registered.
    #[must_use]
    pub fn hosts(&self) -> &[HostId] {
        &self.hosts
    }

    /// The pools configured.
    #[must_use]
    pub fn pools(&self) -> &[PoolSpec] {
        &self.pools
    }

    fn mint(&mut self) -> CommandId {
        self.next += 1;
        CommandId::from_u128(0x5157_0000_0000 + self.next)
    }

    /// Saves `body` on `instance` of `A` as `actor`.
    ///
    /// # Panics
    /// When the store fails: the kit is wrong.
    pub fn command<A: Aggregate>(
        &mut self,
        instance: &str,
        actor: Actor,
        body: A::Body,
    ) -> Result<(), &'static str>
    where
        A::Body: Clone,
    {
        let command = Command::new(self.mint(), self.store.clock().now(), EPOCH, actor, body);
        match block_on(self.store.execute::<A>(SaveRequest::new(instance, command)))
            .unwrap_or_else(|e| panic!("{e}"))
        {
            Outcome::Accepted { .. } => Ok(()),
            Outcome::Refused { rejection, .. } => Err(rejection.code()),
        }
    }

    /// Registers `host` and applies its first report.
    pub fn register_host(&mut self, host: &str, capacity: ReportedCapacity) {
        let id = HostId::new(host).unwrap_or_else(|e| panic!("{e}"));
        let _ = self.command::<Host>(
            host,
            Actor::Startup,
            HostCommand::RegisterHost {
                reserves: Resources::new(0, 0),
            },
        );
        self.hosts.push(id.clone());
        self.report(&id, capacity);
        for pool in self.pools.clone() {
            self.apply_terms(&pool.name);
        }
    }

    /// Applies a newer capacity report on `host` (P22).
    pub fn report(&mut self, host: &HostId, capacity: ReportedCapacity) {
        let id = self.report_id.entry(host.clone()).or_insert(0);
        *id += 1;
        let report_id = *id;
        let _ = self.command::<Host>(
            host.as_str(),
            Actor::Automation { policy: "inbox" },
            HostCommand::RecordCapacityReport {
                report_id,
                capacity,
            },
        );
    }

    /// Configures `spec`, pins the sample image, ensures its scale set and opens a session with
    /// no job assigned, then applies its terms on every host (P4).
    pub fn configure_pool(&mut self, spec: PoolSpec) {
        let pool = spec.name.clone();
        let _ = self.command::<Pool>(
            pool.as_str(),
            Actor::Operator { uid: 501 },
            PoolCommand::ConfigurePool(ConfigurePool {
                revision: 1,
                tier: TrustTier::Trusted,
                os: spec.os,
                vm_resources: Resources::new(spec.memory_gib * GIB, 50 * GIB),
                max_running: spec.max_running,
                min_prepared: Some(spec.min_prepared),
                breaker_threshold: spec.breaker,
                breaker_window: Millis::new(30 * 60_000),
            }),
        );
        let _ = self.command::<Pool>(
            pool.as_str(),
            Actor::Automation { policy: "image" },
            PoolCommand::PinImage {
                digest: ImageDigest::sample(),
                verified_ref: Some(EventId::from_u128(7)),
            },
        );
        let _ = self.command::<ScaleSet>(
            pool.as_str(),
            Actor::Startup,
            ScaleSetCommand::EnsureScaleSet(EnsureScaleSet {
                pool: pool.clone(),
                github_id: ScaleSetId(u64::try_from(self.pools.len()).unwrap_or(0) + 1),
                tier: TrustTier::Trusted,
            }),
        );
        let actor = Actor::Automation { policy: "listener" };
        let _ = self.command::<ScaleSet>(pool.as_str(), actor, ScaleSetCommand::BeginOpen);
        let _ = self.command::<ScaleSet>(
            pool.as_str(),
            actor,
            ScaleSetCommand::RecordSessionOpened {
                session_id: session(),
                statistics: Statistics::default(),
            },
        );
        self.pools.push(spec);
        self.apply_terms(&pool);
    }

    /// P4: the pool's current terms on every host.
    pub fn apply_terms(&mut self, pool: &PoolName) {
        let state =
            block_on(self.store.load::<Pool>(pool.as_str())).unwrap_or_else(|e| panic!("{e}"));
        let Some(t) = terms(&state) else {
            return;
        };
        for host in self.hosts.clone() {
            let _ = self.command::<Host>(
                host.as_str(),
                Actor::Automation { policy: "terms" },
                HostCommand::ApplyPoolTerms(ApplyPoolTerms {
                    pool: pool.clone(),
                    revision: t.revision,
                    open: t.open,
                    max_running: t.max_running,
                    tier: t.tier,
                }),
            );
        }
    }

    /// GitHub's statistics for `pool`: `assigned` jobs, `offers` new offers, and every job
    /// acquired so far completed when `complete` is set; the acquire call then answers every
    /// planned offer acquired.
    pub fn statistics(&mut self, pool: &PoolName, assigned: u32, offers: u32, complete: bool) {
        let before = self.scale_set(pool);
        let mut items: Vec<MessageItem> = Vec::new();
        if complete {
            for request_id in &before.acquired {
                items.push(MessageItem::Fact(JobFact::Completed {
                    request_id: *request_id,
                    result: JobResult::Succeeded,
                    runner_id: None,
                }));
            }
        }
        for _ in 0..offers {
            self.request += 1;
            items.push(MessageItem::Offer(Offer {
                request_id: RunnerRequestId(self.request),
                event_name: EventName::Push,
            }));
        }
        self.message += 1;
        let actor = Actor::Automation { policy: "listener" };
        let _ = self.command::<ScaleSet>(
            pool.as_str(),
            actor,
            ScaleSetCommand::RecordMessage(RecordMessage {
                session_id: session(),
                message_id: MessageId(self.message),
                statistics: Some(Statistics {
                    total_assigned_jobs: assigned,
                    ..Statistics::default()
                }),
                items,
            }),
        );
        let after = self.scale_set(pool);
        if !after.acquiring.is_empty() {
            let _ = self.command::<ScaleSet>(
                pool.as_str(),
                actor,
                ScaleSetCommand::RecordAcquireResult {
                    requested: after.acquiring.clone(),
                    acquired: after.acquiring,
                },
            );
        }
    }

    /// The scale set of `pool`.
    #[must_use]
    pub fn scale_set(&self, pool: &PoolName) -> ScaleSetState {
        block_on(self.store.load::<ScaleSet>(pool.as_str())).unwrap_or_else(|e| panic!("{e}"))
    }

    /// The host `host`.
    #[must_use]
    pub fn host(&self, host: &HostId) -> HostState {
        block_on(self.store.load::<Host>(host.as_str())).unwrap_or_else(|e| panic!("{e}"))
    }

    /// The pool `pool`.
    #[must_use]
    pub fn pool(&self, pool: &PoolName) -> PoolState {
        block_on(self.store.load::<Pool>(pool.as_str())).unwrap_or_else(|e| panic!("{e}"))
    }

    /// The attempt `attempt`.
    #[must_use]
    pub fn attempt(&self, attempt: AttemptId) -> AttemptState {
        block_on(self.store.load::<Attempt>(&attempt.to_string())).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Every lease of every host whose attempt has not started, by host.
    #[must_use]
    pub fn unstarted(&self) -> Vec<(HostId, AttemptId)> {
        let mut out = Vec::new();
        for host in &self.hosts {
            for attempt in self.host(host).leases.keys() {
                if self.attempt(*attempt).identity.is_none() {
                    out.push((host.clone(), *attempt));
                }
            }
        }
        out
    }

    /// Every started attempt that holds a lease, with its host.
    #[must_use]
    pub fn leased_attempts(&self) -> Vec<(HostId, AttemptId)> {
        let mut out = Vec::new();
        for host in &self.hosts {
            for attempt in self.host(host).leases.keys() {
                if self.attempt(*attempt).identity.is_some() {
                    out.push((host.clone(), *attempt));
                }
            }
        }
        out
    }

    /// P2: `StartAttempt` for the lease of `attempt` on `host`.
    pub fn start(&mut self, host: &HostId, attempt: AttemptId) {
        let state = self.host(host);
        let Some(lease) = state.leases.get(&attempt) else {
            return;
        };
        let body = AttemptCommand::StartAttempt(StartAttempt {
            attempt_id: attempt,
            host_id: host.clone(),
            slot: lease.slot,
            generation: lease.generation,
            pool: lease.pool.clone(),
            image_digest: ImageDigest::sample(),
            retry_budget: 2,
        });
        let _ = self.command::<Attempt>(
            &attempt.to_string(),
            Actor::Automation { policy: "start" },
            body,
        );
    }

    /// The data plane answers the attempt's pending directive with `refusal`, or with success;
    /// with no directive pending, the saga issues the next one or closes the stream (P6). A stage
    /// failure that retires the attempt is counted on its pool (P15) and the terms re-applied.
    pub fn progress(&mut self, attempt: AttemptId, refusal: Option<RefusalReason>) {
        let state = self.attempt(attempt);
        let Some(identity) = state.identity.clone() else {
            return;
        };
        let instance = attempt.to_string();
        let saga = Actor::Automation {
            policy: "attempt_saga",
        };
        if let Some(pending) = state.pending {
            let directive_id = pending.directive_id;
            let fact = match refusal {
                Some(reason) => Fact::DirectiveRefused {
                    directive_id,
                    reason,
                },
                None => match pending.tag {
                    DirectiveTag::CloneVm => Fact::VmCloned {
                        directive_id,
                        instance_id: ProviderInstanceId::sample(),
                    },
                    DirectiveTag::StartVm => Fact::VmBooted { directive_id },
                    DirectiveTag::VerifyGuest => Fact::GuestVerified {
                        directive_id,
                        facts: GuestFacts {
                            os: GuestOs::Linux,
                            uptime: Millis::new(1_000),
                        },
                    },
                    DirectiveTag::LaunchRunner => Fact::RunnerLaunched { directive_id },
                    DirectiveTag::ExportDiagnostics => Fact::DiagnosticsExported {
                        directive_id,
                        bytes: 512,
                        sha256: [7; 32],
                    },
                    DirectiveTag::StopVm => Fact::VmStopped { directive_id },
                    DirectiveTag::DeleteVm => Fact::VmDeleted { directive_id },
                },
            };
            let observation = self.mint();
            let _ = self.command::<Attempt>(
                &instance,
                saga,
                AttemptCommand::RecordObservation(Observation {
                    observation_id: ObservationId::from_u128(observation.as_u128()),
                    epoch: EPOCH,
                    generation: identity.generation,
                    fact,
                }),
            );
        } else {
            match next_step(&state) {
                SagaStep::Issue(..) => {
                    let directive = self.mint();
                    let _ = self.command::<Attempt>(
                        &instance,
                        saga,
                        AttemptCommand::IssueNext {
                            directive_id: DirectiveId::from_u128(directive.as_u128()),
                            deadline: Deadline::after(
                                self.store.clock().now(),
                                Millis::new(600_000),
                            ),
                        },
                    );
                }
                SagaStep::Close => {
                    let _ = self.command::<Attempt>(&instance, saga, AttemptCommand::Close);
                }
                SagaStep::AwaitAnswer { .. } | SagaStep::Idle => {}
            }
        }
        let after = self.attempt(attempt);
        if after.retire_reason == Some(RetireReason::StageFailed)
            && state.retire_reason != Some(RetireReason::StageFailed)
        {
            let _ = self.command::<Pool>(
                identity.pool.as_str(),
                Actor::Automation { policy: "breaker" },
                PoolCommand::CountAttemptFailure {
                    attempt_id: attempt,
                    digest: identity.image_digest.clone(),
                    stage: Stage::Clone,
                    at: self.store.clock().now(),
                },
            );
            self.apply_terms(&identity.pool);
        }
    }

    /// A job took the prepared attempt and finished (the activation and job policies, folded
    /// into one retirement).
    pub fn consume(&mut self, attempt: AttemptId) {
        let _ = self.command::<Attempt>(
            &attempt.to_string(),
            Actor::Automation { policy: "job" },
            AttemptCommand::RequestRetirement {
                reason: RetireReason::JobDone,
            },
        );
    }

    /// Quarantines the attempt and its lease (P16).
    pub fn quarantine(&mut self, host: &HostId, attempt: AttemptId) {
        if self
            .command::<Attempt>(
                &attempt.to_string(),
                Actor::Automation {
                    policy: "quarantine",
                },
                AttemptCommand::Quarantine {
                    reason: QuarantineReason::ForensicRetention,
                },
            )
            .is_ok()
        {
            let _ = self.command::<Host>(
                host.as_str(),
                Actor::Automation {
                    policy: "quarantine",
                },
                HostCommand::QuarantineLease {
                    attempt_id: attempt,
                    vm_running: true,
                },
            );
        }
    }
}

/// What a simulation found.
#[derive(Debug, Clone, Default)]
pub struct Simulation {
    /// Every invariant violation, with the step it was found at.
    pub violations: Vec<String>,
    /// Reconciler passes run.
    pub passes: u32,
    /// Reservations the passes made.
    pub reserved: u32,
    /// Retirements the passes made.
    pub retired: u32,
    /// Releases the passes made.
    pub released: u32,
    /// Capacity inputs the passes applied.
    pub capacities: u32,
    /// Offers acquired by the scale sets.
    pub acquired: u32,
}

/// What the checker keeps between steps.
#[derive(Debug, Clone, Default)]
struct Snapshot {
    hosts: BTreeMap<HostId, HostState>,
    pools: BTreeMap<PoolName, PoolState>,
    scale_sets: BTreeMap<PoolName, ScaleSetState>,
}

fn snapshot(world: &World<'_>) -> Snapshot {
    Snapshot {
        hosts: world
            .hosts
            .iter()
            .map(|h| (h.clone(), world.host(h)))
            .collect(),
        pools: world
            .pools
            .iter()
            .map(|p| (p.name.clone(), world.pool(&p.name)))
            .collect(),
        scale_sets: world
            .pools
            .iter()
            .map(|p| (p.name.clone(), world.scale_set(&p.name)))
            .collect(),
    }
}

fn tripped(pool: &PoolState) -> bool {
    pool.image_pin.as_ref().is_some_and(|pin| {
        pool.breakers
            .get(pin)
            .is_some_and(|b| b.tripped_at.is_some())
    })
}

/// The invariants over one step, from `before` to `after`.
fn check(
    step: usize,
    before: &Snapshot,
    after: &Snapshot,
    generations: &mut BTreeMap<(HostId, Slot), (Generation, AttemptId)>,
    out: &mut Vec<String>,
) {
    for (host_id, host) in &after.hosts {
        let prior = before.hosts.get(host_id);
        for (attempt, lease) in &host.leases {
            let key = (host_id.clone(), lease.slot);
            match generations.get(&key) {
                Some((generation, holder)) if holder == attempt => {
                    if *generation != lease.generation {
                        out.push(format!(
                            "step {step}: I02 lease {attempt} changed generation"
                        ));
                    }
                }
                Some((generation, _)) if *generation >= lease.generation => {
                    out.push(format!(
                        "step {step}: I02 slot {} of {host_id} reused generation {}",
                        lease.slot.get(),
                        lease.generation.get()
                    ));
                }
                Some(_) | None => {
                    generations.insert(key, (lease.generation, *attempt));
                }
            }
            let new = prior.is_none_or(|p| !p.leases.contains_key(attempt));
            if !new {
                continue;
            }
            let Some(prior) = prior else {
                continue;
            };
            if prior.capacity.is_none() {
                out.push(format!(
                    "step {step}: I03 {attempt} reserved with no report"
                ));
            }
            let draining = match prior.gate.drain {
                Drain::Open => false,
                Drain::Draining { .. } | Drain::Drained => true,
            };
            if draining {
                out.push(format!(
                    "step {step}: I08 {attempt} reserved on draining {host_id}"
                ));
            }
            if !prior.gate.is_open() {
                out.push(format!(
                    "step {step}: I26 {attempt} reserved while {host_id} closed"
                ));
            }
            if !prior.pool_terms.get(&lease.pool).is_some_and(|t| t.open) {
                out.push(format!(
                    "step {step}: I26 {attempt} reserved under closed terms"
                ));
            }
            if before.pools.get(&lease.pool).is_some_and(tripped) {
                out.push(format!(
                    "step {step}: I27 {attempt} reserved for a tripped breaker"
                ));
            }
            let admissible = host.admissible();
            let allocated = host.allocated();
            let within = allocated.vms <= admissible.vms
                && allocated.resources.memory_bytes <= admissible.resources.memory_bytes
                && allocated.resources.disk_bytes <= admissible.resources.disk_bytes
                && (lease.os == GuestOs::Linux
                    || allocated.macos_guests <= admissible.macos_guests);
            if !within {
                out.push(format!(
                    "step {step}: I03 {host_id} allocated beyond its report"
                ));
            }
        }
    }
    for (pool, scale_set) in &after.scale_sets {
        let capacity = scale_set.capacity;
        if capacity.reported_max > capacity.deliverable {
            out.push(format!(
                "step {step}: I07 {pool} reports more than it delivers"
            ));
        }
        let Some(prior) = before.scale_sets.get(pool) else {
            continue;
        };
        let reserved = |s: &ScaleSetState| -> BTreeSet<RunnerRequestId> {
            s.acquiring.union(&s.acquired).copied().collect()
        };
        let had = reserved(prior);
        let added: Vec<RunnerRequestId> = reserved(scale_set).difference(&had).copied().collect();
        if added.is_empty() {
            continue;
        }
        let assigned = scale_set.statistics.map_or(0, |s| s.total_assigned_jobs);
        // every acquisition needs assigned plus the reservations held then below deliverable,
        // so after the last one they reach it at most (releases in the same message included)
        let budget = u64::from(assigned) + reserved(scale_set).len() as u64;
        if budget > u64::from(prior.capacity.deliverable) {
            out.push(format!(
                "step {step}: I23 {pool} acquired {} beyond deliverable {}",
                added.len(),
                prior.capacity.deliverable
            ));
        }
    }
}

/// The pass's own checks: the `Host` agreed with every reservation planned (the plan never
/// allocates beyond the report it read), the reconciler only retired or released what the
/// journal records, and each host's carve-outs fit its free capacity together (I24).
fn check_pass(
    step: usize,
    world: &World<'_>,
    before: &Snapshot,
    report: &PassReport,
    out: &mut Vec<String>,
) {
    for (command, code) in &report.refused {
        match command {
            PassCommand::ReserveSlot { host_id, .. } => {
                out.push(format!(
                    "step {step}: plan beyond report: {host_id} refused {code}"
                ));
            }
            PassCommand::ApplyCapacity { pool, .. } => {
                out.push(format!("step {step}: capacity of {pool} refused {code}"));
            }
            PassCommand::Retire { .. }
            | PassCommand::ReleaseSlot { .. }
            | PassCommand::CompleteHostDrain { .. }
            | PassCommand::CompletePoolDrain { .. } => {}
        }
    }
    for command in &report.accepted {
        match command {
            PassCommand::Retire { attempt_id, reason } => {
                let leased = before.hosts.values().any(|h| {
                    h.leases
                        .get(attempt_id)
                        .is_some_and(|l| !l.is_quarantined())
                });
                if !leased {
                    out.push(format!(
                        "step {step}: I04 retired {attempt_id} ({reason:?}) not leased"
                    ));
                }
            }
            PassCommand::ReleaseSlot {
                attempt_id,
                evidence,
                ..
            } => {
                let state = world.attempt(*attempt_id);
                let proven = match evidence {
                    mayfly_control_domain::host::ReleaseEvidence::AttemptNeverStarted => {
                        state.identity.is_none()
                    }
                    mayfly_control_domain::host::ReleaseEvidence::VmDeleted
                    | mayfly_control_domain::host::ReleaseEvidence::AttemptLost => {
                        matches!(state.phase, Phase::Deleted | Phase::Lost | Phase::Closed)
                    }
                };
                if !proven {
                    out.push(format!(
                        "step {step}: I04 released {attempt_id} without evidence"
                    ));
                }
            }
            PassCommand::ApplyCapacity { .. }
            | PassCommand::ReserveSlot { .. }
            | PassCommand::CompleteHostDrain { .. }
            | PassCommand::CompletePoolDrain { .. } => {}
        }
    }
    let shapes: BTreeMap<&PoolName, Footprint> = world
        .pools
        .iter()
        .map(|p| {
            (
                &p.name,
                Footprint::vm(p.os, Resources::new(p.memory_gib * GIB, 50 * GIB)),
            )
        })
        .collect();
    for (host_id, plan) in &report.pass.reports {
        let carved = plan.pools.iter().fold(Footprint::ZERO, |sum, line| {
            let vm = shapes.get(&line.pool).copied().unwrap_or(Footprint::ZERO);
            (0..line.carve_out).fold(sum, |s, _| s.saturating_add(vm))
        });
        if !carved.fits_within(plan.free) {
            out.push(format!(
                "step {step}: I24 carve-outs of {host_id} exceed its free capacity"
            ));
        }
    }
}

fn pools_for_simulation() -> Vec<PoolSpec> {
    vec![
        PoolSpec {
            name: pool_name("amac"),
            os: GuestOs::Macos,
            memory_gib: 8,
            max_running: 2,
            min_prepared: 0,
            breaker: 3,
        },
        PoolSpec {
            name: pool_name("blin"),
            os: GuestOs::Linux,
            memory_gib: 4,
            max_running: 6,
            min_prepared: 1,
            breaker: 3,
        },
        PoolSpec {
            name: pool_name("clin"),
            os: GuestOs::Linux,
            memory_gib: 2,
            max_running: 4,
            min_prepared: 0,
            breaker: 3,
        },
    ]
}

fn policies() -> BTreeMap<PoolName, PolicyOf> {
    let demand: PolicyOf =
        |waiting| mayfly_control_domain::capacity::CapacityPolicy::Demand { waiting };
    [(pool_name("amac"), demand), (pool_name("clin"), demand)]
        .into_iter()
        .collect()
}

/// Runs `steps` seeded world steps and reconciler passes over two hosts and three pools (one
/// macOS, two Linux; `exact` and `demand` mixed) and checks the invariants after every step.
///
/// # Panics
/// When the store fails: the kit is wrong.
#[must_use]
pub fn simulate(seed: u64, steps: usize) -> Simulation {
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let rng = SeededRng::new(seed);
    let mut config = ReconcilerConfig::new(EPOCH);
    config.policies = policies();
    let reconciler = Reconciler::new(&store, &journal, config, Kicks::new(64));
    let mut world = World::new(&store);
    world.register_host("h1", report(32, 0, 6));
    world.register_host("h2", report(24, 2, 4));
    for pool in pools_for_simulation() {
        world.configure_pool(pool);
    }
    let mut sim = Simulation::default();
    let mut generations = BTreeMap::new();
    let mut before = snapshot(&world);
    let pick = |n: usize| -> usize { usize::try_from(rng.below(n as u64)).unwrap_or(0) };
    for step in 0..steps {
        let roll = pick(100);
        let mut pass = None;
        match roll {
            0..=19 => {
                let report = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
                pass = Some(report);
            }
            20..=25 => {
                let host = world.hosts[pick(world.hosts.len())].clone();
                let memory = [16, 24, 32, 40][pick(4)];
                let foreign = [0, 0, 2, 6, 12][pick(5)];
                let slots = [2, 4, 6][pick(3)];
                world.report(
                    &host,
                    report(memory, foreign, u32::try_from(slots).unwrap_or(4)),
                );
            }
            26..=37 => {
                let pool = world.pools[pick(world.pools.len())].name.clone();
                let assigned = u32::try_from(pick(7)).unwrap_or(0);
                let offers = u32::try_from(pick(4)).unwrap_or(0);
                world.statistics(&pool, assigned, offers, pick(3) == 0);
            }
            38..=49 => {
                let unstarted = world.unstarted();
                if !unstarted.is_empty() && pick(5) != 0 {
                    let (host, attempt) = unstarted[pick(unstarted.len())].clone();
                    world.start(&host, attempt);
                }
            }
            50..=84 => {
                // the data plane and the saga move every attempt one step, or half of them
                let everyone = pick(2) == 0;
                for (_, attempt) in world.leased_attempts() {
                    if !everyone && pick(2) == 0 {
                        continue;
                    }
                    let refusal = match pick(32) {
                        0 => Some(RefusalReason::InsufficientResources),
                        1 => Some(RefusalReason::EffectFailed),
                        _ => None,
                    };
                    world.progress(attempt, refusal);
                }
            }
            85..=89 => clock.advance(Millis::new(u64::try_from(pick(90_000)).unwrap_or(0))),
            90..=92 => {
                let leased = world.leased_attempts();
                let prepared: Vec<_> = leased
                    .into_iter()
                    .filter(|(_, a)| world.attempt(*a).phase == Phase::Prepared)
                    .collect();
                if !prepared.is_empty() {
                    let (_, attempt) = prepared[pick(prepared.len())];
                    world.consume(attempt);
                }
            }
            93 => {
                let host = world.hosts[pick(world.hosts.len())].clone();
                let state = world.host(&host);
                let body = match state.gate.drain {
                    Drain::Open => HostCommand::StartHostDrain {
                        deadline: Deadline::after(clock.now(), Millis::new(600_000)),
                    },
                    Drain::Draining { .. } | Drain::Drained => HostCommand::CancelHostDrain,
                };
                let _ = world.command::<Host>(host.as_str(), Actor::Operator { uid: 501 }, body);
            }
            94 => {
                let host = world.hosts[pick(world.hosts.len())].clone();
                let state = world.host(&host);
                let body = if state.gate.closed.contains(&AdmissionCause::Woke) {
                    HostCommand::OpenAdmission {
                        cause: AdmissionCause::Woke,
                    }
                } else {
                    HostCommand::CloseAdmission {
                        cause: AdmissionCause::Woke,
                    }
                };
                let _ = world.command::<Host>(
                    host.as_str(),
                    Actor::Automation { policy: "wake" },
                    body,
                );
            }
            95 => {
                let pool = world.pools[pick(world.pools.len())].name.clone();
                let state = world.pool(&pool);
                let body = match state.mode {
                    mayfly_control_domain::pool::PoolMode::Paused => PoolCommand::ResumePool,
                    _ => PoolCommand::PausePool,
                };
                let _ = world.command::<Pool>(pool.as_str(), Actor::Operator { uid: 501 }, body);
                world.apply_terms(&pool);
            }
            96..=97 => {
                let pool = world.pools[pick(world.pools.len())].name.clone();
                if tripped(&world.pool(&pool)) {
                    let _ = world.command::<Pool>(
                        pool.as_str(),
                        Actor::Operator { uid: 501 },
                        PoolCommand::ResetBreaker {
                            digest: ImageDigest::sample(),
                        },
                    );
                    world.apply_terms(&pool);
                }
            }
            _ => {
                let leased = world.leased_attempts();
                if !leased.is_empty() && pick(3) == 0 {
                    let (host, attempt) = leased[pick(leased.len())].clone();
                    world.quarantine(&host, attempt);
                }
            }
        }
        let after = snapshot(&world);
        check(step, &before, &after, &mut generations, &mut sim.violations);
        if let Some(report) = pass {
            sim.passes += 1;
            check_pass(step, &world, &before, &report, &mut sim.violations);
            for command in &report.accepted {
                match command {
                    PassCommand::ReserveSlot { .. } => sim.reserved += 1,
                    PassCommand::Retire { .. } => sim.retired += 1,
                    PassCommand::ReleaseSlot { .. } => sim.released += 1,
                    PassCommand::ApplyCapacity { .. } => sim.capacities += 1,
                    PassCommand::CompleteHostDrain { .. }
                    | PassCommand::CompletePoolDrain { .. } => {}
                }
            }
        }
        for (pool, scale_set) in &after.scale_sets {
            let had = before.scale_sets.get(pool).map_or(0, |s| s.acquired.len());
            sim.acquired +=
                u32::try_from(scale_set.acquired.len().saturating_sub(had)).unwrap_or(0);
        }
        before = after;
    }
    sim
}
