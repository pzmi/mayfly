//! The reconciler (policy P5, `doc/domain-model.md` §7; plan §9): a thin driver around the pure
//! [`plan_pass`]. A pass reads the read side ([`queries::fleet`]), maps it to the pass's inputs
//! ([`inputs_of`]), plans, and issues each planned command through [`AggregateStore`], where
//! the aggregate that owns it decides it again. It reads no aggregate state and no projection
//! inside a command; commands never read projections, and the query takes the read port only.
//!
//! **When.** At most once per [`ReconcilerConfig::interval`] (a minute) by the control plane's
//! clock, and at once when a host, pool or scale-set stream moved ([`Reconciler::tick`] reads
//! the [`Kicks`] the store fills after each commit). A lost kick costs at most the interval: the
//! pass is level-triggered.
//!
//! **Idempotent.** Each command's id is derived from what it is about: a reservation from the
//! revisions the pass read and its ordinal (P5's `hash(input stream versions, ordinal)`) and its
//! intent (host, pool, terms revision), with the attempt id derived from it; a retirement from
//! the attempt, its revision and reason; a release from the attempt and its evidence; a capacity
//! input from its scale set, revision and values; a drain completion from its stream's revision.
//! The intent is part of each id, so a later pass that plans otherwise is never answered
//! `command_id_reused`. A pass repeated over the same revisions is answered by receipts and
//! reserves nothing twice (pinned by
//! `tests/store/reconciler.rs::should_reserve_nothing_twice_when_a_pass_is_repeated_over_the_same_revisions`).
//!
//! **Refusals.** An aggregate's refusal is the answer (P5: a `Host` refusal admits nothing, the
//! reason goes to the report); nothing is retried within a pass, the next pass reads again.
//!
//! **A never-started release is decided at the save, not on the read.** The pass reads the
//! attempts projection, which may be behind the journal or rebuilding, so a lease it sees
//! unstarted may have an `AttemptStarted` already. `ReleaseSlot{attempt_never_started}` is
//! therefore saved with a read of the attempt stream expecting no stream: the unit commits only
//! while the attempt has not started, and an attempt that started since leaves the lease held
//! (`failed` with `conflict`; the next pass reads again). P2's `StartAttempt` reads the host
//! stream the same way, so a release that commits first refuses the start (CP-03F;
//! `formal/scheduling.qnt`'s `staleUnstartedReadKeepsTheSlotTest`; pinned by
//! `tests/store/reconciler.rs::should_keep_the_slot_when_the_attempt_starts_between_the_read_and_the_release`).

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use mayfly_control_domain::attempt::{Attempt, AttemptCommand};
use mayfly_control_domain::capacity::Footprint;
use mayfly_control_domain::host::{Host, HostCommand, ReleaseEvidence, ReserveSlot};
use mayfly_control_domain::pool::{Pool, PoolCommand, PoolMode};
use mayfly_control_domain::reconcile::{
    HostReport, HostTerms, HostView, LeaseView, Pass, PassCommand, PassInputs, PassMemory,
    PolicyOf, PoolView, RunnerStage, ScaleSetView, Settings, exact, plan_pass,
};
use mayfly_control_domain::scaleset::{ScaleSet, ScaleSetCommand};
use mayfly_control_domain::{Actor, Aggregate, Command, Rejection};
use mayfly_domain::{AttemptId, CommandId, Epoch, HostId, Millis, PoolName, Resources};
use sha2::{Digest, Sha256};

use crate::delivery::Kicks;
use crate::journal::{ExpectedRevision, Journal};
use crate::projection::ProjectionStore;
use crate::projection::views::{AttemptRow, SlotRow};
use crate::queries::{self, FleetView, QueryError};
use crate::store::{
    AggregateStore, Clock, Outcome, Rng, SaveError, SaveRequest, StoreNotifier, stream_of,
};

/// The `placements` phase code of a placement clones may be made from.
const MATERIALISED: &str = "materialised";

/// The reconciler's name: the actor of its commands.
pub const RECONCILER: &str = "reconciler";

const ID_DOMAIN: &[u8] = b"mayfly-reconciler-v1\n";

/// The reconciler's configuration.
#[derive(Debug, Clone)]
pub struct ReconcilerConfig {
    /// The pass's timing.
    pub settings: Settings,
    /// The most time between two passes without a kick (P5: at most once a minute).
    pub interval: Millis,
    /// The run epoch its commands are issued under.
    pub run_epoch: Epoch,
    /// Each pool's capacity policy (the composition root maps `capacity_policy` through
    /// `mayfly_composition::config::plan_policy`); a pool not listed is `exact`.
    pub policies: BTreeMap<PoolName, PolicyOf>,
    /// The pure pass: [`plan_pass`]. A test substitutes a broken variant to prove that the
    /// simulation's checks catch it (`mayfly_testkit::fleet`'s negative control); nothing else
    /// sets it.
    pub plan: fn(&PassInputs) -> Pass,
}

impl ReconcilerConfig {
    /// The default timing and a one-minute interval under `run_epoch`, every pool `exact`.
    #[must_use]
    pub fn new(run_epoch: Epoch) -> Self {
        Self {
            settings: Settings::default(),
            interval: Millis::new(60_000),
            run_epoch,
            policies: BTreeMap::new(),
            plan: plan_pass,
        }
    }
}

/// Why a pass did not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileError {
    /// The read side could not be read.
    Query(QueryError),
}

impl std::fmt::Display for ReconcileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Query(e) => write!(f, "reconciler pass not run: {e}"),
        }
    }
}

impl std::error::Error for ReconcileError {}

/// What one pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassReport {
    /// The plan.
    pub pass: Pass,
    /// Commands accepted (also from a receipt).
    pub accepted: Vec<PassCommand>,
    /// Commands refused, with the refusal's code.
    pub refused: Vec<(PassCommand, &'static str)>,
    /// Commands not decided now (a conflict, a store fault, a command id reused under another
    /// intent): the next pass plans again.
    pub failed: Vec<(PassCommand, &'static str)>,
}

#[derive(Debug, Default)]
struct Timing {
    last_pass: Option<Millis>,
    memory: PassMemory,
    safe_mode: bool,
}

/// The reconciler over an [`AggregateStore`] and the [`ProjectionStore`] it reads.
pub struct Reconciler<'s, J, C, R, N, P> {
    store: &'s AggregateStore<J, C, R, N>,
    projections: &'s P,
    config: ReconcilerConfig,
    kicks: Kicks,
    timing: Mutex<Timing>,
}

impl<J, C, R, N, P> std::fmt::Debug for Reconciler<'_, J, C, R, N, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reconciler")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl<'s, J: Journal, C: Clock, R: Rng, N: StoreNotifier, P: ProjectionStore>
    Reconciler<'s, J, C, R, N, P>
{
    /// A reconciler over `store`, reading `projections`, woken by `kicks` (the composition root
    /// makes them one of the store's notifiers).
    pub fn new(
        store: &'s AggregateStore<J, C, R, N>,
        projections: &'s P,
        config: ReconcilerConfig,
        kicks: Kicks,
    ) -> Self {
        Self {
            store,
            projections,
            config,
            kicks,
            timing: Mutex::new(Timing::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Timing> {
        self.timing.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Enters or leaves safe mode: a pass then plans `ApplyCapacity{SafeMode}` only (I29).
    pub fn set_safe_mode(&self, safe_mode: bool) {
        self.lock().safe_mode = safe_mode;
    }

    /// When the timer next makes a pass due: at once before the first pass, else one interval
    /// after the last.
    #[must_use]
    pub fn next_due(&self) -> Millis {
        self.lock().last_pass.map_or(Millis::new(0), |last| {
            Millis::new(last.get().saturating_add(self.config.interval.get()))
        })
    }

    /// Runs a pass when one is due: a host, pool, scale-set or placement stream (a base
    /// materialised or evicted admits or closes a pool on its host, CP-07) was kicked since the last
    /// look, or the interval passed by the clock. `None` when nothing was due. Pinned by
    /// `tests/store/reconciler.rs::should_pass_at_most_once_an_interval_when_no_relevant_stream_is_kicked`.
    ///
    /// # Errors
    /// [`ReconcileError`] when the read side cannot be read.
    pub async fn tick(&self) -> Result<Option<PassReport>, ReconcileError> {
        let kicked = self.kicks.take().iter().any(|stream| {
            let s = stream.as_str();
            s.starts_with("host-")
                || s.starts_with("pool-")
                || s.starts_with("scaleset-")
                || s.starts_with("placement-")
        });
        if !kicked && self.store.clock().now() < self.next_due() {
            return Ok(None);
        }
        self.run_pass().await.map(Some)
    }

    /// Reads, plans and issues one pass now.
    ///
    /// # Errors
    /// [`ReconcileError`] when the read side cannot be read; nothing is issued then.
    pub async fn run_pass(&self) -> Result<PassReport, ReconcileError> {
        let now = self.store.clock().now();
        let view = queries::fleet(self.projections)
            .await
            .map_err(ReconcileError::Query)?;
        let (memory, safe_mode) = {
            let timing = self.lock();
            (timing.memory.clone(), timing.safe_mode)
        };
        let inputs = inputs_of(
            &view,
            &self.config.policies,
            now,
            self.config.settings,
            safe_mode,
            memory,
        );
        let pass = (self.config.plan)(&inputs);
        let digest = versions_digest(&view);
        let mut report = PassReport {
            pass: pass.clone(),
            accepted: Vec::new(),
            refused: Vec::new(),
            failed: Vec::new(),
        };
        let mut ordinal: u32 = 0;
        for command in &pass.commands {
            let answer = self.issue(command, &view, digest, &mut ordinal).await;
            match answer {
                Answer::Accepted => report.accepted.push(command.clone()),
                Answer::Refused(code) => report.refused.push((command.clone(), code)),
                Answer::Failed(code) => report.failed.push((command.clone(), code)),
            }
        }
        let mut timing = self.lock();
        timing.memory = pass.memory;
        timing.last_pass = Some(now);
        Ok(report)
    }

    async fn issue(
        &self,
        command: &PassCommand,
        view: &FleetView,
        digest: [u8; 32],
        ordinal: &mut u32,
    ) -> Answer {
        match command {
            PassCommand::ApplyCapacity { pool, capacity } => {
                let mode = format!("{:?}", capacity.mode);
                let id = derive(
                    b"capacity\n",
                    &[
                        pool.as_str().as_bytes(),
                        &capacity.revision.to_be_bytes(),
                        &capacity.deliverable.to_be_bytes(),
                        &capacity.reported_max.to_be_bytes(),
                        mode.as_bytes(),
                    ],
                );
                self.save::<ScaleSet>(pool.as_str(), id, ScaleSetCommand::ApplyCapacity(*capacity))
                    .await
            }
            PassCommand::ReserveSlot {
                host_id,
                pool,
                os,
                resources,
                terms_revision,
            } => {
                let id = derive(
                    b"reserve\n",
                    &[
                        &digest,
                        &ordinal.to_be_bytes(),
                        host_id.as_str().as_bytes(),
                        pool.as_str().as_bytes(),
                        &terms_revision.to_be_bytes(),
                    ],
                );
                *ordinal = ordinal.saturating_add(1);
                let attempt_id = attempt_of(id);
                self.save::<Host>(
                    host_id.as_str(),
                    id,
                    HostCommand::ReserveSlot(ReserveSlot {
                        attempt_id,
                        pool: pool.clone(),
                        os: *os,
                        resources: *resources,
                        terms_revision: *terms_revision,
                    }),
                )
                .await
            }
            PassCommand::Retire { attempt_id, reason } => {
                let revision = view.attempts.get(attempt_id).map_or(0, |a| a.revision);
                let why = format!("{reason:?}");
                let id = derive(
                    b"retire\n",
                    &[
                        &attempt_id.as_u128().to_be_bytes(),
                        &revision.to_be_bytes(),
                        why.as_bytes(),
                    ],
                );
                self.save::<Attempt>(
                    &attempt_id.to_string(),
                    id,
                    AttemptCommand::RequestRetirement { reason: *reason },
                )
                .await
            }
            PassCommand::ReleaseSlot {
                host_id,
                attempt_id,
                evidence,
            } => {
                let why = format!("{evidence:?}");
                let id = derive(
                    b"release\n",
                    &[&attempt_id.as_u128().to_be_bytes(), why.as_bytes()],
                );
                let body = HostCommand::ReleaseSlot {
                    attempt_id: *attempt_id,
                    evidence: *evidence,
                };
                match evidence {
                    ReleaseEvidence::AttemptNeverStarted => {
                        self.release_unstarted(host_id.as_str(), id, *attempt_id, body)
                            .await
                    }
                    ReleaseEvidence::VmDeleted | ReleaseEvidence::AttemptLost => {
                        self.save::<Host>(host_id.as_str(), id, body).await
                    }
                }
            }
            PassCommand::CompleteHostDrain { host_id } => {
                let revision = view
                    .hosts
                    .get(&format!("host-{host_id}"))
                    .map_or(0, |h| h.revision);
                let id = derive(
                    b"host-drain\n",
                    &[host_id.as_str().as_bytes(), &revision.to_be_bytes()],
                );
                self.save::<Host>(host_id.as_str(), id, HostCommand::CompleteHostDrain)
                    .await
            }
            PassCommand::CompletePoolDrain { pool } => {
                let revision = view
                    .pools
                    .get(&format!("pool-{pool}"))
                    .map_or(0, |p| p.revision);
                let id = derive(
                    b"pool-drain\n",
                    &[pool.as_str().as_bytes(), &revision.to_be_bytes()],
                );
                self.save::<Pool>(pool.as_str(), id, PoolCommand::CompletePoolDrain)
                    .await
            }
        }
    }

    fn command<B>(&self, id: CommandId, body: B) -> Command<B> {
        Command::new(
            id,
            self.store.clock().now(),
            self.config.run_epoch,
            Actor::Automation { policy: RECONCILER },
            body,
        )
    }

    async fn save<A: Aggregate>(&self, instance: &str, id: CommandId, body: A::Body) -> Answer
    where
        A::Body: Clone,
    {
        let request = SaveRequest::new(instance, self.command(id, body));
        answer_of(self.store.execute::<A>(request).await)
    }

    /// `ReleaseSlot{attempt_never_started}` with a read of the attempt stream expecting no
    /// stream, saved once: a started attempt (or any write to the host stream in between) fails
    /// the unit, and the next pass reads again. Never retried here: a retry would re-check the
    /// same expectation (see the module docs).
    async fn release_unstarted(
        &self,
        host: &str,
        id: CommandId,
        attempt_id: AttemptId,
        body: HostCommand,
    ) -> Answer {
        let Ok(attempt_stream) = stream_of::<Attempt>(&attempt_id.to_string()) else {
            return Answer::Failed("save_failed");
        };
        let request = SaveRequest::new(host, self.command(id, body))
            .with_read(attempt_stream, ExpectedRevision::NoStream);
        answer_of(self.store.save::<Host>(request).await)
    }
}

enum Answer {
    Accepted,
    Refused(&'static str),
    Failed(&'static str),
}

fn answer_of<R: Rejection>(saved: Result<Outcome<R>, SaveError>) -> Answer {
    match saved {
        Ok(Outcome::Accepted { .. }) => Answer::Accepted,
        Ok(Outcome::Refused { rejection, .. }) => Answer::Refused(rejection.code()),
        Err(SaveError::CommandIdReused { .. }) => Answer::Failed("command_id_reused"),
        Err(error) if error.is_retryable() => Answer::Failed("conflict"),
        Err(_) => Answer::Failed("save_failed"),
    }
}

fn derive(tag: &[u8], parts: &[&[u8]]) -> CommandId {
    let mut hasher = Sha256::new();
    hasher.update(ID_DOMAIN);
    hasher.update(tag);
    for part in parts {
        hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(part);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    let mut head = [0_u8; 16];
    head.copy_from_slice(&digest[..16]);
    CommandId::from_u128(u128::from_be_bytes(head))
}

/// The attempt a reservation's command mints: one per command id, so a repeated reservation
/// names the same attempt (P2 starts its stream with it).
#[must_use]
pub fn attempt_of(command_id: CommandId) -> AttemptId {
    let mut hasher = Sha256::new();
    hasher.update(ID_DOMAIN);
    hasher.update(b"attempt\n");
    hasher.update(command_id.as_u128().to_be_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    let mut head = [0_u8; 16];
    head.copy_from_slice(&digest[..16]);
    AttemptId::from_u128(u128::from_be_bytes(head))
}

/// A digest of every row revision the pass read: the "input stream versions" of P5's key.
fn versions_digest(view: &FleetView) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(ID_DOMAIN);
    hasher.update(b"versions\n");
    let mut put = |tag: &[u8], key: &str, revision: u64| {
        hasher.update(tag);
        hasher.update(u64::try_from(key.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(key.as_bytes());
        hasher.update(revision.to_be_bytes());
    };
    for (key, row) in &view.hosts {
        put(b"h", key, row.revision);
    }
    for (key, row) in &view.pools {
        put(b"p", key, row.revision);
    }
    for (key, row) in &view.scale_sets {
        put(b"s", key, row.revision);
    }
    for (id, row) in &view.attempts {
        put(b"a", &id.to_string(), row.revision);
    }
    // a placement admits a pool on a host (CP-07): a pass it changed plans under new ids
    for (key, row) in &view.placements {
        put(b"m", key, row.revision);
    }
    hasher.finalize().into()
}

/// The pool mode a `pools` row's code names; an unknown code (a newer build's) admits nothing.
fn pool_mode(code: &str) -> PoolMode {
    match code {
        "active" => PoolMode::Active,
        "paused" => PoolMode::Paused,
        "draining" => PoolMode::Draining,
        "drained" => PoolMode::Drained,
        "decommissioned" => PoolMode::Decommissioned,
        _ => PoolMode::Unconfigured,
    }
}

/// The stage of a lease's attempt; an unknown phase code (a newer build's) is `Ending`: counted
/// as no runner, never retired, never released.
fn stage_of(attempt: Option<&AttemptRow>) -> RunnerStage {
    let Some(row) = attempt else {
        return RunnerStage::NotStarted;
    };
    match row.phase.as_str() {
        "reserved" | "cloning" | "booting" | "verifying" | "backoff" => RunnerStage::Preparing,
        "prepared" => RunnerStage::Prepared,
        "activating" => RunnerStage::Activating,
        "listening" => RunnerStage::Listening,
        "busy" => RunnerStage::Busy,
        "quarantined" => RunnerStage::Quarantined,
        "deleted" | "lost" | "closed" => row.vm_gone.map_or(RunnerStage::Ending, RunnerStage::Gone),
        _ => RunnerStage::Ending,
    }
}

/// What a lease counts against capacity, as `Host` counts it (`Lease::holds`): a VM's slot,
/// macOS-guest slot and resources, and only its disk once it is a stopped quarantined lease.
fn holds_of(slot: &SlotRow) -> Footprint {
    let vm = Footprint::vm(slot.os, slot.resources);
    if slot.phase == "quarantined" && slot.holds.as_deref() == Some("disk_and_slot") {
        Footprint {
            resources: Resources::new(0, slot.resources.disk_bytes),
            ..vm
        }
    } else {
        vm
    }
}

/// The pass's inputs from the read side: hosts with their leases joined to their attempts,
/// configured pools with their policy and scale set. Pure. Pinned by
/// `tests/store/reconciler.rs::should_reserve_for_assigned_jobs_when_a_pass_reads_the_fleet`.
#[must_use]
pub fn inputs_of(
    view: &FleetView,
    policies: &BTreeMap<PoolName, PolicyOf>,
    now: Millis,
    settings: Settings,
    safe_mode: bool,
    memory: PassMemory,
) -> PassInputs {
    let hosts = view
        .hosts
        .iter()
        .filter_map(|(key, row)| {
            let host_id: HostId = key.strip_prefix("host-")?.parse().ok()?;
            let leases = view
                .leases
                .get(key)
                .map(|slots| {
                    slots
                        .iter()
                        .map(|slot| {
                            let attempt = view.attempts.get(&slot.attempt_id);
                            LeaseView {
                                attempt_id: slot.attempt_id,
                                pool: slot.pool.clone(),
                                holds: holds_of(slot),
                                quarantined: slot.phase == "quarantined",
                                stage: stage_of(attempt),
                                image: attempt.map(|a| a.image_digest.clone()),
                                refusal: attempt.and_then(|a| a.capacity_refused_at),
                                retiring: attempt.is_some_and(|a| a.retirement_requested.is_some()),
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            let placed = view
                .placements
                .values()
                .filter(|p| p.host_id == host_id && p.phase == MATERIALISED)
                .map(|p| (p.digest.clone(), p.disk_bytes))
                .collect();
            Some(HostView {
                host_id,
                report: row
                    .report_id
                    .zip(row.report)
                    .map(|(report_id, capacity)| HostReport {
                        report_id,
                        capacity,
                    }),
                reserves: row.reserves,
                admission_open: row.admission_open,
                draining: row.drain == "draining",
                terms: row
                    .terms
                    .iter()
                    .map(|t| {
                        (
                            t.pool.clone(),
                            HostTerms {
                                revision: t.revision,
                                open: t.open,
                                max_running: t.max_running,
                            },
                        )
                    })
                    .collect(),
                placed,
                leases,
            })
        })
        .collect();
    let pools = view
        .pools
        .iter()
        .filter_map(|(key, row)| {
            let pool: PoolName = key.strip_prefix("pool-")?.parse().ok()?;
            let scale_set = view
                .scale_sets
                .get(&format!("scaleset-{pool}"))
                .filter(|s| s.pool.is_some())
                .map(|s| ScaleSetView {
                    assigned: s.assigned,
                    capacity: s.capacity,
                });
            Some(PoolView {
                os: row.os?,
                vm: row.vm_resources?,
                min_prepared: row.min_prepared?,
                mode: pool_mode(&row.mode),
                image_pin: row.image_pin.clone(),
                policy: policies.get(&pool).copied().unwrap_or(exact),
                scale_set,
                pool,
            })
        })
        .collect();
    PassInputs {
        now,
        settings,
        safe_mode,
        hosts,
        pools,
        memory,
    }
}
