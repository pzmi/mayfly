//! `mayflyd --role all`: both planes of one host in one process, joined by the in-process link
//! (plan §3, §10; ADR 0091).
//!
//! # Start-up (plan §10, P1)
//!
//! 0. The places: the configuration's `state_dir` and `tart_home`, else the platform's
//!    ([`crate::paths`], ADR 0091 decision 5); each is created `0700` when missing.
//! 1. **The owner lease**: the state-dir lock ([`StateDirLock`]), which on SQLite is the one
//!    control plane per store of `doc/event-store.md` §4.5; a second daemon exits 69.
//! 2. The host's identity ([`crate::identity`]; never generated beside an existing journal),
//!    then the journal opened and migrated (the adapter takes its backup before a migration).
//! 3. The data plane from observation only: the epoch floor read from `TART_HOME`
//!    ([`FloorFile`], fail closed: unreadable, or missing while the host holds mayfly's VMs or
//!    bases, refuses the start, ADR 0026), the image registry's bases recovered
//!    (`TartImageStore::recover`, ADR 0087), and the executor's ledger rebuilt from the
//!    provider, the image store and the host probe ([`Executor::start`]).
//! 4. **A run** ([`Startup::open`]): stored kinds and structure verified (§6.3), projections
//!    prepared, `StartRun`, the data plane's hello, `MintEpoch` above the journal and the floor,
//!    admission closed, the welcome; the data plane raises its floor (fsync) and enters.
//! 5. The pools of the configuration are applied (`ConfigurePool` when one changed): the
//!    static demand (`min_prepared`, `max_running`) the reconciler reads. Then each pool's
//!    image, from its manifest ([`crate::manifest`], read and checked against the deployment
//!    at [`load`]), is registered, its verification recorded and the pool pinned to it
//!    (`mayfly_control::images`, OPS-02, ADR 0091), which the placement manager then places.
//! 6. An inventory is asked for at once (`ReportInventory`, sent on the link outside the
//!    journal: a read with no effect, ADR 0091 decision 4), and every inventory goes to
//!    [`Startup::recover`] (the same `Startup` for every pass of the run, CP-04) until it
//!    completes, which opens admission, or reports safe mode.
//! 7. Once recovery completes, the saga's deadline timer, the reconciler and the image
//!    placement manager start, and the durable consumers catch up, each with its own worker
//!    (`catch_up` per consumer, ES-05): the attempt saga, the directive relay and the placement
//!    relay; then one complete pass of the terms policy (P4, CP-08) applies each pool's terms
//!    to this host; once all of that succeeded, one ready line is reported ([`Ready`]), and the
//!    terms policy's own loop starts.
//!
//! # The operator socket (RT-03)
//!
//! Right after the state-dir lock, the socket is opened at [`paths::socket`] (`0600`, in a
//! directory private to the daemon's user: [`Listener`]), and it is served for the daemon's
//! whole life, across runs, beside them ([`crate::operator::server`]); it stops accepting when
//! the root is cancelled (or the last run ended), lets commands in flight finish for at most
//! [`SHUTDOWN_DEADLINE`], in parallel with the runs' own stop, and removes its file. `mayfly status` reads the run from the `HostRun`
//! stream, its `ready` field from this daemon's ready signal (set just before each ready line,
//! cleared when a run starts) and the sizing measured at start-up, which `mayflyd` wrote to
//! stderr before RT-03 (RT-06's note).
//!
//! While the run lasts: every observation goes through the inbox (P8); every later inventory
//! also goes to [`Startup::resume_held`] while an attempt's cleanup is held (CP-04); each
//! consumer drains its kicks, and all catch up on a coarse timer ([`COARSE`]); a stored kind
//! this build cannot decode, met by a save, enters safe mode (§6.3 rule 4).
//!
//! # A session that ends is a run that ends
//!
//! The control plane mints a new epoch for every data-plane session (plan §3.2), and `HostRun`
//! mints once per run, so a link that ends (the data plane's serve returned) ends the run: the
//! next run's `StartRun` records the previous one unclean and mints above it, and the data
//! plane enters the new session with its ledger kept. `formal/link.qnt`'s reconnect and
//! `formal/recovery.qnt`'s restart already model exactly this, so no model changes. A run that
//! ends before its recovery completed stops the daemon (exit 1) rather than loop.
//! [`SessionCut`] ends a session on request: the seam a test loses the link through (production
//! never cuts).
//!
//! # Shutdown (P24)
//!
//! SIGTERM, or terminal mode's third Ctrl-C, cancels the root ([`Stops::force`]). Each
//! supervised loop ends between two units of work, so no save is cut; nothing more is admitted
//! or acquired (the reconciler has stopped); `BeginShutdown` and `EndRun` record a clean end;
//! the control plane's link halves are dropped, and the data plane's serve returns once every
//! effect in flight finished. VMs are left running under their shims (ADR 0037) for the next
//! start to adopt. The loops are bounded by [`SHUTDOWN_DEADLINE`] and the runtime by
//! [`RUNTIME_SHUTDOWN`], under a service manager's own.
//!
//! # Terminal mode's graceful stop (RT-05a, ADR 0031's amendment of 2026-11-12)
//!
//! The first Ctrl-C ([`Stops::graceful`]) stops working here. Once the run's recovery
//! completed in normal mode, the reconciler plans this host as draining for the rest of the run
//! ([`Reconciler::set_stopping`]: nothing reserved, started or granted; prepared attempts
//! retired `drain`; unstarted reservations released; granted and listening ones wait, their
//! deregistration being RT-05b's), and the attempt saga
//! stops and deletes each retired VM with its ownership proof (ADR 0026) as after any
//! retirement: never a base, a foreign VM or an unrecorded one. The stop reads the host's
//! leases after every commit; once every lease left is quarantined (an operator's case, never
//! deleted automatically, ADR 0025), or none is left, the run ends as P24 records it and the
//! daemon exits 0, releasing the state-dir lock. A busy job keeps its lease, so the stop waits
//! for it. No drain is recorded on the host's stream, so a forced stop or a crash leaves
//! nothing the next start must undo; the capacity inputs the stopping passes record are
//! re-planned by the next run's passes.
//!
//! The wait is bounded by `[shutdown] busy_wait` (default 30 minutes) from the first Ctrl-C;
//! when it runs out, or on the third Ctrl-C or SIGTERM, the stop is P24 above: what is not done
//! stays recorded (retirements, pending deletes) and the next start's recovery continues it
//! (`formal/recovery.qnt`'s `forcedStopMidDeleteIsFinishedByTheNextStartTest`). In safe mode,
//! or once a stored kind this build cannot decode was met, nothing destructive may run (I29),
//! so the first Ctrl-C is P24 at once. Pinned by
//! `tests/daemon.rs::placement::should_delete_the_idle_vms_and_exit_when_ctrl_c_is_pressed_once`,
//! `should_force_p24_when_ctrl_c_is_pressed_a_third_time_during_the_wait` and
//! `should_take_p24_when_the_busy_wait_runs_out`.
//!
//! # Seams left for later tasks
//!
//! GitHub (RT-07, `crate::github`): one client over `[github]`, its App key asked of a provider
//! built here ([`UriSecretProvider::from_process`], which withholds every `env:` name from
//! children); each run drives one listener per pool, the job facts and the registration manager
//! as durable consumers, the manager's pass once a minute and the reaper's sweeps (at run start
//! once the scale sets are journaled, daily, after a long outage, on `doctor`). Recovery is
//! still handed no GitHub view, so every runner reads `unreachable` (never `absent`) and a held
//! cleanup stays held (CP-04H; RT-08 draws it from the registrations and a listing); the data
//! plane's [`LaunchStore`] is shared with the guest transport and filled by the link's
//! data-plane end from the daemon's one [`JitBroker`] (LNK-02), which the registration manager
//! mints into. Image placement (CP-07): the
//! placement manager places every pool's pin (step 5) on this host and [`Placements`] follows
//! its passes; the terms policy (P4, CP-08) applies each pool's terms to this host, open once
//! the pool is active, pinned and its pin's breaker not tripped. The reconciler's pass is P2's
//! driver (CP-10): it starts each reservation once its pin is materialised on this host, and
//! the attempt saga then prepares its VM over the provider; a VM that becomes Prepared kicks a
//! pass at once (`Notifier::prepared`), so a grant does not wait for the interval (nothing
//! grants under `mayflyd` until a trusted job is recorded: on github.com's direct flow only
//! once GH-08 and GH-10 classify assignments; an acquired GHES offer is one). Logging (RT-04),
//! terminal mode over real jobs (deregistration and the Busy wait, RT-05b), the watchdog's
//! `crash_loop` (OPS-05) and split roles (LNK-01) are separate tasks.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mayfly_adapter_host::{HostConfig as ProbeConfig, OsProbe, ProviderView, ReadFailed, VmShape};
use mayfly_adapter_link::inproc;
use mayfly_adapter_secrets::UriSecretProvider;
use mayfly_adapter_sqlite::{OpenError, SqliteJournal, SqliteLedger, SqliteOptions};
use mayfly_adapter_tart::bases::SignaturePolicy;
use mayfly_adapter_tart::{
    BaseRecords, Deadlines, FloorFile, FloorUnreadable, GuestConfig, GuestDeadlines, ImageConfig,
    ImageDeadlines, RegistryManifests, RunnerPlatform, ShimProgram, TartConfig, TartGuestTransport,
    TartImageStore, TartProvider,
};
use mayfly_control::delivery::{
    DeliveryConfig, DeliveryWorker, DirectiveRelay, Kicks, LinkSink, PlacementRelay,
    PlacementRelayConfig, SinkConfig,
};
use mayfly_control::images::{self, CatalogError, PinError};
use mayfly_control::inbox::Inbox;
use mayfly_control::placement::{PlacementConfig, PlacementIndex, PlacementManager};
use mayfly_control::projection::{self, BuildId};
use mayfly_control::reconciler::{Reconciler, ReconcilerConfig};
use mayfly_control::recovery::{
    Recovery, Startup, StartupConfig, StartupError, enter_safe_mode, session_ended,
};
use mayfly_control::registration::JitBroker;
use mayfly_control::saga::{AttemptSaga, SagaConfig};
use mayfly_control::sessions::HostSessions;
use mayfly_control::store::{AggregateStore, Clock as _, Outcome, SaveError, SaveRequest};
use mayfly_control::terms::{TermsConfig, TermsPolicy};
use mayfly_control_domain::attempt::GuestOs as DomainOs;
use mayfly_control_domain::host::{Host, LeasePhase};
use mayfly_control_domain::image::Verification;
use mayfly_control_domain::operations::hostrun::{HostRun, HostRunCommand, SafeModeReason};
use mayfly_control_domain::pool::{ConfigurePool, Pool, PoolCommand, PoolConfig, TrustTier};
use mayfly_control_domain::{Actor, Command, ControlAggregates};
use mayfly_data::{Config as DataConfig, Executor, Inventory, LaunchSecrets, LaunchStore, Ports};
use mayfly_data_domain::Sizing;
use mayfly_domain::{
    CommandId, Deadline, DirectiveId, Epoch, HostId, Millis, OwnedVmName, PoolName,
    ProviderInstanceId, Resources, Secret,
};
use mayfly_protocol::{
    Directive, DirectiveKind, DirectiveSink as _, LaunchConfigSink, LaunchTarget, Mode,
    ObservationKind, PutRefused, SUPPORTED, SecretHandle,
};
use sha2::{Digest as _, Sha256};
use tokio::sync::watch;

use crate::clocks::{AnchoredStoreClock, DataClock, OsRng, RandomObservationIds, SystemClock};
use crate::config::{Config, ConfigError, ConfigErrorKind, ConfigErrors, GuestOs, LoadError, Tier};
use crate::details::{ConfigDetails, Placements};
use crate::exit_code;
use crate::github::{GithubRun, GithubSide, RunInputs};
use crate::identity::{IdentityError, host_id, random_bytes};
use crate::manifest::{self, Manifest};
use crate::operator;
use crate::operator::server::{Listener, Operators, Service};
use crate::paths::{self, Places, PlatformEnv};
use crate::state_dir::{StateDirError, StateDirLock};
use crate::supervise::{Cuttable, Notifier, SessionCut, Shutdown, ShutdownSignal, Stops};

/// How often every consumer catches up and the reconciler passes without a kick: the
/// level-triggered floor under the kicks (a lost kick costs at most this).
pub const COARSE: Duration = Duration::from_secs(60);

/// The longest a P24 stop takes before the daemon stops waiting for its loops; the runtime is
/// then given [`RUNTIME_SHUTDOWN`] more, so the process exits within 30 s (OPS-01's launchd
/// `ExitTimeOut` must be at least that).
pub const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(25);

/// How long the runtime's remaining tasks (blocking file work) get once the daemon stopped.
pub const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(5);

/// The least a timer loop waits between two turns, so a deadline already past is acted on
/// once a turn, never in a busy loop.
const TIMER_FLOOR: Duration = Duration::from_millis(100);

/// The least time between two passes of the terms policy (P4): a kick on a busy host stream
/// waits this long rather than start a pass per commit.
const TERMS_FLOOR: Duration = Duration::from_secs(1);

/// How long the data plane waits between two inventories while nothing asks.
const OBSERVE_EVERY: Millis = Millis::new(30_000);

/// The deadline of the inventory asked for at the start of a run.
const INVENTORY_DEADLINE: Millis = Millis::new(60_000);

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// The run's actor for the commands of a clean stop.
const SHUTDOWN: &str = "shutdown";

const ID_DOMAIN: &[u8] = b"mayfly-daemon-v1\n";

/// What `mayflyd` is started with besides its configuration.
#[derive(Debug, Clone)]
pub struct Options {
    /// The configuration file.
    pub config: PathBuf,
    /// The `tart` executable, absolute (the binary resolves it from `PATH`).
    pub tart: PathBuf,
    /// The `mayfly` executable that runs `mayfly vm-shim` (ADR 0037), absolute.
    pub shim_program: PathBuf,
    /// The environment children may get: [`child_environment`] of the daemon's.
    pub environment: Vec<(OsString, OsString)>,
    /// This build's version.
    pub version: String,
    /// The variables the default places are read from ([`PlatformEnv::from_process`] in the
    /// binary; a test passes its own).
    pub env: PlatformEnv,
}

/// The ready report of a run: recovery completed (or safe mode reported), the host side's
/// consumers (the attempt saga, the directive relay and the placement relay) caught up once,
/// and the terms policy (P4) passed once; the GitHub side's consumers are not waited for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    /// The host.
    pub host: HostId,
    /// The run's epoch.
    pub epoch: Epoch,
    /// The session's mode: `safe` when the journal is not trusted.
    pub mode: Mode,
}

impl std::fmt::Display for Ready {
    /// The ready line: `mayflyd ready host=<id> epoch=<n> mode=<normal|safe>`. Its promise:
    /// start-up recovery of this run has finished (admission opened, or safe mode reported) and
    /// every durable consumer of the host's side (the attempt saga, the directive relay and the
    /// placement relay) has caught up once (its first `catch_up` returned; the GitHub side's
    /// consumers, RT-07, are not waited for: GitHub never holds a run back), and then one
    /// pass of the terms policy completed (CP-08, `TermsReport::complete`): every pool was read
    /// and every pool's terms due on this host were applied at the pool's revision as that pass
    /// last read it, or refused by `Host` (an older revision than it holds, for one); in safe
    /// mode that pass applies nothing and is complete whatever it could not read. A store fault
    /// that persists keeps the line back, as it does a consumer's first catch-up.
    /// Pinned by
    /// `tests/daemon.rs::should_converge_on_the_same_inventory_when_the_daemon_is_killed_and_restarted`
    /// and `tests/daemon.rs::should_apply_the_pools_terms_to_its_host_when_the_pool_is_pinned`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mode = match self.mode {
            Mode::Normal => "normal",
            Mode::Safe => "safe",
        };
        write!(
            f,
            "mayflyd ready host={} epoch={} mode={mode}",
            self.host,
            self.epoch.get()
        )
    }
}

/// Why the daemon stopped other than by a signal.
#[derive(Debug)]
pub enum DaemonError {
    /// The configuration did not load or lacks what the daemon needs.
    Config(LoadError),
    /// The state directory could not be locked or used.
    StateDir(StateDirError),
    /// The state directory or `TART_HOME`, or an ancestor of either, lets someone else swap it
    /// (ADR 0087's layout rule); nothing was written.
    Places(mayfly_adapter_tart::verify::UnsafeLayout),
    /// The host's identity could not be had.
    Identity(IdentityError),
    /// The journal did not open.
    Journal(OpenError),
    /// The epoch floor exists and does not read.
    Floor(FloorUnreadable),
    /// The epoch floor is missing beside mayfly's own VMs or bases.
    FloorMissing {
        /// The floor file.
        path: PathBuf,
        /// What of mayfly's the host holds.
        evidence: String,
    },
    /// The image store refused its configuration or its directories.
    Images(mayfly_adapter_tart::ConfigError),
    /// Something the daemon needs at start could not be had, by name.
    Start(&'static str),
    /// A run's start-up failed.
    Startup(StartupError),
    /// The data plane did not enter the session the control plane opened.
    Session(String),
    /// The operator socket could not be opened.
    Socket(crate::operator::server::BindError),
    /// The link ended before the run's recovery completed.
    SessionLost,
}

impl DaemonError {
    /// The exit code (ADR 0031, ADR 0046): the configuration 78, the lock 69, the state
    /// directory, `TART_HOME` or the operator socket unusable 73, anything else
    /// [`exit_code::FAILED`].
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::Config(e) => e.exit_code(),
            Self::StateDir(e) => e.exit_code(),
            Self::Places(_) | Self::Socket(_) => exit_code::STATE_DIR_UNUSABLE,
            Self::Identity(_)
            | Self::Journal(_)
            | Self::Floor(_)
            | Self::FloorMissing { .. }
            | Self::Images(_)
            | Self::Start(_)
            | Self::Startup(_)
            | Self::Session(_)
            | Self::SessionLost => exit_code::FAILED,
        }
    }
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(e) => e.fmt(f),
            Self::StateDir(e) => e.fmt(f),
            Self::Places(e) => write!(
                f,
                "refusing an unsafe directory layout: {e:?}; every ancestor must be owned by \
                 root or this user and not writable by others unless sticky"
            ),
            Self::Identity(e) => e.fmt(f),
            Self::Journal(e) => e.fmt(f),
            Self::Floor(e) => e.fmt(f),
            Self::FloorMissing { path, evidence } => write!(
                f,
                "the epoch floor {} is missing but this host holds mayfly's {evidence}: a lost \
                 floor beside a journal restored from a backup could mint an epoch a VM already \
                 saw (ADR 0026); restore the floor file before starting",
                path.display()
            ),
            Self::Images(e) => write!(f, "the image store refused to start: {e:?}"),
            Self::Start(what) => write!(f, "cannot start: {what}"),
            Self::Startup(e) => write!(f, "start-up failed: {e}"),
            Self::Session(why) => write!(f, "the data plane did not enter the session: {why}"),
            Self::Socket(e) => e.fmt(f),
            Self::SessionLost => f.write_str("the link ended before recovery completed"),
        }
    }
}

impl std::error::Error for DaemonError {}

/// The daemon's configuration as `mayflyd` needs it.
#[derive(Debug, Clone)]
pub struct Loaded {
    /// The whole file.
    pub config: Config,
    /// `[images]`, its runner pins joined by every pool manifest's ([`manifest::resolve`]).
    pub images: crate::config::ImagesConfig,
    /// Each pool's image manifest, checked against the deployment.
    pub manifests: BTreeMap<PoolName, Manifest>,
    /// The state directory and `TART_HOME`: the configuration's, else the platform's standard
    /// places (ADR 0091 decision 5, [`crate::paths`]).
    pub places: Places,
}

fn missing(field: &str, problem: &str, fix: &str) -> ConfigError {
    ConfigError {
        field: field.to_owned(),
        kind: ConfigErrorKind::Missing,
        problem: problem.to_owned(),
        fix: fix.to_owned(),
    }
}

/// The daemon's configuration: the file valid, `[images]` present and every pool's image
/// manifest valid and consistent with the deployment ([`manifest::resolve`], ADR 0091), each
/// problem named (exit 78). Without `[images]` the manifests are not read, so that one problem
/// is reported alone: their checks need the repository it names. `[host]`'s sizing keys are optional: absent, each is measured, read
/// from the image's OCI manifest or Apple's limit ([`crate::sizing`], ADR 0052's amendment of
/// 2026-11-12). Pinned by
/// `tests/daemon.rs::should_refuse_to_start_when_the_role_or_the_images_are_missing` and
/// `should_refuse_to_start_when_a_pool_manifest_names_a_tag`.
///
/// # Errors
/// [`LoadError`] when the file does not load or lacks what the daemon needs.
pub fn load(path: &Path, env: &PlatformEnv) -> Result<Loaded, LoadError> {
    let config = Config::load(path)?;
    let mut errors = Vec::new();
    let places = paths::resolve(
        config.state_dir.as_deref(),
        config.provider.tart_home.as_deref(),
        paths::defaults(env),
    );
    if places.is_none() {
        errors.push(missing(
            "state_dir",
            "no default place: HOME is not set to an absolute path",
            "add `state_dir = \"/var/lib/mayfly\"`, or run mayflyd with HOME set",
        ));
    }
    if config.images.is_none() {
        errors.push(missing(
            "images",
            "required to run mayflyd: the data plane pulls images from it",
            "add `[images]` with `repository = \"ghcr.io/acme/runner-macos\"`",
        ));
    }
    let resolved = config.images.as_ref().and_then(|images| {
        manifest::resolve(&config.pools, images, Manifest::load)
            .map_err(|found| errors.extend(found))
            .ok()
    });
    match (places, config.images.clone(), resolved) {
        (Some(places), Some(images), Some(resolved)) if errors.is_empty() => Ok(Loaded {
            config,
            places,
            images: crate::config::ImagesConfig {
                runners: resolved.runners,
                ..images
            },
            manifests: resolved.manifests,
        }),
        _ => Err(LoadError::Invalid(ConfigErrors(errors))),
    }
}

/// The daemon's environment as children may receive it: only the allowlisted names
/// (`mayfly_adapter_tart::config::ALLOWED_VARIABLES`, ADR 0058), so no secret value of the
/// daemon's environment is held in [`Options`].
#[must_use]
pub fn child_environment(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    vars.into_iter()
        .filter(|(name, _)| mayfly_adapter_tart::config::allowed(name))
        .collect()
}

type Journal = SqliteJournal;
type Store = AggregateStore<Journal, SystemClock, OsRng, Notifier>;
type Ledger = SqliteLedger<AnchoredStoreClock>;
type Worker = DeliveryWorker<Journal, Ledger, OsRng, SystemClock>;
type Exec = Executor<
    TartProvider,
    TartImageStore<RegistryManifests>,
    TartGuestTransport<SharedLaunches>,
    OsProbe<ProviderOf>,
    DataClock,
    RandomObservationIds,
>;

/// The data plane's JIT store, shared between the guest transport and the link's data-plane
/// end (ADR 0082, LNK-02), which fills it with the configs the control plane's broker hands the
/// link (CP-05).
#[derive(Debug, Clone, Default)]
pub struct SharedLaunches(Arc<LaunchStore>);

impl LaunchConfigSink for SharedLaunches {
    fn put(
        &self,
        handle: SecretHandle,
        target: LaunchTarget,
        config: Secret<String>,
    ) -> Result<(), PutRefused> {
        LaunchConfigSink::put(&*self.0, handle, target, config)
    }

    fn discard_held(&self) {
        self.0.discard_held();
    }
}

impl LaunchSecrets for SharedLaunches {
    fn take(
        &self,
        handle: SecretHandle,
        vm: &OwnedVmName,
        instance_id: &ProviderInstanceId,
    ) -> Option<Secret<String>> {
        self.0.take(handle, vm, instance_id)
    }

    fn discard(&self, handle: SecretHandle) {
        self.0.discard(handle);
    }
}

/// What the host probe asks the provider (DP-06's `ProviderView`), over a Tart adapter of its
/// own.
#[derive(Debug)]
pub struct ProviderOf(TartProvider);

impl ProviderView for ProviderOf {
    async fn inventory(&self) -> Inventory {
        mayfly_data::VmProvider::inventory(&self.0).await
    }

    async fn version(&self) -> Result<String, ReadFailed> {
        self.0.probe().await.map_err(|_| ReadFailed)
    }
}

fn private_dir(path: &Path) -> Result<(), DaemonError> {
    paths::prepare_private(path).map_err(DaemonError::Places)
}

fn derive(parts: &[&[u8]]) -> u128 {
    let mut hasher = Sha256::new();
    hasher.update(ID_DOMAIN);
    for part in parts {
        hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(part);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    let mut head = [0_u8; 16];
    head.copy_from_slice(&digest[..16]);
    u128::from_be_bytes(head)
}

/// The data plane's sizing: disks from `[host]` when configured, else measured per image from
/// its manifest (`None`, ADR 0052's amendment), a base sized as its image, a VM's memory floor
/// the smallest pool's.
fn sizing(config: &Config) -> Sizing {
    let image = config
        .host
        .image_disk_gib
        .map(|gib| gib.saturating_mul(GIB));
    Sizing {
        image_disk_bytes: image,
        base_disk_bytes: image,
        clone_disk_bytes: config
            .host
            .clone_disk_gib
            .map(|gib| gib.saturating_mul(GIB)),
        vm_memory_bytes: config
            .pools
            .iter()
            .map(|p| p.memory_mib)
            .min()
            .unwrap_or(0)
            .saturating_mul(MIB),
    }
}

/// What one measured slot must hold: the largest memory and the most CPUs of any pool, so a
/// measured count never offers a slot the largest VM does not fit.
fn vm_shape(config: &Config) -> VmShape {
    VmShape {
        memory_bytes: config
            .pools
            .iter()
            .map(|p| p.memory_mib)
            .max()
            .unwrap_or(0)
            .saturating_mul(MIB),
        cpus: config.pools.iter().map(|p| p.cpu).max().unwrap_or(0),
    }
}

/// What of mayfly's this host holds while its epoch floor is missing, if anything: a recorded
/// base, an `mf-*` or `mfb-*` VM, or a registry or listing that cannot be read (which cannot
/// prove there is none). A missing floor is "no session yet" only on a host with none of it
/// (ADR 0026). Pinned by
/// `tests/daemon.rs::should_refuse_to_start_when_the_floor_is_missing_beside_our_vms`.
async fn ours_without_floor(bases: &BaseRecords, provider: &TartProvider) -> Option<String> {
    match bases.read() {
        Err(_) => return Some("the base registry does not read".to_owned()),
        Ok(record) if !record.bases.is_empty() => {
            return Some(format!("{} recorded base(s)", record.bases.len()));
        }
        Ok(_) => {}
    }
    match mayfly_data::VmProvider::inventory(provider).await {
        Inventory::Incomplete { .. } => Some("the VM listing is incomplete".to_owned()),
        Inventory::Complete { vms } => {
            let ours: Vec<&str> = vms
                .iter()
                .map(|vm| vm.name.as_str())
                .filter(|name| name.starts_with("mf-") || name.starts_with("mfb-"))
                .collect();
            (!ours.is_empty()).then(|| format!("VM(s) {}", ours.join(", ")))
        }
    }
}

fn tart_config(places: &Places, options: &Options) -> TartConfig {
    TartConfig {
        tart: options.tart.clone(),
        tart_home: places.tart_home.clone(),
        shims: places.state_dir.join("shims"),
        shim: ShimProgram {
            program: options.shim_program.clone(),
            args: vec!["vm-shim".into()],
        },
        environment: options.environment.clone(),
        deadlines: Deadlines::host(),
        output_limit: 1 << 20,
        run_log_limit: 16 << 20,
        poll: Duration::from_millis(200),
        redact: Vec::new(),
    }
}

/// The data plane, built from observation only.
async fn data_plane(
    loaded: &Loaded,
    options: &Options,
    host: &HostId,
) -> Result<(Exec, FloorFile, String), DaemonError> {
    let (config, images) = (&loaded.config, &loaded.images);
    let state_dir = &loaded.places.state_dir;
    let tart = tart_config(&loaded.places, options);
    private_dir(&tart.tart_home)?;
    private_dir(&tart.shims)?;
    private_dir(&state_dir.join("diagnostics"))?;
    let floor = FloorFile::in_home(&tart.tart_home);
    let bases = Arc::new(BaseRecords::open(
        state_dir.join("images").join("bases.json"),
    ));
    let view = ProviderOf(TartProvider::new(tart.clone(), Arc::clone(&bases)));
    let floor_epoch = match floor.stored().map_err(DaemonError::Floor)? {
        Some(epoch) => epoch,
        None => {
            if let Some(evidence) = ours_without_floor(&bases, &view.0).await {
                return Err(DaemonError::FloorMissing {
                    path: floor.path().to_path_buf(),
                    evidence,
                });
            }
            Epoch::new(0)
        }
    };
    let manifests = RegistryManifests::with_realm_hosts(images.realm_hosts.clone())
        .map_err(|_| DaemonError::Start("the registry client's TLS setup"))?;
    let store = TartImageStore::new(
        tart.clone(),
        ImageConfig {
            repository: images.repository.clone(),
            runners: RunnerPlatform::pin_table(&images.runners),
            deployment_id: host.to_string(),
            signature: SignaturePolicy::DigestOnly,
            deadlines: ImageDeadlines {
                pull: Duration::from_secs(3600),
                install: Duration::from_secs(900),
                halt: Duration::from_secs(120),
            },
        },
        Arc::clone(&bases),
        manifests,
    )
    .map_err(DaemonError::Images)?;
    if store.recover().await.is_err() {
        tracing::warn!("image_bases_not_recovered");
    }
    let probe = OsProbe::new(
        ProbeConfig {
            provider_home: tart.tart_home.clone(),
            vm_slots: config.host.vm_slots,
            vm_shape: vm_shape(config),
            macos_guests: config.host.macos_guests(),
            reserve_memory_bytes: config.host.reserve_memory_bytes(),
            reserve_disk_bytes: config.host.reserve_disk_bytes(),
            reserve_cpus: config.host.reserve_cpus(),
            disk_low_water_bytes: config.host.reserve_disk_bytes(),
            memory_low_water_bytes: None,
            wake_gap: Duration::from_secs(60),
        },
        view,
    );
    let boot_id = probe
        .boot_id()
        .map_err(|_| DaemonError::Start("the host's boot id"))?;
    let guest = TartGuestTransport::new(
        tart.clone(),
        GuestConfig {
            deadlines: GuestDeadlines {
                verify: Duration::from_secs(60),
                launch: Duration::from_secs(120),
                status: Duration::from_secs(30),
                diagnostics: Duration::from_secs(120),
            },
            claims_limit: 64 << 10,
            diagnostics_limit: 8 << 20,
            diagnostics_dir: state_dir.join("diagnostics"),
        },
        SharedLaunches::default(),
    );
    let ids = RandomObservationIds::seeded()
        .map_err(|_| DaemonError::Start("the operating system's random generator"))?;
    let exec = Executor::start(
        DataConfig {
            host_id: host.clone(),
            sizing: sizing(config),
            concurrency: 4,
            observe_every: OBSERVE_EVERY,
            listing_timeout: Millis::new(30_000),
            boot_id: boot_id.clone(),
            dp_version: options.version.clone(),
        },
        Ports {
            provider: TartProvider::new(tart, bases),
            images: store,
            guest,
            host: probe,
            clock: DataClock::started(),
            ids,
        },
        floor_epoch,
    )
    .await
    .map_err(|_| DaemonError::Start("a probe of the host"))?;
    Ok((exec, floor, boot_id))
}

/// How often the data plane's JIT store drops the configs whose launch's deadline passed.
const LAUNCH_EXPIRY_EVERY: Millis = Millis::new(30_000);

/// The first wait after a failed start-up `catch_up`; it doubles up to [`CATCH_UP_BACKOFF_MAX`].
const CATCH_UP_BACKOFF_FIRST: Duration = Duration::from_millis(500);

/// The longest wait between two start-up `catch_up` attempts.
const CATCH_UP_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Runs `catch_up` (every consumer's, `true` when all returned `Ok`) until it succeeds, waiting
/// through `sleep` with a doubling backoff from [`CATCH_UP_BACKOFF_FIRST`] to
/// [`CATCH_UP_BACKOFF_MAX`] between attempts; `false` when `cancel` ends it first. The ready line
/// follows only a `true`, so it never claims consumers that have not caught up. Pinned by
/// `tests::should_report_caught_up_only_after_a_failed_catch_up_succeeds_when_retried`.
async fn until_caught_up<C, CF, S, SF>(
    mut catch_up: C,
    mut sleep: S,
    cancel: &mut ShutdownSignal,
) -> bool
where
    C: FnMut() -> CF,
    CF: Future<Output = bool>,
    S: FnMut(Duration) -> SF,
    SF: Future<Output = ()>,
{
    let mut wait = CATCH_UP_BACKOFF_FIRST;
    loop {
        if cancel.is_cancelled() {
            return false;
        }
        if catch_up().await {
            return true;
        }
        tokio::select! {
            () = cancel.cancelled() => return false,
            () = sleep(wait) => {}
        }
        wait = (wait * 2).min(CATCH_UP_BACKOFF_MAX);
    }
}

/// One pass of the terms policy's loop (P4, CP-08) when one is due, then how long the loop may
/// wait for a kick before the next falls due: between a second and [`COARSE`]. An incomplete or
/// failed pass is reported; the next pass reads again. Pinned by
/// `tests::should_pass_and_wait_until_due_when_the_terms_loop_runs` and
/// `tests::should_wait_for_the_next_pass_when_a_terms_pass_is_incomplete`.
async fn terms_pass<J, C, R, N, P>(terms: &TermsPolicy<'_, J, C, R, N, P>) -> Duration
where
    J: mayfly_control::journal::Journal,
    C: mayfly_control::store::Clock,
    R: mayfly_control::store::Rng,
    N: mayfly_control::store::StoreNotifier,
    P: mayfly_control::projection::ProjectionStore,
{
    match terms.tick().await {
        Ok(Some(report)) if !report.complete() => {
            tracing::warn!(
                failed = report.failed.len(),
                unread = report.unread.len(),
                "terms_pass_incomplete"
            );
        }
        Ok(_) => {}
        Err(_) => tracing::warn!("terms_pass_failed"),
    }
    Duration::from_millis(terms.until_due().get()).clamp(Duration::from_secs(1), COARSE)
}

/// What the reconciler's loop drives: the [`Reconciler`] ([`RunPasses`]), or a test's
/// counter.
trait Passes {
    /// A pass when one is due (kicks or the interval); `false` when it could not run.
    async fn tick(&self) -> bool;
    /// A pass now; `false` when it could not run.
    async fn pass(&self) -> bool;
    /// Plans `host` as draining from now on (terminal mode's graceful stop, RT-05a).
    fn stop_on(&self, host: &HostId);
    /// How long the loop may wait for a kick before the next pass falls due.
    fn until_due(&self) -> Duration;
}

/// The run's reconciler and the clock its interval is read by.
struct RunPasses<'r, 's, C> {
    reconciler: &'r Reconciler<'s, Journal, SystemClock, OsRng, Notifier, Journal>,
    clock: &'r C,
}

impl<C: mayfly_control::store::Clock> Passes for RunPasses<'_, '_, C> {
    async fn tick(&self) -> bool {
        self.reconciler.tick().await.is_ok()
    }

    async fn pass(&self) -> bool {
        self.reconciler.run_pass().await.is_ok()
    }

    fn stop_on(&self, host: &HostId) {
        self.reconciler.set_stopping(Some(host.clone()));
    }

    fn until_due(&self) -> Duration {
        Duration::from_millis(
            self.reconciler
                .next_due()
                .get()
                .saturating_sub(self.clock.now().get()),
        )
    }
}

/// The kicks that wake the reconciler's loop.
struct ReconcileWakes<'k> {
    /// A host, pool, scale-set or placement stream moved (taken by the reconciler's `tick`).
    kicked: &'k Kicks,
    /// An attempt became Prepared (taken by `tick` too).
    prepared: &'k Kicks,
    /// Any commit: the only wake while the run stops, taken here.
    stopping: &'k Kicks,
}

/// The loop's run: its host, its mode, terminal mode's stop and the run's cancel, and where it
/// says its first stopping pass ran.
struct LoopControl<'c> {
    host: &'c HostId,
    normal: bool,
    stop: ShutdownSignal,
    cancel: ShutdownSignal,
    planned: &'c watch::Sender<bool>,
}

/// The reconciler's loop (P5): a pass when one is due, on its kicks and its interval. Once
/// terminal mode's graceful stop is asked in normal mode, the host is planned as draining and a
/// pass runs at once, then one on every commit (a deleted VM's lease is released at once; a pass
/// that changes nothing commits nothing) and on the interval. While stopping, `tick` no longer
/// runs, so the kick sets only it takes (`kicked`, `prepared`) wake nothing: waiting on them
/// would find them ready forever and pass back to back. Pinned by
/// `tests::should_pass_a_bounded_number_of_times_when_the_run_stops_with_kicks_pending`.
async fn reconcile_loop<P: Passes>(
    passes: &P,
    wakes: ReconcileWakes<'_>,
    mut run: LoopControl<'_>,
) {
    let mut stopping = false;
    loop {
        if run.normal && !stopping && run.stop.is_cancelled() {
            stopping = true;
            passes.stop_on(run.host);
            let _ = wakes.stopping.take();
            if !passes.pass().await {
                tracing::warn!("reconciler_pass_failed");
            }
            run.planned.send_replace(true);
        } else if stopping {
            let _ = wakes.stopping.take();
            if !passes.pass().await {
                tracing::warn!("reconciler_pass_failed");
            }
        } else if !passes.tick().await {
            tracing::warn!("reconciler_pass_failed");
        }
        let wait = passes.until_due().clamp(Duration::from_secs(1), COARSE);
        tokio::select! {
            () = run.cancel.cancelled() => return,
            () = run.stop.cancelled(), if run.normal && !stopping => {}
            () = wakes.stopping.wait(), if stopping => {}
            () = wakes.kicked.wait(), if !stopping => {}
            () = wakes.prepared.wait(), if !stopping => {}
            () = tokio::time::sleep(wait) => {}
        }
    }
}

/// Everything a run borrows.
struct Daemon<'a> {
    config: &'a Config,
    /// Each pool's image manifest, checked at [`load`].
    manifests: &'a BTreeMap<PoolName, Manifest>,
    /// The operator reserve in effect at start: configured, or measured by the probe.
    reserves: Resources,
    host: HostId,
    boot_id: String,
    version: String,
    journal: Journal,
    store: Store,
    notifier: Notifier,
    details: ConfigDetails,
    /// The bases clones are made from, shared with `details`, replaced after every placement
    /// pass (CP-07).
    placements: Placements,
    build: BuildId,
    delivery: DeliveryConfig,
    store_clock: AnchoredStoreClock,
    /// The sizing in effect at start-up, which `mayfly status` returns (RT-06, RT-03).
    sizing: crate::sizing::SizingReport,
    /// `true` from just before a run's ready line until the next run starts: `status`'s
    /// `run.ready`.
    readied: watch::Sender<bool>,
    /// The GitHub client, the scale-set directory, the reaper's desk and the listeners' leases,
    /// kept across runs (RT-07).
    github: GithubSide,
    /// The minted JIT configs held for their launch, which each run's control-plane link end
    /// takes from (LNK-02) and the registration manager mints into (RT-07).
    broker: Arc<JitBroker>,
}

/// The opened session one run's control plane serves.
struct Session<'r, 's, D, O> {
    startup: &'r Startup<'s, Journal, SystemClock, OsRng, Notifier>,
    sessions: &'r HostSessions,
    epoch: Epoch,
    mode: Mode,
    directives: D,
    observations: O,
}

/// How a run ended.
enum RunEnd {
    /// The root was cancelled: the daemon stops.
    Shutdown,
    /// The link ended after recovery completed: a new run follows.
    SessionEnded,
}

/// Runs the daemon until `stops.force` is cancelled, or `stops.graceful` ended the run clean
/// (both by [`crate::supervise::on_signals`] in the binary), reporting each run's [`Ready`] to
/// `ready`.
///
/// # Errors
/// [`DaemonError`]; a stopped run is `Ok`.
pub async fn run(
    options: Options,
    stops: Stops,
    ready: &(dyn Fn(&Ready) + Sync),
) -> Result<(), DaemonError> {
    run_with(options, stops, ready, &SessionCut::default()).await
}

/// [`run`], each data-plane session ended early by `cut` when a test asks (production never
/// cuts).
///
/// # Errors
/// As [`run`].
pub async fn run_with(
    options: Options,
    stops: Stops,
    ready: &(dyn Fn(&Ready) + Sync),
    cut: &SessionCut,
) -> Result<(), DaemonError> {
    let shutdown = &stops.force;
    let loaded = load(&options.config, &options.env).map_err(DaemonError::Config)?;
    let config = &loaded.config;
    let state_dir = &loaded.places.state_dir;
    // Both places are checked, ancestors included, before anything is written in either.
    mayfly_adapter_tart::verify::check_path(state_dir).map_err(DaemonError::Places)?;
    mayfly_adapter_tart::verify::check_path(&loaded.places.tart_home)
        .map_err(DaemonError::Places)?;
    private_dir(state_dir)?;
    private_dir(&loaded.places.tart_home)?;
    let _owner = StateDirLock::acquire(state_dir).map_err(DaemonError::StateDir)?;
    let listener = Listener::bind(&paths::socket(
        config.state_dir.as_deref(),
        &loaded.places,
        &options.env,
    ))
    .map_err(DaemonError::Socket)?;
    let journal_path = state_dir.join("journal.db");
    let host = host_id(state_dir, &journal_path).map_err(DaemonError::Identity)?;
    let secrets = UriSecretProvider::from_process(
        std::slice::from_ref(&config.github.private_key),
        mayfly_adapter_secrets::DEFAULT_DEADLINE,
    );
    let github = crate::github::client(&config.github, secrets)
        .map_err(|_| DaemonError::Start("the GitHub client"))?;
    let journal = SqliteJournal::open(&journal_path, SqliteOptions::new(SystemClock.now()))
        .map_err(DaemonError::Journal)?;
    let (mut exec, floor, boot_id) = data_plane(&loaded, &options, &host).await?;
    let reading = exec.ledger().probe.clone();
    let sizing = crate::sizing::report(
        &config.host,
        Some(&reading),
        &exec.ledger().unmeasured_listed(),
    );
    let notifier = Notifier::new();
    let rng = OsRng::seeded().map_err(|_| DaemonError::Start("the random generator"))?;
    let projections =
        projection::control().map_err(|_| DaemonError::Start("the projection specs"))?;
    let store = AggregateStore::new::<ControlAggregates>(
        journal.clone(),
        SystemClock,
        rng,
        notifier.clone(),
    )
    .map_err(|_| DaemonError::Start("the aggregate store"))?
    .with_projections(projections);
    let key: [u8; 32] = random_bytes().map_err(|_| DaemonError::Start("the random generator"))?;
    let placements = Placements::default();
    let daemon = Daemon {
        config,
        manifests: &loaded.manifests,
        reserves: reading.reserve.resources,
        boot_id,
        host,
        version: options.version.clone(),
        journal,
        store,
        notifier,
        details: ConfigDetails::new(config, config.host.clone_disk_gib, key, placements.clone()),
        placements,
        build: BuildId::new(&options.version).map_err(|_| DaemonError::Start("the build id"))?,
        delivery: DeliveryConfig::default(),
        store_clock: AnchoredStoreClock::now_anchored(),
        sizing,
        readied: watch::channel(false).0,
        github: GithubSide::new(github),
        broker: Arc::new(JitBroker::new()),
    };
    let service = Service {
        store: &daemon.store,
        host: &daemon.host,
        version: &daemon.version,
        sizing: &daemon.sizing,
        ready: &daemon.readied,
        follow: &daemon.notifier.follow,
        operators: Operators::this_user(),
        reaper: &daemon.github.reaper,
    };
    // the socket stops with the root (its post-stop wait is bounded like the runs', in
    // parallel with them) or once the runs ended for another reason
    let stop_serving = Shutdown::new();
    let serving = operator::server::serve(&listener, &service, stop_serving.signal());
    let forward = async {
        let (mut root, mut done) = (shutdown.signal(), stop_serving.signal());
        tokio::select! {
            () = root.cancelled() => stop_serving.cancel(),
            () = done.cancelled() => {}
        }
    };
    // terminal mode's wait, from the first Ctrl-C: once it runs out, the stop is P24
    let busy_wait = config.shutdown.busy_wait;
    let expire = async {
        let (mut graceful, mut done) = (stops.graceful.signal(), stop_serving.signal());
        tokio::select! {
            () = graceful.cancelled() => {}
            () = done.cancelled() => return,
        }
        tokio::select! {
            () = tokio::time::sleep(busy_wait) => {
                tracing::warn!(busy_wait_s = busy_wait.as_secs(), "terminal_stop_wait_expired");
                shutdown.cancel();
            }
            () = done.cancelled() => {}
        }
    };
    let runs = async {
        let ended = daemon
            .runs(&mut exec, &floor, shutdown, &stops.graceful, ready, cut)
            .await;
        stop_serving.cancel();
        ended
    };
    let ((), (), (), ended) = tokio::join!(serving, forward, expire, runs);
    ended
}

impl Daemon<'_> {
    /// Runs until `shutdown` is cancelled or a run fails: one run per data-plane session.
    async fn runs(
        &self,
        exec: &mut Exec,
        floor: &FloorFile,
        shutdown: &Shutdown,
        graceful: &Shutdown,
        ready: &(dyn Fn(&Ready) + Sync),
        cut: &SessionCut,
    ) -> Result<(), DaemonError> {
        let readied = &self.readied;
        // the flag is set before the line is out, so a reader of the line sees it set
        let report = |line: &Ready| {
            readied.send_replace(true);
            ready(line);
        };
        loop {
            if shutdown.is_cancelled() {
                return Ok(());
            }
            self.readied.send_replace(false);
            let mut root = shutdown.signal();
            let bounded = async {
                root.cancelled().await;
                tokio::time::sleep(SHUTDOWN_DEADLINE).await;
            };
            let ended = tokio::select! {
                ended = self.run_once(exec, floor, shutdown, graceful, &report, cut) => ended?,
                () = bounded => {
                    tracing::error!("shutdown_deadline_passed");
                    RunEnd::Shutdown
                }
            };
            match ended {
                RunEnd::Shutdown => return Ok(()),
                RunEnd::SessionEnded => {
                    tracing::warn!("data_plane_session_ended");
                }
            }
        }
    }

    async fn run_once(
        &self,
        exec: &mut Exec,
        floor: &FloorFile,
        shutdown: &Shutdown,
        graceful: &Shutdown,
        ready: &(dyn Fn(&Ready) + Sync),
        cut: &SessionCut,
    ) -> Result<RunEnd, DaemonError> {
        let sessions = HostSessions::new();
        let startup = Startup::new(
            &self.store,
            sessions.clone(),
            StartupConfig {
                host: self.host.clone(),
                boot_id: self.boot_id.clone(),
                version: self.version.clone(),
                reserves: self.reserves,
                crash_loop: false,
            },
        );
        let (dp_link, cp_link) = inproc::link(
            inproc::DEFAULT_CAPACITY,
            SUPPORTED.to_vec(),
            SUPPORTED.to_vec(),
        );
        let launches = exec.ports().guest.secrets().clone();
        let mut dp_link = dp_link.with_launch_configs(Arc::new(launches.clone()));
        let cp_link = cp_link.with_launch_configs(self.broker.clone());
        let opening = async {
            let mut cp_link = cp_link;
            let opened = startup
                .open(
                    &mut cp_link,
                    &self.journal,
                    (&self.journal, self.store.projections(), &self.build),
                )
                .await;
            opened.map(|opened| (opened, cp_link))
        };
        let (entered, opened) = tokio::join!(exec.enter(&mut dp_link, floor), opening);
        let (opened, cp_link) = opened.map_err(DaemonError::Startup)?;
        entered.map_err(|e| DaemonError::Session(format!("{e:?}")))?;
        let (cp_directives, cp_observations) = cp_link
            .split()
            .map_err(|e| DaemonError::Session(e.to_string()))?;
        let (dp_directives, dp_observations) = dp_link
            .split()
            .map_err(|e| DaemonError::Session(e.to_string()))?;
        self.configure_pools(opened.epoch).await;
        self.pin_images(opened.epoch).await;
        let inventory = Directive {
            directive_id: DirectiveId::from_u128(derive(&[
                b"report_inventory",
                self.host.as_str().as_bytes(),
                &opened.epoch.get().to_be_bytes(),
            ])),
            epoch: opened.epoch,
            attempt: None,
            deadline: Deadline::after(self.store.clock().now(), INVENTORY_DEADLINE),
            kind: DirectiveKind::ReportInventory,
        };
        if cp_directives.send(inventory).await.is_err() {
            tracing::warn!("inventory_request_not_sent");
        }
        let data = async move {
            let (mut source, sink) = (Cuttable::new(dp_directives, cut.clone()), dp_observations);
            exec.serve(&mut source, &sink).await
        };
        let control = self.control_plane(
            Session {
                startup: &startup,
                sessions: &sessions,
                epoch: opened.epoch,
                mode: opened.mode,
                directives: cp_directives,
                observations: cp_observations,
            },
            shutdown,
            graceful,
            ready,
        );
        // boxed: the run's two planes are a large future, and polling it beside the expiry
        // inline overflows a debug build's 2 MiB test-thread stack
        let planes = Box::pin(async { tokio::join!(data, control) });
        let (served, ended) = tokio::select! {
            both = planes => both,
            never = self.expire_launches(&launches) => match never {},
        };
        session_ended(&sessions, &self.host);
        if !matches!(ended, Ok(RunEnd::Shutdown)) {
            tracing::warn!(end = ?served, "data_plane_serve_ended");
        }
        ended
    }

    /// Drops the JIT configs no launch took once their directive's deadline passed (ADR 0082),
    /// every [`LAUNCH_EXPIRY_EVERY`], read on the control plane's clock, the one the deadlines
    /// are written in (both planes read one host's clock under `--role all`; split roles will
    /// read it through the handshake's offset, LNK-01). Never returns: the run's end drops it.
    async fn expire_launches(&self, launches: &SharedLaunches) -> std::convert::Infallible {
        loop {
            self.store.clock().sleep(LAUNCH_EXPIRY_EVERY).await;
            launches.0.expire(self.store.clock().now());
        }
    }

    /// Applies each configured pool to its `Pool` stream when it changed (`ConfigurePool` at
    /// the next revision): the static demand the reconciler plans from. A pool the file no
    /// longer names is left as it is (decommissioning is the operator's).
    async fn configure_pools(&self, epoch: Epoch) {
        for pool in &self.config.pools {
            let Ok(state) = self.store.load::<Pool>(pool.name.as_str()).await else {
                tracing::warn!(pool = pool.name.as_str(), "pool_not_configured");
                continue;
            };
            let wanted = PoolConfig {
                revision: state.config.map_or(1, |c| c.revision.saturating_add(1)),
                tier: match pool.tier {
                    Tier::Trusted => TrustTier::Trusted,
                },
                os: match pool.guest_os {
                    GuestOs::Macos => DomainOs::Macos,
                    GuestOs::Linux => DomainOs::Linux,
                },
                vm_resources: Resources::new(
                    pool.memory_mib.saturating_mul(MIB),
                    self.config
                        .host
                        .clone_disk_gib
                        .map_or(0, |gib| gib.saturating_mul(GIB)),
                ),
                max_running: pool.max_running,
                min_prepared: pool.min_prepared,
                breaker_threshold: pool.breaker.failures,
                breaker_window: Millis::new(
                    u64::try_from(pool.breaker.window.as_millis()).unwrap_or(u64::MAX),
                ),
            };
            if state.config.is_some_and(|c| {
                PoolConfig {
                    revision: wanted.revision,
                    ..c
                } == wanted
            }) {
                continue;
            }
            let id = CommandId::from_u128(derive(&[
                b"configure_pool",
                pool.name.as_str().as_bytes(),
                format!("{wanted:?}").as_bytes(),
            ]));
            let command = Command::new(
                id,
                self.store.clock().now(),
                epoch,
                Actor::Startup,
                PoolCommand::ConfigurePool(ConfigurePool {
                    revision: wanted.revision,
                    tier: wanted.tier,
                    os: wanted.os,
                    vm_resources: wanted.vm_resources,
                    max_running: wanted.max_running,
                    min_prepared: Some(wanted.min_prepared),
                    breaker_threshold: wanted.breaker_threshold,
                    breaker_window: wanted.breaker_window,
                }),
            );
            match self
                .store
                .execute::<Pool>(SaveRequest::new(pool.name.as_str(), command))
                .await
            {
                Ok(Outcome::Accepted { .. }) => {}
                Ok(Outcome::Refused { rejection, .. }) => {
                    tracing::warn!(
                        pool = pool.name.as_str(),
                        code = mayfly_control_domain::Rejection::code(&rejection),
                        "pool_configuration_refused"
                    );
                }
                Err(_) => {
                    tracing::warn!(pool = pool.name.as_str(), "pool_not_configured");
                }
            }
        }
    }

    /// Registers each pool's image from its manifest, records its verification and pins the pool
    /// to it (OPS-02, ADR 0091): the manifest passed [`load`]'s checks against this deployment,
    /// which is what `Verified` means here; the content is verified by the data plane at its
    /// pull (ADR 0087 decision 7). Each step is idempotent, so every run repeats them, and a
    /// manifest whose digest changed moves the pin (`ImagePinned{previous}`). A refusal (the
    /// digest registered with another manifest, a rejected or withdrawn image) leaves the pool's
    /// pin as it was and is logged for the operator; the run goes on. Pinned by
    /// `tests/daemon.rs::should_pin_each_pool_to_its_manifest_image_when_the_daemon_starts`.
    async fn pin_images(&self, epoch: Epoch) {
        for (pool, manifest) in self.manifests {
            let digest = &manifest.source.digest;
            let registered = images::register_image(
                &self.store,
                digest,
                manifest.recorded(),
                Actor::Startup,
                epoch,
            )
            .await;
            match &registered {
                Ok(()) => {}
                Err(error @ CatalogError::Save(_)) => tracing::warn!(
                    pool = pool.as_str(),
                    digest = digest.as_str(),
                    code = error.code(),
                    "image_registration_failed"
                ),
                Err(error @ CatalogError::Refused(_)) => tracing::error!(
                    pool = pool.as_str(),
                    digest = digest.as_str(),
                    code = error.code(),
                    "image_registration_refused"
                ),
            }
            if registered.is_err() {
                continue;
            }
            let verified = images::record_verification(
                &self.store,
                digest,
                Verification::Passed,
                Actor::Startup,
                epoch,
            )
            .await;
            match &verified {
                Ok(()) => {}
                Err(error @ CatalogError::Save(_)) => tracing::warn!(
                    pool = pool.as_str(),
                    digest = digest.as_str(),
                    code = error.code(),
                    "image_verification_failed"
                ),
                Err(error @ CatalogError::Refused(_)) => tracing::error!(
                    pool = pool.as_str(),
                    digest = digest.as_str(),
                    code = error.code(),
                    "image_verification_refused"
                ),
            }
            if verified.is_err() {
                continue;
            }
            match images::pin_image(&self.store, pool, digest, Actor::Startup, epoch).await {
                Ok(_) => {}
                Err(error @ PinError::Save(_)) => tracing::warn!(
                    pool = pool.as_str(),
                    digest = digest.as_str(),
                    code = error.code(),
                    "image_pin_failed"
                ),
                Err(error) => tracing::error!(
                    pool = pool.as_str(),
                    digest = digest.as_str(),
                    code = error.code(),
                    "image_pin_refused"
                ),
            }
        }
    }

    /// The control plane's side of one run: the supervised loops, then the clean end when the
    /// root was cancelled or terminal mode's graceful stop settled. Owns the link's
    /// control-plane halves, so the data plane's serve ends once this returns.
    async fn control_plane<D, O>(
        &self,
        session: Session<'_, '_, D, O>,
        shutdown: &Shutdown,
        graceful: &Shutdown,
        ready: &(dyn Fn(&Ready) + Sync),
    ) -> Result<RunEnd, DaemonError>
    where
        D: mayfly_protocol::DirectiveSink + Clone + 'static,
        O: mayfly_protocol::ObservationSource,
    {
        let Session {
            startup,
            sessions,
            epoch,
            mode,
            directives,
            mut observations,
        } = session;
        let workers = || -> Result<Worker, DaemonError> {
            Ok(DeliveryWorker::new(
                self.journal.clone(),
                self.journal.ledger(self.store_clock),
                OsRng::seeded().map_err(|_| DaemonError::Start("the random generator"))?,
                SystemClock,
                self.delivery,
            ))
        };
        let (saga_worker, relay_worker, placement_worker) = (workers()?, workers()?, workers()?);
        let saga = AttemptSaga::new(&self.store, sessions.clone(), SagaConfig::default());
        let index = PlacementIndex::default();
        let placement_relay = PlacementRelay::new(
            &self.store,
            directives.clone(),
            SystemClock,
            sessions.clone(),
            PlacementRelayConfig::default(),
            &self.delivery,
            index.clone(),
        );
        let manager = PlacementManager::new(
            &self.store,
            &self.journal,
            PlacementConfig::new(epoch),
            self.notifier.placement.clone(),
            index.clone(),
        );
        let relay = DirectiveRelay::new(LinkSink::new(
            &self.store,
            directives,
            SystemClock,
            self.details.clone(),
            sessions.clone(),
            SinkConfig::default(),
            &self.delivery,
        ));
        let reconciler = Reconciler::new(
            &self.store,
            &self.journal,
            ReconcilerConfig {
                policies: crate::config::reconciler_policies(self.config),
                ..ReconcilerConfig::new(epoch)
            },
            self.notifier.reconciler.clone(),
        )
        .with_prepared(self.notifier.prepared.clone());
        let terms = TermsPolicy::new(
            &self.store,
            &self.journal,
            TermsConfig::new(epoch),
            self.notifier.terms.clone(),
        );
        reconciler.set_safe_mode(mode == Mode::Safe);
        manager.set_safe_mode(mode == Mode::Safe);
        terms.set_safe_mode(mode == Mode::Safe);
        let run = Shutdown::new();
        let (recovered_tx, recovered) = watch::channel(None::<Mode>);
        // set once the ready line is out: the terms policy's loop starts only then, so its
        // passes never race the ready line's own pass on the host stream
        let (readied_tx, readied) = watch::channel(false);
        // set once the reconciler passed with this host stopping: the graceful stop reads the
        // leases only then, so a pass that read the fleet before the stop cannot reserve after
        // the stop found the host empty
        let (planned_tx, planned) = watch::channel(false);
        let inbox = Inbox::new(&self.store, self.host.clone(), epoch).with_placements(index);
        let empty = BTreeMap::new();
        // RT-07: the listeners, the job facts, the registration manager and the reaper
        let github = GithubRun::new(RunInputs {
            store: &self.store,
            journal: &self.journal,
            store_clock: self.store_clock,
            notifier: &self.notifier,
            side: &self.github,
            broker: &self.broker,
            host: &self.host,
            pools: &self.config.pools,
            sessions,
            epoch,
        })
        .map_err(|_| DaemonError::Start("the random generator"))?;
        let serve_github = {
            let (recovered, cancel) = (recovered.clone(), run.signal());
            let github = &github;
            async move { github.serve(&recovered, &cancel).await }
        };

        let forward = {
            let mut root = shutdown.signal();
            let mut ours = run.signal();
            let run = run.clone();
            async move {
                tokio::select! {
                    () = root.cancelled() => run.cancel(),
                    () = ours.cancelled() => {}
                }
            }
        };

        let observe = {
            let mut cancel = run.signal();
            let run = run.clone();
            let saga = &saga;
            let inbox = &inbox;
            let empty = &empty;
            let reconciler = &reconciler;
            let manager = &manager;
            let terms = &terms;
            async move {
                let mut done = false;
                let mut held = false;
                loop {
                    let observation = tokio::select! {
                        biased;
                        () = cancel.cancelled() => return Ok(RunEnd::Shutdown),
                        received = observations.recv() => received,
                    };
                    let Ok(observation) = observation else {
                        run.cancel();
                        return if done {
                            Ok(RunEnd::SessionEnded)
                        } else {
                            Err(DaemonError::SessionLost)
                        };
                    };
                    let is_inventory = matches!(
                        observation.kind,
                        ObservationKind::Inventory { .. }
                            | ObservationKind::InventoryIncomplete { .. }
                    );
                    if is_inventory && !done {
                        match startup.recover(&observation, empty, saga).await {
                            Ok(Recovery::Waiting { .. }) => {}
                            Ok(Recovery::Completed(recovered)) => {
                                done = true;
                                held = !recovered.held.is_empty();
                                for name in &recovered.reported {
                                    tracing::warn!(vm = name.as_str(), "unrecorded_vm_reported");
                                }
                                let _ = recovered_tx.send(Some(Mode::Normal));
                            }
                            Ok(Recovery::SafeMode { .. }) => {
                                done = true;
                                reconciler.set_safe_mode(true);
                                manager.set_safe_mode(true);
                                terms.set_safe_mode(true);
                                let _ = recovered_tx.send(Some(Mode::Safe));
                            }
                            Err(error) => {
                                run.cancel();
                                return Err(DaemonError::Startup(error));
                            }
                        }
                        continue;
                    }
                    if inbox.record(&observation).await.is_err() {
                        tracing::warn!("observation_not_recorded");
                    }
                    if is_inventory && held {
                        match startup.resume_held(&observation, empty, saga).await {
                            Ok(still) => held = !still.is_empty(),
                            Err(_) => tracing::warn!("held_cleanup_not_reconsidered"),
                        }
                    }
                }
            }
        };

        let consume = {
            let mut cancel = run.signal();
            let mut recovered = recovered.clone();
            let (saga, relay, placement_relay) = (&saga, &relay, &placement_relay);
            let (saga_worker, relay_worker, placement_worker) =
                (&saga_worker, &relay_worker, &placement_worker);
            let terms = &terms;
            async move {
                tokio::select! {
                    () = cancel.cancelled() => return,
                    // `Ok` only: a run whose recovery failed drops the sender, and an `Err` here must
                    // not read as recovered (no ready, no consumer); the run's cancel ends this loop.
                    Ok(_) = recovered.wait_for(Option::is_some) => {}
                }
                let catch_up = || async {
                    let saga_ok = saga_worker.catch_up(saga).await.is_ok();
                    if !saga_ok {
                        tracing::warn!(consumer = "attempt_saga", "consumer_catch_up_failed");
                    }
                    let relay_ok = relay_worker.catch_up(relay).await.is_ok();
                    if !relay_ok {
                        tracing::warn!(consumer = "directive_relay", "consumer_catch_up_failed");
                    }
                    let placement_ok = placement_worker.catch_up(placement_relay).await.is_ok();
                    if !placement_ok {
                        tracing::warn!(consumer = "placement_relay", "consumer_catch_up_failed");
                    }
                    saga_ok && relay_ok && placement_ok
                };
                // the ready line's promise: the host side's consumers caught up once and, after
                // them, one terms pass (P4) over the recovered journal (the GitHub side's
                // consumers are not waited for)
                let first = {
                    let catch_up = &catch_up;
                    move || async move {
                        let caught_up = catch_up().await;
                        let applied = terms.run_pass().await.is_ok_and(|r| r.complete());
                        if !applied {
                            tracing::warn!("terms_pass_failed");
                        }
                        caught_up && applied
                    }
                };
                if !until_caught_up(first, tokio::time::sleep, &mut cancel).await {
                    return;
                }
                let mode = (*recovered.borrow()).unwrap_or(Mode::Safe);
                ready(&Ready {
                    host: self.host.clone(),
                    epoch,
                    mode,
                });
                let _ = readied_tx.send(true);
                let mut coarse = tokio::time::Instant::now() + COARSE;
                loop {
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => return,
                        () = self.notifier.saga.wait() => {
                            if saga_worker.drain_kicked(saga, &self.notifier.saga).await.is_err() {
                                tracing::warn!(consumer = "attempt_saga", "consumer_drain_failed");
                            }
                        }
                        () = self.notifier.relay.wait() => {
                            if relay_worker.drain_kicked(relay, &self.notifier.relay).await.is_err() {
                                tracing::warn!(consumer = "directive_relay", "consumer_drain_failed");
                            }
                        }
                        () = self.notifier.placement_relay.wait() => {
                            if placement_worker
                                .drain_kicked(placement_relay, &self.notifier.placement_relay)
                                .await
                                .is_err()
                            {
                                tracing::warn!(consumer = "placement_relay", "consumer_drain_failed");
                            }
                        }
                        () = tokio::time::sleep_until(coarse) => {
                            catch_up().await;
                            coarse = tokio::time::Instant::now() + COARSE;
                        }
                    }
                }
            }
        };

        let timeouts = {
            let mut cancel = run.signal();
            let mut recovered = recovered.clone();
            let saga = &saga;
            async move {
                tokio::select! {
                    () = cancel.cancelled() => return,
                    Ok(_) = recovered.wait_for(Option::is_some) => {}
                }
                loop {
                    let wait = saga.next_due().map_or(COARSE, |due| {
                        Duration::from_millis(
                            due.get().saturating_sub(self.store.clock().now().get()),
                        )
                        .clamp(TIMER_FLOOR, COARSE)
                    });
                    tokio::select! {
                        () = cancel.cancelled() => return,
                        () = tokio::time::sleep(wait) => {}
                    }
                    let _report = saga.expire_due().await;
                }
            }
        };

        let reconcile = {
            let mut cancel = run.signal();
            let mut recovered = recovered.clone();
            let reconciler = &reconciler;
            let planned_tx = &planned_tx;
            let stop = graceful.signal();
            async move {
                tokio::select! {
                    () = cancel.cancelled() => return,
                    Ok(_) = recovered.wait_for(Option::is_some) => {}
                }
                let normal = *recovered.borrow() == Some(Mode::Normal);
                reconcile_loop(
                    &RunPasses {
                        reconciler,
                        clock: self.store.clock(),
                    },
                    ReconcileWakes {
                        kicked: &self.notifier.reconciler,
                        prepared: &self.notifier.prepared,
                        stopping: &self.notifier.stopping,
                    },
                    LoopControl {
                        host: &self.host,
                        normal,
                        stop,
                        cancel,
                        planned: planned_tx,
                    },
                )
                .await;
            }
        };

        // P17 (CP-07): the placement manager's passes, on its kicks, its interval and the next
        // directive deadline; after each, the clone details' bases follow this host's
        // materialised placements
        let place = {
            let mut cancel = run.signal();
            let mut recovered = recovered.clone();
            let manager = &manager;
            async move {
                tokio::select! {
                    () = cancel.cancelled() => return,
                    Ok(_) = recovered.wait_for(Option::is_some) => {}
                }
                loop {
                    match manager.tick().await {
                        Ok(Some(report)) => {
                            self.placements.replace(
                                report
                                    .bases
                                    .iter()
                                    .filter(|((host, _), _)| *host == self.host)
                                    .map(|((_, digest), name)| (digest.clone(), name.as_str())),
                            );
                        }
                        Ok(None) => {}
                        Err(_) => tracing::warn!("placement_pass_failed"),
                    }
                    let wait = Duration::from_millis(
                        manager
                            .next_due()
                            .get()
                            .saturating_sub(self.store.clock().now().get()),
                    )
                    .clamp(Duration::from_secs(1), COARSE);
                    tokio::select! {
                        () = cancel.cancelled() => return,
                        () = self.notifier.placement.wait() => {}
                        () = tokio::time::sleep(wait) => {}
                    }
                }
            }
        };

        // P4 (CP-08): the terms policy's passes, on its kicks and its interval, from the ready
        // line on (the ready line's own pass is the first): each pool's terms applied on this
        // host at the pool's terms revision, so the reconciler can reserve. The host stream moves
        // with every reservation and report, so a pass waits at least `TERMS_FLOOR` after the
        // last before a kick starts another.
        let apply_terms = {
            let mut cancel = run.signal();
            let mut readied = readied;
            let terms = &terms;
            async move {
                tokio::select! {
                    () = cancel.cancelled() => return,
                    Ok(_) = readied.wait_for(|r| *r) => {}
                }
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => return,
                        () = tokio::time::sleep(TERMS_FLOOR) => {}
                    }
                    let wait = terms_pass(terms).await;
                    tokio::select! {
                        () = cancel.cancelled() => return,
                        () = self.notifier.terms.wait() => {}
                        () = tokio::time::sleep(wait) => {}
                    }
                }
            }
        };

        // RT-05a: terminal mode's graceful stop of this run, once recovery completed: the run
        // ends clean once no lease but a quarantined one is left on the host; in safe mode (or
        // once an unknown kind was met) nothing destructive may run, so the stop is P24 at once
        let terminal = {
            let mut cancel = run.signal();
            let mut stop = graceful.signal();
            let mut recovered = recovered.clone();
            let mut planned = planned;
            let run = run.clone();
            async move {
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = stop.cancelled() => {}
                }
                tokio::select! {
                    () = cancel.cancelled() => return,
                    Ok(_) = recovered.wait_for(Option::is_some) => {}
                }
                if *recovered.borrow() != Some(Mode::Normal) || self.notifier.unknown_kind_seen() {
                    tracing::warn!("terminal_stop_in_safe_mode");
                    shutdown.cancel();
                    return;
                }
                tokio::select! {
                    () = cancel.cancelled() => return,
                    Ok(_) = planned.wait_for(|p| *p) => {}
                }
                loop {
                    // taken before the read, so a commit after it wakes the next turn
                    let _ = self.notifier.stop.take();
                    if self.stop_settled().await {
                        run.cancel();
                        return;
                    }
                    tokio::select! {
                        () = cancel.cancelled() => return,
                        () = self.notifier.unknown_kind() => {
                            tracing::warn!("terminal_stop_in_safe_mode");
                            shutdown.cancel();
                            return;
                        }
                        () = self.notifier.stop.wait() => {}
                        () = tokio::time::sleep(COARSE) => {}
                    }
                }
            }
        };

        let unknown = {
            let mut cancel = run.signal();
            let reconciler = &reconciler;
            let manager = &manager;
            let terms = &terms;
            async move {
                tokio::select! {
                    () = cancel.cancelled() => {}
                    () = self.notifier.unknown_kind() => {
                        reconciler.set_safe_mode(true);
                        manager.set_safe_mode(true);
                        terms.set_safe_mode(true);
                        if enter_safe_mode(
                            &self.store,
                            sessions,
                            &self.host,
                            epoch,
                            SafeModeReason::UnknownEventKind,
                        )
                        .await
                        .is_err()
                        {
                            tracing::error!("safe_mode_not_recorded");
                        }
                    }
                }
            }
        };

        let ((), ended, (), (), (), (), (), (), (), ()) = tokio::join!(
            forward,
            observe,
            consume,
            timeouts,
            reconcile,
            place,
            apply_terms,
            terminal,
            unknown,
            serve_github
        );
        if matches!(ended, Ok(RunEnd::Shutdown)) {
            self.end_run(epoch).await;
        }
        ended
    }

    /// Whether terminal mode's graceful stop has nothing left to wait for on this host: every
    /// lease left is quarantined, or none is. A lease is released only on its VM's deletion or
    /// loss being observed (`vm_deleted`, `attempt_lost`) or for a reservation never started, and
    /// a quarantined one is an operator's case (ADR 0025), so this is
    /// `formal/recovery.qnt`'s `settled` for every record. A host stream that cannot be read
    /// is not settled.
    async fn stop_settled(&self) -> bool {
        match self.store.load::<Host>(self.host.as_str()).await {
            Ok(host) => host
                .leases
                .values()
                .all(|lease| matches!(lease.phase, LeasePhase::Quarantined { .. })),
            Err(_) => {
                tracing::warn!("terminal_stop_host_unread");
                false
            }
        }
    }

    /// P24's record: `BeginShutdown` then `EndRun`, under the run's epoch.
    async fn end_run(&self, epoch: Epoch) {
        let Ok(state) = self.store.load::<HostRun>(self.host.as_str()).await else {
            tracing::error!("run_end_not_recorded");
            return;
        };
        for (step, body) in [
            (&b"begin_shutdown"[..], HostRunCommand::BeginShutdown),
            (&b"end_run"[..], HostRunCommand::EndRun),
        ] {
            let id = CommandId::from_u128(derive(&[
                step,
                self.host.as_str().as_bytes(),
                &state.run_no.to_be_bytes(),
            ]));
            let command = Command::new(
                id,
                self.store.clock().now(),
                epoch,
                Actor::Automation { policy: SHUTDOWN },
                body,
            );
            let saved: Result<Outcome<_>, SaveError> = self
                .store
                .execute::<HostRun>(SaveRequest::new(self.host.as_str(), command))
                .await;
            if !matches!(saved, Ok(Outcome::Accepted { .. })) {
                tracing::error!("run_end_not_recorded");
                return;
            }
        }
    }
}

/// Runs `mayflyd --role all` on its own runtime until SIGTERM, or Ctrl-C's escalation ends it
/// ([`crate::supervise::on_signals`]: the graceful stop, a warning on stderr, then P24): the
/// binary's whole body, under umask `077`. Each run's ready line goes to stdout ([`Ready`]'s
/// `Display`), flushed.
///
/// # Errors
/// [`DaemonError`]; its [`DaemonError::exit_code`] is the process's.
pub fn serve(options: Options) -> Result<(), DaemonError> {
    // Everything the daemon creates (journal, floor, records, logs) is its user's alone (plan
    // §13), whatever umask it was started with; children inherit it.
    rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o077));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|_| DaemonError::Start("the async runtime"))?;
    let ended = runtime.block_on(async {
        let stops = Stops::default();
        let print = |ready: &Ready| {
            use std::io::Write as _;
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{ready}");
            let _ = out.flush();
        };
        let warn = |line: &str| {
            use std::io::Write as _;
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "{line}");
            let _ = err.flush();
        };
        let daemon = run(options, stops.clone(), &print);
        tokio::pin!(daemon);
        tokio::select! {
            ended = &mut daemon => ended,
            installed = crate::supervise::on_signals(stops, warn) => {
                if installed.is_err() {
                    return Err(DaemonError::Start("the signal handlers"));
                }
                daemon.await
            }
        }
    });
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
    ended
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use mayfly_control_domain::host::Host;
    use mayfly_domain::PoolName;
    use mayfly_testkit::contracts::journal::InjectedFault;
    use mayfly_testkit::fleet::{EPOCH, PoolSpec, T0, World, report, store};
    use mayfly_testkit::journal::MemoryJournal;

    use super::*;

    /// A fleet with one host and one pool, both terms already applied by the kit, then the
    /// pool paused without them: terms due on the host.
    fn paused(journal: &MemoryJournal) -> mayfly_testkit::fleet::FleetStore {
        let fleet = store(journal, &mayfly_testkit::clock::FakeClock::at(T0));
        {
            let mut world = World::new(&fleet);
            world.register_host("h1", report(64, 0, 8));
            world.configure_pool(PoolSpec {
                name: PoolName::new("lin").unwrap_or_else(|e| panic!("{e}")),
                os: DomainOs::Linux,
                memory_gib: 4,
                max_running: 2,
                min_prepared: 0,
                breaker: 3,
            });
            let paused = world.command::<Pool>("lin", Actor::Startup, PoolCommand::PausePool);
            assert_eq!(paused, Ok(()));
        }
        fleet
    }

    fn open_on(fleet: &mayfly_testkit::fleet::FleetStore) -> Option<bool> {
        let lin = PoolName::new("lin").unwrap_or_else(|e| panic!("{e}"));
        mayfly_testkit::block_on(fleet.load::<Host>("h1"))
            .unwrap_or_else(|e| panic!("{e}"))
            .pool_terms
            .get(&lin)
            .map(|t| t.open)
    }

    #[test]
    fn should_pass_and_wait_until_due_when_the_terms_loop_runs() {
        // given: the pause's terms due on the host
        let journal = MemoryJournal::new();
        let fleet = paused(&journal);
        let terms = TermsPolicy::new(&fleet, &journal, TermsConfig::new(EPOCH), Kicks::new(8));
        let before = open_on(&fleet);

        // when
        let wait = runtime().block_on(terms_pass(&terms));

        // then: applied, and the next pass is an interval away
        assert_eq!(before, Some(true));
        assert_eq!(open_on(&fleet), Some(false));
        assert_eq!(wait, COARSE);
    }

    #[test]
    fn should_wait_for_the_next_pass_when_a_terms_pass_is_incomplete() {
        // given: the store fails the terms' save
        let journal = MemoryJournal::new();
        let fleet = paused(&journal);
        let terms = TermsPolicy::new(&fleet, &journal, TermsConfig::new(EPOCH), Kicks::new(8));
        assert!(journal.inject(InjectedFault::StorageBeforeCommit));

        // when
        let wait = runtime().block_on(terms_pass(&terms));

        // then: nothing applied; the loop waits for a kick or the interval
        assert_eq!(open_on(&fleet), Some(true));
        assert_eq!(wait, COARSE);
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap_or_else(|e| panic!("{e}"))
    }

    /// A [`Passes`] that records when each pass ran on the paused clock, and cancels the loop
    /// once it was asked how long to wait twice.
    struct Counted {
        ticks: Cell<u32>,
        passes: RefCell<Vec<tokio::time::Instant>>,
        stopped: RefCell<Option<HostId>>,
        asked: Cell<u32>,
        cancel: Shutdown,
    }

    impl Passes for Counted {
        async fn tick(&self) -> bool {
            self.ticks.set(self.ticks.get() + 1);
            true
        }

        async fn pass(&self) -> bool {
            self.passes.borrow_mut().push(tokio::time::Instant::now());
            true
        }

        fn stop_on(&self, host: &HostId) {
            *self.stopped.borrow_mut() = Some(host.clone());
        }

        fn until_due(&self) -> Duration {
            self.asked.set(self.asked.get() + 1);
            if self.asked.get() >= 2 {
                self.cancel.cancel();
            }
            COARSE
        }
    }

    #[test]
    fn should_pass_a_bounded_number_of_times_when_the_run_stops_with_kicks_pending() {
        // given: the reconciler's own kicks pending (a host stream and a Prepared attempt
        // moved), which only `tick` takes, and the graceful stop asked
        let stream =
            mayfly_control::journal::StreamId::new("host-h1").unwrap_or_else(|e| panic!("{e}"));
        let (kicked, prepared, stopping) = (Kicks::new(8), Kicks::new(8), Kicks::new(8));
        kicked.kick(&stream);
        prepared.kick(&stream);
        stopping.kick(&stream);
        let (graceful, run) = (Shutdown::new(), Shutdown::new());
        graceful.cancel();
        let counted = Counted {
            ticks: Cell::new(0),
            passes: RefCell::new(Vec::new()),
            stopped: RefCell::new(None),
            asked: Cell::new(0),
            cancel: run.clone(),
        };
        let host = HostId::new("h1").unwrap_or_else(|e| panic!("{e}"));
        let planned = watch::channel(false).0;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap_or_else(|e| panic!("{e}"));

        // when: the loop runs until its second pass (the paused clock jumps to each timer)
        runtime.block_on(reconcile_loop(
            &counted,
            ReconcileWakes {
                kicked: &kicked,
                prepared: &prepared,
                stopping: &stopping,
            },
            LoopControl {
                host: &host,
                normal: true,
                stop: graceful.signal(),
                cancel: run.signal(),
                planned: &planned,
            },
        ));

        // then: no tick, the host planned as stopping and the first pass reported; the second
        // pass waited for the interval, not back to back on kicks only `tick` takes
        let passes = counted.passes.borrow();
        assert_eq!(counted.ticks.get(), 0);
        assert_eq!(passes.len(), 2);
        assert!(passes[1] - passes[0] >= COARSE, "{passes:?}");
        assert_eq!(counted.stopped.borrow().as_ref(), Some(&host));
        assert!(*planned.borrow());
    }

    #[test]
    fn should_report_caught_up_only_after_a_failed_catch_up_succeeds_when_retried() {
        // given
        let attempts = Cell::new(0);
        let waits = RefCell::new(Vec::new());
        let shutdown = Shutdown::new();
        let mut cancel = shutdown.signal();

        // when
        let caught_up = runtime().block_on(until_caught_up(
            || {
                attempts.set(attempts.get() + 1);
                let ok = attempts.get() > 2;
                async move { ok }
            },
            |wait| {
                waits.borrow_mut().push(wait);
                async {}
            },
            &mut cancel,
        ));

        // then
        assert!(caught_up);
        assert_eq!(attempts.get(), 3);
        assert_eq!(
            waits.into_inner(),
            [CATCH_UP_BACKOFF_FIRST, CATCH_UP_BACKOFF_FIRST * 2]
        );
    }

    #[test]
    fn should_not_report_caught_up_when_cancelled_while_catch_up_fails() {
        // given
        let shutdown = Shutdown::new();
        let mut cancel = shutdown.signal();
        let attempts = Cell::new(0);

        // when
        let caught_up = runtime().block_on(until_caught_up(
            || {
                attempts.set(attempts.get() + 1);
                shutdown.cancel();
                async { false }
            },
            |_| std::future::pending::<()>(),
            &mut cancel,
        ));

        // then
        assert!(!caught_up);
        assert_eq!(attempts.get(), 1);
    }
}
