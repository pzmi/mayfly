//! A fleet in a real `AggregateStore` over `MemoryJournal`, for the reconciler's component tests
//! and its seeded simulation (CP-03): hosts with capacity reports, pools with their scale sets
//! and terms, and a world that plays the other policies (P2, P4, P6, P15, P16), the data plane's
//! answers and GitHub's statistics, so a reconciler pass can be run against it step by step.
//!
//! [`simulate`] runs a seeded sequence of world steps and reconciler passes and checks, after
//! each step, the invariants the reconciler must keep (`doc/domain-model.md` §5.4): no slot
//! generation reused (I02), never allocated beyond the applied report (I03, computed here from
//! the raw report, the reserves and each lease's footprint, not through `HostState`'s own
//! arithmetic), the reconciler touches no VM and only attempts and leases the journal records
//! (I04), the reported max fits the deliverable and the carve-outs fit each host together (I07,
//! I24), nothing admitted on a draining host (I08), acquisition within the deliverable (I23),
//! nothing admitted while the gate or the pool's terms are closed (I26), nothing admitted for a
//! tripped breaker (I27), nothing reserved on a host held by a data-plane refusal until a newer
//! report, no VM without its lease (a slot released under a live VM: I33's precondition), and,
//! while the reconciler reads the journal's own rows, the `Host` never refuses a reservation
//! the pass planned for capacity: the plan and the report agree.
//!
//! The world plays what the reconciler does not control: the data plane's real capacity
//! ([`World::drift`], reported later or never), its `insufficient_resources` refusal of a clone
//! that does not fit that capacity, `target_absent` answers that record a vanished VM (ADR
//! 0066's replacement path), pool drains and resumes, image pin changes, host drains, the gate,
//! the breaker and quarantines. [`SimOptions::lag`] makes the reconciler read the projections
//! as they were some steps before (a read model behind the journal), and [`SimOptions::plan`]
//! swaps the pure pass for a variant: the negative control that proves the checks are not
//! vacuous (`tests/store/reconciler.rs::should_find_violations_when_a_plan_ignores_admission`).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Mutex, PoisonError};

use mayfly_control::delivery::Kicks;
use mayfly_control::journal::{
    ExpectedRevision, Journal, Payload, ProjectionName, StorageError, StreamId,
};
use mayfly_control::projection::control;
use mayfly_control::projection::{
    ProjectionMeta, ProjectionSpec, ProjectionStore, StreamRows, views,
};
use mayfly_control::reconciler::{PassReport, Reconciler, ReconcilerConfig};
use mayfly_control::store::{
    AggregateStore, Clock, NoNotifier, Outcome, Rng, SaveError, SaveRequest, stream_of,
};
use mayfly_control_domain::attempt::RunnerRequestId;
use mayfly_control_domain::attempt::{
    Attempt, AttemptCommand, AttemptState, DirectiveTag, Fact, GuestFacts, GuestOs, Observation,
    Phase, QuarantineReason, RefusalReason, RetireReason, SagaStep, Stage, StartAttempt, next_step,
};
use mayfly_control_domain::capacity::{self, Footprint, ReportedCapacity};
use mayfly_control_domain::demand::Statistics;
use mayfly_control_domain::demand::{EventName, Offer};
use mayfly_control_domain::host::{
    AdmissionCause, ApplyPoolTerms, Drain, Holds, Host, HostCommand, HostState, LeasePhase,
    ReleaseEvidence,
};
use mayfly_control_domain::pool::{
    ConfigurePool, Pool, PoolCommand, PoolMode, PoolState, TrustTier, terms,
};
use mayfly_control_domain::reconcile::{Pass, PassCommand, PassInputs, PolicyOf, plan_pass};
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
    /// What each host's data plane really has: reported only by [`World::report`] and
    /// [`World::report_truth`].
    truth: BTreeMap<HostId, ReportedCapacity>,
    /// Every attempt started, in start order.
    started: Vec<AttemptId>,
    /// The hosts whose data plane refused a clone `insufficient_resources`, since last taken.
    refused_on: Vec<HostId>,
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
            truth: BTreeMap::new(),
            started: Vec::new(),
            refused_on: Vec::new(),
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

    /// The data plane's capacity on `host` becomes `capacity` (foreign use grew or shrank), not
    /// reported yet.
    pub fn drift(&mut self, host: &HostId, capacity: ReportedCapacity) {
        self.truth.insert(host.clone(), capacity);
    }

    /// Reports what `host`'s data plane really has (P22).
    pub fn report_truth(&mut self, host: &HostId) {
        if let Some(capacity) = self.truth.get(host).copied() {
            self.report(host, capacity);
        }
    }

    /// The hosts whose data plane refused a clone `insufficient_resources` since the last call.
    pub fn take_refusals(&mut self) -> Vec<HostId> {
        std::mem::take(&mut self.refused_on)
    }

    /// Applies a newer capacity report on `host` (P22): what its data plane now has.
    pub fn report(&mut self, host: &HostId, capacity: ReportedCapacity) {
        self.truth.insert(host.clone(), capacity);
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

    /// P2: `StartAttempt` for the lease of `attempt` on `host`, with the pool's pinned image,
    /// saved with a read of the host stream at the revision whose lease it starts from: a
    /// release that commits in between refuses the start (CP-03F), and an attempt is never
    /// started for a lease the host no longer holds. `false` when it did not start.
    ///
    /// # Panics
    /// When the store fails other than by a conflict: the kit is wrong.
    pub fn start(&mut self, host: &HostId, attempt: AttemptId) -> bool {
        let stream = stream_of::<Host>(host.as_str()).unwrap_or_else(|e| panic!("{e}"));
        let head = block_on(self.store.journal().head(&stream)).unwrap_or_else(|e| panic!("{e}"));
        let state = self.host(host);
        let Some(lease) = state.leases.get(&attempt) else {
            return false;
        };
        let image_digest = self
            .pool(&lease.pool)
            .image_pin
            .unwrap_or_else(ImageDigest::sample);
        let body = AttemptCommand::StartAttempt(StartAttempt {
            attempt_id: attempt,
            host_id: host.clone(),
            slot: lease.slot,
            generation: lease.generation,
            pool: lease.pool.clone(),
            image_digest,
            retry_budget: 2,
        });
        let command = Command::new(
            self.mint(),
            self.store.clock().now(),
            EPOCH,
            Actor::Automation { policy: "start" },
            body,
        );
        let request = SaveRequest::new(attempt.to_string(), command)
            .with_read(stream, ExpectedRevision::of(head.map(|h| h.revision)));
        match block_on(self.store.save::<Attempt>(request)) {
            Ok(Outcome::Accepted { .. }) => {
                self.started.push(attempt);
                true
            }
            Ok(Outcome::Refused { .. }) | Err(SaveError::Conflict { .. }) => false,
            Err(e) => panic!("{e}"),
        }
    }

    /// What one VM of `pool` needs, by the pool's spec.
    fn footprint_of(&self, pool: &PoolName) -> Footprint {
        self.pools
            .iter()
            .find(|p| &p.name == pool)
            .map_or(Footprint::ZERO, |p| {
                Footprint::vm(p.os, Resources::new(p.memory_gib * GIB, 50 * GIB))
            })
    }

    /// Whether the data plane's own ledger on the attempt's host fits one more VM of its pool:
    /// every VM of ours that exists there, against the host's real capacity (ADR 0024; the
    /// data plane counts no operator reserve: that is the control plane's).
    #[must_use]
    pub fn clone_fits(&self, attempt: AttemptId) -> bool {
        let Some(identity) = self.attempt(attempt).identity else {
            return true;
        };
        let Some(truth) = self.truth.get(&identity.host_id) else {
            return true;
        };
        let used = self
            .started
            .iter()
            .filter(|a| **a != attempt)
            .map(|a| self.attempt(*a))
            .filter(|s| {
                s.identity
                    .as_ref()
                    .is_some_and(|i| i.host_id == identity.host_id)
                    && vm_exists(s)
            })
            .filter_map(|s| s.identity.map(|i| self.footprint_of(&i.pool)))
            .fold(Footprint::ZERO, Footprint::saturating_add);
        used.saturating_add(self.footprint_of(&identity.pool))
            .fits_within(capacity::admissible(truth, Resources::new(0, 0)))
    }

    /// The data plane's answer to the attempt's pending directive, as the world decides it: a
    /// clone that does not fit the host's real capacity is refused `insufficient_resources`;
    /// otherwise `roll` (0..48) refuses `effect_failed` now and then, and answers a start or
    /// stop `target_absent` (the VM vanished, ADR 0066) now and then.
    #[must_use]
    pub fn answer(&self, attempt: AttemptId, roll: usize) -> Option<RefusalReason> {
        let tag = self.attempt(attempt).pending.map(|p| p.tag)?;
        match tag {
            DirectiveTag::CloneVm if !self.clone_fits(attempt) => {
                Some(RefusalReason::InsufficientResources)
            }
            DirectiveTag::StartVm | DirectiveTag::StopVm if roll == 0 => {
                Some(RefusalReason::TargetAbsent)
            }
            _ if roll == 1 => Some(RefusalReason::EffectFailed),
            _ => None,
        }
    }

    /// Every started attempt whose VM exists and whose host holds no lease for it: a slot
    /// released under a live VM.
    #[must_use]
    pub fn vms_without_lease(&self) -> Vec<AttemptId> {
        self.started
            .iter()
            .copied()
            .filter(|a| {
                let state = self.attempt(*a);
                vm_exists(&state)
                    && state
                        .identity
                        .is_some_and(|i| !self.host(&i.host_id).leases.contains_key(a))
            })
            .collect()
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
            if refusal == Some(RefusalReason::InsufficientResources) {
                self.refused_on.push(identity.host_id.clone());
            }
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

/// Whether the attempt's VM exists as far as the attempt knows: it recorded one and has not
/// recorded it deleted or lost.
fn vm_exists(state: &AttemptState) -> bool {
    state.instance_id.is_some()
        && !matches!(state.phase, Phase::Deleted | Phase::Lost | Phase::Closed)
}

/// The projections as they were some steps ago: the reconciler's read model behind the journal
/// (or rebuilding). With no lag it reads the journal's own rows.
#[derive(Debug)]
pub struct LaggedReads {
    journal: MemoryJournal,
    lag: usize,
    frames: Mutex<VecDeque<BTreeMap<ProjectionName, BTreeMap<String, Payload>>>>,
}

impl LaggedReads {
    /// Reads of `journal`'s projections `lag` captures behind.
    #[must_use]
    pub fn new(journal: &MemoryJournal, lag: usize) -> Self {
        let reads = Self {
            journal: journal.clone(),
            lag,
            frames: Mutex::new(VecDeque::new()),
        };
        reads.capture();
        reads
    }

    /// Captures the journal's rows now; the reads serve the capture `lag` ago.
    ///
    /// # Panics
    /// When the journal fails: the kit is wrong.
    pub fn capture(&self) {
        if self.lag == 0 {
            return;
        }
        let mut frame = BTreeMap::new();
        // what the reconciler's query reads (`queries::fleet`)
        for name in [
            views::Hosts::NAME,
            views::Slots::NAME,
            views::Attempts::NAME,
            views::Pools::NAME,
            views::ScaleSets::NAME,
        ] {
            let spec = ProjectionName::new(name).unwrap_or_else(|e| panic!("{e}"));
            let mut rows = BTreeMap::new();
            let mut after: Option<String> = None;
            loop {
                let page = block_on(ProjectionStore::rows(
                    &self.journal,
                    &spec,
                    after.as_deref(),
                    500,
                ))
                .unwrap_or_else(|e| panic!("{e}"));
                let Some((last, _)) = page.last() else {
                    break;
                };
                after = Some(last.clone());
                rows.extend(page);
            }
            frame.insert(spec, rows);
        }
        let mut frames = self.frames.lock().unwrap_or_else(PoisonError::into_inner);
        frames.push_back(frame);
        while frames.len() > self.lag {
            frames.pop_front();
        }
    }

    fn frame_rows(&self, spec: &ProjectionName) -> BTreeMap<String, Payload> {
        self.frames
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .front()
            .and_then(|f| f.get(spec).cloned())
            .unwrap_or_default()
    }
}

impl ProjectionStore for LaggedReads {
    async fn meta(&self) -> Result<Vec<ProjectionMeta>, StorageError> {
        ProjectionStore::meta(&self.journal).await
    }

    async fn stream_rows(
        &self,
        spec: &ProjectionName,
        stream: &StreamId,
    ) -> Result<StreamRows, StorageError> {
        ProjectionStore::stream_rows(&self.journal, spec, stream).await
    }

    async fn row(&self, spec: &ProjectionName, key: &str) -> Result<Option<Payload>, StorageError> {
        if self.lag == 0 {
            return ProjectionStore::row(&self.journal, spec, key).await;
        }
        Ok(self.frame_rows(spec).get(key).cloned())
    }

    async fn rows(
        &self,
        spec: &ProjectionName,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<(String, Payload)>, StorageError> {
        if self.lag == 0 {
            return ProjectionStore::rows(&self.journal, spec, after, limit).await;
        }
        let take = usize::try_from(limit).unwrap_or(usize::MAX);
        Ok(self
            .frame_rows(spec)
            .into_iter()
            .filter(|(key, _)| after.is_none_or(|a| key.as_str() > a))
            .take(take)
            .collect())
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
    /// Clones the data plane refused `insufficient_resources`.
    pub refusals: u32,
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

/// What the checker carries between steps.
#[derive(Debug, Default)]
struct Checker {
    /// The last generation per slot, and its holder (I02).
    generations: BTreeMap<(HostId, Slot), (Generation, AttemptId)>,
    /// Hosts held by a data-plane refusal: the applied report id when it was refused, and the
    /// step from which the reconciler can read it (the refusal's step plus the read lag).
    held: BTreeMap<HostId, (u64, usize)>,
}

fn report_id_of(host: &HostState) -> u64 {
    host.capacity.map_or(0, |r| r.report_id)
}

/// I03 from the raw report: the total less foreign use, our base use and the operator's
/// reserves on memory and disk, the VM slots less foreign VMs, at most two macOS guests less
/// foreign ones (G3); against every live lease's footprint, a stopped quarantined lease's
/// memory excluded. Computed here, not through `capacity::admissible` or
/// `HostState::allocated`, so a fault in either is caught. Checks the dimensions a VM of `os`
/// uses.
fn within_report(host: &HostState, os: GuestOs) -> bool {
    let Some(applied) = host.capacity else {
        return false;
    };
    let r = applied.capacity;
    let limit = |total: u64, foreign: u64, base: u64, reserve: u64| {
        total
            .saturating_sub(foreign)
            .saturating_sub(base)
            .saturating_sub(reserve)
    };
    let memory = limit(
        r.total.memory_bytes,
        r.foreign.memory_bytes,
        r.base.memory_bytes,
        host.reserves.memory_bytes,
    );
    let disk = limit(
        r.total.disk_bytes,
        r.foreign.disk_bytes,
        r.base.disk_bytes,
        host.reserves.disk_bytes,
    );
    let slots = u64::from(r.vm_slots.saturating_sub(r.foreign_vms));
    let macos = u64::from(r.macos_guests.min(2).saturating_sub(r.foreign_macos_guests));
    let (mut used_memory, mut used_disk, mut used_slots, mut used_macos) = (0_u64, 0_u64, 0, 0);
    for lease in host.leases.values() {
        let stopped = matches!(
            lease.phase,
            LeasePhase::Quarantined {
                holds: Holds::DiskAndSlot
            }
        );
        if !stopped {
            used_memory = used_memory.saturating_add(lease.resources.memory_bytes);
        }
        used_disk = used_disk.saturating_add(lease.resources.disk_bytes);
        used_slots += 1_u64;
        if lease.os == GuestOs::Macos {
            used_macos += 1_u64;
        }
    }
    used_memory <= memory
        && used_disk <= disk
        && used_slots <= slots
        && (os == GuestOs::Linux || used_macos <= macos)
}

/// The invariants over one step, from `before` to `after`.
fn check(
    step: usize,
    before: &Snapshot,
    after: &Snapshot,
    checker: &mut Checker,
    out: &mut Vec<String>,
) {
    for (host_id, host) in &after.hosts {
        if let Some((at, _)) = checker.held.get(host_id)
            && report_id_of(host) > *at
        {
            checker.held.remove(host_id);
        }
        let prior = before.hosts.get(host_id);
        for (attempt, lease) in &host.leases {
            let key = (host_id.clone(), lease.slot);
            match checker.generations.get(&key) {
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
                    checker
                        .generations
                        .insert(key, (lease.generation, *attempt));
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
            if checker
                .held
                .get(host_id)
                .is_some_and(|(_, visible)| *visible <= step)
            {
                out.push(format!(
                    "step {step}: {attempt} reserved on {host_id}, held by a refusal"
                ));
            }
            if !within_report(host, lease.os) {
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

/// The pass's own checks: while it reads the journal's own rows, the `Host` agreed with every
/// reservation planned (the plan never allocates beyond the report it read) and each scale set
/// with its capacity input (behind the journal, a stale revision is refused, which is the
/// aggregates doing their work); the reconciler
/// only retired or released what the journal records; and each host's carve-outs fit its free
/// capacity together (I24).
fn check_pass(
    step: usize,
    world: &World<'_>,
    before: &Snapshot,
    report: &PassReport,
    fresh: bool,
    out: &mut Vec<String>,
) {
    for (command, code) in &report.refused {
        match command {
            PassCommand::ReserveSlot { host_id, .. } if fresh => {
                out.push(format!(
                    "step {step}: plan beyond report: {host_id} refused {code}"
                ));
            }
            PassCommand::ApplyCapacity { pool, .. } if fresh => {
                out.push(format!("step {step}: capacity of {pool} refused {code}"));
            }
            PassCommand::ReserveSlot { .. }
            | PassCommand::ApplyCapacity { .. }
            | PassCommand::Retire { .. }
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
                    ReleaseEvidence::AttemptNeverStarted => state.identity.is_none(),
                    ReleaseEvidence::VmDeleted | ReleaseEvidence::AttemptLost => {
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

/// The image a pin change alternates with the sample one.
fn other_image() -> ImageDigest {
    ImageDigest::new(&format!("sha256:{}", "b".repeat(64))).unwrap_or_else(|e| panic!("{e}"))
}

/// How a simulation runs.
#[derive(Debug, Clone, Copy)]
pub struct SimOptions {
    /// How many steps behind the journal the reconciler reads the projections.
    pub lag: usize,
    /// The pure pass the reconciler runs: [`plan_pass`], or a variant to prove the checks.
    pub plan: fn(&PassInputs) -> Pass,
}

impl Default for SimOptions {
    fn default() -> Self {
        Self {
            lag: 0,
            plan: plan_pass,
        }
    }
}

/// Runs `steps` seeded world steps and reconciler passes over two hosts and three pools (one
/// macOS, two Linux; `exact` and `demand` mixed) and checks the invariants after every step.
///
/// # Panics
/// When the store fails: the kit is wrong.
#[must_use]
pub fn simulate(seed: u64, steps: usize) -> Simulation {
    simulate_with(seed, steps, SimOptions::default())
}

/// [`simulate`] with `options`.
///
/// # Panics
/// When the store fails: the kit is wrong.
#[must_use]
pub fn simulate_with(seed: u64, steps: usize, options: SimOptions) -> Simulation {
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let rng = SeededRng::new(seed);
    let mut config = ReconcilerConfig::new(EPOCH);
    config.policies = policies();
    config.plan = options.plan;
    let reads = LaggedReads::new(&journal, options.lag);
    let reconciler = Reconciler::new(&store, &reads, config, Kicks::new(64));
    let mut world = World::new(&store);
    world.register_host("h1", report(32, 0, 6));
    world.register_host("h2", report(24, 2, 4));
    for pool in pools_for_simulation() {
        world.configure_pool(pool);
    }
    let mut sim = Simulation::default();
    let mut checker = Checker::default();
    let mut before = snapshot(&world);
    reads.capture();
    let pick = |n: usize| -> usize { usize::try_from(rng.below(n as u64)).unwrap_or(0) };
    for step in 0..steps {
        let roll = pick(100);
        let mut pass = None;
        match roll {
            0..=19 => {
                let report = block_on(reconciler.run_pass()).unwrap_or_else(|e| panic!("{e}"));
                pass = Some(report);
            }
            20..=22 => {
                // the data plane's real capacity moves; the control plane learns of it later
                let host = world.hosts[pick(world.hosts.len())].clone();
                let memory = [16, 24, 32, 40][pick(4)];
                let foreign = [0, 0, 2, 6, 12][pick(5)];
                let slots = [2, 4, 6][pick(3)];
                world.drift(
                    &host,
                    report(memory, foreign, u32::try_from(slots).unwrap_or(4)),
                );
            }
            23..=25 => {
                let host = world.hosts[pick(world.hosts.len())].clone();
                world.report_truth(&host);
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
                    let refusal = world.answer(attempt, pick(48));
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
                let body = match world.pool(&pool).mode {
                    PoolMode::Active => PoolCommand::PausePool,
                    _ => PoolCommand::ResumePool,
                };
                let _ = world.command::<Pool>(pool.as_str(), Actor::Operator { uid: 501 }, body);
                world.apply_terms(&pool);
            }
            96 => {
                let pool = world.pools[pick(world.pools.len())].name.clone();
                if tripped(&world.pool(&pool)) {
                    let _ = world.command::<Pool>(
                        pool.as_str(),
                        Actor::Operator { uid: 501 },
                        PoolCommand::ResetBreaker {
                            digest: world
                                .pool(&pool)
                                .image_pin
                                .unwrap_or_else(ImageDigest::sample),
                        },
                    );
                    world.apply_terms(&pool);
                }
            }
            97 => {
                // an operator drains a pool, or resumes a draining or drained one
                let pool = world.pools[pick(world.pools.len())].name.clone();
                let body = match world.pool(&pool).mode {
                    PoolMode::Draining | PoolMode::Drained => PoolCommand::ResumePool,
                    _ => PoolCommand::StartPoolDrain,
                };
                let _ = world.command::<Pool>(pool.as_str(), Actor::Operator { uid: 501 }, body);
                world.apply_terms(&pool);
            }
            98 => {
                // the image pin changes (P17): prepared VMs of the old image are retired
                let pool = world.pools[pick(world.pools.len())].name.clone();
                let digest = if world.pool(&pool).image_pin == Some(ImageDigest::sample()) {
                    other_image()
                } else {
                    ImageDigest::sample()
                };
                let _ = world.command::<Pool>(
                    pool.as_str(),
                    Actor::Automation { policy: "image" },
                    PoolCommand::PinImage {
                        digest,
                        verified_ref: Some(EventId::from_u128(8)),
                    },
                );
                world.apply_terms(&pool);
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
        check(step, &before, &after, &mut checker, &mut sim.violations);
        for host in world.take_refusals() {
            // the reconciler reads it once the projections it reads include this step
            let at = after.hosts.get(&host).map_or(0, report_id_of);
            checker.held.insert(host, (at, step + options.lag.max(1)));
            sim.refusals += 1;
        }
        for attempt in world.vms_without_lease() {
            sim.violations
                .push(format!("step {step}: the VM of {attempt} has no lease"));
        }
        if let Some(report) = pass {
            sim.passes += 1;
            check_pass(
                step,
                &world,
                &before,
                &report,
                options.lag == 0,
                &mut sim.violations,
            );
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
        reads.capture();
    }
    sim
}
