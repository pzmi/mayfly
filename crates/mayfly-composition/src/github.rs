//! `mayflyd`'s GitHub side (RT-07; plan §7, dm P9–P14; ADRs 0029, 0033, 0040, 0045, 0091): the
//! one client, the scale-set directory the registration manager reads, and the reaper's desk
//! that `mayfly doctor` asks.
//!
//! - **One client** ([`client`]): the App's auth chain (the key asked of the daemon's
//!   `SecretProvider` on each refresh, by its reference only, ADR 0033) under one
//!   `ScaleSetClient`, which serves every pool's listener, the registration manager's mints
//!   and removals and the reaper's listing. Its bases are `[github] api_url` and `web_url`
//!   (github.com when absent, owner 2026-11-13): nothing is sent anywhere else (ADR 0036).
//! - **The scale-set spec** of a pool ([`spec_of`]): the scale set is named after the pool, in
//!   the runner group `Default`, with the pool's name as its one label (ADR 0040: the name is
//!   the label workflows use; owner 2026-11-13: no new pool keys).
//! - **The directory** ([`Directory`]): each pool's scale set and its session's health as the
//!   journal last showed them, refreshed by the pool's listener after every tick, so the
//!   manager's synchronous read never waits on the store.
//! - **The reaper's desk** ([`ReaperDesk`]): the last sweep's report, kept across runs, and the
//!   `doctor` request's way to ask the run for a sweep (P14's `doctor` trigger).
//!
//! # One run's GitHub loops ([`GithubRun`])
//!
//! Each starts once the run's recovery completed (or reported safe mode) and ends with the
//! run; none is part of the ready line's promise, so a GitHub that does not answer never holds
//! a run back (`crate::daemon::Ready`).
//!
//! - **One listener per pool** (GH-04's note): `Listener::tick` in a loop, sleeping only until
//!   a `Tick::Waiting` instant, under the pool's local lease ([`ListenerLeases`]), given the
//!   reconciler's kicks (an empty poll passes it); on the run's end it closes its session
//!   (`Listener::close`, bounded by [`LISTENER_CLOSE`]). A tick is cut by the run's end only at
//!   an await: each save is one atomic unit, and a recorded message not yet acked is delivered
//!   again and acked as the cursor's duplicate (I12), so nothing is half done. A fenced
//!   listener (a newer run owns the stream) stops.
//! - **The job facts** (P12) and **the registration manager** (P9–P13) as durable consumers,
//!   each on its own worker and kicks. The manager's worker gives each `handle`
//!   [`registration_delivery`]'s deadline, longer than a mint's whole call budget with its
//!   `Retry-After` waits ([`DEFAULT_CALL_POLICY`]), so a mint is never cut mid-call and
//!   recorded as an unknown issue for that alone (CP-05's note). A run that ends mid-`handle`
//!   drops it without releasing its claim: the stream is taken again once that lease lapses
//!   (about four minutes, [`registration_delivery`]), so after a link that flapped the manager's
//!   consumer may lag that long; the pass and the sweep do not wait on it.
//! - **The pass** (`RegistrationManager::pass`) once a minute, and **the sweep** (P14): at run
//!   start once every pool's scale set is journaled, daily, when a session opens after an
//!   outage of [`LONG_OUTAGE`] or more, and when `doctor` asks; at most one listing an hour
//!   whatever asks, counted across the process's runs: each listing asked (answered or not) is
//!   kept on the [`ReaperDesk`] and seeds the next run's manager, so a flapping link never lists
//!   once per run. The consumer, the pass and the sweep share one manager per run (its step
//!   guard and busy backoffs, which a new run starts afresh: CP-05's note). A removal a sweep
//!   asks for, `doctor`'s included, is journaled under the automation actor
//!   `registration_manager`, never an operator's: `doctor` only asks for a listing.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::task::Poll;
use std::time::Duration;

use mayfly_adapter_github::{
    ApiClient, AppAuth, AppConfig, DEFAULT_CALL_POLICY, ReqwestTransport, ScaleSetClient, Scope,
};
use mayfly_adapter_secrets::UriSecretProvider;
use mayfly_control::delivery::{DeliveryConfig, DeliveryWorker, Kicks};
use mayfly_control::github::ScaleSetSpec;
use mayfly_control::listener::{JobFacts, Listener, ListenerConfig, ListenerError, Tick};
use mayfly_control::registration::{
    JitBroker, RegistrationConfig, RegistrationManager, ScaleSetDirectory, SweepError, SweepReport,
    SweepTrigger,
};
use mayfly_control::sessions::HostSessions;
use mayfly_control::store::{AggregateStore, Clock as _};
use mayfly_control_domain::pool::TrustTier;
use mayfly_control_domain::registration::{ScaleSetId, SessionHealth};
use mayfly_control_domain::scaleset::{Phase, ScaleSet, ScaleSetState};
use mayfly_domain::{Epoch, HostId, Millis, PoolName};
use mayfly_protocol::Mode;
use tokio::sync::{Notify, watch};

use crate::clocks::{AnchoredStoreClock, OsRng, SystemClock};
use crate::config::{GithubConfig, GithubScope, PoolConfig, Tier};
use crate::operator::wire::{Reaper, Sweep, UnrecognisedRunner};
use crate::supervise::{Notifier, ShutdownSignal};

/// The client every GitHub call of the daemon goes through.
pub type Client = ScaleSetClient<ReqwestTransport, SystemClock, OsRng, UriSecretProvider>;

type Journal = mayfly_adapter_sqlite::SqliteJournal;
type Store = AggregateStore<Journal, SystemClock, OsRng, Notifier>;
type Ledger = mayfly_adapter_sqlite::SqliteLedger<AnchoredStoreClock>;
type Worker = DeliveryWorker<Journal, Ledger, OsRng, SystemClock>;
type Manager<'r> =
    RegistrationManager<'r, Journal, SystemClock, OsRng, Notifier, Client, Client, Directory>;
type PoolListener<'r> = Listener<'r, Journal, SystemClock, OsRng, Notifier, Client>;

/// The least time between two passes of the registration manager (CP-05: at most once a
/// minute).
pub const PASS_EVERY: Duration = Duration::from_secs(60);

/// How often the daily sweep comes.
pub const DAILY: Duration = Duration::from_secs(86_400);

/// An outage after which a session that opens again is followed by a sweep (P14: "after a long
/// outage"): the listing is limited to one an hour anyway.
pub const LONG_OUTAGE: Duration = Duration::from_secs(15 * 60);

/// How long a listener's orderly close (the session's delete) may take at the run's end.
pub const LISTENER_CLOSE: Duration = Duration::from_secs(10);

/// The first wait after a listener's tick failed on the store or a refusal (not GitHub: those
/// back off inside the listener), doubled per failure in a row up to five minutes, so a fault
/// that persists is retried and logged at a falling rate, never spun on.
const LISTENER_RETRY: Duration = Duration::from_secs(5);

/// The least and most a listener sleeps toward a `Tick::Waiting` instant.
const WAIT_FLOOR: Duration = Duration::from_millis(100);
const WAIT_CAP: Duration = Duration::from_secs(300);

/// The registration manager's delivery: each `handle` may take a mint's whole call budget, its
/// retries and `Retry-After` waits included ([`DEFAULT_CALL_POLICY`]'s deadline), and a minute
/// more, under a lease that outlives it (CP-05's note). Pinned by
/// `tests::should_give_a_handle_more_than_a_mints_call_budget_when_the_manager_is_delivered_to`.
#[must_use]
pub fn registration_delivery() -> DeliveryConfig {
    let handle = Millis::new(DEFAULT_CALL_POLICY.deadline.get().saturating_add(60_000));
    DeliveryConfig {
        handle_deadline: handle,
        lease_ttl: Millis::new(handle.get().saturating_mul(4) / 3 + 1_000),
        ..DeliveryConfig::default()
    }
}

/// What the daemon keeps of GitHub across its runs.
#[derive(Debug)]
pub struct GithubSide {
    /// The one client.
    pub client: Client,
    /// Each pool's scale set.
    pub directory: Directory,
    /// The reaper's last report and `doctor`'s asks.
    pub reaper: ReaperDesk,
    /// One listener per scale set.
    pub leases: ListenerLeases,
}

impl GithubSide {
    /// The side over `client`, nothing known yet.
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client,
            directory: Directory::default(),
            reaper: ReaperDesk::default(),
            leases: ListenerLeases::default(),
        }
    }
}

/// The runner group every pool's scale set is looked up in (owner, 2026-11-13).
pub const RUNNER_GROUP: &str = "Default";

/// Why the client could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientError {
    /// The TLS stack could not be set up.
    Tls,
    /// The random generator could not be seeded.
    Random,
    /// The configuration was refused by the auth chain (a programming error: the reader checks
    /// the same rules).
    Config,
}

/// The App's configuration from `[github]`.
#[must_use]
pub fn app_config(github: &GithubConfig) -> AppConfig {
    AppConfig {
        api_base: github.api_url.clone(),
        web_base: github.web_url.clone(),
        client_id: github.app_client_id.clone(),
        installation_id: github.installation_id,
        scope: match &github.scope {
            GithubScope::Organisation { organisation } => Scope::Organization {
                org: organisation.clone(),
            },
            GithubScope::Repository {
                organisation,
                repository,
            } => Scope::Repository {
                owner: organisation.clone(),
                repo: repository.clone(),
            },
        },
        app_key: github.private_key.clone(),
    }
}

/// The client over `[github]`, the key asked of `secrets`.
///
/// # Errors
/// [`ClientError`].
pub fn client(github: &GithubConfig, secrets: UriSecretProvider) -> Result<Client, ClientError> {
    let transport = ReqwestTransport::new().map_err(|_| ClientError::Tls)?;
    let rng = OsRng::seeded().map_err(|_| ClientError::Random)?;
    let api = ApiClient::new(transport, SystemClock, rng, DEFAULT_CALL_POLICY);
    let auth = AppAuth::new(app_config(github), api, secrets).map_err(|_| ClientError::Config)?;
    Ok(ScaleSetClient::new(auth))
}

/// The scale set serving `pool`: named after it, in [`RUNNER_GROUP`], its name its one label.
/// Pinned by `tests::should_name_the_scale_set_after_the_pool_when_a_spec_is_made`.
#[must_use]
pub fn spec_of(pool: &PoolName) -> ScaleSetSpec {
    ScaleSetSpec {
        name: pool.as_str().to_owned(),
        runner_group: RUNNER_GROUP.to_owned(),
        labels: vec![pool.as_str().to_owned()],
    }
}

/// The session's health the registration manager reads (P21): minting is frozen only while
/// the session is degraded; any other phase lets the mint ask GitHub, whose answer decides.
#[must_use]
pub const fn health(phase: Phase) -> SessionHealth {
    match phase {
        Phase::Degraded(_) => SessionHealth::Degraded,
        Phase::Unconfigured
        | Phase::Ensured
        | Phase::Opening
        | Phase::Active
        | Phase::Backoff(_)
        | Phase::Closing
        | Phase::Closed => SessionHealth::Healthy,
    }
}

/// Each pool's scale set as its stream last showed it.
#[derive(Debug, Default)]
pub struct Directory {
    sets: Mutex<BTreeMap<PoolName, (ScaleSetId, SessionHealth)>>,
}

impl Directory {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<PoolName, (ScaleSetId, SessionHealth)>> {
        self.sets.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records `pool`'s stream state: its scale set once journaled, none before.
    pub fn record(&self, pool: &PoolName, state: &ScaleSetState) {
        let mut sets = self.lock();
        match state.github_id {
            Some(id) => {
                sets.insert(pool.clone(), (id, health(state.phase)));
            }
            None => {
                sets.remove(pool);
            }
        }
    }

    /// Whether every one of `pools` has a scale set journaled.
    #[must_use]
    pub fn covers<'p>(&self, pools: impl IntoIterator<Item = &'p PoolName>) -> bool {
        let sets = self.lock();
        pools.into_iter().all(|pool| sets.contains_key(pool))
    }
}

impl ScaleSetDirectory for Directory {
    fn scale_set(&self, pool: &PoolName) -> Option<(ScaleSetId, SessionHealth)> {
        self.lock().get(pool).copied()
    }

    fn scale_sets(&self) -> BTreeSet<ScaleSetId> {
        self.lock().values().map(|(id, _)| *id).collect()
    }
}

/// How long `doctor`'s request waits for the run's sweep before it answers with what is known.
pub const DOCTOR_WAIT: Duration = Duration::from_secs(15);

/// The reaper's last report, for `mayfly doctor`, and `doctor`'s asks of the run's reaper.
#[derive(Debug)]
pub struct ReaperDesk {
    last: Mutex<Reaper>,
    /// When this process last asked GitHub for every runner (answered or not): P14's hourly
    /// budget, kept across runs so a new run (a flapping link) does not list again at once.
    listed_at: Mutex<Option<Millis>>,
    asked: Notify,
    answered: watch::Sender<u64>,
}

impl Default for ReaperDesk {
    fn default() -> Self {
        Self {
            last: Mutex::new(Reaper::default()),
            listed_at: Mutex::new(None),
            asked: Notify::new(),
            answered: watch::channel(0).0,
        }
    }
}

impl ReaperDesk {
    fn lock(&self) -> MutexGuard<'_, Reaper> {
        self.last.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What the desk holds now.
    #[must_use]
    pub fn report(&self) -> Reaper {
        self.lock().clone()
    }

    /// When this process last asked GitHub for every runner, if it did.
    #[must_use]
    pub fn listed_at(&self) -> Option<Millis> {
        *self
            .listed_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Records the last listing a manager asked for (never moving it back).
    pub fn listed(&self, at: Option<Millis>) {
        let mut listed = self
            .listed_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *listed = (*listed).max(at);
    }

    /// Asks the run's reaper for a sweep (`doctor`; it lists only within P14's hourly budget)
    /// and waits for its answer, at most `within`, then answers what the desk holds. With no
    /// run to answer, the wait runs out and the last report is answered.
    pub async fn ask(&self, within: Duration) -> Reaper {
        let mut answered = self.answered.subscribe();
        let seen = *answered.borrow_and_update();
        self.asked.notify_one();
        let _ = tokio::time::timeout(within, answered.wait_for(|n| *n != seen)).await;
        self.report()
    }

    /// Completes once `doctor` asked (a permit is kept when nobody waits).
    pub async fn asked(&self) {
        self.asked.notified().await;
    }

    /// Tells the asker that its sweep was taken (or refused by the budget).
    pub fn answered(&self) {
        self.answered.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// Records a sweep that listed.
    pub fn record(&self, report: &SweepReport) {
        let mut last = self.lock();
        last.last = Some(sweep_of(report));
        last.failure = None;
    }

    /// Records a sweep whose listing failed, by its fixed code; the last report is kept.
    pub fn failed(&self, code: &str) {
        self.lock().failure = Some(code.to_owned());
    }
}

fn trigger_code(trigger: SweepTrigger) -> &'static str {
    match trigger {
        SweepTrigger::RunStarted => "run_started",
        SweepTrigger::AfterOutage => "after_outage",
        SweepTrigger::Daily => "daily",
        SweepTrigger::Doctor => "doctor",
    }
}

fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn sweep_of(report: &SweepReport) -> Sweep {
    Sweep {
        trigger: trigger_code(report.trigger).to_owned(),
        at_ms: report.at.get(),
        listed: count(report.listed),
        deregistered: count(report.deregistered.len()),
        pending: count(report.pending.len()),
        unrecognised: report
            .unrecognised
            .iter()
            .map(|runner| UnrecognisedRunner {
                id: runner.id.0,
                name: runner.name.clone(),
                scale_set: runner.scale_set.map(|s| s.0),
            })
            .collect(),
        foreign: count(report.foreign),
    }
}

/// One listener per scale set (plan §7.2): the pools whose listener runs in this process. A
/// run's listener holds its pool's lease for its whole life, so a listener left behind by an
/// earlier run of the same process can never poll beside the next one's.
#[derive(Debug, Default)]
pub struct ListenerLeases {
    held: Mutex<BTreeSet<PoolName>>,
}

/// A held lease, released when dropped.
#[derive(Debug)]
pub struct ListenerLease<'l> {
    leases: &'l ListenerLeases,
    pool: PoolName,
}

impl ListenerLeases {
    /// `pool`'s lease, unless a listener holds it. Pinned by
    /// `tests::should_refuse_a_second_lease_when_the_first_is_still_held`.
    #[must_use]
    pub fn acquire(&self, pool: &PoolName) -> Option<ListenerLease<'_>> {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        held.insert(pool.clone()).then(|| ListenerLease {
            leases: self,
            pool: pool.clone(),
        })
    }
}

impl Drop for ListenerLease<'_> {
    fn drop(&mut self) {
        self.leases
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.pool);
    }
}

/// What one run's GitHub loops need from the daemon.
#[derive(Clone, Copy)]
pub struct RunInputs<'r> {
    /// The store.
    pub store: &'r Store,
    /// Its journal, which the consumers' ledger is over.
    pub journal: &'r Journal,
    /// The store clock the ledger reads.
    pub store_clock: AnchoredStoreClock,
    /// The after-commit kicks.
    pub notifier: &'r Notifier,
    /// The client, directory, desk and leases.
    pub side: &'r GithubSide,
    /// The minted JIT configs, held for their launch (LNK-02 takes them).
    pub broker: &'r JitBroker,
    /// This host, which names each session's owner.
    pub host: &'r HostId,
    /// The configured pools: one listener each.
    pub pools: &'r [PoolConfig],
    /// The run's host sessions, which the manager reads epochs from.
    pub sessions: &'r HostSessions,
    /// The run's epoch.
    pub epoch: Epoch,
}

impl std::fmt::Debug for RunInputs<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunInputs")
            .field("host", self.host)
            .field("pools", &self.pools.len())
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

/// One run's GitHub loops (module docs).
pub struct GithubRun<'r> {
    inputs: RunInputs<'r>,
    listeners: Vec<(PoolName, PoolListener<'r>)>,
    manager: Manager<'r>,
    facts: JobFacts<'r, Journal, SystemClock, OsRng, Notifier>,
    manager_worker: Worker,
    facts_worker: Worker,
    /// Bumped after every listener tick: the reaper's first sweep waits on the directory.
    ticked: watch::Sender<u64>,
    /// A session opened after a long outage.
    outage: Notify,
}

impl std::fmt::Debug for GithubRun<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GithubRun")
            .field("pools", &self.listeners.len())
            .finish_non_exhaustive()
    }
}

fn worker(inputs: &RunInputs<'_>, config: DeliveryConfig) -> Result<Worker, ClientError> {
    Ok(DeliveryWorker::new(
        inputs.journal.clone(),
        inputs.journal.ledger(inputs.store_clock),
        OsRng::seeded().map_err(|_| ClientError::Random)?,
        SystemClock,
        config,
    ))
}

/// Polls every future until all are done: a join of a set sized at run time, nothing spawned
/// (`crate::supervise`).
async fn join_all(mut futures: Vec<Pin<Box<dyn Future<Output = ()> + '_>>>) {
    std::future::poll_fn(|cx| {
        futures.retain_mut(|f| f.as_mut().poll(cx).is_pending());
        if futures.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}

/// Sleeps `duration`, or less when `cancel` comes first; whether it was cancelled.
async fn sleep_or(cancel: &mut ShutdownSignal, duration: Duration) -> bool {
    tokio::select! {
        biased;
        () = cancel.cancelled() => true,
        () = tokio::time::sleep(duration) => false,
    }
}

/// `work`'s output, or `None` when `cancel` came first: the run's end does not wait for a GitHub
/// call (its deadline is up to two minutes), it drops it, and what it would have recorded is
/// left to the next run (an unanswered mint is an unknown issue, never minted again: I19).
async fn unless_cancelled<T>(
    cancel: &mut ShutdownSignal,
    work: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        out = work => Some(out),
    }
}

/// Waits until recovery reported a mode; `false` when the run ended first.
async fn recovered(
    recovered: &mut watch::Receiver<Option<Mode>>,
    cancel: &mut ShutdownSignal,
) -> bool {
    tokio::select! {
        () = cancel.cancelled() => false,
        // `Ok` only: a run whose recovery failed drops the sender
        Ok(_) = recovered.wait_for(Option::is_some) => true,
    }
}

impl<'r> GithubRun<'r> {
    /// The run's listeners, consumers and workers.
    ///
    /// # Errors
    /// [`ClientError::Random`] when a worker's generator cannot be seeded.
    pub fn new(inputs: RunInputs<'r>) -> Result<Self, ClientError> {
        let listeners = inputs
            .pools
            .iter()
            .map(|pool| {
                let config = ListenerConfig::new(
                    pool.name.clone(),
                    spec_of(&pool.name),
                    match pool.tier {
                        Tier::Trusted => TrustTier::Trusted,
                    },
                    format!("{}-{}", inputs.host, pool.name.as_str()),
                    inputs.epoch,
                );
                let listener = Listener::new(inputs.store, &inputs.side.client, config)
                    .with_kicks(inputs.notifier.reconciler.clone());
                (pool.name.clone(), listener)
            })
            .collect();
        let manager = RegistrationManager::new(
            inputs.store,
            inputs.sessions.clone(),
            &inputs.side.client,
            &inputs.side.client,
            &inputs.side.directory,
            inputs.broker,
            RegistrationConfig::default(),
        );
        // P14's budget outlives the run (a flapping link lists at most once an hour); the busy
        // backoffs and the online deadlines do not, on purpose: a new run waits anew, and a busy
        // leaked runner's removal may be asked once more at its first step
        manager.seed_listing(inputs.side.reaper.listed_at());
        Ok(Self {
            listeners,
            manager,
            facts: JobFacts::new(inputs.store, inputs.epoch),
            manager_worker: worker(&inputs, registration_delivery())?,
            facts_worker: worker(&inputs, DeliveryConfig::default())?,
            ticked: watch::channel(0).0,
            outage: Notify::new(),
            inputs,
        })
    }

    /// Every loop of the run's GitHub side, until `cancel`; each waits for `recovery` first.
    pub async fn serve(&self, recovery: &watch::Receiver<Option<Mode>>, cancel: &ShutdownSignal) {
        let mut loops: Vec<Pin<Box<dyn Future<Output = ()> + '_>>> = Vec::new();
        for (pool, listener) in &self.listeners {
            loops.push(Box::pin(self.listen(
                pool,
                listener,
                recovery.clone(),
                cancel.clone(),
            )));
        }
        loops.push(Box::pin(
            self.consume_facts(recovery.clone(), cancel.clone()),
        ));
        loops.push(Box::pin(
            self.consume_registrations(recovery.clone(), cancel.clone()),
        ));
        loops.push(Box::pin(self.pass(recovery.clone(), cancel.clone())));
        loops.push(Box::pin(self.reap(recovery.clone(), cancel.clone())));
        join_all(loops).await;
    }

    /// Records `pool`'s stream state in the directory, then wakes the reaper's first wait.
    async fn refresh(&self, pool: &PoolName) {
        match self.inputs.store.load::<ScaleSet>(pool.as_str()).await {
            Ok(state) => self.inputs.side.directory.record(pool, &state),
            Err(_) => tracing::warn!(pool = pool.as_str(), "scale_set_unread"),
        }
        self.ticked.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// One pool's listener (module docs).
    async fn listen(
        &self,
        pool: &PoolName,
        listener: &PoolListener<'r>,
        mut recovery: watch::Receiver<Option<Mode>>,
        mut cancel: ShutdownSignal,
    ) {
        if !recovered(&mut recovery, &mut cancel).await {
            return;
        }
        let Some(_lease) = self.inputs.side.leases.acquire(pool) else {
            tracing::error!(pool = pool.as_str(), "listener_lease_held");
            return;
        };
        self.refresh(pool).await;
        let mut lost_since: Option<tokio::time::Instant> = None;
        let mut retry = LISTENER_RETRY;
        loop {
            let ticked = tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                ticked = listener.tick() => ticked,
            };
            match ticked {
                Ok(Tick::Waiting(until)) => {
                    let now = self.inputs.store.clock().now().get();
                    let wait = Duration::from_millis(until.get().saturating_sub(now))
                        .clamp(WAIT_FLOOR, WAIT_CAP);
                    if sleep_or(&mut cancel, wait).await {
                        break;
                    }
                    continue;
                }
                Ok(Tick::Opened(_)) => {
                    if lost_since.is_some_and(|since| since.elapsed() >= LONG_OUTAGE) {
                        self.outage.notify_one();
                    }
                    lost_since = None;
                }
                Ok(Tick::SessionLost(_) | Tick::OpenFailed(_)) => {
                    lost_since.get_or_insert_with(tokio::time::Instant::now);
                }
                // a poll that answered: the session lives, whatever failed around it
                Ok(Tick::Polled(polled)) if polled.poll_failure.is_none() => lost_since = None,
                Ok(_) => {}
                Err(ListenerError::Fenced) => {
                    tracing::warn!(pool = pool.as_str(), "listener_fenced");
                    return;
                }
                Err(error) => {
                    let code = match &error {
                        ListenerError::Store(_) => "store",
                        ListenerError::Refused(code) => *code,
                        ListenerError::Name(_) => "name",
                        ListenerError::Fenced => "fenced",
                    };
                    tracing::warn!(
                        pool = pool.as_str(),
                        code,
                        retry_s = retry.as_secs(),
                        "listener_tick_failed"
                    );
                    if sleep_or(&mut cancel, retry).await {
                        break;
                    }
                    retry = retry.saturating_mul(2).min(WAIT_CAP);
                    self.refresh(pool).await;
                    continue;
                }
            }
            retry = LISTENER_RETRY;
            self.refresh(pool).await;
        }
        match tokio::time::timeout(LISTENER_CLOSE, listener.close()).await {
            Ok(Ok(Tick::Closed)) => {}
            Ok(Ok(_) | Err(_)) | Err(_) => {
                tracing::warn!(pool = pool.as_str(), "listener_close_failed");
            }
        }
    }

    /// P12's consumer of recorded messages.
    async fn consume_facts(
        &self,
        mut recovery: watch::Receiver<Option<Mode>>,
        mut cancel: ShutdownSignal,
    ) {
        if !recovered(&mut recovery, &mut cancel).await {
            return;
        }
        let kicks: &Kicks = &self.inputs.notifier.job_facts;
        loop {
            if self.facts_worker.catch_up(&self.facts).await.is_err() {
                tracing::warn!(consumer = "job_facts", "consumer_catch_up_failed");
            }
            let coarse = tokio::time::Instant::now() + crate::daemon::COARSE;
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return,
                    () = kicks.wait() => {
                        if self.facts_worker.drain_kicked(&self.facts, kicks).await.is_err() {
                            tracing::warn!(consumer = "job_facts", "consumer_drain_failed");
                        }
                    }
                    () = tokio::time::sleep_until(coarse) => break,
                }
            }
        }
    }

    /// The registration manager's consumer (P9, P11, P13).
    async fn consume_registrations(
        &self,
        mut recovery: watch::Receiver<Option<Mode>>,
        mut cancel: ShutdownSignal,
    ) {
        if !recovered(&mut recovery, &mut cancel).await {
            return;
        }
        let kicks: &Kicks = &self.inputs.notifier.registration;
        loop {
            let caught_up = self.manager_worker.catch_up(&self.manager);
            match unless_cancelled(&mut cancel, caught_up).await {
                None => return,
                Some(Err(_)) => {
                    tracing::warn!(
                        consumer = "registration_manager",
                        "consumer_catch_up_failed"
                    );
                }
                Some(Ok(_)) => {}
            }
            let coarse = tokio::time::Instant::now() + crate::daemon::COARSE;
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return,
                    () = kicks.wait() => {}
                    () = tokio::time::sleep_until(coarse) => break,
                }
                let drained = self.manager_worker.drain_kicked(&self.manager, kicks);
                match unless_cancelled(&mut cancel, drained).await {
                    None => return,
                    Some(Err(_)) => {
                        tracing::warn!(consumer = "registration_manager", "consumer_drain_failed");
                    }
                    Some(Ok(_)) => {}
                }
            }
        }
    }

    /// The manager's level-triggered pass, at most once a minute.
    async fn pass(&self, mut recovery: watch::Receiver<Option<Mode>>, mut cancel: ShutdownSignal) {
        if !recovered(&mut recovery, &mut cancel).await {
            return;
        }
        loop {
            if sleep_or(&mut cancel, PASS_EVERY).await {
                return;
            }
            let Some(report) = unless_cancelled(&mut cancel, self.manager.pass()).await else {
                return;
            };
            if !report.failed.is_empty() {
                tracing::warn!(failed = report.failed.len(), "registration_pass_failed");
            }
        }
    }

    /// P14's sweeps (module docs).
    async fn reap(&self, mut recovery: watch::Receiver<Option<Mode>>, mut cancel: ShutdownSignal) {
        let desk = &self.inputs.side.reaper;
        // until recovery reported, `doctor` gets the desk's report at once
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                Ok(_) = recovery.wait_for(Option::is_some) => break,
                () = desk.asked() => desk.answered(),
            }
        }
        let pools: Vec<&PoolName> = self.inputs.pools.iter().map(|p| &p.name).collect();
        // run start: once every pool's scale set is journaled (a sweep before that would read
        // our own scale sets' runners as foreign); `doctor` meanwhile gets the desk's report
        let mut ticked = self.ticked.subscribe();
        while !self.inputs.side.directory.covers(pools.iter().copied()) {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                () = desk.asked() => desk.answered(),
                changed = ticked.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
        if unless_cancelled(&mut cancel, self.sweep(SweepTrigger::RunStarted))
            .await
            .is_none()
        {
            return;
        }
        let mut daily = tokio::time::Instant::now() + DAILY;
        loop {
            let trigger = tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                () = desk.asked() => SweepTrigger::Doctor,
                () = self.outage.notified() => SweepTrigger::AfterOutage,
                () = tokio::time::sleep_until(daily) => {
                    daily = tokio::time::Instant::now() + DAILY;
                    SweepTrigger::Daily
                }
            };
            if unless_cancelled(&mut cancel, self.sweep(trigger))
                .await
                .is_none()
            {
                return;
            }
            if trigger == SweepTrigger::Doctor {
                desk.answered();
            }
        }
    }

    async fn sweep(&self, trigger: SweepTrigger) {
        let desk = &self.inputs.side.reaper;
        let swept = self.manager.sweep(trigger).await;
        desk.listed(self.manager.last_listing());
        match swept {
            Ok(report) => desk.record(&report),
            Err(SweepError::BudgetSpent { .. }) => {}
            Err(SweepError::GitHub(failure)) => {
                let code = failure.as_str();
                tracing::warn!(code, "reaper_listing_failed");
                desk.failed(code);
            }
            Err(SweepError::Journal(code)) => {
                tracing::warn!(code, "reaper_journal_unread");
                desk.failed(code);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use mayfly_control_domain::scaleset::{DegradedCause, LossCause};

    use super::*;

    fn pool(name: &str) -> PoolName {
        PoolName::new(name).unwrap_or_else(|e| panic!("{e:?}"))
    }

    #[test]
    fn should_scope_the_app_to_the_repository_when_the_config_names_one() {
        // given
        let github = GithubConfig {
            scope: GithubScope::Repository {
                organisation: "acme".to_owned(),
                repository: "widgets".to_owned(),
            },
            app_client_id: "Iv1.synthetic".to_owned(),
            installation_id: 7,
            private_key: mayfly_domain::SecretRef::File {
                path: std::path::PathBuf::from("/synthetic/key.pem"),
            },
            api_url: mayfly_adapter_github::parse_base("https://ghes.example/api/v3")
                .unwrap_or_else(|e| panic!("{e:?}")),
            web_url: mayfly_adapter_github::parse_base("https://ghes.example")
                .unwrap_or_else(|e| panic!("{e:?}")),
        };

        // when
        let app = app_config(&github);

        // then
        assert_eq!(
            app.scope,
            Scope::Repository {
                owner: "acme".to_owned(),
                repo: "widgets".to_owned()
            }
        );
        assert_eq!(
            app.config_url().as_str(),
            "https://ghes.example/acme/widgets"
        );
        assert_eq!(
            app.registration_token_url().as_str(),
            "https://ghes.example/api/v3/repos/acme/widgets/actions/runners/registration-token"
        );
    }

    #[test]
    fn should_keep_each_sweep_and_the_latest_listing_when_the_desk_records_them() {
        // given
        use mayfly_control::registration::UnrecognisedRunner as Unrecognised;
        use mayfly_control_domain::registration::RunnerId;
        let desk = ReaperDesk::default();
        let report = |trigger| SweepReport {
            trigger,
            at: Millis::new(5),
            listed: 2,
            deregistered: vec![RunnerId(1)],
            pending: Vec::new(),
            unrecognised: vec![Unrecognised {
                id: RunnerId(9),
                name: "mf-stray".to_owned(),
                scale_set: Some(ScaleSetId(3)),
            }],
            awaiting_issue_unknown: 0,
            foreign: 1,
        };

        // when
        let mut triggers = Vec::new();
        for trigger in [
            SweepTrigger::RunStarted,
            SweepTrigger::AfterOutage,
            SweepTrigger::Daily,
            SweepTrigger::Doctor,
        ] {
            desk.record(&report(trigger));
            triggers.push(desk.report().last.map(|s| s.trigger));
        }
        desk.failed("unavailable");
        let failed = desk.report();
        desk.listed(Some(Millis::new(10)));
        desk.listed(Some(Millis::new(4)));
        desk.listed(None);

        // then
        assert_eq!(
            triggers,
            ["run_started", "after_outage", "daily", "doctor"].map(|t| Some(t.to_owned()))
        );
        let last = failed.last.unwrap_or_else(|| panic!("a sweep"));
        assert_eq!((last.listed, last.deregistered, last.foreign), (2, 1, 1));
        assert_eq!(last.unrecognised[0].scale_set, Some(3));
        assert_eq!(failed.failure.as_deref(), Some("unavailable"));
        assert_eq!(desk.listed_at(), Some(Millis::new(10)), "never moved back");
    }

    #[test]
    fn should_give_a_handle_more_than_a_mints_call_budget_when_the_manager_is_delivered_to() {
        // given
        let config = registration_delivery();

        // when
        let budget = config.handle_budget();

        // then: the whole call budget, `Retry-After` waits included, fits in one handle
        assert!(
            budget.get() > DEFAULT_CALL_POLICY.deadline.get(),
            "{config:?}"
        );
        assert_eq!(budget, config.handle_deadline, "not clamped by the lease");
    }

    #[test]
    fn should_name_the_scale_set_after_the_pool_when_a_spec_is_made() {
        // given
        let lin = pool("lin");

        // when
        let spec = spec_of(&lin);

        // then
        assert_eq!(spec.name, "lin");
        assert_eq!(spec.runner_group, "Default");
        assert_eq!(spec.labels, ["lin"]);
    }

    #[test]
    fn should_refuse_a_second_lease_when_the_first_is_still_held() {
        // given
        let leases = ListenerLeases::default();
        let lin = pool("lin");

        // when
        let first = leases.acquire(&lin);
        let second = leases.acquire(&lin);
        let other = leases.acquire(&pool("mac"));
        drop(first);
        let after = leases.acquire(&lin);

        // then
        assert!(second.is_none());
        assert!(other.is_some());
        assert!(after.is_some());
    }

    #[test]
    fn should_freeze_minting_only_when_the_session_is_degraded() {
        // given
        let phases = [
            Phase::Unconfigured,
            Phase::Ensured,
            Phase::Opening,
            Phase::Active,
            Phase::Backoff(LossCause::Failed),
            Phase::Closing,
            Phase::Closed,
        ];

        // when
        let healths: Vec<SessionHealth> = phases.into_iter().map(health).collect();
        let degraded = health(Phase::Degraded(DegradedCause::ProtocolDrift));

        // then
        assert!(healths.iter().all(|h| *h == SessionHealth::Healthy));
        assert_eq!(degraded, SessionHealth::Degraded);
    }
}
