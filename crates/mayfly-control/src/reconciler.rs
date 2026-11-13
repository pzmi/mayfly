//! The reconciler (policy P5, `doc/domain-model.md` §7; plan §9): a thin driver around the pure
//! [`plan_pass`]. A pass reads the read side ([`queries::fleet`]), maps it to the pass's inputs
//! ([`inputs_of`]), plans, and issues each planned command through [`AggregateStore`], where
//! the aggregate that owns it decides it again. It reads no aggregate state and no projection
//! inside a command; commands never read projections, and the query takes the read port only.
//!
//! **When.** At most once per [`ReconcilerConfig::interval`] (a minute) by the control plane's
//! clock, and at once when a host, pool, scale-set or placement stream moved ([`Reconciler::tick`]
//! reads the [`Kicks`] the store fills after each commit), or when an attempt became Prepared
//! ([`kick_prepared`], [`Reconciler::with_prepared`]: it may be granted now, CP-10). Any other
//! commit of an attempt stream starts no pass. A lost kick costs at most the interval: the pass
//! is level-triggered.
//!
//! **P2's driver (CP-10).** `PassCommand::Start` is decided by
//! [`start_attempt`](crate::start::start_attempt) itself, with its own reads of the host and
//! placement streams, its revision retry and its ids, so the pass adds no guard of its own and
//! takes none away: a start repeated over the same host revision is answered by its receipt
//! (pinned by `tests/store/reconciler.rs::should_start_once_when_a_stale_read_plans_the_start_again`),
//! a pin not materialised on the host starts nothing and leaves the lease held for the next pass
//! (`failed` with `not_placed`, never a release or a retry within the pass:
//! `should_start_a_lease_once_its_placement_materialises_when_its_pin_was_not_placed`), and a
//! lease that never starts is released after the start grace (`attempt_never_started`, below).
//! The attempt's stage retry budget is [`ReconcilerConfig::retry_budget`].
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
//! **A stopping run (RT-05a).** Terminal mode's graceful stop names this run's host with
//! [`Reconciler::set_stopping`]: every pass is then [`stopping_pass`], which plans that host as
//! draining (nothing reserved, started or granted there, its prepared attempts retired `drain`,
//! its unstarted reservations released; granted and listening ones wait, their deregistration
//! being RT-05b's). No drain is recorded on the host's stream, so nothing is left that the next
//! start must undo; the capacity inputs the passes record are re-planned by the next run's
//! passes. Pinned by
//! `tests/store/reconciler.rs::should_retire_and_admit_nothing_on_its_host_when_the_run_stops`.
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
//!
//! **A grant is decided at the save too (P9, CP-09).** `PassCommand::Activate` saves
//! `GrantActivation` on the lease's host with the host stream expected at the revision the pass
//! read and reads of every other host stream, the attempt stream, the pool's stream and its
//! scale-set stream at theirs. The pass's own commits move those revisions forward when they
//! follow them directly; a unit of the pass that appended past a write it had not read leaves
//! that stream unknown, and a later grant conditioned on it is not decided ([`Heads`]). So a
//! pass whose read is older than another pass's grant, on the same host or another one, or
//! older than a job leaving the scale set, grants nothing (`failed` with `conflict`; the next
//! pass reads again): between passes of the reconciler, the number of granted runners and
//! the scale set's trusted jobs a grant was decided on are those at its commit
//! (`formal/scheduling.qnt`'s `canGrant`, `jobLeaves` and `grantsWithinDemand`; pinned by
//! `tests/store/activation.rs::should_grant_no_second_attempt_when_a_racing_pass_read_before_the_first_grant`,
//! `should_grant_no_second_attempt_when_a_racing_pass_on_another_host_read_before_the_first_grant`
//! and `should_grant_nothing_when_a_job_leaves_the_scale_set_before_the_pass_applies_its_capacity`).
//! The pool's granted runners the read showed retiring (not counted) are read too, so a job
//! bound to one since, withdrawing its retirement, refuses the grant (pinned by
//! `tests/store/activation.rs::should_grant_nothing_when_a_retiring_runner_binds_a_job_after_the_read`); any other change
//! of another attempt's stream only makes the read count more runners than there are, which
//! grants less, never more.
//! The grant's id is derived from the attempt alone, so a grant recorded before (a pass that
//! died, or whose `BeginActivation` conflicted) is answered by its receipt, which names its
//! `LeaseActivated` event; then `BeginActivation{grant_ref}` is saved with a read of the host
//! stream at a revision that shows the lease granted. A refused grant (the gate or the pool's
//! terms closed, the lease gone) leaves the attempt Prepared for the next pass. The reconciler
//! is the only grantor: a lease granted under any other id is answered `already_activated`,
//! and its attempt is not activated by the pass (a limitation, CP-09's note).

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use mayfly_control_domain::attempt::{Attempt, AttemptCommand};
use mayfly_control_domain::capacity::Footprint;
use mayfly_control_domain::host::{Host, HostCommand, ReleaseEvidence, ReserveSlot};
use mayfly_control_domain::pool::{Pool, PoolCommand, PoolMode};
use mayfly_control_domain::reconcile::{
    HostReport, HostTerms, HostView, LeaseView, Pass, PassCommand, PassInputs, PassMemory,
    PolicyOf, PoolView, RunnerStage, ScaleSetView, Settings, exact, plan_pass, stopping_pass,
};
use mayfly_control_domain::scaleset::{ScaleSet, ScaleSetCommand};
use mayfly_control_domain::{Actor, Aggregate, Command, Rejection};
use mayfly_domain::{AttemptId, CommandId, Epoch, EventId, HostId, Millis, PoolName, Resources};
use sha2::{Digest, Sha256};

use crate::delivery::Kicks;
use crate::journal::{Appended, ExpectedRevision, Journal, StreamId, StreamRevision};
use crate::projection::ProjectionStore;
use crate::projection::views::{AttemptRow, SlotRow};
use crate::queries::{self, FleetView, QueryError};
use crate::start::{StartRequest, Started, start_attempt};
use crate::store::{
    AggregateStore, Clock, Committed, Outcome, Rng, SaveError, SaveRequest, StoreNotifier,
    stream_of,
};

/// The event type whose commit makes a pass due at once: an attempt that became Prepared may be
/// granted now (CP-10).
pub const PREPARED: &str = "attempt_prepared";

/// The default stage retry budget of the attempts the reconciler starts: `formal/lifecycle.qnt`'s
/// `BUDGET`, the depth its invariants are checked at.
pub const RETRY_BUDGET: u32 = 2;

/// Kicks `prepared` with every attempt stream `commit` appended an `attempt_prepared` to, and
/// nothing for any other commit: the reconciler's own wake for a VM that became Prepared
/// ([`Reconciler::with_prepared`]), which the stream kicks do not give, since a pass on every
/// attempt event would pass on every directive's reply. Pinned by
/// `tests/store/reconciler.rs::should_pass_at_once_when_an_attempt_becomes_prepared_and_not_on_its_other_events`.
pub fn kick_prepared(prepared: &Kicks, commit: &Committed) {
    if !commit.kinds.iter().any(|k| k.event_type == PREPARED) {
        return;
    }
    for appended in &commit.appended.streams {
        if appended.stream.as_str().starts_with("attempt-") {
            prepared.kick(&appended.stream);
        }
    }
}

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
    /// The stage retry budget of every attempt it starts (P2): [`RETRY_BUDGET`]. Not per pool
    /// yet: a pool's configuration carries none (CP-10's note).
    pub retry_budget: u32,
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
            retry_budget: RETRY_BUDGET,
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
    stopping: Option<HostId>,
}

/// The reconciler over an [`AggregateStore`] and the [`ProjectionStore`] it reads.
pub struct Reconciler<'s, J, C, R, N, P> {
    store: &'s AggregateStore<J, C, R, N>,
    projections: &'s P,
    config: ReconcilerConfig,
    kicks: Kicks,
    prepared: Option<Kicks>,
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
            prepared: None,
            timing: Mutex::new(Timing::default()),
        }
    }

    /// Also passes at once when `prepared` was kicked: the composition root feeds it with
    /// [`kick_prepared`] from the store's notifier, so a VM that became Prepared is granted
    /// without waiting for the interval (CP-10).
    #[must_use]
    pub fn with_prepared(mut self, prepared: Kicks) -> Self {
        self.prepared = Some(prepared);
        self
    }

    fn lock(&self) -> MutexGuard<'_, Timing> {
        self.timing.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Enters or leaves safe mode: a pass then plans `ApplyCapacity{SafeMode}` only (I29).
    pub fn set_safe_mode(&self, safe_mode: bool) {
        self.lock().safe_mode = safe_mode;
    }

    /// Plans `host` as draining from the next pass on, without a drain on its stream: terminal
    /// mode's graceful stop of this run (RT-05a); `None` plans every host as its stream says.
    pub fn set_stopping(&self, host: Option<HostId>) {
        self.lock().stopping = host;
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
    /// look, an attempt became Prepared ([`Reconciler::with_prepared`]), or the interval passed by
    /// the clock. `None` when nothing was due. Pinned by
    /// `tests/store/reconciler.rs::should_pass_at_most_once_an_interval_when_no_relevant_stream_is_kicked`
    /// and `should_pass_at_once_when_an_attempt_becomes_prepared_and_not_on_its_other_events`.
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
        let prepared = self
            .prepared
            .as_ref()
            .is_some_and(|kicks| !kicks.take().is_empty());
        if !kicked && !prepared && self.store.clock().now() < self.next_due() {
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
        let (memory, safe_mode, stopping) = {
            let timing = self.lock();
            (
                timing.memory.clone(),
                timing.safe_mode,
                timing.stopping.clone(),
            )
        };
        let inputs = inputs_of(
            &view,
            &self.config.policies,
            now,
            self.config.settings,
            safe_mode,
            memory,
        );
        let pass = match &stopping {
            Some(host) => stopping_pass(self.config.plan, &inputs, host),
            None => (self.config.plan)(&inputs),
        };
        let digest = versions_digest(&view);
        let mut report = PassReport {
            pass: pass.clone(),
            accepted: Vec::new(),
            refused: Vec::new(),
            failed: Vec::new(),
        };
        let mut ordinal: u32 = 0;
        let mut heads = Heads::of(&view);
        for command in &pass.commands {
            let answer = self
                .issue(command, &view, digest, &mut ordinal, &mut heads)
                .await;
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
        heads: &mut Heads,
    ) -> Answer {
        let answer = self.decide(command, view, digest, ordinal, heads).await;
        if let Some(appended) = &answer.appended {
            heads.advance(appended);
        }
        answer.code
    }

    async fn decide(
        &self,
        command: &PassCommand,
        view: &FleetView,
        digest: [u8; 32],
        ordinal: &mut u32,
        heads: &Heads,
    ) -> Saved {
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
            PassCommand::Start {
                host_id,
                attempt_id,
            } => self.start(host_id, *attempt_id).await,
            PassCommand::Activate {
                host_id,
                attempt_id,
            } => self.activate(host_id, *attempt_id, view, heads).await,
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

    async fn save<A: Aggregate>(&self, instance: &str, id: CommandId, body: A::Body) -> Saved
    where
        A::Body: Clone,
    {
        let request = SaveRequest::new(instance, self.command(id, body));
        saved_of(self.store.execute::<A>(request).await)
    }

    /// P2 through [`start_attempt`], its guards, retry and ids unchanged (see the module docs).
    async fn start(&self, host_id: &HostId, attempt_id: AttemptId) -> Saved {
        let request = StartRequest {
            host: host_id.clone(),
            attempt: attempt_id,
            retry_budget: self.config.retry_budget,
            run_epoch: self.config.run_epoch,
        };
        match start_attempt(self.store, &request).await {
            Ok(Started::Started { appended, replayed }) => Saved {
                code: Answer::Accepted,
                appended: (!replayed).then_some(appended),
            },
            Ok(Started::Refused(rejection)) => Saved {
                code: Answer::Refused(rejection.code()),
                appended: None,
            },
            // the host holds no lease for it any more: a definite no
            Ok(Started::NoLease) => Saved {
                code: Answer::Refused("no_lease"),
                appended: None,
            },
            // nothing to start from yet: the lease waits for a pin or a placement kick
            Ok(Started::NoImage) => Saved::failed("no_image"),
            Ok(Started::NotPlaced) => Saved::failed("not_placed"),
            Err(error) => failed_of(&error),
        }
    }

    /// P9: `GrantActivation` with the reads of what the pass read, then `BeginActivation` with
    /// the grant's event (see the module docs). Each half is saved once; a conflict leaves the
    /// rest to the next pass.
    async fn activate(
        &self,
        host_id: &HostId,
        attempt_id: AttemptId,
        view: &FleetView,
        heads: &Heads,
    ) -> Saved {
        let Some(row) = view.attempts.get(&attempt_id) else {
            return Saved::failed("save_failed");
        };
        let (Ok(host_stream), Ok(attempt_stream), Ok(scale_set_stream), Ok(pool_stream)) = (
            stream_of::<Host>(host_id.as_str()),
            stream_of::<Attempt>(&attempt_id.to_string()),
            stream_of::<ScaleSet>(row.pool.as_str()),
            stream_of::<Pool>(row.pool.as_str()),
        ) else {
            return Saved::failed("save_failed");
        };
        // a stream this pass appended to after a write it did not read is no longer known: the
        // grant is not decided now (the next pass reads again)
        // the pool's granted runners the read showed retiring are not counted; a job bound to
        // one since (its retirement withdrawn) makes it count again, so their streams are read
        // too (usually none)
        let retiring: Vec<StreamId> = view
            .leases
            .values()
            .flatten()
            .filter(|slot| slot.pool == row.pool && slot.phase == "activated")
            .filter(|slot| {
                slot.attempt_id != attempt_id
                    && view
                        .attempts
                        .get(&slot.attempt_id)
                        .is_some_and(|a| a.retirement_requested.is_some())
            })
            .filter_map(|slot| stream_of::<Attempt>(&slot.attempt_id.to_string()).ok())
            .collect();
        let reads = [&attempt_stream, &scale_set_stream, &pool_stream]
            .into_iter()
            .chain(&retiring)
            .chain(heads.hosts().filter(|s| **s != host_stream))
            .map(|s| heads.expected(s).map(|e| (s.clone(), e)))
            .collect::<Option<Vec<_>>>();
        let (Some(own), Some(reads)) = (heads.expected(&host_stream), reads) else {
            return Saved::failed("conflict");
        };
        let mut grant = SaveRequest::new(
            host_id.as_str(),
            self.command(
                grant_command_id(attempt_id),
                HostCommand::GrantActivation { attempt_id },
            ),
        )
        .with_expected(own);
        for (stream, expected) in reads {
            grant = grant.with_read(stream, expected);
        }
        let granted = self.store.save::<Host>(grant).await;
        let (grant_ref, at, mut appended) = match granted {
            Ok(Outcome::Accepted { appended, replayed }) => {
                let Some(event) = appended
                    .streams
                    .iter()
                    .find(|s| s.stream == host_stream)
                    .and_then(|s| s.event_ids.first().copied().zip(Some(s.last)))
                else {
                    return Saved::failed("save_failed");
                };
                // the begin reads the host stream where it shows the lease granted: this grant's
                // own revision when it committed now; when its receipt answered (a grant an
                // earlier pass recorded), the revision this pass knows the host stream at,
                // which already showed it (`RunnerStage::Granted`)
                let at = if replayed {
                    own
                } else {
                    ExpectedRevision::Exactly(event.1)
                };
                (event.0, at, (!replayed).then_some(appended))
            }
            other => return saved_of(other),
        };
        let begin = SaveRequest::new(
            attempt_id.to_string(),
            self.command(
                begin_command_id(attempt_id, grant_ref),
                AttemptCommand::BeginActivation { grant_ref },
            ),
        )
        .with_read(host_stream, at);
        let begun = saved_of(self.store.save::<Attempt>(begin).await);
        if let Some(more) = begun.appended {
            let streams = appended.get_or_insert_with(Appended::default);
            streams.streams.extend(more.streams);
        }
        Saved {
            code: begun.code,
            appended,
        }
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
    ) -> Saved {
        let Ok(attempt_stream) = stream_of::<Attempt>(&attempt_id.to_string()) else {
            return Saved::failed("save_failed");
        };
        let request = SaveRequest::new(host, self.command(id, body))
            .with_read(attempt_stream, ExpectedRevision::NoStream);
        saved_of(self.store.save::<Host>(request).await)
    }
}

enum Answer {
    Accepted,
    Refused(&'static str),
    Failed(&'static str),
}

/// A command's answer and, when this call committed it, what it appended.
struct Saved {
    code: Answer,
    appended: Option<Appended>,
}

impl Saved {
    const fn failed(code: &'static str) -> Self {
        Self {
            code: Answer::Failed(code),
            appended: None,
        }
    }
}

fn saved_of<R: Rejection>(saved: Result<Outcome<R>, SaveError>) -> Saved {
    match saved {
        Ok(Outcome::Accepted { appended, replayed }) => Saved {
            code: Answer::Accepted,
            appended: (!replayed).then_some(appended),
        },
        Ok(Outcome::Refused { rejection, .. }) => Saved {
            code: Answer::Refused(rejection.code()),
            appended: None,
        },
        Err(error) => failed_of(&error),
    }
}

/// A save that did not decide: the next pass plans again.
fn failed_of(error: &SaveError) -> Saved {
    match error {
        SaveError::CommandIdReused { .. } => Saved::failed("command_id_reused"),
        e if e.is_retryable() => Saved::failed("conflict"),
        _ => Saved::failed("save_failed"),
    }
}

/// The revision of every stream a grant is conditioned on: the hosts, pools, attempts and
/// scale sets as the pass read them (`Some`), moved forward by each unit the pass itself
/// commits right after it, so a pass's own reservations and grants never refuse its later
/// grants while anything else's write does. A unit of the pass that appended after a write the
/// pass did not read (its commands are decided on the stream as it is, `execute` retrying a
/// conflict) leaves the stream unknown (`None`): a later grant conditioned on it is not
/// decided. Pinned by
/// `tests/store/activation.rs::should_grant_nothing_when_a_job_leaves_the_scale_set_before_the_pass_applies_its_capacity`.
#[derive(Debug)]
struct Heads(BTreeMap<StreamId, Option<u64>>);

impl Heads {
    fn of(view: &FleetView) -> Self {
        let mut heads = BTreeMap::new();
        let mut put = |stream: Result<StreamId, _>, revision: u64| {
            if let Ok(stream) = stream {
                heads.insert(stream, Some(revision));
            }
        };
        for (key, row) in &view.hosts {
            put(StreamId::new(key), row.revision);
        }
        for (key, row) in &view.pools {
            put(StreamId::new(key), row.revision);
        }
        for (key, row) in &view.scale_sets {
            put(StreamId::new(key), row.revision);
        }
        for (id, row) in &view.attempts {
            put(stream_of::<Attempt>(&id.to_string()), row.revision);
        }
        Self(heads)
    }

    /// The revision `stream` is known at (no stream when the pass saw none), or `None` once a
    /// unit of the pass appended to it past a write the pass did not read.
    fn expected(&self, stream: &StreamId) -> Option<ExpectedRevision> {
        match self.0.get(stream) {
            None => Some(ExpectedRevision::NoStream),
            Some(Some(r)) => Some(ExpectedRevision::Exactly(StreamRevision::new(*r))),
            Some(None) => None,
        }
    }

    fn hosts(&self) -> impl Iterator<Item = &StreamId> {
        self.0.keys().filter(|s| s.as_str().starts_with("host-"))
    }

    /// Moves every stream a unit of this pass appended to its new head when the unit's first
    /// event follows the head it was known at; otherwise something else wrote in between, and
    /// the stream is unknown for the rest of the pass.
    fn advance(&mut self, appended: &Appended) {
        for stream in &appended.streams {
            let follows = match self.0.get(&stream.stream) {
                None => stream.first.get() == 0,
                Some(Some(head)) => stream.first.get() == head.saturating_add(1),
                Some(None) => false,
            };
            self.0
                .insert(stream.stream.clone(), follows.then(|| stream.last.get()));
        }
    }
}

/// The id of the reconciler's `GrantActivation` for `attempt`: one per attempt, since a lease
/// is granted once, so a grant repeated by a later pass is answered by its receipt, which names
/// the `LeaseActivated` its `BeginActivation` refers to. Pinned by
/// `tests/store/activation.rs::should_activate_a_granted_attempt_when_its_activation_was_lost`.
#[must_use]
pub fn grant_command_id(attempt: AttemptId) -> CommandId {
    derive(b"grant\n", &[&attempt.as_u128().to_be_bytes()])
}

/// The id of the reconciler's `BeginActivation` of `attempt` under the grant `grant_ref`.
#[must_use]
pub fn begin_command_id(attempt: AttemptId, grant_ref: EventId) -> CommandId {
    derive(
        b"begin\n",
        &[
            &attempt.as_u128().to_be_bytes(),
            &grant_ref.as_u128().to_be_bytes(),
        ],
    )
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
/// as no runner, never retired, never released. A lease the host shows granted is a runner from
/// the grant on (CP-09): its prepared attempt is `Granted` (its `BeginActivation` not recorded
/// yet), and one backing off from its activation is `Activating`, never a VM on its way to
/// Prepared, so the pass grants no other attempt for the job it serves.
fn stage_of(attempt: Option<&AttemptRow>, slot: &SlotRow) -> RunnerStage {
    let Some(row) = attempt else {
        return RunnerStage::NotStarted;
    };
    let granted = slot.phase == "activated";
    match row.phase.as_str() {
        "reserved" | "cloning" | "booting" | "verifying" | "backoff" if granted => {
            RunnerStage::Activating
        }
        "reserved" | "cloning" | "booting" | "verifying" | "backoff" => RunnerStage::Preparing,
        "prepared" if granted => RunnerStage::Granted,
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
                                stage: stage_of(attempt, slot),
                                image: attempt.map(|a| a.image_digest.clone()),
                                refusal: attempt.and_then(|a| a.capacity_refused_at),
                                retiring: attempt.is_some_and(|a| a.retirement_requested.is_some()),
                                prepared_at: attempt.and_then(|a| a.prepared_at),
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
                    waiting: s.waiting,
                    active: s.active,
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
