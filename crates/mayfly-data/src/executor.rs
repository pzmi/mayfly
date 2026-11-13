//! The data plane's executor (plan §3.1, §6, §8.1, §9; ADRs 0024, 0026, 0042, 0052, 0053).
//!
//! It drives the pure [`DataPlane`] of `mayfly-data-domain` against the ports: it rebuilds the
//! resource ledger from observation on start ([`Executor::start`]), enters a session through the
//! handshake and the epoch floor ([`Executor::enter`]), then serves the session
//! ([`Executor::serve`]): each directive is fenced, deduplicated and admitted by the domain,
//! re-observed against the provider's [`Inventory`] first when it is a `CloneVm` or `StartVm`
//! (by name, so a re-delivery under a new epoch is a re-observation, ADR 0042) or a `StopVm` or
//! `DeleteVm` (by proof, ADR 0026, ADR 0053), and executed; each outcome goes back to the
//! domain, which answers it and keeps the ledger. The executor decides nothing the domain
//! decides; it schedules:
//!
//! - **Bounded concurrency.** At most [`Config::concurrency`] directives run at once; the rest
//!   wait in arrival order. Directives on one target (a VM, a base, an image) run one at a time.
//!   Nothing is spawned: every in-flight future is polled on the executor's own task, also while
//!   an observation waits for room in the sink.
//! - **Deadlines** ([`admission_now`], [`local_due`]: the one place deadlines are read, in the
//!   control plane's clock as the handshake's offset gives it, ADR 0020). A
//!   directive whose deadline passed before a stage could start is refused `deadline_expired`.
//!   An effect still running at its deadline is answered `Unknown{deadline}` then, never success
//!   (I10), but it is not dropped: its target stays busy, it counts against the bound and taints
//!   every listing until its future completes, which the adapter promises only once nothing it
//!   started still runs (its own per-call deadline, DP-04). A read (a re-observation, a listing)
//!   is dropped at its deadline: it changes nothing.
//! - **Listings against effects** ([`crate::gate`]). The observer lists the provider, the image
//!   store and the host probe every [`Config::observe_every`] of the monotonic clock and for
//!   each `ReportInventory`, only while no effect that changes a listing runs; while a pass is
//!   due or running, no new such effect starts, so effects cannot starve listings. A listing
//!   during which one ran anyway (the effect stage of a directive already re-observed) is
//!   discarded, never applied and never reported.
//! - **Capacity.** `CapacityReported` is sent when a session opens, whenever the ledger's report
//!   changes, and as the answer to `ReportCapacity` (ADR 0024).
//! - **The image listing.** A clean pass's inventory carries the image store's listing beside it
//!   (its verified images and `Ready` bases, protocol 1.5), none when the store could not be
//!   listed, so the control plane can resolve an unknown placement from it (DP-08, `beside`).
//! - **Session end.** When [`Executor::serve`] returns, the domain's session is ended (its
//!   reservations held); the next session needs [`Executor::enter`].

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::{Future, poll_fn};
use std::pin::pin;
use std::task::Poll;

use mayfly_data_domain::{
    DataPlane, Inventory, Ledger, Link, LinkIgnored, LinkOutcome, LinkStimulus, Progress, Receipt,
    Reply, Sizing, Stage, Stimulus,
};
use mayfly_domain::{Deadline, DirectiveId, Epoch, HostId, ImageDigest, Millis, OwnedVmName};
use mayfly_protocol::{
    BaseName, CapacityReport, Directive, DirectiveKind, DirectiveRef, DirectiveSource,
    HandshakeInitiator, Hello, ImageStoreListing, InventoryGap, LinkError, ListedBase, Observation,
    ObservationKind, ObservationSink, ProtocolVersion, ResourceDimension, SUPPORTED, UnknownReason,
};

use crate::gate::{ListingGate, Ticket};
use crate::inflight::InFlight;
use crate::ports::{
    Diagnostics, FloorFailed, FloorStore, GuestAnswer, GuestTransport, HostProbe, ImageListing,
    ImageStore, LaunchAnswer, ObservationIds, ProbeFailed, ProcessClock, VmError, VmProvider,
};

/// What the operator configured for this host's data plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The host it runs on.
    pub host_id: HostId,
    /// The footprints of what it creates (ADR 0052: set by the operator, not a default).
    pub sizing: Sizing,
    /// How many directives run at once (at least one).
    pub concurrency: usize,
    /// How often, on the monotonic clock, the observer lists the provider, the image store and
    /// the host.
    pub observe_every: Millis,
    /// How long one observer pass may take before it is incomplete (`deadline`).
    pub listing_timeout: Millis,
    /// This process's boot id, for the hello.
    pub boot_id: String,
    /// This build's version, for the hello.
    pub dp_version: String,
}

/// The ports the executor drives.
#[derive(Debug, Clone)]
pub struct Ports<V, I, G, H, C, D> {
    /// The VM provider.
    pub provider: V,
    /// The image store.
    pub images: I,
    /// The guest transport.
    pub guest: G,
    /// The host probe.
    pub host: H,
    /// The clocks.
    pub clock: C,
    /// The observation ids.
    pub ids: D,
}

/// What the executor dropped or could not apply, for status and diagnostics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Directive frames discarded unanswered (a session not entered, another version).
    pub discarded_frames: u64,
    /// Listings discarded because an effect overlapped them.
    pub discarded_listings: u64,
    /// Outcomes the domain refused as not fitting the directive's phase (a fault here).
    pub misfits: u64,
}

/// Why the executor could not start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartError {
    /// The host could not be probed: without its totals nothing can be admitted.
    Probe(ProbeFailed),
}

/// Why no session was entered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnterError {
    /// The handshake failed or was refused.
    Link(LinkError),
    /// The welcome was not entered (at or below the floor, or a version not spoken).
    Ignored(LinkIgnored),
    /// The floor could not be made durable; no frame of the session runs.
    Floor(FloorFailed),
}

/// Why [`Executor::serve`] returned. Every effect in flight was finished first; directives that
/// had not started were left, and the domain's session ended, so their reservations are held
/// until a complete listing settles them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEnd {
    /// No session is open: call [`Executor::enter`] first.
    NotOpen,
    /// The directive source ended.
    SourceClosed(LinkError),
    /// An observation could not be sent.
    SinkFailed(LinkError),
}

type Key = (Epoch, DirectiveId);

/// What a directive works on: directives on one target run one at a time.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Target {
    Vm(OwnedVmName),
    Base(BaseName),
    Image(ImageDigest),
}

fn target(kind: &DirectiveKind) -> Option<Target> {
    match kind {
        DirectiveKind::PullImage { digest } => Some(Target::Image(digest.clone())),
        DirectiveKind::MaterialiseImage { base_name, .. }
        | DirectiveKind::EvictImage { base_name } => Some(Target::Base(base_name.clone())),
        DirectiveKind::CloneVm { .. }
        | DirectiveKind::StartVm { .. }
        | DirectiveKind::VerifyGuest { .. }
        | DirectiveKind::LaunchRunner { .. }
        | DirectiveKind::ExportDiagnostics { .. }
        | DirectiveKind::StopVm { .. }
        | DirectiveKind::DeleteVm { .. }
        | DirectiveKind::ReportInventory
        | DirectiveKind::ReportCapacity
        | DirectiveKind::ReportHostFacts => kind.vm_name().cloned().map(Target::Vm),
    }
}

/// Whether the effect changes what a VM or image listing shows ([`crate::gate`]).
const fn changes_listing(kind: &DirectiveKind) -> bool {
    match kind {
        DirectiveKind::PullImage { .. }
        | DirectiveKind::MaterialiseImage { .. }
        | DirectiveKind::EvictImage { .. }
        | DirectiveKind::CloneVm { .. }
        | DirectiveKind::StartVm { .. }
        | DirectiveKind::StopVm { .. }
        | DirectiveKind::DeleteVm { .. } => true,
        DirectiveKind::VerifyGuest { .. }
        | DirectiveKind::LaunchRunner { .. }
        | DirectiveKind::ExportDiagnostics { .. }
        | DirectiveKind::ReportInventory
        | DirectiveKind::ReportCapacity
        | DirectiveKind::ReportHostFacts => false,
    }
}

/// How a session reads the control plane's clock, which its directive deadlines are written in
/// (ADR 0020, amendment accepted by the owner 2026-11-10: the clock offset at the handshake).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeadlineClock {
    /// The control plane's clock read `cp` when this data plane's monotonic clock read
    /// `monotonic`, both taken when the `Welcome` arrived: the control plane's clock now is `cp`
    /// plus the monotonic time since, so no step of either wall clock after the handshake moves
    /// a deadline. A reconnect takes it again.
    Offset {
        /// The `Welcome`'s `cp_clock`.
        cp: Millis,
        /// This data plane's monotonic reading at its receipt.
        monotonic: Millis,
    },
    /// The `Welcome` carried no clock (a control plane older than protocol 1.2): the host's wall
    /// clock, read when the directive is received. A wall-clock step between the control plane
    /// writing a deadline and its receipt then lengthens or shortens that directive's time, and
    /// it holds only while both planes read one host's clock.
    WallAtReceipt,
}

impl DeadlineClock {
    /// The clock for a session opened by a welcome carrying `cp_clock`, received when the
    /// monotonic clock read `monotonic`.
    fn of(cp_clock: Option<Millis>, monotonic: Millis) -> Self {
        cp_clock.map_or(Self::WallAtReceipt, |cp| Self::Offset { cp, monotonic })
    }
}

/// The control plane's clock now, as this session reads it ([`DeadlineClock`]): the reading a
/// directive's deadline is compared with at admission. With [`local_due`], the one place
/// deadlines are read. Pinned by
/// `tests/executor/session.rs::should_decide_no_deadline_differently_when_the_wall_clock_steps_after_the_handshake`,
/// `should_compare_deadlines_in_the_control_planes_clock_when_the_welcome_carries_it` and
/// `should_compare_deadlines_by_the_wall_clock_at_receipt_when_an_older_control_plane_sends_no_clock`.
fn admission_now<C: ProcessClock>(clock: &C, deadlines: DeadlineClock) -> Millis {
    match deadlines {
        DeadlineClock::Offset { cp, monotonic } => after(
            cp,
            Millis::new(clock.monotonic().get().saturating_sub(monotonic.get())),
        ),
        DeadlineClock::WallAtReceipt => clock.wall(),
    }
}

/// `deadline` as an instant of the monotonic clock, fixed when the directive is received: every
/// later wait and comparison is monotonic, so a step of the wall clock moves no timer after it.
fn local_due<C: ProcessClock>(clock: &C, deadlines: DeadlineClock, deadline: Deadline) -> Millis {
    after(
        clock.monotonic(),
        deadline.remaining(admission_now(clock, deadlines)),
    )
}

/// One observer pass: the probe, the inventory and the image listing.
struct Pass {
    ticket: Ticket,
    inventory: Inventory,
    images: Option<ImageListing>,
    probe: Option<Result<mayfly_data_domain::HostProbe, ProbeFailed>>,
}

/// What an in-flight future finished with.
enum Work {
    ReObserved {
        key: Key,
        listing: Inventory,
    },
    Effect {
        key: Key,
        stimulus: Stimulus,
        gated: bool,
    },
    Observed(Pass),
    /// A `PullImage` whose image's disk was measured (or not) before its admission.
    Measured {
        version: ProtocolVersion,
        directive: Directive,
        disk_bytes: Option<u64>,
    },
}

/// An admitted directive waiting to run a stage.
struct Queued {
    key: Key,
    stage: Stage,
    due: Millis,
    kind: DirectiveKind,
}

/// A `ReportInventory` waiting for a clean listing.
struct Waiter {
    key: Key,
    due: Millis,
}

/// An effect in flight: when it is due, and whether it was answered `Unknown{deadline}`.
struct Running {
    due: Millis,
    overdue: bool,
}

/// An observation to send: the loop adds its id and clock reading.
struct Out {
    epoch: Epoch,
    answers: Option<DirectiveRef>,
    kind: ObservationKind,
}

impl Out {
    fn reply(reply: Reply) -> Self {
        Self {
            epoch: reply.epoch,
            answers: Some(reply.answers),
            kind: reply.kind,
        }
    }
}

/// The state of one [`Executor::serve`].
struct Run<'a> {
    epoch: Epoch,
    in_flight: InFlight<'a, Work>,
    done: Vec<Work>,
    queue: VecDeque<Queued>,
    busy: BTreeSet<Target>,
    holding: BTreeMap<Key, Target>,
    running: BTreeMap<Key, Running>,
    jobs: usize,
    passing: bool,
    observe_due: bool,
    next_tick: Millis,
    waiters: Vec<Waiter>,
    outbox: Vec<Out>,
    closing: bool,
}

impl Run<'_> {
    fn unsolicited(&mut self, kind: ObservationKind) {
        self.outbox.push(Out {
            epoch: self.epoch,
            answers: None,
            kind,
        });
    }

    /// When the loop must wake without any other event, on the monotonic clock: the next
    /// observer pass, the first `ReportInventory` due, or the first effect due.
    fn wake_at(&self) -> Millis {
        let effects = self.running.values().filter(|r| !r.overdue).map(|r| r.due);
        self.waiters
            .iter()
            .map(|w| w.due)
            .chain(effects)
            .fold(self.next_tick, Millis::min)
    }

    fn release(&mut self, key: Key) {
        if let Some(target) = self.holding.remove(&key) {
            self.busy.remove(&target);
        }
    }
}

fn after(now: Millis, by: Millis) -> Millis {
    Millis::new(now.get().saturating_add(by.get()))
}

/// Runs the read `work` until the monotonic instant `due`: `None` when that came first (`work`
/// is dropped; only reads are bounded this way, never an effect).
async fn bounded<C: ProcessClock, F: Future>(clock: &C, due: Millis, work: F) -> Option<F::Output> {
    let mut work = pin!(work);
    let mut timer = pin!(clock.sleep_until(due));
    poll_fn(|cx| {
        if let Poll::Ready(out) = work.as_mut().poll(cx) {
            return Poll::Ready(Some(out));
        }
        if timer.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        Poll::Pending
    })
    .await
}

fn outcome<T>(result: Result<T, VmError>, applied: impl FnOnce(T) -> ObservationKind) -> Stimulus {
    match result {
        Ok(value) => Stimulus::Applied(applied(value)),
        Err(VmError::Definite | VmError::HypervisorLimit) => Stimulus::Failed,
        Err(VmError::Ambiguous(cause)) => Stimulus::Ambiguous(cause.reason()),
    }
}

/// The data plane's executor over its ports.
pub struct Executor<V, I, G, H, C, D> {
    ports: Ports<V, I, G, H, C, D>,
    core: Core,
}

/// Everything but the ports, so in-flight futures can borrow the ports while the loop changes
/// the rest.
struct Core {
    config: Config,
    plane: DataPlane,
    gate: ListingGate,
    stats: Stats,
    last_report: Option<CapacityReport>,
    deadlines: DeadlineClock,
}

impl<V, I, G, H, C, D> Executor<V, I, G, H, C, D>
where
    V: VmProvider,
    I: ImageStore,
    G: GuestTransport,
    H: HostProbe,
    C: ProcessClock,
    D: ObservationIds,
{
    /// Starts with nothing but observation (plan §3.1): probes the host, lists the provider and
    /// the image store, and builds the ledger from what they say, under the persisted `floor`.
    /// An incomplete listing leaves our VMs unknown, so every dimension is unknown and nothing
    /// is admitted until a complete one (ADR 0052). Pinned by
    /// `tests/executor/main.rs::should_rebuild_the_ledger_from_observation_when_the_executor_restarts`.
    ///
    /// # Errors
    /// [`StartError::Probe`] when the host cannot be probed.
    pub async fn start(
        config: Config,
        ports: Ports<V, I, G, H, C, D>,
        floor: Epoch,
    ) -> Result<Self, StartError> {
        let due = after(ports.clock.monotonic(), config.listing_timeout);
        let probe = bounded(&ports.clock, due, ports.host.resources())
            .await
            .unwrap_or(Err(ProbeFailed))
            .map_err(StartError::Probe)?;
        let mut ledger = Ledger::new(config.sizing, probe);
        let inventory = bounded(&ports.clock, due, ports.provider.inventory())
            .await
            .unwrap_or(Inventory::Incomplete {
                reason: InventoryGap::Deadline,
            });
        ledger.apply_inventory(&inventory);
        if let Some(Ok(mut listing)) = bounded(&ports.clock, due, ports.images.list()).await {
            if config.sizing.measures_disks() {
                remeasure(&ports, due, &mut listing).await;
            }
            ledger.apply_images(&listing.images, &listing.bases, &listing.disks);
        }
        let plane = DataPlane::start(config.host_id.clone(), floor, ledger);
        Ok(Self {
            ports,
            core: Core {
                config,
                plane,
                gate: ListingGate::default(),
                stats: Stats::default(),
                last_report: None,
                deadlines: DeadlineClock::WallAtReceipt,
            },
        })
    }

    /// The resource ledger now.
    #[must_use]
    pub const fn ledger(&self) -> &Ledger {
        &self.core.plane.ledger
    }

    /// The domain state: the session, its table and the ledger.
    #[must_use]
    pub const fn plane(&self) -> &DataPlane {
        &self.core.plane
    }

    /// What was dropped so far.
    #[must_use]
    pub const fn stats(&self) -> Stats {
        self.core.stats
    }

    /// The ports.
    #[must_use]
    pub const fn ports(&self) -> &Ports<V, I, G, H, C, D> {
        &self.ports
    }

    /// Says hello over `link` (ending any open session, whose reservations are then held), and
    /// enters the welcome's session once `floor` made its epoch durable (ADR 0026, ADR 0042).
    /// The session's deadlines are read in the control plane's clock from the welcome's
    /// `cp_clock` and the monotonic reading at its receipt ([`DeadlineClock`]). Pinned by
    /// `tests/executor/session.rs::should_raise_the_floor_before_entering_when_welcomed`
    /// and `should_enter_nothing_when_the_welcome_is_at_or_below_the_floor`.
    ///
    /// # Errors
    /// [`EnterError`]; no session is open then.
    pub async fn enter<L: HandshakeInitiator, F: FloorStore>(
        &mut self,
        link: &mut L,
        floor: &F,
    ) -> Result<Epoch, EnterError> {
        let plane = &mut self.core.plane;
        plane.link(LinkStimulus::Hello);
        let hello = Hello {
            host_id: plane.host_id.clone(),
            boot_id: self.core.config.boot_id.clone(),
            dp_version: self.core.config.dp_version.clone(),
            supported: SUPPORTED.to_vec(),
            capabilities: BTreeSet::new(),
            epoch_floor: plane.floor,
        };
        let welcome = link.hello(hello).await.map_err(EnterError::Link)?;
        let deadlines = DeadlineClock::of(welcome.cp_clock, self.ports.clock.monotonic());
        let epoch = match plane.link(LinkStimulus::Welcome(welcome)) {
            LinkOutcome::RaiseFloor(epoch) => epoch,
            LinkOutcome::Ignored(why) => return Err(EnterError::Ignored(why)),
            LinkOutcome::SendHello | LinkOutcome::Entered(_) => {
                return Err(EnterError::Ignored(LinkIgnored::NotAwaiting));
            }
        };
        floor.raise(epoch).await.map_err(EnterError::Floor)?;
        match plane.link(LinkStimulus::FloorRaised(epoch)) {
            LinkOutcome::Entered(entered) => {
                self.core.deadlines = deadlines;
                Ok(entered)
            }
            LinkOutcome::Ignored(why) => Err(EnterError::Ignored(why)),
            LinkOutcome::SendHello | LinkOutcome::RaiseFloor(_) => {
                Err(EnterError::Ignored(LinkIgnored::NotEntering))
            }
        }
    }

    /// Serves the open session: directives from `source`, observations to `sink`, until the
    /// source ends or an observation cannot be sent. It first reports capacity (the
    /// handshake's `CapacityReported`), and ends the domain's session when it returns, so a
    /// repeat can never wait on a session nothing serves. Pinned by the component tests of
    /// `tests/executor/`, and by
    /// `tests/executor/session.rs::should_serve_nothing_more_when_a_served_session_ended`.
    pub async fn serve<S: DirectiveSource, O: ObservationSink>(
        &mut self,
        source: &mut S,
        sink: &O,
    ) -> SessionEnd {
        let Self { ports, core } = self;
        let ports = &*ports;
        let Some((epoch, version)) = core.open() else {
            return SessionEnd::NotOpen;
        };
        let mut run = Run {
            epoch,
            in_flight: InFlight::default(),
            done: Vec::new(),
            queue: VecDeque::new(),
            busy: BTreeSet::new(),
            holding: BTreeMap::new(),
            running: BTreeMap::new(),
            jobs: 0,
            passing: false,
            observe_due: false,
            next_tick: after(ports.clock.monotonic(), core.config.observe_every),
            waiters: Vec::new(),
            outbox: Vec::new(),
            closing: false,
        };
        core.last_report = None;
        let end = loop {
            for work in std::mem::take(&mut run.done) {
                core.complete(ports, &mut run, work);
            }
            core.start_ready(ports, &mut run);
            core.capacity_on_change(&mut run);
            let flushed = flush(
                ports,
                sink,
                &mut run.outbox,
                &mut run.in_flight,
                &mut run.done,
            )
            .await;
            if let Err(error) = flushed {
                break SessionEnd::SinkFailed(error);
            }
            if !run.done.is_empty() {
                continue;
            }
            let event = {
                let mut recv = pin!(source.recv());
                let mut timer = pin!(ports.clock.sleep_until(run.wake_at()));
                let in_flight = &mut run.in_flight;
                poll_fn(|cx| {
                    if let Poll::Ready(work) = in_flight.poll_next(cx) {
                        return Poll::Ready(Event::Done(work));
                    }
                    if timer.as_mut().poll(cx).is_ready() {
                        return Poll::Ready(Event::Timer);
                    }
                    if let Poll::Ready(received) = recv.as_mut().poll(cx) {
                        return Poll::Ready(Event::Received(received));
                    }
                    Poll::Pending
                })
                .await
            };
            match event {
                Event::Received(Ok(directive)) => {
                    core.receive(ports, &mut run, version, &directive);
                }
                Event::Received(Err(error)) => break SessionEnd::SourceClosed(error),
                Event::Done(work) => core.complete(ports, &mut run, work),
                Event::Timer => core.on_timer(ports, &mut run),
            }
        };
        run.closing = true;
        run.queue.clear();
        run.waiters.clear();
        for work in std::mem::take(&mut run.done) {
            core.complete(ports, &mut run, work);
        }
        while !run.in_flight.is_empty() {
            let work = poll_fn(|cx| run.in_flight.poll_next(cx)).await;
            core.complete(ports, &mut run, work);
        }
        core.capacity_on_change(&mut run);
        core.plane.link(LinkStimulus::Hello);
        if let SessionEnd::SourceClosed(_) = end {
            // The source ended; the sink may still take what the drain settled.
            let mut spare = InFlight::default();
            let flushed = flush(ports, sink, &mut run.outbox, &mut spare, &mut run.done).await;
            if let Err(error) = flushed {
                return SessionEnd::SinkFailed(error);
            }
        }
        end
    }
}

enum Event {
    Received(Result<Directive, LinkError>),
    Done(Work),
    Timer,
}

/// Sends the outbox, polling `in_flight` meanwhile so effects progress while the sink is full;
/// what finishes goes to `done`. Pinned by
/// `tests/executor/session.rs::should_keep_effects_running_when_the_sink_is_full`.
async fn flush<V, I, G, H, C, D, O>(
    ports: &Ports<V, I, G, H, C, D>,
    sink: &O,
    outbox: &mut Vec<Out>,
    in_flight: &mut InFlight<'_, Work>,
    done: &mut Vec<Work>,
) -> Result<(), LinkError>
where
    C: ProcessClock,
    D: ObservationIds,
    O: ObservationSink,
{
    for out in outbox.drain(..) {
        let observation = Observation {
            observation_id: ports.ids.next_id(),
            epoch: out.epoch,
            answers: out.answers,
            observed_at_dp: ports.clock.wall(),
            kind: out.kind,
        };
        let mut send = pin!(sink.send(observation));
        poll_fn(|cx| {
            while let Poll::Ready(work) = in_flight.poll_next(cx) {
                done.push(work);
            }
            send.as_mut().poll(cx)
        })
        .await?;
    }
    Ok(())
}

/// Measures again, from its manifest fetched by digest, each image `listing` names without a
/// size, and the image of each listed base without one (a kept manifest that no longer reads),
/// bounded by the pass's `due`. What still does not measure stays unknown (ADR 0052: counted
/// neither at a guess nor as absent, which would undercount the disk it holds) and is measured
/// again on the next pass. Pinned by
/// `tests/executor/effects.rs::should_measure_a_listed_image_again_when_its_size_did_not_read`.
async fn remeasure<V, I, G, H, C, D>(
    ports: &Ports<V, I, G, H, C, D>,
    due: Millis,
    listing: &mut ImageListing,
) where
    I: ImageStore,
    C: ProcessClock,
{
    let disks = &mut listing.disks;
    let wanted: BTreeSet<ImageDigest> = listing
        .images
        .iter()
        .filter(|digest| !disks.images.contains_key(*digest))
        .cloned()
        .chain(
            listing
                .bases
                .keys()
                .filter(|base| !disks.bases.contains_key(*base))
                .filter_map(|base| disks.base_images.get(base).cloned())
                .filter(|digest| !disks.images.contains_key(digest)),
        )
        .collect();
    for digest in wanted {
        if let Some(Ok(bytes)) = bounded(&ports.clock, due, ports.images.measure(&digest)).await {
            disks.images.insert(digest, bytes);
        }
    }
}

/// The image store's listing as the link carries it (protocol 1.5, DP-08): its verified images
/// and its `Ready` bases, each with the image it was made from and the disk measured from that
/// image's manifest (the base's own, else its image's), in order. A base whose image the store
/// did not name is left out: the control plane matches a base by name and digest, and resolves
/// nothing from what it cannot match.
fn store_listing(listing: &ImageListing) -> ImageStoreListing {
    let disks = &listing.disks;
    ImageStoreListing {
        images: listing.images.iter().cloned().collect(),
        bases: listing
            .bases
            .keys()
            .filter_map(|base| {
                let digest = disks.base_images.get(base)?;
                Some(ListedBase {
                    base_name: base.clone(),
                    digest: digest.clone(),
                    disk_bytes: disks
                        .bases
                        .get(base)
                        .or_else(|| disks.images.get(digest))
                        .copied(),
                })
            })
            .collect(),
    }
}

/// A complete inventory with the image store's listing of the same clean pass beside it, when
/// that listing was complete; anything else as it is. Only a pass the listing gate kept reaches
/// here, so the listing ran while no effect that changes it did, and the control plane may
/// resolve an unknown placement from it (DP-08). A session older than 1.5 gets the inventory
/// alone (`ObservationKind::written_in`). Pinned by
/// `tests/executor/effects.rs::should_send_the_image_listing_beside_the_inventory_when_a_pass_is_clean`
/// and `should_send_no_image_listing_when_the_store_cannot_be_listed`.
fn beside(observed: ObservationKind, listing: Option<&ImageListing>) -> ObservationKind {
    match observed {
        ObservationKind::Inventory { vms, .. } => ObservationKind::Inventory {
            vms,
            image_store: listing.map(store_listing),
        },
        other => other,
    }
}

/// Runs the effect of `kind`, answering what was observed.
async fn effect<V, I, G, H, C, D>(ports: &Ports<V, I, G, H, C, D>, kind: DirectiveKind) -> Stimulus
where
    V: VmProvider,
    I: ImageStore,
    G: GuestTransport,
    H: HostProbe,
{
    match kind {
        DirectiveKind::PullImage { digest } => outcome(ports.images.pull(&digest).await, |()| {
            ObservationKind::ImagePulled { digest }
        }),
        DirectiveKind::MaterialiseImage {
            digest,
            base_name,
            runner_version,
        } => {
            let made = ports
                .images
                .materialise(&digest, &base_name, &runner_version)
                .await;
            // the base's measured disk, as the store lists it (ADR 0052's amendment): the
            // control plane plans its clones at it (CP-07); a listing that fails or a base
            // that did not measure answers none, never a default
            let disk_bytes = if made.is_ok() {
                ports
                    .images
                    .list()
                    .await
                    .ok()
                    .and_then(|listing| listing.disks.bases.get(&base_name).copied())
            } else {
                None
            };
            outcome(made, |()| ObservationKind::ImageMaterialised {
                digest,
                base_name,
                disk_bytes,
            })
        }
        DirectiveKind::EvictImage { base_name } => {
            outcome(ports.images.evict(&base_name).await, |()| {
                ObservationKind::ImageEvicted { base_name }
            })
        }
        DirectiveKind::CloneVm { base_name, vm_name } => outcome(
            ports.provider.clone_vm(&base_name, &vm_name).await,
            |instance_id| ObservationKind::VmCloned { instance_id },
        ),
        DirectiveKind::StartVm {
            vm_name,
            run_options,
        } => outcome(ports.provider.start(&vm_name, &run_options).await, |()| {
            ObservationKind::VmBooted
        }),
        DirectiveKind::VerifyGuest { vm_name, nonce } => outcome(
            ports.guest.verify(&vm_name, nonce).await,
            |answer| match answer {
                GuestAnswer::Verified(facts) => ObservationKind::GuestVerified { facts },
                GuestAnswer::Rejected(reason) => ObservationKind::GuestRejected { reason },
            },
        ),
        // Without the recorded instance id there is nothing to check the VM against, so the
        // config is never delivered unchecked (ADR 0038 decision 2, ADR 0082).
        DirectiveKind::LaunchRunner {
            instance_id: None, ..
        } => Stimulus::IdentityRefused,
        DirectiveKind::LaunchRunner {
            vm_name,
            launch,
            instance_id: Some(instance_id),
        } => match ports.guest.launch(&vm_name, &instance_id, launch).await {
            Ok(LaunchAnswer::Delivered) => Stimulus::Applied(ObservationKind::RunnerLaunched),
            Ok(LaunchAnswer::IdentityRefused) => Stimulus::IdentityRefused,
            Err(error) => outcome::<()>(Err(error), |()| ObservationKind::RunnerLaunched),
        },
        DirectiveKind::ExportDiagnostics { vm_name } => outcome(
            ports.guest.export_diagnostics(&vm_name).await,
            |exported| match exported {
                Diagnostics::Exported { bytes, sha256 } => {
                    ObservationKind::DiagnosticsExported { bytes, sha256 }
                }
                Diagnostics::Lost(reason) => ObservationKind::DiagnosticsLost { reason },
            },
        ),
        DirectiveKind::StopVm { grace, proof } => {
            outcome(ports.provider.stop(&proof, grace).await, |()| {
                ObservationKind::VmStopped
            })
        }
        DirectiveKind::DeleteVm { proof } => outcome(ports.provider.delete(&proof).await, |()| {
            ObservationKind::VmDeleted
        }),
        DirectiveKind::ReportHostFacts => match ports.host.facts().await {
            Ok(facts) => Stimulus::Applied(ObservationKind::HostFacts(facts)),
            Err(ProbeFailed) => Stimulus::Failed,
        },
        // Answered by the executor itself before any effect would start.
        DirectiveKind::ReportInventory | DirectiveKind::ReportCapacity => Stimulus::Failed,
    }
}

impl Core {
    fn open(&self) -> Option<(Epoch, ProtocolVersion)> {
        match &self.plane.link {
            Link::Open(session) => Some((session.epoch, session.version)),
            Link::Idle | Link::Awaiting | Link::Entering(_) => None,
        }
    }

    /// Sends `CapacityReported` when the report differs from the last one sent (ADR 0024);
    /// [`Executor::serve`] clears the last one first, so a session opens with one.
    fn capacity_on_change(&mut self, run: &mut Run<'_>) {
        let mut now = self.plane.ledger.clone().report();
        now.report_id = 0;
        if self.last_report.as_ref() == Some(&now) {
            return;
        }
        self.last_report = Some(now);
        let report = self.plane.ledger.report();
        run.unsolicited(ObservationKind::CapacityReported(report));
    }

    /// Receives `directive`: a `PullImage` whose image's disk ceiling is neither configured nor
    /// measured, and that would otherwise be admitted (fenced first: session, version, epoch,
    /// duplicate, deadline, safe mode; `DataPlane::worth_measuring`), has its manifest measured first, bounded by the directive's deadline, and is
    /// admitted once that read returns, measured or not (unknown is then refused
    /// `capacity_unknown{disk}`, ADR 0052); everything else is admitted now. Pinned by
    /// `tests/executor/effects.rs::should_measure_the_manifest_before_admitting_a_pull_when_no_ceiling_is_configured`.
    fn receive<'a, V, I, G, H, C, D>(
        &mut self,
        ports: &'a Ports<V, I, G, H, C, D>,
        run: &mut Run<'a>,
        version: ProtocolVersion,
        directive: &Directive,
    ) where
        V: VmProvider,
        I: ImageStore,
        G: GuestTransport,
        H: HostProbe,
        C: ProcessClock,
        D: ObservationIds,
    {
        let now = admission_now(&ports.clock, self.deadlines);
        let digest = self.plane.ledger.unmeasured(&directive.kind).cloned();
        let Some(digest) = digest.filter(|_| self.plane.worth_measuring(version, directive, now))
        else {
            self.admit(ports, run, version, directive);
            return;
        };
        let due = local_due(&ports.clock, self.deadlines, directive.deadline);
        let directive = directive.clone();
        run.in_flight.push(Box::pin(async move {
            let disk_bytes = bounded(&ports.clock, due, ports.images.measure(&digest))
                .await
                .and_then(Result::ok);
            Work::Measured {
                version,
                directive,
                disk_bytes,
            }
        }));
    }

    fn admit<V, I, G, H, C, D>(
        &mut self,
        ports: &Ports<V, I, G, H, C, D>,
        run: &mut Run<'_>,
        version: ProtocolVersion,
        directive: &Directive,
    ) where
        C: ProcessClock,
    {
        let key = (run.epoch, directive.directive_id);
        let clock = &ports.clock;
        let due = local_due(clock, self.deadlines, directive.deadline);
        match self
            .plane
            .receive(version, directive, admission_now(clock, self.deadlines))
        {
            Receipt::Discard(_) => self.stats.discarded_frames += 1,
            Receipt::Reply(reply) => run.outbox.push(Out::reply(*reply)),
            Receipt::Pending => {}
            Receipt::ReObserve => run.queue.push_back(Queued {
                key,
                stage: Stage::ReObserve,
                due,
                kind: directive.kind.clone(),
            }),
            Receipt::Execute => match &directive.kind {
                DirectiveKind::ReportCapacity => {
                    let report = self.plane.ledger.report();
                    let mut sent = report.clone();
                    sent.report_id = 0;
                    self.last_report = Some(sent);
                    let progress = self.plane.finish(
                        key.0,
                        key.1,
                        Stimulus::Applied(ObservationKind::CapacityReported(report)),
                    );
                    self.answer(run, progress);
                }
                DirectiveKind::ReportInventory => {
                    run.waiters.push(Waiter { key, due });
                    run.observe_due = true;
                }
                kind => run.queue.push_back(Queued {
                    key,
                    stage: Stage::Effect,
                    due,
                    kind: kind.clone(),
                }),
            },
        }
    }

    /// Starts what may start: queued directives within the bound whose target is free (while a
    /// pass is due or running, none whose effect changes a listing), then an observer pass if
    /// one is due and nothing that changes a listing runs. Pinned by
    /// `tests/executor/main.rs::should_list_before_further_effects_when_a_pass_is_due`.
    fn start_ready<'a, V, I, G, H, C, D>(
        &mut self,
        ports: &'a Ports<V, I, G, H, C, D>,
        run: &mut Run<'a>,
    ) where
        V: VmProvider,
        I: ImageStore,
        G: GuestTransport,
        H: HostProbe,
        C: ProcessClock,
        D: ObservationIds,
    {
        let bound = self.config.concurrency.max(1);
        let listing_first = run.observe_due || run.passing;
        let mut index = 0;
        while run.jobs < bound && index < run.queue.len() {
            let queued = &run.queue[index];
            let blocked = target(&queued.kind).is_some_and(|t| run.busy.contains(&t))
                || (listing_first && changes_listing(&queued.kind));
            if blocked {
                index += 1;
            } else if let Some(queued) = run.queue.remove(index) {
                self.launch(ports, run, queued);
            }
        }
        if run.observe_due && !run.passing && self.gate.idle() {
            run.observe_due = false;
            run.passing = true;
            let ticket = self.gate.begin_listing();
            let due = after(ports.clock.monotonic(), self.config.listing_timeout);
            let measures = self.config.sizing.measures_disks();
            run.in_flight.push(Box::pin(async move {
                let clock = &ports.clock;
                let inventory = bounded(clock, due, ports.provider.inventory())
                    .await
                    .unwrap_or(Inventory::Incomplete {
                        reason: InventoryGap::Deadline,
                    });
                let mut images = bounded(clock, due, ports.images.list())
                    .await
                    .and_then(Result::ok);
                if measures && let Some(listing) = images.as_mut() {
                    remeasure(ports, due, listing).await;
                }
                let probe = bounded(clock, due, ports.host.resources()).await;
                Work::Observed(Pass {
                    ticket,
                    inventory,
                    images,
                    probe,
                })
            }));
        }
    }

    /// Starts a stage of `queued` now, unless its deadline passed while it waited: then it is
    /// refused `deadline_expired`, nothing having been done.
    fn launch<'a, V, I, G, H, C, D>(
        &mut self,
        ports: &'a Ports<V, I, G, H, C, D>,
        run: &mut Run<'a>,
        queued: Queued,
    ) where
        V: VmProvider,
        I: ImageStore,
        G: GuestTransport,
        H: HostProbe,
        C: ProcessClock,
        D: ObservationIds,
    {
        let Queued {
            key,
            stage,
            due,
            kind,
        } = queued;
        if ports.clock.monotonic() >= due {
            let progress = self.plane.finish(key.0, key.1, Stimulus::Expired);
            self.answer(run, progress);
            run.release(key);
            return;
        }
        if let Some(held) = target(&kind) {
            run.busy.insert(held.clone());
            run.holding.insert(key, held);
        }
        run.jobs += 1;
        let clock = &ports.clock;
        match stage {
            Stage::ReObserve => run.in_flight.push(Box::pin(async move {
                let listing = bounded(clock, due, ports.provider.inventory())
                    .await
                    .unwrap_or(Inventory::Incomplete {
                        reason: InventoryGap::Deadline,
                    });
                Work::ReObserved { key, listing }
            })),
            Stage::Effect => {
                let gated = changes_listing(&kind);
                if gated {
                    self.gate.begin_effect();
                }
                run.running.insert(
                    key,
                    Running {
                        due,
                        overdue: false,
                    },
                );
                run.in_flight.push(Box::pin(async move {
                    let stimulus = effect(ports, kind).await;
                    Work::Effect {
                        key,
                        stimulus,
                        gated,
                    }
                }));
            }
        }
    }

    fn complete<'a, V, I, G, H, C, D>(
        &mut self,
        ports: &'a Ports<V, I, G, H, C, D>,
        run: &mut Run<'a>,
        work: Work,
    ) where
        V: VmProvider,
        I: ImageStore,
        G: GuestTransport,
        H: HostProbe,
        C: ProcessClock,
        D: ObservationIds,
    {
        match work {
            Work::ReObserved { key, listing } => {
                run.jobs -= 1;
                let progress = self.plane.reobserved(key.0, key.1, &listing);
                if let Progress::Execute = progress
                    && !run.closing
                    && let Some(queued) = self.continuation(ports, key)
                {
                    self.launch(ports, run, queued);
                    return;
                }
                self.answer(run, progress);
                run.release(key);
            }
            Work::Effect {
                key,
                stimulus,
                gated,
            } => {
                run.jobs -= 1;
                if gated {
                    self.gate.end_effect();
                }
                let overdue = run.running.remove(&key).is_some_and(|r| r.overdue);
                if !overdue {
                    let progress = self.plane.finish(key.0, key.1, stimulus);
                    self.answer(run, progress);
                }
                run.release(key);
            }
            Work::Observed(pass) => {
                run.passing = false;
                self.apply_pass(run, pass);
            }
            Work::Measured {
                version,
                directive,
                disk_bytes,
            } => {
                if let (Some(bytes), DirectiveKind::PullImage { digest }) =
                    (disk_bytes, &directive.kind)
                {
                    self.plane.ledger.learn_image(digest, bytes);
                }
                // A session that is closing admits nothing more: the control plane resends.
                if !run.closing {
                    self.admit(ports, run, version, &directive);
                }
            }
        }
    }

    /// The effect stage of a directive whose re-observation let its effect run.
    fn continuation<V, I, G, H, C, D>(
        &self,
        ports: &Ports<V, I, G, H, C, D>,
        key: Key,
    ) -> Option<Queued>
    where
        C: ProcessClock,
    {
        let Link::Open(session) = &self.plane.link else {
            return None;
        };
        session.table.get(&key.1).map(|entry| Queued {
            key,
            stage: Stage::Effect,
            due: local_due(&ports.clock, self.deadlines, entry.directive.deadline),
            kind: entry.directive.kind.clone(),
        })
    }

    fn answer(&mut self, run: &mut Run<'_>, progress: Progress) {
        match progress {
            Progress::Reply(reply) => run.outbox.push(Out::reply(*reply)),
            Progress::Misfit(_) => self.stats.misfits += 1,
            Progress::Execute | Progress::Orphaned => {}
        }
    }

    fn apply_pass(&mut self, run: &mut Run<'_>, pass: Pass) {
        let ledger = &mut self.plane.ledger;
        // Applied even from a discarded pass: the probe measures the host and foreign VMs, not
        // our effects (`HostProbe::resources`), so an effect cannot make it stale. This holds
        // only while DP-06 measures foreign use independently of our VMs.
        match pass.probe {
            Some(Ok(probe)) => ledger.probe = probe,
            Some(Err(ProbeFailed)) | None => {
                ledger.probe.foreign_unsized = ResourceDimension::ALL.into_iter().collect();
            }
        }
        if !self.gate.clean(pass.ticket) {
            self.stats.discarded_listings += 1;
            run.observe_due = !run.waiters.is_empty();
            return;
        }
        ledger.apply_inventory(&pass.inventory);
        if let Some(listing) = &pass.images {
            ledger.apply_images(&listing.images, &listing.bases, &listing.disks);
        }
        let observed = beside(pass.inventory.observation(), pass.images.as_ref());
        run.unsolicited(observed.clone());
        for waiter in std::mem::take(&mut run.waiters) {
            let progress = self.plane.finish(
                waiter.key.0,
                waiter.key.1,
                Stimulus::Applied(observed.clone()),
            );
            self.answer(run, progress);
        }
    }

    /// The monotonic clock reached a wake-up: an observer pass is due, a `ReportInventory`
    /// that found no clean listing in time is answered incomplete, and an effect still running
    /// at its deadline is answered `Unknown{deadline}` while it keeps its target, its slot and
    /// the listing gate until it completes. Pinned by
    /// `tests/executor/main.rs::should_hold_the_target_and_capacity_when_an_effect_runs_past_its_deadline`.
    fn on_timer<V, I, G, H, C, D>(&mut self, ports: &Ports<V, I, G, H, C, D>, run: &mut Run<'_>)
    where
        C: ProcessClock,
    {
        let now = ports.clock.monotonic();
        if now >= run.next_tick {
            run.observe_due = true;
            run.next_tick = after(now, self.config.observe_every);
        }
        let (expired, waiting): (Vec<Waiter>, Vec<Waiter>) = std::mem::take(&mut run.waiters)
            .into_iter()
            .partition(|w| now >= w.due);
        run.waiters = waiting;
        for waiter in expired {
            let progress = self.plane.finish(
                waiter.key.0,
                waiter.key.1,
                Stimulus::Applied(ObservationKind::InventoryIncomplete {
                    reason: InventoryGap::Deadline,
                }),
            );
            self.answer(run, progress);
        }
        let overdue: Vec<Key> = run
            .running
            .iter_mut()
            .filter(|(_, r)| !r.overdue && now >= r.due)
            .map(|(key, r)| {
                r.overdue = true;
                *key
            })
            .collect();
        for key in overdue {
            let progress =
                self.plane
                    .finish(key.0, key.1, Stimulus::Ambiguous(UnknownReason::Deadline));
            self.answer(run, progress);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_answer_a_hypervisor_refusal_as_a_definite_failure_when_a_start_meets_the_limit() {
        // given
        let refused: Result<(), VmError> = Err(VmError::HypervisorLimit);

        // when
        let stimulus = outcome(refused, |()| ObservationKind::HypervisorLimitHit);

        // then
        assert_eq!(stimulus, Stimulus::Failed);
    }
}
