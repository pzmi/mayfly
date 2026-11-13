//! The image placement manager (policies P17 and P18 of `doc/domain-model.md` §7; ADRs 0030,
//! 0039): a thin driver around the pure [`plan`]. A pass reads the read side
//! ([`queries::fleet`]: pools, images, placements, hosts, leases and attempts), maps it to the
//! plan's inputs ([`inputs_of`]), plans, and issues each planned command through
//! [`AggregateStore`], where the aggregate that owns it decides it again: `RequestPlacement`,
//! `IssueDirective`, `RecordOutcomeUnknown` and `RequestEviction` on `ImagePlacement`,
//! `TripBreaker{runner_version}` on `Pool`.
//!
//! **When.** At most once per [`PlacementConfig::interval`] by the control plane's clock, at
//! once when a pool, image, placement or host stream moved ([`PlacementManager::tick`] reads its
//! [`Kicks`]), and when a sent directive's deadline plus the reply grace falls due
//! ([`PlacementReport::next_due`]). Level-triggered: a lost kick costs at most the interval.
//!
//! **Idempotent.** Each command's id is derived from what it is about and the placement's
//! revision the pass read (a request, an issue, an eviction), the directive (a timeout) or the
//! pool, image and instant (a trip); a directive's id is derived from its issue's command id, so
//! a repeated pass issues one directive under one id, which the relay resends and the data
//! plane executes once per epoch.
//!
//! **What the directives do** is the placement relay's ([`crate::delivery::PlacementRelay`])
//! and the observation inbox's ([`crate::inbox`]): the relay puts a placement's pending
//! directive on the link (held in safe mode, I29), the inbox records its answer on the placement
//! stream, found through the [`PlacementIndex`] this manager keeps of the directives pending.
//!
//! **Never a clone from an OCI reference** (ADR 0039): the manager's directives are the only
//! way a base comes to exist, a base is named by digest and runner version
//! (`image::base_name`, `mfb-…`), and a clone's base is read from the materialised placements
//! ([`bases_of`]); a clone of an image with no materialised base is held `image_base_unknown`
//! by the relay's sink, never sent with a guessed base.
//!
//! **Withdrawal** ([`withdraw_image`]) is refused while a pool pins the image (P17): the pools
//! are listed from the `pools` projection, each pool's stream is read for its pin, and the
//! `WithdrawImage` is saved with a read of every pool stream at the revision the check found it,
//! so a pin committed in between refuses it at the save rather than withdraw a pinned image.
//!
//! **Eviction** is saved with a read of the host stream and every pool stream at the revisions
//! the pass read, and a lease whose attempt the pass did not see counts as referencing every
//! base on its host; a start reads the placement stream ([`crate::start`]). Together they make
//! the eviction's reference check hold at its commit (`formal/scheduling.qnt`'s `evict`).
//!
//! **Safe mode** ([`PlacementManager::set_safe_mode`]): only timeouts are recorded.
//!
//! **A pull or materialisation of `Unknown` outcome** is resolved only by an observation (ADR
//! 0025, P7: no new directive while it is unknown): a late answer, or a complete inventory, with
//! the image store's listing beside it from protocol 1.5 (DP-08), which shows a pulled image or
//! a `Ready` base and so the directive completed, or shows the image or the base absent and so
//! it did not apply; a VM under the base's name that is not a `Ready` base resolves nothing
//! ([`crate::inbox`]). A pull or materialisation sent before a restart stays in flight until its
//! deadline (the relay never sends a sent directive again, and an answer comes only in the
//! session it ran in), and only then becomes `Unknown` and resolvable (DP-08's open question).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use mayfly_control_domain::image::{Image, ImageCommand, ImageRejection};
use mayfly_control_domain::placement::policy::{
    DueDirective, ImageView, PinView, PlacementAction, PlacementView, PolicyInputs, pinned_by, plan,
};
use mayfly_control_domain::placement::{
    EvictionScope, ImagePlacement, Pending, PendingOutcome, PlacementCommand, PlacementDirective,
    placement_instance,
};
use mayfly_control_domain::pool::{Pool, PoolCommand, TripReason};
use mayfly_control_domain::{Actor, Aggregate, Command, Rejection};
use mayfly_domain::{
    CommandId, Deadline, DirectiveId, Epoch, HostId, ImageDigest, Millis, PoolName,
};
use sha2::{Digest, Sha256};

use crate::delivery::Kicks;
use crate::journal::{ExpectedRevision, Journal, StreamId, StreamRevision};
use crate::projection::ProjectionStore;
use crate::projection::views::{self, phase_of};
use crate::queries::{self, FleetView, QueryError};
use crate::store::{
    AggregateStore, Clock, Outcome, Rng, SaveError, SaveRequest, StoreNotifier, stream_of,
};

/// The manager's name: the actor of its commands.
pub const PLACEMENT_MANAGER: &str = "placement_manager";

const ID_DOMAIN: &[u8] = b"mayfly-placement-manager-v1\n";

/// The `placements` phase code of a placement clones may be made from.
const MATERIALISED: &str = "materialised";

/// How long each placement directive may take once sent (P17: "pull deadline (large)").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacementTimeouts {
    /// `PullImage`: an image of tens of GiB, fetched and re-hashed.
    pub pull: Millis,
    /// `MaterialiseImage`: a clone, one boot and the runner's installation.
    pub materialise: Millis,
    /// `EvictImage`.
    pub evict: Millis,
}

impl Default for PlacementTimeouts {
    /// Two hours for a pull, one for a materialisation, five minutes for an eviction.
    fn default() -> Self {
        Self {
            pull: Millis::new(7_200_000),
            materialise: Millis::new(3_600_000),
            evict: Millis::new(300_000),
        }
    }
}

impl PlacementTimeouts {
    /// The time a directive of `kind` is given.
    #[must_use]
    pub const fn of(&self, kind: DueDirective) -> Millis {
        match kind {
            DueDirective::Pull => self.pull,
            DueDirective::Materialise => self.materialise,
            DueDirective::Evict => self.evict,
        }
    }

    /// The time `directive` is given.
    #[must_use]
    pub const fn timeout(&self, directive: PlacementDirective) -> Millis {
        match directive {
            PlacementDirective::PullImage => self.pull,
            PlacementDirective::MaterialiseImage { .. } => self.materialise,
            PlacementDirective::EvictImage { .. } => self.evict,
        }
    }
}

/// The manager's configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacementConfig {
    /// The run epoch its commands are issued under.
    pub run_epoch: Epoch,
    /// Each directive kind's time once sent.
    pub timeouts: PlacementTimeouts,
    /// Past a deadline, the time an answer sent at the deadline may still take to arrive.
    pub reply_grace: Millis,
    /// The most time between two passes without a kick.
    pub interval: Millis,
}

impl PlacementConfig {
    /// The default timeouts, five seconds of grace and a one-minute interval under `run_epoch`.
    #[must_use]
    pub fn new(run_epoch: Epoch) -> Self {
        Self {
            run_epoch,
            timeouts: PlacementTimeouts::default(),
            reply_grace: Millis::new(5_000),
            interval: Millis::new(60_000),
        }
    }
}

/// A placement directive pending, as the inbox needs it to route an answer that names only the
/// directive (a refusal, an unknown outcome, an eviction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPlacement {
    /// The placement stream's instance (`<host>-<digest12>`).
    pub instance: String,
    /// The host.
    pub host_id: HostId,
    /// The image.
    pub digest: ImageDigest,
}

/// The placement directives pending, by directive id: kept by the manager from the
/// `placements` projection on every pass and on every issue, read by the observation inbox.
/// Memory only: a restart's first pass fills it again before any answer to a directive of the
/// previous run can be routed (an answer that finds no entry is not routed, and the directive's
/// timeout makes its outcome `Unknown`, never a success).
#[derive(Debug, Clone, Default)]
pub struct PlacementIndex(Arc<Mutex<BTreeMap<DirectiveId, PendingPlacement>>>);

impl PlacementIndex {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<DirectiveId, PendingPlacement>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Notes that `directive_id` is pending on `pending`'s placement.
    pub fn insert(&self, directive_id: DirectiveId, pending: PendingPlacement) {
        self.lock().insert(directive_id, pending);
    }

    /// The placement `directive_id` is pending on, if known.
    #[must_use]
    pub fn get(&self, directive_id: DirectiveId) -> Option<PendingPlacement> {
        self.lock().get(&directive_id).cloned()
    }

    /// Forgets `directive_id` once it is answered.
    pub fn remove(&self, directive_id: DirectiveId) {
        self.lock().remove(&directive_id);
    }

    /// Every directive pending on `host`.
    #[must_use]
    pub fn on_host(&self, host: &HostId) -> Vec<(DirectiveId, PendingPlacement)> {
        self.lock()
            .iter()
            .filter(|(_, p)| p.host_id == *host)
            .map(|(id, p)| (*id, p.clone()))
            .collect()
    }

    fn replace(&self, entries: BTreeMap<DirectiveId, PendingPlacement>) {
        *self.lock() = entries;
    }
}

/// Why a pass did not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementError {
    /// The read side could not be read.
    Query(QueryError),
}

impl std::fmt::Display for PlacementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Query(e) => write!(f, "placement pass not run: {e}"),
        }
    }
}

impl std::error::Error for PlacementError {}

/// What one pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementReport {
    /// The plan.
    pub actions: Vec<PlacementAction>,
    /// Commands accepted (also from a receipt).
    pub accepted: Vec<PlacementAction>,
    /// Commands refused, with the refusal's code.
    pub refused: Vec<(PlacementAction, &'static str)>,
    /// Commands not decided now: the next pass plans again.
    pub failed: Vec<(PlacementAction, &'static str)>,
    /// The materialised bases the pass read, by host and image: what clones are made from.
    pub bases: BTreeMap<(HostId, ImageDigest), String>,
    /// When a sent directive's deadline plus the reply grace next falls due, if one is pending.
    pub next_due: Option<Millis>,
}

/// The plan's inputs from the read side (see [`plan`]). Pure. Pinned by
/// `tests/store/placement.rs::should_place_the_pin_and_admit_the_pool_when_the_base_is_materialised`.
#[must_use]
pub fn inputs_of(view: &FleetView, now: Millis, reply_grace: Millis) -> PolicyInputs {
    let hosts = view
        .hosts
        .keys()
        .filter_map(|key| key.strip_prefix("host-")?.parse().ok())
        .collect();
    let pins = view
        .pools
        .iter()
        .filter_map(|(key, row)| {
            let pool: PoolName = key.strip_prefix("pool-")?.parse().ok()?;
            let digest = row.image_pin.clone()?;
            let tripped = row.tripped.iter().any(|t| t.digest == digest);
            Some(PinView {
                pool,
                digest,
                tripped,
            })
        })
        .collect();
    let images = view
        .images
        .values()
        .filter_map(|row| {
            Some(ImageView {
                digest: row.digest.clone()?,
                runner_version: row.runner_version,
                expired_at: row.expired_at,
            })
        })
        .collect();
    let placements = view
        .placements
        .values()
        .filter_map(|row| {
            Some(PlacementView {
                host_id: row.host_id.clone(),
                digest: row.digest.clone(),
                phase: phase_of(&row.phase)?,
                wanted: row.wanted,
                base: row.base,
                pending: row.pending.map(|p| Pending {
                    directive_id: p.directive_id,
                    directive: p.directive,
                    deadline: p.deadline,
                    sent: p.sent,
                    // The pass plans from phase, deadline and outcome; only the inbox reads the
                    // send's epoch, from the placement stream itself.
                    sent_epoch: None,
                    outcome: p.unknown_since.map_or(PendingOutcome::InFlight, |since| {
                        PendingOutcome::Unknown { since }
                    }),
                }),
            })
        })
        .collect();
    let mut references: BTreeMap<(HostId, ImageDigest), u32> = BTreeMap::new();
    for (key, slots) in &view.leases {
        let Some(host) = key
            .strip_prefix("host-")
            .and_then(|h| h.parse::<HostId>().ok())
        else {
            continue;
        };
        for slot in slots {
            if let Some(attempt) = view.attempts.get(&slot.attempt_id) {
                let count = references
                    .entry((host.clone(), attempt.image_digest.clone()))
                    .or_default();
                *count = count.saturating_add(1);
                continue;
            }
            // a lease with no attempt row (not started yet, or the row not read) runs no known
            // image yet: it counts as referencing every base on its host, so nothing there is
            // evicted under it (fail closed, `formal/scheduling.qnt`'s `unstartedLease`)
            for row in view.placements.values().filter(|p| p.host_id == host) {
                let count = references
                    .entry((host.clone(), row.digest.clone()))
                    .or_default();
                *count = count.saturating_add(1);
            }
        }
    }
    PolicyInputs {
        now,
        reply_grace,
        hosts,
        pins,
        images,
        placements,
        references,
    }
}

/// The materialised bases of `view`, by host and image: the base clones of that image are made
/// from on that host (ADR 0039). Pinned by
/// `tests/store/placement.rs::should_place_the_pin_and_admit_the_pool_when_the_base_is_materialised`.
#[must_use]
pub fn bases_of(view: &FleetView) -> BTreeMap<(HostId, ImageDigest), String> {
    view.placements
        .values()
        .filter(|row| row.phase == MATERIALISED)
        .filter_map(|row| {
            Some((
                (row.host_id.clone(), row.digest.clone()),
                row.base_name.clone()?,
            ))
        })
        .collect()
}

/// The directives `view` shows pending, by directive id.
fn pending_of(view: &FleetView) -> BTreeMap<DirectiveId, PendingPlacement> {
    view.placements
        .values()
        .filter_map(|row| {
            let pending = row.pending?;
            Some((
                pending.directive_id,
                PendingPlacement {
                    instance: placement_instance(&row.host_id, &row.digest),
                    host_id: row.host_id.clone(),
                    digest: row.digest.clone(),
                },
            ))
        })
        .collect()
}

/// When the earliest sent, unanswered directive's deadline plus `grace` falls due.
fn next_due_of(view: &FleetView, grace: Millis) -> Option<Millis> {
    view.placements
        .values()
        .filter_map(|row| row.pending)
        .filter(|p| p.sent && p.unknown_since.is_none())
        .map(|p| Millis::new(p.deadline.instant().get().saturating_add(grace.get())))
        .min()
}

/// The image placement manager over an [`AggregateStore`] and the [`ProjectionStore`] it reads.
pub struct PlacementManager<'s, J, C, R, N, P> {
    store: &'s AggregateStore<J, C, R, N>,
    projections: &'s P,
    config: PlacementConfig,
    kicks: Kicks,
    index: PlacementIndex,
    timing: Mutex<Timing>,
}

#[derive(Debug, Default)]
struct Timing {
    last_pass: Option<Millis>,
    deadline: Option<Millis>,
    safe_mode: bool,
}

impl<J, C, R, N, P> std::fmt::Debug for PlacementManager<'_, J, C, R, N, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlacementManager")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl<'s, J: Journal, C: Clock, R: Rng, N: StoreNotifier, P: ProjectionStore>
    PlacementManager<'s, J, C, R, N, P>
{
    /// A manager over `store`, reading `projections`, woken by `kicks`, keeping `index` for the
    /// inbox.
    pub fn new(
        store: &'s AggregateStore<J, C, R, N>,
        projections: &'s P,
        config: PlacementConfig,
        kicks: Kicks,
        index: PlacementIndex,
    ) -> Self {
        Self {
            store,
            projections,
            config,
            kicks,
            index,
            timing: Mutex::new(Timing::default()),
        }
    }

    fn timing(&self) -> MutexGuard<'_, Timing> {
        self.timing.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Enters or leaves safe mode (I29): a pass then records only timeouts (an observed fact: the
    /// deadline passed unanswered) and decides nothing new: no request, no directive, no
    /// eviction and no breaker trip, as the reconciler plans nothing but the scale sets' safe
    /// mode. Nothing is lost by waiting: admission is closed in safe mode, and the next normal
    /// pass plans from the state as it then is. Pinned by
    /// `tests/store/placement.rs::should_record_only_timeouts_when_the_manager_is_in_safe_mode`.
    pub fn set_safe_mode(&self, safe_mode: bool) {
        self.timing().safe_mode = safe_mode;
    }

    /// When the timer next makes a pass due: at once before the first pass, else one interval
    /// after the last, or earlier when a sent directive's deadline plus the reply grace falls
    /// due before that (its timeout is recorded by that pass).
    #[must_use]
    pub fn next_due(&self) -> Millis {
        let timing = self.timing();
        let interval = timing.last_pass.map_or(Millis::new(0), |last| {
            Millis::new(last.get().saturating_add(self.config.interval.get()))
        });
        timing
            .deadline
            .map_or(interval, |deadline| interval.min(deadline))
    }

    /// Runs a pass when one is due: a pool, image, placement or host stream was kicked since the
    /// last look (a host stream moves with its leases, which reference bases), or the interval
    /// passed. `None` when nothing was due.
    ///
    /// # Errors
    /// [`PlacementError`] when the read side cannot be read.
    pub async fn tick(&self) -> Result<Option<PlacementReport>, PlacementError> {
        let kicked = self.kicks.take().iter().any(|stream| {
            let s = stream.as_str();
            s.starts_with("pool-")
                || s.starts_with("image-")
                || s.starts_with("placement-")
                || s.starts_with("host-")
        });
        if !kicked && self.store.clock().now() < self.next_due() {
            return Ok(None);
        }
        self.run_pass().await.map(Some)
    }

    /// Reads, plans and issues one pass now.
    ///
    /// # Errors
    /// [`PlacementError`] when the read side cannot be read; nothing is issued then.
    pub async fn run_pass(&self) -> Result<PlacementReport, PlacementError> {
        let now = self.store.clock().now();
        let view = queries::fleet(self.projections)
            .await
            .map_err(PlacementError::Query)?;
        self.index.replace(pending_of(&view));
        let safe_mode = self.timing().safe_mode;
        let actions: Vec<PlacementAction> = plan(&inputs_of(&view, now, self.config.reply_grace))
            .into_iter()
            .filter(|action| !safe_mode || decided_in_safe_mode(action))
            .collect();
        let mut report = PlacementReport {
            actions: actions.clone(),
            accepted: Vec::new(),
            refused: Vec::new(),
            failed: Vec::new(),
            bases: bases_of(&view),
            next_due: next_due_of(&view, self.config.reply_grace),
        };
        for action in actions {
            match self.issue(&action, &view, now).await {
                Answer::Accepted => report.accepted.push(action),
                Answer::Refused(code) => report.refused.push((action, code)),
                Answer::Failed(code) => report.failed.push((action, code)),
            }
        }
        let mut timing = self.timing();
        timing.last_pass = Some(now);
        timing.deadline = report.next_due;
        drop(timing);
        Ok(report)
    }

    async fn issue(&self, action: &PlacementAction, view: &FleetView, now: Millis) -> Answer {
        let revision = |host: &HostId, digest: &ImageDigest| {
            let instance = placement_instance(host, digest);
            view.placements
                .get(&format!("placement-{instance}"))
                .map_or(0, |row| row.revision)
        };
        match action {
            PlacementAction::Request {
                host_id,
                digest,
                runner_version,
            } => {
                let instance = placement_instance(host_id, digest);
                let id = derive(
                    b"request\n",
                    &[
                        instance.as_bytes(),
                        &revision(host_id, digest).to_be_bytes(),
                    ],
                );
                let body = PlacementCommand::RequestPlacement {
                    host_id: host_id.clone(),
                    digest: digest.clone(),
                    runner_version: *runner_version,
                };
                self.save::<ImagePlacement>(&instance, id, body).await
            }
            PlacementAction::Issue {
                host_id,
                digest,
                kind,
            } => {
                let instance = placement_instance(host_id, digest);
                let id = derive(
                    b"issue\n",
                    &[
                        instance.as_bytes(),
                        &revision(host_id, digest).to_be_bytes(),
                    ],
                );
                let directive_id = directive_of(id);
                let body = PlacementCommand::IssueDirective {
                    directive_id,
                    deadline: Deadline::after(now, self.config.timeouts.of(*kind)),
                };
                let answer = self.save::<ImagePlacement>(&instance, id, body).await;
                if matches!(answer, Answer::Accepted) {
                    self.index.insert(
                        directive_id,
                        PendingPlacement {
                            instance,
                            host_id: host_id.clone(),
                            digest: digest.clone(),
                        },
                    );
                }
                answer
            }
            PlacementAction::Timeout {
                host_id,
                digest,
                directive_id,
            } => {
                let instance = placement_instance(host_id, digest);
                let id = derive(b"timeout\n", &[&directive_id.as_u128().to_be_bytes()]);
                let body = PlacementCommand::RecordOutcomeUnknown {
                    directive_id: *directive_id,
                };
                self.save::<ImagePlacement>(&instance, id, body).await
            }
            PlacementAction::Evict { host_id, digest } => {
                let instance = placement_instance(host_id, digest);
                let id = derive(
                    b"evict\n",
                    &[
                        instance.as_bytes(),
                        &revision(host_id, digest).to_be_bytes(),
                    ],
                );
                let body = PlacementCommand::RequestEviction {
                    scope: EvictionScope::Placement,
                    references: 0,
                };
                self.evict(&instance, id, body, host_id, view).await
            }
            PlacementAction::Trip { pool, digest, at } => {
                let id = derive(
                    b"trip\n",
                    &[
                        pool.as_str().as_bytes(),
                        digest.as_str().as_bytes(),
                        &at.get().to_be_bytes(),
                    ],
                );
                let body = PoolCommand::TripBreaker {
                    digest: digest.clone(),
                    reason: TripReason::RunnerVersion,
                    at: *at,
                };
                self.save::<Pool>(pool.as_str(), id, body).await
            }
        }
    }

    /// `RequestEviction` saved once with a read of the host stream and of every pool stream at
    /// the revisions the pass read them at: the reference count (the host's leases, through
    /// their attempts) and the pins it was decided from must be unchanged at the commit, or the
    /// unit is refused (`conflict`) and the next pass reads again. A lease reserved, released or
    /// quarantined meanwhile moves the host stream; a pin moved meanwhile moves its pool stream;
    /// an attempt started meanwhile was a lease the pass counted as referencing every base
    /// (`inputs_of`); a start that read the placement before this commit is refused by its own
    /// read of the placement stream (`crate::start`). Never retried here: a retry would re-check
    /// the same revisions. Pinned by
    /// `tests/store/placement.rs::should_refuse_the_eviction_when_a_lease_is_reserved_between_the_read_and_the_save`.
    async fn evict(
        &self,
        instance: &str,
        id: CommandId,
        body: PlacementCommand,
        host: &HostId,
        view: &FleetView,
    ) -> Answer {
        let mut request = SaveRequest::new(instance, self.command(id, body));
        let host_key = format!("host-{host}");
        let streams = view
            .hosts
            .get_key_value(&host_key)
            .map(|(key, row)| (key, row.revision))
            .into_iter()
            .chain(view.pools.iter().map(|(key, row)| (key, row.revision)));
        for (key, revision) in streams {
            let Ok(stream) = StreamId::new(key) else {
                return Answer::Failed("save_failed");
            };
            request = request.with_read(
                stream,
                ExpectedRevision::Exactly(StreamRevision::new(revision)),
            );
        }
        answer_of(self.store.save::<ImagePlacement>(request).await)
    }

    fn command<B>(&self, id: CommandId, body: B) -> Command<B> {
        Command::new(
            id,
            self.store.clock().now(),
            self.config.run_epoch,
            Actor::Automation {
                policy: PLACEMENT_MANAGER,
            },
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
}

/// Whether `action` is taken in safe mode: only a timeout, which records that a deadline passed
/// unanswered (the outcome becomes `Unknown`, never a success).
const fn decided_in_safe_mode(action: &PlacementAction) -> bool {
    match action {
        PlacementAction::Timeout { .. } => true,
        PlacementAction::Request { .. }
        | PlacementAction::Issue { .. }
        | PlacementAction::Evict { .. }
        | PlacementAction::Trip { .. } => false,
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

/// The id of the directive an `IssueDirective` with `command_id` issues: what the relay sends
/// and the data plane dedupes by, so every resend of it carries this one id.
#[must_use]
pub fn directive_of(command_id: CommandId) -> DirectiveId {
    DirectiveId::from_u128(derive(b"directive\n", &[&command_id.as_u128().to_be_bytes()]).as_u128())
}

/// The instance of `digest`'s `Image` stream: its 64 hex digits (`image-<hex>`,
/// `doc/domain-model.md` §6.6), since a stream id is `[a-z0-9-]` and holds no `sha256:`.
#[must_use]
pub fn image_instance(digest: &ImageDigest) -> &str {
    digest
        .as_str()
        .strip_prefix("sha256:")
        .unwrap_or(digest.as_str())
}

/// Why [`withdraw_image`] withdrew nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WithdrawError {
    /// A pool pins the image (P17): unpin it first.
    Pinned(PoolName),
    /// The `Image` aggregate refused, by its code.
    Refused(&'static str),
    /// The read side could not be read.
    Query(QueryError),
    /// The store gave no outcome.
    Save(SaveError),
}

impl std::fmt::Display for WithdrawError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pinned(pool) => write!(f, "image pinned by pool {pool}"),
            Self::Refused(code) => write!(f, "withdrawal refused: {code}"),
            Self::Query(e) => write!(f, "withdrawal not decided: {e}"),
            Self::Save(e) => write!(f, "withdrawal not decided: {e}"),
        }
    }
}

impl std::error::Error for WithdrawError {}

/// `WithdrawImage` of `digest` by `actor` under `command_id`, refused while a pool pins it
/// (P17). The aggregate refuses any actor but an operator, and only the operator socket builds
/// one (`ARCH-operator-actor`, RT-03), so this use case takes the actor it is given. On every try the pools are listed from the `pools` projection, each pool's stream
/// is read for its pin, and the withdrawal is saved with a read of every one at that revision,
/// so a pin committed after the check refuses the unit (`conflict`), and the retry lists and
/// reads again, then refuses the withdrawal as pinned. Pinned by
/// `tests/store/placement.rs::should_refuse_a_withdrawal_when_a_pool_pins_the_image` and
/// `tests/store/placement.rs::should_refuse_a_withdrawal_when_a_pin_commits_between_the_check_and_the_save`.
///
/// What a read condition here cannot cover: a pool stream that did not exist at the last
/// listing and is created and pinned before the save (there is no global order to read it
/// under, ES-I3). The pin narrows it: [`crate::images::pin_image`] reads the image stream,
/// refuses a withdrawn image and saves with that read, so a withdrawal that commits first
/// refuses such a pin. A pin of such a pool that commits first, before this withdrawal's save,
/// still leaves a withdrawn image pinned, since neither unit reads a stream the other writes
/// (the limitation recorded on OPS-02's card).
///
/// # Errors
/// [`WithdrawError`].
pub async fn withdraw_image<J, C, R, N, P>(
    store: &AggregateStore<J, C, R, N>,
    projections: &P,
    digest: &ImageDigest,
    actor: Actor,
    command_id: CommandId,
    run_epoch: Epoch,
) -> Result<(), WithdrawError>
where
    J: Journal,
    C: Clock,
    R: Rng,
    N: StoreNotifier,
    P: ProjectionStore,
{
    let decided: Result<Outcome<ImageRejection>, WithdrawError> = store
        .with_revision_retry(|| async move {
            // the pools are listed afresh on every try, so a pool a conflict revealed is read
            let pools: Vec<String> = match queries::all_rows::<views::Pools, P>(projections).await {
                Ok(rows) => rows
                    .into_iter()
                    .filter_map(|(key, _)| key.strip_prefix("pool-").map(str::to_owned))
                    .collect(),
                Err(error) => return Ok(Err(WithdrawError::Query(error))),
            };
            let mut pins = Vec::with_capacity(pools.len());
            let mut reads = Vec::with_capacity(pools.len());
            for instance in &pools {
                let (state, head) = store.load_with_head::<Pool>(instance).await?;
                if let (Some(pin), Ok(pool)) = (state.image_pin, instance.parse::<PoolName>()) {
                    pins.push(PinView {
                        pool,
                        digest: pin,
                        tripped: false,
                    });
                }
                let stream = stream_of::<Pool>(instance).map_err(SaveError::InvalidName)?;
                reads.push((stream, ExpectedRevision::of(head)));
            }
            if let Some(pool) = pinned_by(&pins, digest) {
                return Ok(Err(WithdrawError::Pinned(pool.clone())));
            }
            let command = Command::new(
                command_id,
                store.clock().now(),
                run_epoch,
                actor,
                ImageCommand::WithdrawImage,
            );
            let mut request = SaveRequest::new(image_instance(digest), command);
            for (stream, expected) in reads {
                request = request.with_read(stream, expected);
            }
            store.save::<Image>(request).await.map(Ok)
        })
        .await
        .map_err(WithdrawError::Save)?;
    match decided {
        Err(error) => Err(error),
        Ok(Outcome::Accepted { .. }) => Ok(()),
        Ok(Outcome::Refused { rejection, .. }) => Err(WithdrawError::Refused(
            <ImageRejection as Rejection>::code(&rejection),
        )),
    }
}
