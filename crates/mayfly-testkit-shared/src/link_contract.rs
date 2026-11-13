//! The link contract suite (ver §5, plan §3.3): one set of cases every CP↔DP transport passes,
//! written once over a [`LinkFactory`] and run from each transport's crate (`inproc`: DP-02;
//! `uds`: LNK-01).
//!
//! Cases are named `LK-01` … ([`LinkCase`]). They drive the link ports of `mayfly_protocol`
//! (`HandshakeInitiator`, `HandshakeResponder`, `DirectiveSink`/`DirectiveSource`,
//! `ObservationSink`/`ObservationSource`) between a data plane and a control plane, and, where
//! a promise is about the bytes (the encoding, the version a frame is written in, what a reader
//! does with a frame it may not hand up), between one typed end and a [`RawWire`]: the peer's
//! side of the same connection, carrying one encoded JSON frame (ADR 0010) at a time. The suite
//! plays the applications: it negotiates the version (`mayfly_protocol::negotiate`) and mints
//! the welcome; epochs are the data plane's to judge (ADR 0042), so the transport hands up a
//! directive of any epoch (LK-15).
//!
//! **What "acknowledged" means.** A transport acknowledges nothing to its caller beyond the
//! `Ok` of a send, which means *accepted*, not delivered (`mayfly_protocol::DirectiveSink`).
//! The contract on an accepted frame is: while the link stays open, it is delivered, at least
//! once (LK-10); a full transport makes the next send wait rather than drop (LK-11); after a
//! close, what the closing side had not received is discarded and *counted* by its close
//! (LK-14), and the peer sees the close only once what it sent before is drained (LK-12,
//! LK-13). End-to-end acknowledgement is the protocol's: the relay re-sends a directive until
//! an observation answers it definitely, and both ends are idempotent (plan §3.2,
//! `formal/link.qnt`).
//!
//! **Order is never assumed.** Every case that receives several frames collects them as a set
//! until all it waits for have arrived, tolerates duplicates, and tells a forbidden frame from
//! an allowed one by content, never by position. A case about frames that may not be handed up
//! (LK-04, LK-05, LK-09, LK-16) sends the forbidden frames, then the readable ones, then
//! finishes the peer's direction ([`RawWire::finish`]), and drains until `Closed`: every frame
//! is examined whatever order the transport delivers them in, so a transport that hands up the
//! readable frame first cannot hide that it would hand up a forbidden one next. The suite's own
//! tests prove both halves: a transport that delivers in reverse and duplicates frames passes
//! every case (`crates/mayfly-adapter-link/tests/link_contract.rs`, `ReorderingLink`), and the
//! same transport without the version check fails the version cases
//! (`ReorderingNoVersionCheckLink`).
//!
//! **No case relies on time.** Each case runs on its own current-thread Tokio runtime, with the
//! clock paused unless the factory says otherwise ([`LinkFactory::paused_clock`]); a bounded
//! wait ([`crate::poll::within`]) turns a hang into a failure with a reason, at once under the
//! paused clock. Nothing is spawned: concurrent halves are polled together on the case's task.
//!
//! **The JIT config of a launch** (protocol 1.6, LNK-02, `formal/link.qnt`'s `configSentOnce`,
//! `configDeliveredOnce` and `configOnlyInItsEpoch`). LK-17 to LK-19 wire the ends to the
//! suite's own broker and store ([`SuiteBroker`], [`SuiteStore`], through the factory's
//! `*_with_configs` methods) and check that a launch's config crosses once, in the one frame of
//! its first send, into the store bound to the launch, that no other frame and no error carries
//! a byte of it, and that the data plane puts none for another epoch, another kind, a launch
//! without an instance id, a session below 1.6 or a handle already used. Every config is built
//! at run time.
//!
//! [`check_link_contract`] returns a report of failed cases instead of panicking: the suite's
//! own tests use it to show deliberately broken transports failing named cases (ver §5:
//! `SilentDropLink`, `NoVersionCheckLink`, `ReorderingNoVersionCheckLink`, `DeafCloseLink`,
//! `LossyCloseLink`, `CorruptingLink`). A factory that cannot build what a case needs fails
//! that case: no case is skipped.

use std::collections::BTreeSet;
use std::fmt::{self, Debug, Display};
use std::future::{Future, poll_fn};
use std::path::Path;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use mayfly_domain::{Deadline, DirectiveId, Epoch, HostId, Millis, ObservationId};
use mayfly_protocol::{
    CURRENT, Directive, DirectiveKind, DirectiveRef, DirectiveSink, DirectiveSource, Frame,
    HandshakeInitiator, HandshakeRefusal, HandshakeRefused, HandshakeResponder, Hello,
    LaunchConfigSink, LaunchConfigSource, LinkError, Message, Mode, Observation, ObservationKind,
    ObservationSink, ObservationSource, ProtocolVersion, RefusalReason, ResourceDimension,
    SUPPORTED, Welcome, admit_frame, negotiate,
};

use crate::fixtures::read_tree;
use crate::poll::within;

mod launch;

pub use launch::{SuiteBroker, SuiteStore};

/// How long a case waits for one step before it calls it a hang. Under the paused clock it
/// costs nothing; under a real clock it bounds a socket transport's slowest step.
pub const STEP_BOUND: Duration = Duration::from_secs(10);

/// The peer's side of a connection, one encoded frame at a time, for the cases about bytes.
pub trait RawWire {
    /// Writes one frame's bytes as the peer would.
    ///
    /// # Errors
    /// [`LinkError::Closed`] when the typed end closed.
    fn send(&mut self, frame: Vec<u8>) -> impl Future<Output = Result<(), LinkError>>;

    /// The next frame's bytes the typed end wrote, or `None` once it closed.
    fn recv(&mut self) -> impl Future<Output = Option<Vec<u8>>>;

    /// Closes the peer's sending direction only (a socket's `shutdown(Write)`): the typed end
    /// reads what was sent before and then `Closed`, while this side can still [`recv`]. Sends
    /// after it fail with [`LinkError::Closed`].
    ///
    /// [`recv`]: RawWire::recv
    fn finish(&mut self) -> impl Future<Output = ()>;
}

/// What an endpoint discarded instead of handing up, by cause: the transport-neutral view of
/// `mayfly_adapter_link::Discards` (LK-16).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiscardCounts {
    /// Frames of a version not admitted (`mayfly_protocol::admit_frame`).
    pub version: u64,
    /// Bytes that are not a frame of an admitted version.
    pub malformed: u64,
    /// Admitted frames of a kind the end does not take at that point.
    pub unexpected: u64,
    /// JIT configs read and dropped unread, or refused by the store (LK-19).
    pub configs: u64,
}

/// Makes the links the suite runs against.
///
/// Every method is required (but [`LinkFactory::paused_clock`]), so a transport cannot skip a
/// case by omission.
pub trait LinkFactory {
    /// The data plane's handshake end.
    type DpLink: HandshakeInitiator;
    /// The control plane's handshake end.
    type CpLink: HandshakeResponder;
    /// The data plane's receiving half of a session.
    type DpDirectives: DirectiveSource;
    /// The data plane's sending half of a session.
    type DpObservations: ObservationSink;
    /// The control plane's sending half of a session.
    type CpDirectives: DirectiveSink;
    /// The control plane's receiving half of a session.
    type CpObservations: ObservationSource;
    /// The peer's side of a connection, as bytes.
    type Raw: RawWire;

    /// The bound the transport promises on one direction: the most frames it holds while its
    /// reader reads nothing, so a send beyond it waits (LK-11). At least 1. It is an upper
    /// bound, not an exact size: a send may wait sooner (a socket's buffers count bytes, not
    /// frames), never later.
    fn capacity(&self) -> usize;

    /// Whether the cases run under Tokio's paused clock (the default). A transport whose
    /// readiness comes from the operating system (a socket) answers `false`, so a bounded wait
    /// does not elapse while the kernel still has work to do.
    fn paused_clock(&self) -> bool {
        true
    }

    /// A connected pair: the data plane speaking `dp`, the control plane speaking `cp`.
    fn connect(
        &self,
        dp: &[ProtocolVersion],
        cp: &[ProtocolVersion],
    ) -> impl Future<Output = (Self::DpLink, Self::CpLink)>;

    /// A connected pair as [`LinkFactory::connect`] makes it, the control plane taking each
    /// launch's JIT config from `broker` and the data plane putting the configs it reads into
    /// `store` (LK-17).
    fn connect_with_configs(
        &self,
        dp: &[ProtocolVersion],
        cp: &[ProtocolVersion],
        broker: Arc<dyn LaunchConfigSource>,
        store: Arc<dyn LaunchConfigSink>,
    ) -> impl Future<Output = (Self::DpLink, Self::CpLink)>;

    /// A data plane speaking `dp` that puts the configs it reads into `store`, connected to a
    /// raw control-plane peer (LK-19).
    fn data_plane_facing_raw_with_configs(
        &self,
        dp: &[ProtocolVersion],
        store: Arc<dyn LaunchConfigSink>,
    ) -> impl Future<Output = (Self::DpLink, Self::Raw)>;

    /// A control plane speaking `cp` that takes launch configs from `broker`, connected to a raw
    /// data-plane peer (LK-18).
    fn control_plane_facing_raw_with_configs(
        &self,
        cp: &[ProtocolVersion],
        broker: Arc<dyn LaunchConfigSource>,
    ) -> impl Future<Output = (Self::CpLink, Self::Raw)>;

    /// A data plane speaking `dp`, connected to a raw control-plane peer.
    fn data_plane_facing_raw(
        &self,
        dp: &[ProtocolVersion],
    ) -> impl Future<Output = (Self::DpLink, Self::Raw)>;

    /// A control plane speaking `cp`, connected to a raw data-plane peer.
    fn control_plane_facing_raw(
        &self,
        cp: &[ProtocolVersion],
    ) -> impl Future<Output = (Self::CpLink, Self::Raw)>;

    /// The data plane's session halves, once its handshake returned a welcome.
    ///
    /// # Errors
    /// When the link has no session.
    fn data_plane_session(
        &self,
        link: Self::DpLink,
    ) -> Result<(Self::DpDirectives, Self::DpObservations), LinkError>;

    /// The control plane's session halves, once it welcomed a hello.
    ///
    /// # Errors
    /// When the link has no session.
    fn control_plane_session(
        &self,
        link: Self::CpLink,
    ) -> Result<(Self::CpDirectives, Self::CpObservations), LinkError>;

    /// The data plane closes its session; the frames it had not received and discarded.
    fn close_data_plane(
        &self,
        directives: Self::DpDirectives,
        observations: Self::DpObservations,
    ) -> impl Future<Output = usize>;

    /// The control plane closes its session; the frames it had not received and discarded.
    fn close_control_plane(
        &self,
        directives: Self::CpDirectives,
        observations: Self::CpObservations,
    ) -> impl Future<Output = usize>;

    /// What the data plane's handshake end discarded so far (LK-16).
    fn data_plane_discards(&self, link: &Self::DpLink) -> DiscardCounts;

    /// What the data plane's receiving half, and the handshake before it, discarded so far.
    fn data_plane_session_discards(&self, directives: &Self::DpDirectives) -> DiscardCounts;

    /// What the control plane's handshake end discarded so far (LK-16).
    fn control_plane_discards(&self, link: &Self::CpLink) -> DiscardCounts;

    /// What the control plane's receiving half, and the handshake before it, discarded so far.
    fn control_plane_session_discards(&self, observations: &Self::CpObservations) -> DiscardCounts;
}

/// One case of the link contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LinkCase {
    /// LK-01: a hello and its welcome cross intact, the session opens on both ends, and a
    /// directive and an observation cross it.
    Handshake,
    /// LK-02: a 1.0 peer and a 1.1 peer (either way round) agree on 1.0; the session's frames
    /// are stamped 1.0 and an observation is written down to it (`written_in`).
    MixedMinors,
    /// LK-03: a refused handshake reaches the data plane as `Refused` with its reason.
    HandshakeRefused,
    /// LK-04: a handshake frame of a major the reader does not speak is never handed up, in
    /// any order: a data plane offered only a 2.0 welcome ends its hello in `Closed` when the
    /// peer finishes, a readable welcome sent after it is handed up, and a control plane drained
    /// to `Closed` hands up the 1.x hello and never the 2.0 one.
    UnknownMajorDiscarded,
    /// LK-05: within a session, a frame of a newer minor or another major is never handed up,
    /// in either direction: drained to `Closed`, exactly the readable frames arrive.
    SessionVersionHeld,
    /// LK-06: a welcome in a version the end does not speak opens no session, on either end.
    WelcomeOutsideOffer,
    /// LK-07: handshake frames are written in the oldest major the writer speaks.
    HandshakeInOldestMajor,
    /// LK-08: every message survives the serde round trip, in 1.0 and 1.1 sessions: the hello
    /// on the wire is its JSON frame; every directive and observation of the protocol's golden
    /// fixtures (`crates/mayfly-protocol/tests/fixtures`, whose harness proves they show every
    /// directive kind, observation kind, refusal reason and unknown reason) goes on the wire as
    /// the JSON frame of what was sent, stamped with the session's version (an observation
    /// written down to it), and each fixture's bytes, written by the peer, are read as
    /// themselves when the session admits their version.
    WireEncoding,
    /// LK-09: bytes that are not a frame are discarded; the link stays open, and drained to
    /// `Closed` it hands up exactly the frames sent after them.
    MalformedDiscarded,
    /// LK-10: every accepted frame is delivered while the link is open (at least once, any
    /// order), in both directions, past the capacity.
    AtLeastOnce,
    /// LK-11: a direction holds at most its capacity unread ([`LinkFactory::capacity`], an
    /// upper bound: a send by then waits, and may wait sooner); the send that waits completes
    /// once the reader takes a frame, losing none.
    Backpressure,
    /// LK-12: the control plane's close is seen by the data plane: its receive hands up every
    /// directive sent before the close and then ends in `Closed` (drained, then `Closed`), and
    /// its sends then fail with `Closed`.
    CloseSeenByDataPlane,
    /// LK-13: the data plane's close is seen by the control plane, likewise.
    CloseSeenByControlPlane,
    /// LK-14: a close discards the frames its side had not received and reports them: at least
    /// one when any was unread, so a lost burst never reads as zero. The count is a lower bound,
    /// not exact: at-least-once may count a duplicate, and a socket transport cannot count what
    /// is still in its peer's writer when it closes.
    CloseReportsInFlight,
    /// LK-15: a directive of any epoch is handed up: fencing is the data plane's (I09, ADR
    /// 0042), so a stale frame can be answered `OutcomeUnknown{stale_epoch}`.
    EveryEpochHandedUp,
    /// LK-16: what an end may not hand up is discarded and counted by cause
    /// ([`DiscardCounts`], lower bounds under at-least-once): a directive before the welcome is
    /// discarded unanswered (ADR 0042), as are garbage and a frame of another major; within a
    /// session a duplicate `Welcome`, a `Hello` and a message of the other direction are
    /// discarded as unexpected, on both ends, and the link stays open.
    DiscardsCounted,
    /// LK-17: a launch's JIT config is delivered once per handle: sent three times (the relay's
    /// resends) over a transport that may duplicate frames, it reaches the data plane's store
    /// exactly once, bound to the launch's VM, instance id and deadline, and the broker holds
    /// it no more; every copy of the launch is handed up.
    ConfigDeliveredOnce,
    /// LK-18: no other frame carries a byte of a config, nor does an error: of the frames the
    /// control plane writes, only the first send of a launch whose config the broker holds for
    /// its attempt and epoch carries it, in one member beside the message, the frame otherwise
    /// the plain one; a resend, another directive, a launch of another attempt (whose config
    /// the take discards) and any launch of a session below 1.6 carry none, and a launch
    /// without an instance id or of another epoch than the session's takes nothing (the data
    /// plane would put neither); a send that fails names none.
    ConfigOnlyInItsFrame,
    /// LK-19: the data plane empties its store when a session opens, and puts a config only for
    /// a launch of its session's epoch that names an instance id, in a session of 1.6 or newer,
    /// once per handle; a config carried by a launch of another epoch, by another kind or by a
    /// launch without an instance id, or read in a 1.5 session, is dropped and counted, and
    /// every directive is still handed up.
    ConfigPutOnlyForItsSession,
}

impl LinkCase {
    /// Every case, in id order.
    pub const ALL: [Self; 19] = [
        Self::Handshake,
        Self::MixedMinors,
        Self::HandshakeRefused,
        Self::UnknownMajorDiscarded,
        Self::SessionVersionHeld,
        Self::WelcomeOutsideOffer,
        Self::HandshakeInOldestMajor,
        Self::WireEncoding,
        Self::MalformedDiscarded,
        Self::AtLeastOnce,
        Self::Backpressure,
        Self::CloseSeenByDataPlane,
        Self::CloseSeenByControlPlane,
        Self::CloseReportsInFlight,
        Self::EveryEpochHandedUp,
        Self::DiscardsCounted,
        Self::ConfigDeliveredOnce,
        Self::ConfigOnlyInItsFrame,
        Self::ConfigPutOnlyForItsSession,
    ];

    /// The case's id, `LK-01` …
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Handshake => "LK-01",
            Self::MixedMinors => "LK-02",
            Self::HandshakeRefused => "LK-03",
            Self::UnknownMajorDiscarded => "LK-04",
            Self::SessionVersionHeld => "LK-05",
            Self::WelcomeOutsideOffer => "LK-06",
            Self::HandshakeInOldestMajor => "LK-07",
            Self::WireEncoding => "LK-08",
            Self::MalformedDiscarded => "LK-09",
            Self::AtLeastOnce => "LK-10",
            Self::Backpressure => "LK-11",
            Self::CloseSeenByDataPlane => "LK-12",
            Self::CloseSeenByControlPlane => "LK-13",
            Self::CloseReportsInFlight => "LK-14",
            Self::EveryEpochHandedUp => "LK-15",
            Self::DiscardsCounted => "LK-16",
            Self::ConfigDeliveredOnce => "LK-17",
            Self::ConfigOnlyInItsFrame => "LK-18",
            Self::ConfigPutOnlyForItsSession => "LK-19",
        }
    }
}

impl Display for LinkCase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {self:?}", self.id())
    }
}

/// A failed case and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseFailure {
    /// The case.
    pub case: LinkCase,
    /// What was expected and what the transport did, at the first check that failed.
    pub detail: String,
}

/// The failed cases of one run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkContractReport {
    /// Each failed case, in id order.
    pub failures: Vec<CaseFailure>,
}

impl LinkContractReport {
    /// Whether every case passed.
    #[must_use]
    pub fn is_green(&self) -> bool {
        self.failures.is_empty()
    }

    /// The failed cases.
    #[must_use]
    pub fn failed_cases(&self) -> BTreeSet<LinkCase> {
        self.failures.iter().map(|f| f.case).collect()
    }
}

impl Display for LinkContractReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "link contract: {} case(s) failed", self.failures.len())?;
        for failure in &self.failures {
            writeln!(f, "- {}: {}", failure.case, failure.detail)?;
        }
        Ok(())
    }
}

/// Runs every case against links from `factory`, each on its own runtime.
pub fn check_link_contract<F: LinkFactory>(factory: &F) -> LinkContractReport {
    let mut report = LinkContractReport::default();
    for case in LinkCase::ALL {
        if let Err(detail) = check_case(factory, case) {
            report.failures.push(CaseFailure { case, detail });
        }
    }
    report
}

/// Runs every case and panics with the report unless all pass: the form a transport's own test
/// calls.
///
/// # Panics
/// When a case fails; the message lists each failed case and why.
pub fn run_link_contract<F: LinkFactory>(factory: &F) {
    let report = check_link_contract(factory);
    assert!(report.is_green(), "{report}");
}

/// Runs one case on a fresh current-thread runtime.
///
/// # Errors
/// What was expected and what the transport did, at the first check that failed.
pub fn check_case<F: LinkFactory>(factory: &F, case: LinkCase) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(factory.paused_clock())
        .build()
        .map_err(|e| format!("the case's runtime did not build: {e}"))?;
    runtime.block_on(run_case(factory, case))
}

async fn run_case<F: LinkFactory>(f: &F, case: LinkCase) -> Check {
    match case {
        LinkCase::Handshake => handshake_case(f).await,
        LinkCase::MixedMinors => mixed_minors(f).await,
        LinkCase::HandshakeRefused => handshake_refused(f).await,
        LinkCase::UnknownMajorDiscarded => unknown_major_discarded(f).await,
        LinkCase::SessionVersionHeld => session_version_held(f).await,
        LinkCase::WelcomeOutsideOffer => welcome_outside_offer(f).await,
        LinkCase::HandshakeInOldestMajor => handshake_in_oldest_major(f).await,
        LinkCase::WireEncoding => wire_encoding(f).await,
        LinkCase::MalformedDiscarded => malformed_discarded(f).await,
        LinkCase::AtLeastOnce => at_least_once(f).await,
        LinkCase::Backpressure => backpressure(f).await,
        LinkCase::CloseSeenByDataPlane => close_seen_by_data_plane(f).await,
        LinkCase::CloseSeenByControlPlane => close_seen_by_control_plane(f).await,
        LinkCase::CloseReportsInFlight => close_reports_in_flight(f).await,
        LinkCase::EveryEpochHandedUp => every_epoch_handed_up(f).await,
        LinkCase::DiscardsCounted => discards_counted(f).await,
        LinkCase::ConfigDeliveredOnce => launch::config_delivered_once(f).await,
        LinkCase::ConfigOnlyInItsFrame => launch::config_only_in_its_frame(f).await,
        LinkCase::ConfigPutOnlyForItsSession => launch::config_put_only_for_its_session(f).await,
    }
}

// ---------------------------------------------------------------------------------------------
// Checks and driving

type Check = Result<(), String>;

fn eq<T: PartialEq + Debug>(what: &str, got: &T, want: &T) -> Check {
    if got == want {
        Ok(())
    } else {
        Err(format!("{what}: got {got:?}, want {want:?}"))
    }
}

fn ok<T, E: Display>(what: &str, result: Result<T, E>) -> Result<T, String> {
    result.map_err(|e| format!("{what}: unexpected error: {e}"))
}

/// Awaits one step, bounded: a hang is a failure naming the step.
async fn step<F: Future>(what: &str, future: F) -> Result<F::Output, String> {
    within(STEP_BOUND, what, future)
        .await
        .map_err(|timed_out| timed_out.to_string())
}

/// Awaits one fallible step, bounded.
async fn step_ok<T, E: Display, F: Future<Output = Result<T, E>>>(
    what: &str,
    future: F,
) -> Result<T, String> {
    ok(what, step(what, future).await?)
}

/// Polls `future` once more for at most the bound and expects it still waiting.
async fn still_waits<F: Future>(what: &str, future: Pin<&mut F>) -> Result<bool, String> {
    match within(STEP_BOUND, what, future).await {
        Err(_) => Ok(true),
        Ok(_) => Ok(false),
    }
}

/// Both futures to completion, polled together on this task.
async fn join2<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let mut a = pin!(a);
    let mut b = pin!(b);
    let mut a_out = None;
    let mut b_out = None;
    poll_fn(|cx| {
        if a_out.is_none()
            && let Poll::Ready(out) = a.as_mut().poll(cx)
        {
            a_out = Some(out);
        }
        if b_out.is_none()
            && let Poll::Ready(out) = b.as_mut().poll(cx)
        {
            b_out = Some(out);
        }
        match (a_out.take(), b_out.take()) {
            (Some(a), Some(b)) => Poll::Ready((a, b)),
            (a, b) => {
                a_out = a;
                b_out = b;
                Poll::Pending
            }
        }
    })
    .await
}

/// Runs `side` to completion while `main` is also polled; `Err` with `main`'s output if `main`
/// completed first (it is then not polled again).
async fn beside<A: Future, B: Future>(
    mut main: Pin<&mut A>,
    side: B,
) -> Result<B::Output, A::Output> {
    let mut side = pin!(side);
    poll_fn(|cx| {
        if let Poll::Ready(out) = side.as_mut().poll(cx) {
            return Poll::Ready(Ok(out));
        }
        if let Poll::Ready(out) = main.as_mut().poll(cx) {
            return Poll::Ready(Err(out));
        }
        Poll::Pending
    })
    .await
}

/// A receiving half of either direction.
trait Receive<T> {
    fn receive(&mut self) -> impl Future<Output = Result<T, LinkError>>;
}

impl<S: DirectiveSource> Receive<Directive> for S {
    fn receive(&mut self) -> impl Future<Output = Result<Directive, LinkError>> {
        self.recv()
    }
}

impl<S: ObservationSource> Receive<Observation> for S {
    fn receive(&mut self) -> impl Future<Output = Result<Observation, LinkError>> {
        self.recv()
    }
}

/// A sending half of either direction.
trait Transmit<T> {
    fn transmit(&self, item: T) -> impl Future<Output = Result<(), LinkError>>;
}

impl<S: DirectiveSink> Transmit<Directive> for S {
    fn transmit(&self, item: Directive) -> impl Future<Output = Result<(), LinkError>> {
        self.send(item)
    }
}

impl<S: ObservationSink> Transmit<Observation> for S {
    fn transmit(&self, item: Observation) -> impl Future<Output = Result<(), LinkError>> {
        self.send(item)
    }
}

/// Receives until every item `want` names has arrived; duplicates and order are ignored, an
/// item outside `allowed` fails. At most `4 × want + 4` receives, each bounded.
async fn collect<T: Debug, K: Ord + Clone + Debug>(
    what: &str,
    source: &mut impl Receive<T>,
    want: &BTreeSet<K>,
    allowed: &BTreeSet<K>,
    key: impl Fn(&T) -> K,
) -> Result<Vec<T>, String> {
    let mut seen = BTreeSet::new();
    let mut got = Vec::new();
    let mut budget = want.len() * 4 + 4;
    while !want.is_subset(&seen) {
        if budget == 0 {
            return Err(format!(
                "{what}: still missing {:?} after {} receives",
                want.difference(&seen).collect::<Vec<_>>(),
                got.len()
            ));
        }
        budget -= 1;
        let item = step_ok(what, source.receive()).await?;
        let k = key(&item);
        if !allowed.contains(&k) {
            return Err(format!("{what}: handed up {item:?}, which it may not"));
        }
        seen.insert(k);
        got.push(item);
    }
    Ok(got)
}

/// The control plane's handshake end as a source of hellos, so a case can drain it.
struct Hellos<'a, C>(&'a mut C);

impl<C: HandshakeResponder> Receive<Hello> for Hellos<'_, C> {
    fn receive(&mut self) -> impl Future<Output = Result<Hello, LinkError>> {
        self.0.next_hello()
    }
}

/// Receives until `Closed`: every item `want` names must arrive before it (a finished or
/// closed sender's direction is drained first, then ends: "drained, then `Closed`"), and no
/// item outside `want` may arrive at all. Duplicates and order are ignored. At most
/// `4 × want + 4` receives, each bounded.
async fn until_closed<T: Debug, K: Ord + Debug>(
    what: &str,
    source: &mut impl Receive<T>,
    want: &BTreeSet<K>,
    key: impl Fn(&T) -> K,
) -> Check {
    let mut seen = BTreeSet::new();
    for _ in 0..want.len() * 4 + 4 {
        match step(what, source.receive()).await? {
            Err(LinkError::Closed) => {
                let missing: Vec<&K> = want.difference(&seen).collect();
                return if missing.is_empty() {
                    Ok(())
                } else {
                    Err(format!(
                        "{what}: Closed before {missing:?} arrived (what was sent before a close is drained first)"
                    ))
                };
            }
            Err(other) => return Err(format!("{what}: got {other}, want Closed")),
            Ok(item) => {
                let k = key(&item);
                if !want.contains(&k) {
                    return Err(format!("{what}: handed up {item:?}, which it may not"));
                }
                seen.insert(k);
            }
        }
    }
    Err(format!("{what}: never ended in Closed"))
}

/// Sends each frame as the raw peer, then finishes the peer's sending direction.
async fn send_then_finish<R: RawWire>(raw: &mut R, frames: Vec<Vec<u8>>) -> Check {
    for bytes in frames {
        ok("a raw frame", raw.send(bytes).await)?;
    }
    raw.finish().await;
    Ok(())
}

/// The raw peer sends `frames` and finishes while `source` is drained until `Closed`; exactly
/// the items `want` names must arrive (see [`until_closed`]).
async fn drain_after<R: RawWire, T: Debug, K: Ord + Debug>(
    what: &str,
    raw: &mut R,
    frames: Vec<Vec<u8>>,
    source: &mut impl Receive<T>,
    want: &BTreeSet<K>,
    key: impl Fn(&T) -> K,
) -> Check {
    let (sent, drained) = step(
        what,
        join2(
            send_then_finish(raw, frames),
            until_closed(what, source, want, key),
        ),
    )
    .await?;
    sent.map_err(|e| format!("{what}: {e}"))?;
    drained
}

/// Reads frames from the raw peer until every frame `want` names (by [`frame_json`]) has
/// arrived; a frame outside `want` fails. Duplicates and order are ignored.
async fn raw_collect<R: RawWire>(what: &str, raw: &mut R, want: &BTreeSet<String>) -> Check {
    let mut seen = BTreeSet::new();
    let mut budget = want.len() * 4 + 4;
    while !want.is_subset(&seen) {
        if budget == 0 {
            return Err(format!(
                "{what}: still missing {:?}",
                want.difference(&seen).collect::<Vec<_>>()
            ));
        }
        budget -= 1;
        let frame = read_frame(what, step(what, raw.recv()).await?)?;
        let k = frame_json(&frame);
        if !want.contains(&k) {
            return Err(format!("{what}: the typed end wrote {k}, which it may not"));
        }
        seen.insert(k);
    }
    Ok(())
}

/// Fails unless each count of `got` reaches the one of `want`.
fn at_least(what: &str, got: DiscardCounts, want: DiscardCounts) -> Check {
    if got.version >= want.version
        && got.malformed >= want.malformed
        && got.unexpected >= want.unexpected
        && got.configs >= want.configs
    {
        Ok(())
    } else {
        Err(format!("{what}: got {got:?}, want at least {want:?}"))
    }
}

const fn counts(version: u64, malformed: u64, unexpected: u64) -> DiscardCounts {
    DiscardCounts {
        version,
        malformed,
        unexpected,
        configs: 0,
    }
}

fn hello_key(h: &Hello) -> String {
    h.boot_id.clone()
}

fn directive_key(d: &Directive) -> (DirectiveId, Epoch) {
    (d.directive_id, d.epoch)
}

fn observation_key(o: &Observation) -> ObservationId {
    o.observation_id
}

fn set<K: Ord>(items: impl IntoIterator<Item = K>) -> BTreeSet<K> {
    items.into_iter().collect()
}

// ---------------------------------------------------------------------------------------------
// Sample messages

const fn v(major: u16, minor: u16) -> ProtocolVersion {
    ProtocolVersion::new(major, minor)
}

const V1_0: ProtocolVersion = v(1, 0);
const V2_0: ProtocolVersion = v(2, 0);
/// The epoch every sample session is welcomed at.
const SESSION_EPOCH: Epoch = Epoch::new(5);

fn hello(supported: &[ProtocolVersion], boot: &str) -> Result<Hello, String> {
    Ok(Hello {
        host_id: ok("host id", HostId::new("host0"))?,
        boot_id: boot.to_owned(),
        dp_version: "0.1.0".to_owned(),
        supported: supported.to_vec(),
        capabilities: set(["inventory-described".to_owned()]),
        epoch_floor: Epoch::new(4),
    })
}

const fn welcome(accepted_version: ProtocolVersion, epoch: Epoch) -> Welcome {
    Welcome {
        epoch,
        mode: Mode::Normal,
        accepted_version,
        cp_clock: None,
    }
}

fn directive(n: u128, epoch: Epoch) -> Directive {
    Directive {
        directive_id: DirectiveId::from_u128(n),
        epoch,
        attempt: None,
        deadline: Deadline::at(Millis::new(1_700_000_060_000)),
        kind: DirectiveKind::ReportInventory,
    }
}

/// One golden fixture of the protocol: its bytes as written there and the frame they decode to.
struct Golden {
    bytes: Vec<u8>,
    frame: Frame,
}

/// The protocol's golden frames (`crates/mayfly-protocol/tests/fixtures`, ver §4). Their
/// harness (`crates/mayfly-protocol/tests/golden.rs`) proves they show every directive kind,
/// observation kind, refusal reason and unknown reason, read from serde's own variant lists,
/// so LK-08 covers a new variant as soon as its fixture lands.
fn golden_frames() -> Result<Vec<Golden>, String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mayfly-protocol/tests/fixtures");
    let files =
        read_tree(&root).map_err(|e| format!("reading the protocol's golden fixtures: {e}"))?;
    files
        .into_iter()
        .map(|(path, text)| {
            let frame = serde_json::from_str(&text)
                .map_err(|e| format!("the golden fixture {path} does not decode: {e}"))?;
            Ok(Golden {
                bytes: text.into_bytes(),
                frame,
            })
        })
        .collect()
}

/// The golden directives and observations, each with its fixture.
type GoldenMessages<'a> = (Vec<(&'a Golden, Directive)>, Vec<(&'a Golden, Observation)>);

/// Splits the golden frames into directives and observations, each with its fixture.
fn golden_messages(golden: &[Golden]) -> Result<GoldenMessages<'_>, String> {
    let mut directives = Vec::new();
    let mut observations = Vec::new();
    for g in golden {
        match &g.frame.message {
            Message::Directive(d) => directives.push((g, d.clone())),
            Message::Observation(o) => observations.push((g, o.clone())),
            Message::Hello(_) | Message::Welcome(_) | Message::HandshakeRefused(_) => {}
        }
    }
    if directives.is_empty() || observations.is_empty() {
        return Err(
            "the protocol's golden fixtures hold no directive or no observation".to_owned(),
        );
    }
    Ok((directives, observations))
}

/// A frame's identity for a set: its JSON (protocol data always encodes).
fn frame_json(frame: &Frame) -> String {
    serde_json::to_string(frame).unwrap_or_else(|e| format!("a frame that does not encode: {e}"))
}

fn directive_json(d: &Directive) -> String {
    frame_json(&Frame {
        protocol_version: CURRENT,
        message: Message::Directive(d.clone()),
    })
}

fn observation_json(o: &Observation) -> String {
    frame_json(&Frame {
        protocol_version: CURRENT,
        message: Message::Observation(o.clone()),
    })
}

fn observation(n: u128, kind: ObservationKind) -> Observation {
    Observation {
        observation_id: ObservationId::from_u128(n),
        epoch: SESSION_EPOCH,
        answers: Some(DirectiveRef {
            directive_id: DirectiveId::from_u128(n),
            attempt: None,
        }),
        observed_at_dp: Millis::new(1_700_000_001_000),
        kind,
    }
}

fn booted(n: u128) -> Observation {
    observation(n, ObservationKind::VmBooted)
}

/// A refusal only protocol 1.1 can say (ADR 0052).
fn capacity_unknown(n: u128) -> Observation {
    observation(
        n,
        ObservationKind::DirectiveRefused {
            reason: RefusalReason::CapacityUnknown {
                dimension: ResourceDimension::Memory,
            },
        },
    )
}

/// The same refusal as a 1.0 session carries it.
fn insufficient_resources(n: u128) -> Observation {
    observation(
        n,
        ObservationKind::DirectiveRefused {
            reason: RefusalReason::InsufficientResources {
                dimension: ResourceDimension::Memory,
            },
        },
    )
}

fn frame_bytes(protocol_version: ProtocolVersion, message: Message) -> Result<Vec<u8>, String> {
    ok(
        "encoding the peer's frame",
        serde_json::to_vec(&Frame {
            protocol_version,
            message,
        }),
    )
}

fn read_frame(what: &str, bytes: Option<Vec<u8>>) -> Result<Frame, String> {
    let bytes = bytes.ok_or_else(|| format!("{what}: the typed end closed"))?;
    serde_json::from_slice(&bytes).map_err(|e| {
        format!(
            "{what}: the bytes are not a JSON frame ({e}): {:?}",
            String::from_utf8_lossy(&bytes)
        )
    })
}

// ---------------------------------------------------------------------------------------------
// Opening sessions

/// The control plane's application: answer one hello by negotiating against `cp`.
async fn answer_hello<C: HandshakeResponder>(
    cp: &mut C,
    supported: &[ProtocolVersion],
) -> Result<Hello, String> {
    let hello = ok("the control plane's next hello", cp.next_hello().await)?;
    let accepted = negotiate(&hello.supported, supported)
        .map_err(|refusal| format!("negotiating: {refusal:?}"))?;
    ok(
        "the control plane's welcome",
        cp.welcome(welcome(accepted, SESSION_EPOCH)).await,
    )?;
    Ok(hello)
}

/// Both ends of an open session.
struct Session<F: LinkFactory> {
    dp_directives: F::DpDirectives,
    dp_observations: F::DpObservations,
    cp_directives: F::CpDirectives,
    cp_observations: F::CpObservations,
}

/// Opens a session between a data plane speaking `dp` and a control plane speaking `cp`;
/// returns it with the welcome the data plane read.
async fn open_session<F: LinkFactory>(
    f: &F,
    dp: &[ProtocolVersion],
    cp: &[ProtocolVersion],
) -> Result<(Session<F>, Welcome), String> {
    let links = step("connecting", f.connect(dp, cp)).await?;
    open_session_over(f, dp, cp, links).await
}

/// Opens a session over a connected pair, as [`open_session`] does.
async fn open_session_over<F: LinkFactory>(
    f: &F,
    dp: &[ProtocolVersion],
    cp: &[ProtocolVersion],
    (mut dp_link, mut cp_link): (F::DpLink, F::CpLink),
) -> Result<(Session<F>, Welcome), String> {
    let sent = hello(dp, "boot-1")?;
    let (welcomed, heard) = step(
        "the handshake",
        join2(dp_link.hello(sent.clone()), answer_hello(&mut cp_link, cp)),
    )
    .await?;
    eq("the hello the control plane read", &heard?, &sent)?;
    let welcomed = ok("the data plane's hello", welcomed)?;
    let (dp_directives, dp_observations) =
        ok("the data plane's session", f.data_plane_session(dp_link))?;
    let (cp_directives, cp_observations) = ok(
        "the control plane's session",
        f.control_plane_session(cp_link),
    )?;
    Ok((
        Session {
            dp_directives,
            dp_observations,
            cp_directives,
            cp_observations,
        },
        welcomed,
    ))
}

/// A data plane in a session opened by a raw control plane at `accepted`.
async fn data_plane_session_against_raw<F: LinkFactory>(
    f: &F,
    dp: &[ProtocolVersion],
    accepted: ProtocolVersion,
) -> Result<(F::DpDirectives, F::DpObservations, F::Raw), String> {
    let links = step("connecting", f.data_plane_facing_raw(dp)).await?;
    data_plane_session_against_raw_over(f, dp, accepted, links).await
}

/// A data plane in a session opened by a raw control plane at `accepted`, over a connected
/// pair.
async fn data_plane_session_against_raw_over<F: LinkFactory>(
    f: &F,
    dp: &[ProtocolVersion],
    accepted: ProtocolVersion,
    (mut dp_link, mut raw): (F::DpLink, F::Raw),
) -> Result<(F::DpDirectives, F::DpObservations, F::Raw), String> {
    let (welcomed, sent) = step(
        "the handshake against a raw control plane",
        join2(dp_link.hello(hello(dp, "boot-1")?), async {
            raw.recv().await;
            raw.send(frame_bytes(
                V1_0,
                Message::Welcome(welcome(accepted, SESSION_EPOCH)),
            )?)
            .await
            .map_err(|e| e.to_string())
        }),
    )
    .await?;
    ok("the raw welcome", sent)?;
    eq(
        "the welcome the data plane read",
        &ok("the data plane's hello", welcomed)?,
        &welcome(accepted, SESSION_EPOCH),
    )?;
    let (directives, observations) = ok("the data plane's session", f.data_plane_session(dp_link))?;
    Ok((directives, observations, raw))
}

/// A control plane in a session it opened at `accepted` for a raw data plane.
async fn control_plane_session_against_raw<F: LinkFactory>(
    f: &F,
    cp: &[ProtocolVersion],
    accepted: ProtocolVersion,
) -> Result<(F::CpDirectives, F::CpObservations, F::Raw), String> {
    let links = step("connecting", f.control_plane_facing_raw(cp)).await?;
    control_plane_session_against_raw_over(f, accepted, links).await
}

/// A control plane in a session it opened at `accepted` for a raw data plane, over a connected
/// pair.
async fn control_plane_session_against_raw_over<F: LinkFactory>(
    f: &F,
    accepted: ProtocolVersion,
    (mut cp_link, mut raw): (F::CpLink, F::Raw),
) -> Result<(F::CpDirectives, F::CpObservations, F::Raw), String> {
    ok(
        "the raw hello",
        step(
            "the raw hello",
            raw.send(frame_bytes(
                V1_0,
                Message::Hello(hello(&[accepted], "boot-1")?),
            )?),
        )
        .await?,
    )?;
    let heard = step_ok("the control plane's next hello", cp_link.next_hello()).await?;
    eq(
        "the hello's supported list",
        &heard.supported,
        &vec![accepted],
    )?;
    step_ok(
        "the control plane's welcome",
        cp_link.welcome(welcome(accepted, SESSION_EPOCH)),
    )
    .await?;
    let welcome_frame = read_frame("the welcome", step("the welcome", raw.recv()).await?)?;
    eq(
        "the welcome on the wire",
        &welcome_frame.message,
        &Message::Welcome(welcome(accepted, SESSION_EPOCH)),
    )?;
    let (directives, observations) = ok(
        "the control plane's session",
        f.control_plane_session(cp_link),
    )?;
    Ok((directives, observations, raw))
}

// ---------------------------------------------------------------------------------------------
// The cases

async fn handshake_case<F: LinkFactory>(f: &F) -> Check {
    let (mut s, welcomed) = open_session(f, SUPPORTED, SUPPORTED).await?;
    eq(
        "the welcome the data plane read",
        &welcomed,
        &welcome(CURRENT, SESSION_EPOCH),
    )?;
    let sent = directive(1, SESSION_EPOCH);
    step_ok("sending a directive", s.cp_directives.send(sent.clone())).await?;
    let got = step_ok("receiving the directive", s.dp_directives.recv()).await?;
    eq("the directive", &got, &sent)?;
    step_ok("sending an observation", s.dp_observations.send(booted(1))).await?;
    let got = step_ok("receiving the observation", s.cp_observations.recv()).await?;
    eq("the observation", &got, &booted(1))
}

async fn mixed_minors<F: LinkFactory>(f: &F) -> Check {
    for (dp, cp) in [([V1_0], [CURRENT]), ([CURRENT], [V1_0])] {
        let label = format!("data plane {dp:?}, control plane {cp:?}");
        let (mut s, welcomed) = open_session(f, &dp, &cp)
            .await
            .map_err(|e| format!("{label}: {e}"))?;
        eq(
            &format!("{label}: the session version"),
            &welcomed.accepted_version,
            &V1_0,
        )?;
        let sent = directive(2, SESSION_EPOCH);
        step_ok("sending a directive", s.cp_directives.send(sent.clone())).await?;
        let got = step_ok("receiving the directive", s.dp_directives.recv()).await?;
        eq(&format!("{label}: the directive"), &got, &sent)?;
        step_ok(
            "sending a 1.1-only refusal",
            s.dp_observations.send(capacity_unknown(3)),
        )
        .await?;
        let got = step_ok("receiving the refusal", s.cp_observations.recv()).await?;
        eq(
            &format!("{label}: the refusal written down to 1.0"),
            &got,
            &insufficient_resources(3),
        )?;
    }
    Ok(())
}

async fn handshake_refused<F: LinkFactory>(f: &F) -> Check {
    let (mut dp_link, mut cp_link) = step("connecting", f.connect(SUPPORTED, SUPPORTED)).await?;
    let refusal = HandshakeRefused {
        reason: HandshakeRefusal::UnsupportedVersion {
            supported: vec![CURRENT],
        },
    };
    let (answer, refused) = step(
        "a refused handshake",
        join2(dp_link.hello(hello(SUPPORTED, "boot-1")?), async {
            cp_link.next_hello().await?;
            cp_link.refuse(refusal.clone()).await
        }),
    )
    .await?;
    ok("the control plane's refusal", refused)?;
    eq(
        "the data plane's answer",
        &answer,
        &Err(LinkError::Refused(refusal.clone())),
    )
}

async fn unknown_major_discarded<F: LinkFactory>(f: &F) -> Check {
    // The data plane offered only a 2.0 welcome: its hello ends in Closed when the peer
    // finishes, whatever order the transport delivers in.
    let (mut dp_link, mut raw) = step("connecting", f.data_plane_facing_raw(SUPPORTED)).await?;
    let foreign = welcome(CURRENT, Epoch::new(9));
    let frames = vec![frame_bytes(V2_0, Message::Welcome(foreign))?];
    let (answer, sent) = step(
        "a 2.0 welcome, then the peer's finish",
        join2(dp_link.hello(hello(SUPPORTED, "boot-1")?), async {
            read_frame("the hello", raw.recv().await)?;
            send_then_finish(&mut raw, frames).await
        }),
    )
    .await?;
    sent?;
    eq(
        "the data plane's answer to a 2.0 welcome alone",
        &answer,
        &Err(LinkError::Closed),
    )?;

    // The data plane: after a 2.0 welcome (epoch 9), the 1.x one (epoch 5) is handed up.
    let (mut dp_link, mut raw) = step("connecting", f.data_plane_facing_raw(SUPPORTED)).await?;
    let mut hello_sent = pin!(dp_link.hello(hello(SUPPORTED, "boot-1")?));
    let bytes = step("the hello", beside(hello_sent.as_mut(), raw.recv()))
        .await?
        .map_err(|r| format!("the hello completed before any welcome: {r:?}"))?;
    read_frame("the hello", bytes)?;
    let foreign = welcome(CURRENT, Epoch::new(9));
    let readable = welcome(CURRENT, SESSION_EPOCH);
    for (version, w) in [(V2_0, foreign), (CURRENT, readable.clone())] {
        let sent = beside(
            hello_sent.as_mut(),
            raw.send(frame_bytes(version, Message::Welcome(w))?),
        );
        ok(
            "the raw welcome",
            step("the raw welcome", sent).await?.map_err(|r| {
                format!("the hello completed with {r:?} before the readable welcome was sent")
            })?,
        )?;
    }
    let welcomed = step_ok("the data plane's hello", hello_sent).await?;
    eq("the welcome handed up", &welcomed, &readable)?;

    // The control plane, drained to Closed: the 1.x hello is handed up, the 2.0 one never.
    let (mut cp_link, mut raw) = step("connecting", f.control_plane_facing_raw(SUPPORTED)).await?;
    let foreign = hello(&[V2_0], "boot-foreign")?;
    let readable = hello(SUPPORTED, "boot-1")?;
    let frames = vec![
        frame_bytes(V2_0, Message::Hello(foreign))?,
        frame_bytes(CURRENT, Message::Hello(readable.clone()))?,
    ];
    drain_after(
        "hellos after a 2.0 one",
        &mut raw,
        frames,
        &mut Hellos(&mut cp_link),
        &set([hello_key(&readable)]),
        hello_key,
    )
    .await
}

async fn session_version_held<F: LinkFactory>(f: &F) -> Check {
    // The data plane in a 1.0 session: a 1.1 and a 2.0 directive are discarded.
    let (mut directives, _observations, mut raw) =
        data_plane_session_against_raw(f, SUPPORTED, V1_0).await?;
    let sent = [
        (CURRENT, directive(11, SESSION_EPOCH)),
        (V2_0, directive(12, SESSION_EPOCH)),
        (V1_0, directive(13, SESSION_EPOCH)),
        (V1_0, directive(14, SESSION_EPOCH)),
    ];
    let frames = sent
        .iter()
        .map(|(version, d)| frame_bytes(*version, Message::Directive(d.clone())))
        .collect::<Result<_, _>>()?;
    drain_after(
        "directives of a 1.0 session",
        &mut raw,
        frames,
        &mut directives,
        &set([&sent[2].1, &sent[3].1].map(directive_key)),
        directive_key,
    )
    .await?;

    // The control plane in a 1.0 session: a 1.1 and a 2.0 observation are discarded.
    let (_directives, mut observations, mut raw) =
        control_plane_session_against_raw(f, SUPPORTED, V1_0).await?;
    let sent = [
        (CURRENT, booted(21)),
        (V2_0, booted(22)),
        (V1_0, booted(23)),
        (V1_0, booted(24)),
    ];
    let frames = sent
        .iter()
        .map(|(version, o)| frame_bytes(*version, Message::Observation(o.clone())))
        .collect::<Result<_, _>>()?;
    drain_after(
        "observations of a 1.0 session",
        &mut raw,
        frames,
        &mut observations,
        &set([&sent[2].1, &sent[3].1].map(observation_key)),
        observation_key,
    )
    .await
}

async fn welcome_outside_offer<F: LinkFactory>(f: &F) -> Check {
    // A data plane that speaks 1.0 is welcomed at 1.1.
    let (mut dp_link, mut raw) = step("connecting", f.data_plane_facing_raw(&[V1_0])).await?;
    let (answer, sent) = step(
        "a welcome above the offer",
        join2(dp_link.hello(hello(&[V1_0], "boot-1")?), async {
            raw.recv().await;
            raw.send(frame_bytes(
                V1_0,
                Message::Welcome(welcome(CURRENT, SESSION_EPOCH)),
            )?)
            .await
            .map_err(|e| e.to_string())
        }),
    )
    .await?;
    ok("the raw welcome", sent)?;
    if !matches!(answer, Err(LinkError::Version(_))) {
        return Err(format!(
            "the data plane's answer to a 1.1 welcome: got {answer:?}, want a version error"
        ));
    }
    if f.data_plane_session(dp_link).is_ok() {
        return Err("the data plane opened a session at a version it does not speak".to_owned());
    }

    // A control plane that speaks 1.x is asked to welcome at 2.0.
    let (mut cp_link, mut raw) = step("connecting", f.control_plane_facing_raw(SUPPORTED)).await?;
    step_ok(
        "the raw hello",
        raw.send(frame_bytes(
            CURRENT,
            Message::Hello(hello(SUPPORTED, "boot-1")?),
        )?),
    )
    .await?;
    step_ok("the control plane's next hello", cp_link.next_hello()).await?;
    let answer = step(
        "a welcome at 2.0",
        cp_link.welcome(welcome(V2_0, SESSION_EPOCH)),
    )
    .await?;
    if !matches!(answer, Err(LinkError::Version(_))) {
        return Err(format!(
            "the control plane's welcome at 2.0: got {answer:?}, want a version error"
        ));
    }
    if f.control_plane_session(cp_link).is_ok() {
        return Err("the control plane opened a session at a version it does not speak".to_owned());
    }
    Ok(())
}

async fn handshake_in_oldest_major<F: LinkFactory>(f: &F) -> Check {
    let both = [V2_0, CURRENT];

    let (mut dp_link, mut raw) = step("connecting", f.data_plane_facing_raw(&both)).await?;
    let sent = hello(&both, "boot-1")?;
    let mut hello_sent = pin!(dp_link.hello(sent.clone()));
    let bytes = step("the hello", beside(hello_sent.as_mut(), raw.recv()))
        .await?
        .map_err(|r| format!("the hello completed before any welcome: {r:?}"))?;
    eq(
        "the hello on the wire",
        &read_frame("the hello", bytes)?,
        &Frame {
            protocol_version: CURRENT,
            message: Message::Hello(sent),
        },
    )?;

    let (mut cp_link, mut raw) = step("connecting", f.control_plane_facing_raw(&both)).await?;
    step_ok(
        "the raw hello",
        raw.send(frame_bytes(
            CURRENT,
            Message::Hello(hello(&both, "boot-1")?),
        )?),
    )
    .await?;
    step_ok("the control plane's next hello", cp_link.next_hello()).await?;
    let refusal = HandshakeRefused {
        reason: HandshakeRefusal::UnsupportedVersion {
            supported: both.to_vec(),
        },
    };
    step_ok("a refusal", cp_link.refuse(refusal.clone())).await?;
    eq(
        "the refusal on the wire",
        &read_frame("the refusal", step("the refusal", raw.recv()).await?)?,
        &Frame {
            protocol_version: CURRENT,
            message: Message::HandshakeRefused(refusal),
        },
    )
}

async fn wire_encoding<F: LinkFactory>(f: &F) -> Check {
    // The data plane, in a 1.1 session.
    let (mut dp_link, mut raw) = step("connecting", f.data_plane_facing_raw(SUPPORTED)).await?;
    let sent = hello(SUPPORTED, "boot-1")?;
    let readable = welcome(CURRENT, SESSION_EPOCH);
    {
        let mut hello_sent = pin!(dp_link.hello(sent.clone()));
        let bytes = step("the hello", beside(hello_sent.as_mut(), raw.recv()))
            .await?
            .map_err(|r| format!("the hello completed before any welcome: {r:?}"))?;
        eq(
            "the hello on the wire",
            &read_frame("the hello", bytes)?,
            &Frame {
                protocol_version: CURRENT,
                message: Message::Hello(sent),
            },
        )?;
        let welcome_sent = beside(
            hello_sent.as_mut(),
            raw.send(frame_bytes(CURRENT, Message::Welcome(readable.clone()))?),
        );
        ok(
            "the raw welcome",
            step("the raw welcome", welcome_sent)
                .await?
                .map_err(|r| format!("{r:?}"))?,
        )?;
        eq(
            "the welcome read",
            &step_ok("the data plane's hello", hello_sent).await?,
            &readable,
        )?;
    }

    // Every golden directive and observation, through both ends, in a 1.0 and a 1.1 session.
    let golden = golden_frames()?;
    let (directives, observations) = golden_messages(&golden)?;
    for session in [V1_0, CURRENT] {
        let label = format!("a {}.{} session", session.major, session.minor);
        data_plane_wire(f, session, &directives, &observations)
            .await
            .map_err(|e| format!("the data plane in {label}: {e}"))?;
        control_plane_wire(f, session, &directives, &observations)
            .await
            .map_err(|e| format!("the control plane in {label}: {e}"))?;
    }
    Ok(())
}

/// Whether a session at `session` admits the golden frame's version.
fn admitted(session: ProtocolVersion, g: &Golden) -> bool {
    admit_frame(Some(session), g.frame.protocol_version, SUPPORTED).is_ok()
}

/// LK-08 for the data plane: every golden observation it sends is on the wire as its frame,
/// stamped `session` and written down to it; every golden directive frame the session admits,
/// written by the peer as the fixture's bytes, is read as itself.
async fn data_plane_wire<F: LinkFactory>(
    f: &F,
    session: ProtocolVersion,
    directives: &[(&Golden, Directive)],
    observations: &[(&Golden, Observation)],
) -> Check {
    let (mut source, sink, mut raw) = data_plane_session_against_raw(f, SUPPORTED, session).await?;
    let want = set(observations.iter().map(|(_, o)| {
        frame_json(&Frame {
            protocol_version: session,
            message: Message::Observation(Observation {
                kind: o.kind.clone().written_in(session),
                ..o.clone()
            }),
        })
    }));
    let (sent, written) = step(
        "golden observations",
        join2(
            async {
                for (_, o) in observations {
                    sink.send(o.clone()).await?;
                }
                Ok::<(), LinkError>(())
            },
            raw_collect("golden observations on the wire", &mut raw, &want),
        ),
    )
    .await?;
    ok("sending golden observations", sent)?;
    written?;

    let readable: Vec<_> = directives
        .iter()
        .filter(|(g, _)| admitted(session, g))
        .collect();
    let want = set(readable.iter().map(|(_, d)| directive_json(d)));
    let (sent, read) = step(
        "golden directive bytes",
        join2(
            async {
                for (g, _) in &readable {
                    raw.send(g.bytes.clone()).await?;
                }
                Ok::<(), LinkError>(())
            },
            collect(
                "golden directives read",
                &mut source,
                &want,
                &want,
                directive_json,
            ),
        ),
    )
    .await?;
    ok("the peer's golden directives", sent)?;
    read.map(|_| ())
}

/// LK-08 for the control plane: every golden directive it sends is on the wire as its frame,
/// stamped `session` and written down to it (`DirectiveKind::written_in`); every golden observation frame the session admits, written by the peer
/// as the fixture's bytes, is read as itself.
async fn control_plane_wire<F: LinkFactory>(
    f: &F,
    session: ProtocolVersion,
    directives: &[(&Golden, Directive)],
    observations: &[(&Golden, Observation)],
) -> Check {
    let (sink, mut source, mut raw) =
        control_plane_session_against_raw(f, SUPPORTED, session).await?;
    let want = set(directives.iter().map(|(_, d)| {
        frame_json(&Frame {
            protocol_version: session,
            message: Message::Directive(Directive {
                kind: d.kind.clone().written_in(session),
                ..d.clone()
            }),
        })
    }));
    let (sent, written) = step(
        "golden directives",
        join2(
            async {
                for (_, d) in directives {
                    sink.send(d.clone()).await?;
                }
                Ok::<(), LinkError>(())
            },
            raw_collect("golden directives on the wire", &mut raw, &want),
        ),
    )
    .await?;
    ok("sending golden directives", sent)?;
    written?;

    let readable: Vec<_> = observations
        .iter()
        .filter(|(g, _)| admitted(session, g))
        .collect();
    let want = set(readable.iter().map(|(_, o)| observation_json(o)));
    let (sent, read) = step(
        "golden observation bytes",
        join2(
            async {
                for (g, _) in &readable {
                    raw.send(g.bytes.clone()).await?;
                }
                Ok::<(), LinkError>(())
            },
            collect(
                "golden observations read",
                &mut source,
                &want,
                &want,
                observation_json,
            ),
        ),
    )
    .await?;
    ok("the peer's golden observations", sent)?;
    read.map(|_| ())
}

async fn malformed_discarded<F: LinkFactory>(f: &F) -> Check {
    let garbage = || -> Vec<Vec<u8>> {
        [
            &b"not json"[..],
            br#"{"message":{"hello":{}}}"#,
            br#"{"protocol_version":{"major":1,"minor":1},"message":{"hello":{"nonsense":1}}}"#,
        ]
        .map(<[u8]>::to_vec)
        .to_vec()
    };

    let (mut cp_link, mut raw) = step("connecting", f.control_plane_facing_raw(SUPPORTED)).await?;
    let readable = hello(SUPPORTED, "boot-2")?;
    let mut frames = garbage();
    frames.push(frame_bytes(CURRENT, Message::Hello(readable.clone()))?);
    drain_after(
        "hellos after garbage",
        &mut raw,
        frames,
        &mut Hellos(&mut cp_link),
        &set([hello_key(&readable)]),
        hello_key,
    )
    .await?;

    let (mut directives, _observations, mut raw) =
        data_plane_session_against_raw(f, SUPPORTED, CURRENT).await?;
    let sent = directive(41, SESSION_EPOCH);
    let mut frames = garbage();
    frames.push(frame_bytes(CURRENT, Message::Directive(sent.clone()))?);
    drain_after(
        "directives after garbage",
        &mut raw,
        frames,
        &mut directives,
        &set([directive_key(&sent)]),
        directive_key,
    )
    .await
}

async fn at_least_once<F: LinkFactory>(f: &F) -> Check {
    let (mut s, _) = open_session(f, SUPPORTED, SUPPORTED).await?;
    let n = u128::try_from(f.capacity() * 2 + 1).map_err(|e| e.to_string())?;

    let sent: Vec<Directive> = (1..=n).map(|i| directive(i, SESSION_EPOCH)).collect();
    let want = set(sent.iter().map(directive_key));
    let cp_directives = &s.cp_directives;
    let (sends, got) = join2(
        async {
            for d in &sent {
                cp_directives.send(d.clone()).await?;
            }
            Ok::<(), LinkError>(())
        },
        collect(
            "directives past the capacity",
            &mut s.dp_directives,
            &want,
            &want,
            directive_key,
        ),
    )
    .await;
    ok("sending directives", sends)?;
    got?;

    let sent: Vec<Observation> = (1..=n).map(booted).collect();
    let want = set(sent.iter().map(observation_key));
    let dp_observations = &s.dp_observations;
    let (sends, got) = join2(
        async {
            for o in &sent {
                dp_observations.send(o.clone()).await?;
            }
            Ok::<(), LinkError>(())
        },
        collect(
            "observations past the capacity",
            &mut s.cp_observations,
            &want,
            &want,
            observation_key,
        ),
    )
    .await;
    ok("sending observations", sends)?;
    got.map(|_| ())
}

/// Sends with no reader until a send waits, which must happen within `capacity + 1` sends;
/// then reads everything back, the waiting send included.
async fn fill_and_drain<T: Clone + Debug, K: Ord + Clone + Debug>(
    what: &str,
    capacity: usize,
    make: impl Fn(u128) -> T,
    sink: &impl Transmit<T>,
    source: &mut impl Receive<T>,
    key: impl Fn(&T) -> K + Copy,
) -> Check {
    let mut accepted = Vec::new();
    for i in 1..=capacity + 1 {
        let item = make(u128::try_from(i).map_err(|e| e.to_string())?);
        let mut sending = pin!(sink.transmit(item.clone()));
        accepted.push(item);
        if still_waits(what, sending.as_mut()).await? {
            let want = set(accepted.iter().map(key));
            let (sent, got) = join2(sending, collect(what, source, &want, &want, key)).await;
            ok(&format!("{what}: the waiting send"), sent)?;
            return got.map(|_| ());
        }
    }
    Err(format!(
        "{what}: {} sends completed with no reader, over a capacity of {capacity}",
        capacity + 1
    ))
}

async fn backpressure<F: LinkFactory>(f: &F) -> Check {
    let (mut s, _) = open_session(f, SUPPORTED, SUPPORTED).await?;
    let capacity = f.capacity();
    let cp_directives = &s.cp_directives;
    fill_and_drain(
        "directives",
        capacity,
        |i| directive(i, SESSION_EPOCH),
        cp_directives,
        &mut s.dp_directives,
        directive_key,
    )
    .await?;
    let dp_observations = &s.dp_observations;
    fill_and_drain(
        "observations",
        capacity,
        booted,
        dp_observations,
        &mut s.cp_observations,
        observation_key,
    )
    .await
}

async fn close_seen_by_data_plane<F: LinkFactory>(f: &F) -> Check {
    let (mut s, _) = open_session(f, SUPPORTED, SUPPORTED).await?;
    let sent = [directive(51, SESSION_EPOCH), directive(52, SESSION_EPOCH)];
    for d in &sent {
        step_ok("a directive", s.cp_directives.send(d.clone())).await?;
    }
    step(
        "the control plane's close",
        f.close_control_plane(s.cp_directives, s.cp_observations),
    )
    .await?;
    let want = set(sent.iter().map(directive_key));
    until_closed(
        "the data plane's receive after the close",
        &mut s.dp_directives,
        &want,
        directive_key,
    )
    .await?;
    eq(
        "a receive after Closed",
        &step("a receive after Closed", s.dp_directives.recv()).await?,
        &Err(LinkError::Closed),
    )?;
    eq(
        "a send after Closed",
        &step("a send after Closed", s.dp_observations.send(booted(53))).await?,
        &Err(LinkError::Closed),
    )
}

async fn close_seen_by_control_plane<F: LinkFactory>(f: &F) -> Check {
    let (mut s, _) = open_session(f, SUPPORTED, SUPPORTED).await?;
    let sent = [booted(61), booted(62)];
    for o in &sent {
        step_ok("an observation", s.dp_observations.send(o.clone())).await?;
    }
    step(
        "the data plane's close",
        f.close_data_plane(s.dp_directives, s.dp_observations),
    )
    .await?;
    let want = set(sent.iter().map(observation_key));
    until_closed(
        "the control plane's receive after the close",
        &mut s.cp_observations,
        &want,
        observation_key,
    )
    .await?;
    eq(
        "a receive after Closed",
        &step("a receive after Closed", s.cp_observations.recv()).await?,
        &Err(LinkError::Closed),
    )?;
    eq(
        "a send after Closed",
        &step(
            "a send after Closed",
            s.cp_directives.send(directive(63, SESSION_EPOCH)),
        )
        .await?,
        &Err(LinkError::Closed),
    )
}

async fn close_reports_in_flight<F: LinkFactory>(f: &F) -> Check {
    let unread = f.capacity().min(3);

    // The control plane closes with observations unread.
    let (mut s, _) = open_session(f, SUPPORTED, SUPPORTED).await?;
    for i in 0..unread {
        let n = 71 + u128::try_from(i).map_err(|e| e.to_string())?;
        step_ok("an observation", s.dp_observations.send(booted(n))).await?;
    }
    let discarded = step(
        "the control plane's close",
        f.close_control_plane(s.cp_directives, s.cp_observations),
    )
    .await?;
    reported("observations the close discarded", discarded, unread)?;
    until_closed(
        "the data plane's receive after the close",
        &mut s.dp_directives,
        &BTreeSet::new(),
        directive_key,
    )
    .await?;
    eq(
        "a send after the close",
        &step("a send after the close", s.dp_observations.send(booted(79))).await?,
        &Err(LinkError::Closed),
    )?;

    // The data plane closes with directives unread.
    let (s, _) = open_session(f, SUPPORTED, SUPPORTED).await?;
    for i in 0..unread {
        let n = 81 + u128::try_from(i).map_err(|e| e.to_string())?;
        step_ok(
            "a directive",
            s.cp_directives.send(directive(n, SESSION_EPOCH)),
        )
        .await?;
    }
    let discarded = step(
        "the data plane's close",
        f.close_data_plane(s.dp_directives, s.dp_observations),
    )
    .await?;
    reported("directives the close discarded", discarded, unread)
}

/// LK-14's count: at least one when `unread` frames were unread. A lower bound, not exact
/// (see [`LinkCase::CloseReportsInFlight`]).
fn reported(what: &str, discarded: usize, unread: usize) -> Check {
    if unread == 0 || discarded >= 1 {
        Ok(())
    } else {
        Err(format!(
            "{what}: got {discarded} with {unread} unread, want at least 1"
        ))
    }
}

async fn every_epoch_handed_up<F: LinkFactory>(f: &F) -> Check {
    let (mut s, welcomed) = open_session(f, SUPPORTED, SUPPORTED).await?;
    let stale = Epoch::new(welcomed.epoch.get().saturating_sub(2));
    let ahead = Epoch::new(welcomed.epoch.get().saturating_add(4));
    let sent = [
        directive(91, stale),
        directive(92, welcomed.epoch),
        directive(93, ahead),
    ];
    for d in &sent {
        step_ok("a directive", s.cp_directives.send(d.clone())).await?;
    }
    let want = set(sent.iter().map(directive_key));
    collect(
        "directives of three epochs",
        &mut s.dp_directives,
        &want,
        &want,
        directive_key,
    )
    .await
    .map(|_| ())
}

async fn discards_counted<F: LinkFactory>(f: &F) -> Check {
    let garbage = || b"not json".to_vec();

    // The data plane before the welcome: a directive (of a session not entered yet), garbage
    // and a 2.0 welcome are discarded; the hello ends in Closed when the peer finishes.
    let (mut dp_link, mut raw) = step("connecting", f.data_plane_facing_raw(SUPPORTED)).await?;
    let frames = vec![
        frame_bytes(CURRENT, Message::Directive(directive(0xa1, SESSION_EPOCH)))?,
        garbage(),
        frame_bytes(V2_0, Message::Welcome(welcome(CURRENT, SESSION_EPOCH)))?,
    ];
    let (answer, sent) = step(
        "frames before any welcome",
        join2(dp_link.hello(hello(SUPPORTED, "boot-1")?), async {
            read_frame("the hello", raw.recv().await)?;
            send_then_finish(&mut raw, frames).await
        }),
    )
    .await?;
    sent?;
    eq(
        "the hello's answer when no welcome came",
        &answer,
        &Err(LinkError::Closed),
    )?;
    at_least(
        "the data plane's discards before the welcome",
        f.data_plane_discards(&dp_link),
        counts(1, 1, 1),
    )?;
    // Unanswered (ADR 0042): the next frame the data plane writes is a second hello, not an
    // answer to the directive.
    let again = hello(SUPPORTED, "boot-2")?;
    eq(
        "a second hello's answer",
        &step("a second hello", dp_link.hello(again.clone())).await?,
        &Err(LinkError::Closed),
    )?;
    eq(
        "the frame the data plane wrote after its first hello",
        &read_frame("the next frame", step("the next frame", raw.recv()).await?)?.message,
        &Message::Hello(again),
    )?;

    // The data plane in a session: a duplicate welcome, a hello and an observation are
    // unexpected; garbage and a 2.0 directive are discarded too; the readable directive arrives.
    let (mut directives, _observations, mut raw) =
        data_plane_session_against_raw(f, SUPPORTED, CURRENT).await?;
    let readable = directive(0xa2, SESSION_EPOCH);
    let frames = vec![
        frame_bytes(CURRENT, Message::Welcome(welcome(CURRENT, SESSION_EPOCH)))?,
        frame_bytes(CURRENT, Message::Hello(hello(SUPPORTED, "boot-3")?))?,
        frame_bytes(CURRENT, Message::Observation(booted(0xa3)))?,
        garbage(),
        frame_bytes(V2_0, Message::Directive(directive(0xa4, SESSION_EPOCH)))?,
        frame_bytes(CURRENT, Message::Directive(readable.clone()))?,
    ];
    drain_after(
        "directives among discarded frames",
        &mut raw,
        frames,
        &mut directives,
        &set([directive_key(&readable)]),
        directive_key,
    )
    .await?;
    at_least(
        "the data plane's discards in a session",
        f.data_plane_session_discards(&directives),
        counts(1, 1, 3),
    )?;

    // The control plane before a hello: a directive and an observation are unexpected;
    // garbage and a 2.0 hello are discarded too; the readable hello arrives.
    let (mut cp_link, mut raw) = step("connecting", f.control_plane_facing_raw(SUPPORTED)).await?;
    let readable = hello(SUPPORTED, "boot-4")?;
    let frames = vec![
        frame_bytes(CURRENT, Message::Directive(directive(0xa5, SESSION_EPOCH)))?,
        frame_bytes(CURRENT, Message::Observation(booted(0xa6)))?,
        garbage(),
        frame_bytes(V2_0, Message::Hello(hello(&[V2_0], "boot-foreign")?))?,
        frame_bytes(CURRENT, Message::Hello(readable.clone()))?,
    ];
    drain_after(
        "hellos among discarded frames",
        &mut raw,
        frames,
        &mut Hellos(&mut cp_link),
        &set([hello_key(&readable)]),
        hello_key,
    )
    .await?;
    at_least(
        "the control plane's discards before a session",
        f.control_plane_discards(&cp_link),
        counts(1, 1, 2),
    )?;

    // The control plane in a session: a welcome, a hello and a directive are unexpected;
    // garbage and a 2.0 observation are discarded too; the readable observation arrives.
    let (_directives, mut observations, mut raw) =
        control_plane_session_against_raw(f, SUPPORTED, CURRENT).await?;
    let readable = booted(0xa7);
    let frames = vec![
        frame_bytes(CURRENT, Message::Welcome(welcome(CURRENT, SESSION_EPOCH)))?,
        frame_bytes(CURRENT, Message::Hello(hello(SUPPORTED, "boot-5")?))?,
        frame_bytes(CURRENT, Message::Directive(directive(0xa8, SESSION_EPOCH)))?,
        garbage(),
        frame_bytes(V2_0, Message::Observation(booted(0xa9)))?,
        frame_bytes(CURRENT, Message::Observation(readable.clone()))?,
    ];
    drain_after(
        "observations among discarded frames",
        &mut raw,
        frames,
        &mut observations,
        &set([observation_key(&readable)]),
        observation_key,
    )
    .await?;
    at_least(
        "the control plane's discards in a session",
        f.control_plane_session_discards(&observations),
        counts(1, 1, 3),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_number_the_cases_in_order_when_they_are_listed() {
        // given
        let cases = LinkCase::ALL;

        // when
        let ids: Vec<&str> = cases.iter().map(|c| c.id()).collect();

        // then
        let want: Vec<String> = (1..=cases.len()).map(|i| format!("LK-{i:02}")).collect();
        assert_eq!(ids, want);
        assert_eq!(set(cases).len(), cases.len());
    }

    #[test]
    fn should_list_each_failure_when_a_report_is_shown() {
        // given
        let report = LinkContractReport {
            failures: vec![CaseFailure {
                case: LinkCase::Backpressure,
                detail: "no send waited".to_owned(),
            }],
        };

        // when
        let shown = report.to_string();

        // then
        assert!(!report.is_green());
        assert_eq!(report.failed_cases(), set([LinkCase::Backpressure]));
        assert_eq!(
            shown,
            "link contract: 1 case(s) failed\n- LK-11 Backpressure: no send waited\n"
        );
    }

    #[test]
    fn should_write_a_capacity_unknown_refusal_down_when_the_session_is_1_0() {
        // given
        let current = capacity_unknown(1);

        // when
        let written = current.kind.written_in(V1_0);

        // then
        assert_eq!(written, insufficient_resources(1).kind);
    }
}
