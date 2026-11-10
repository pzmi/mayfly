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
//! (LK-14), and the peer sees the close (LK-12, LK-13). End-to-end acknowledgement is the
//! protocol's: the relay re-sends a directive until an observation answers it definitely, and
//! both ends are idempotent (plan §3.2, `formal/link.qnt`).
//!
//! **Order is never assumed.** Every case that receives several frames collects them as a set
//! until all it waits for have arrived, tolerates duplicates, and tells a forbidden frame from
//! an allowed one by content, never by position. The suite's own tests prove it: a transport
//! that delivers in reverse and duplicates frames passes every case
//! (`crates/mayfly-adapter-link/tests/link_contract.rs`, `ReorderingLink`).
//!
//! **No case relies on time.** Each case runs on its own current-thread Tokio runtime, with the
//! clock paused unless the factory says otherwise ([`LinkFactory::paused_clock`]); a bounded
//! wait ([`crate::poll::within`]) turns a hang into a failure with a reason, at once under the
//! paused clock. Nothing is spawned: concurrent halves are polled together on the case's task.
//!
//! [`check_link_contract`] returns a report of failed cases instead of panicking: the suite's
//! own tests use it to show deliberately broken transports failing named cases (ver §5:
//! `SilentDropLink`, `NoVersionCheckLink`, `DeafCloseLink`, `CorruptingLink`). A factory that
//! cannot build what a case needs fails that case: no case is skipped.

use std::collections::BTreeSet;
use std::fmt::{self, Debug, Display};
use std::future::{Future, poll_fn};
use std::pin::{Pin, pin};
use std::task::Poll;
use std::time::Duration;

use mayfly_domain::{
    AttemptId, Deadline, DirectiveId, Epoch, Generation, HostId, Millis, ObservationId,
    OwnedVmName, SecretHandleId,
};
use mayfly_protocol::{
    AttemptRef, CURRENT, Directive, DirectiveKind, DirectiveRef, DirectiveSink, DirectiveSource,
    Frame, HandshakeInitiator, HandshakeRefusal, HandshakeRefused, HandshakeResponder, Hello,
    LinkError, Message, Mode, Observation, ObservationKind, ObservationSink, ObservationSource,
    ProtocolVersion, RefusalReason, ResourceDimension, SUPPORTED, SecretHandle, Welcome, negotiate,
};

use crate::poll::within;

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

    /// The most frames one direction holds while its reader reads nothing: a send beyond it
    /// waits (LK-11). At least 1.
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
    /// LK-04: a handshake frame of a major the reader does not speak is never handed up; the
    /// next readable one is.
    UnknownMajorDiscarded,
    /// LK-05: within a session, a frame of a newer minor or another major is never handed up,
    /// in either direction.
    SessionVersionHeld,
    /// LK-06: a welcome in a version the end does not speak opens no session, on either end.
    WelcomeOutsideOffer,
    /// LK-07: handshake frames are written in the oldest major the writer speaks.
    HandshakeInOldestMajor,
    /// LK-08: every message survives the serde round trip: the bytes on the wire are the JSON
    /// frame of what was sent, stamped with the session's version, and a JSON frame written by
    /// the peer is read as itself.
    WireEncoding,
    /// LK-09: bytes that are not a frame are discarded; the link stays open and the next frame
    /// is handed up.
    MalformedDiscarded,
    /// LK-10: every accepted frame is delivered while the link is open (at least once, any
    /// order), in both directions, past the capacity.
    AtLeastOnce,
    /// LK-11: a direction holds at most its capacity unread; the next send waits, and completes
    /// once the reader takes a frame, losing none.
    Backpressure,
    /// LK-12: the control plane's close is seen by the data plane: its receive ends in
    /// `Closed`, and its sends then fail with `Closed`.
    CloseSeenByDataPlane,
    /// LK-13: the data plane's close is seen by the control plane, likewise.
    CloseSeenByControlPlane,
    /// LK-14: a close discards the frames its side had not received and reports how many.
    CloseReportsInFlight,
    /// LK-15: a directive of any epoch is handed up: fencing is the data plane's (I09, ADR
    /// 0042), so a stale frame can be answered `OutcomeUnknown{stale_epoch}`.
    EveryEpochHandedUp,
}

impl LinkCase {
    /// Every case, in id order.
    pub const ALL: [Self; 15] = [
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

/// Receives until `Closed`; every frame before it must be one of `allowed`. At most
/// `4 × allowed + 4` receives, each bounded.
async fn until_closed<T: Debug, K: Ord>(
    what: &str,
    source: &mut impl Receive<T>,
    allowed: &BTreeSet<K>,
    key: impl Fn(&T) -> K,
) -> Check {
    for _ in 0..allowed.len() * 4 + 4 {
        match step(what, source.receive()).await? {
            Err(LinkError::Closed) => return Ok(()),
            Err(other) => return Err(format!("{what}: got {other}, want Closed")),
            Ok(item) if allowed.contains(&key(&item)) => {}
            Ok(item) => return Err(format!("{what}: handed up {item:?}, which was never sent")),
        }
    }
    Err(format!("{what}: never ended in Closed"))
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

/// A directive with every optional part filled: an attempt, a VM name and a secret handle.
fn launch_runner() -> Result<Directive, String> {
    let generation = Generation::new(1).ok_or("generation 1")?;
    Ok(Directive {
        directive_id: DirectiveId::from_u128(0xd1),
        epoch: SESSION_EPOCH,
        attempt: Some(AttemptRef {
            attempt_id: AttemptId::from_u128(0x0a11_ce01),
            generation,
        }),
        deadline: Deadline::at(Millis::new(1_700_000_060_000)),
        kind: DirectiveKind::LaunchRunner {
            vm_name: ok(
                "vm name",
                OwnedVmName::parse("mf-pool0-host0-s1-g1-0a11ce01"),
            )?,
            launch: SecretHandle {
                handle_id: SecretHandleId::from_u128(0x05ec),
            },
        },
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
    let (mut dp_link, mut cp_link) = step("connecting", f.connect(dp, cp)).await?;
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
    let (mut dp_link, mut raw) = step("connecting", f.data_plane_facing_raw(dp)).await?;
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
    let (mut cp_link, mut raw) = step("connecting", f.control_plane_facing_raw(cp)).await?;
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
    // The data plane: a 2.0 welcome (epoch 9) is not handed up; the 1.x one (epoch 5) is.
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

    // The control plane: a 2.0 hello is not handed up; the 1.x one is.
    let (mut cp_link, mut raw) = step("connecting", f.control_plane_facing_raw(SUPPORTED)).await?;
    let foreign = hello(&[V2_0], "boot-foreign")?;
    let readable = hello(SUPPORTED, "boot-1")?;
    step_ok(
        "a 2.0 hello",
        raw.send(frame_bytes(V2_0, Message::Hello(foreign))?),
    )
    .await?;
    step_ok(
        "a 1.x hello",
        raw.send(frame_bytes(CURRENT, Message::Hello(readable.clone()))?),
    )
    .await?;
    let heard = step_ok("the control plane's next hello", cp_link.next_hello()).await?;
    eq("the hello handed up", &heard, &readable)
}

async fn session_version_held<F: LinkFactory>(f: &F) -> Check {
    // The data plane in a 1.0 session: a 1.1 and a 2.0 directive are discarded.
    let (mut directives, _observations, mut raw) =
        data_plane_session_against_raw(f, SUPPORTED, V1_0).await?;
    let frames = [
        (CURRENT, directive(11, SESSION_EPOCH)),
        (V2_0, directive(12, SESSION_EPOCH)),
        (V1_0, directive(13, SESSION_EPOCH)),
        (V1_0, directive(14, SESSION_EPOCH)),
    ];
    for (version, d) in &frames {
        step_ok(
            "a raw directive",
            raw.send(frame_bytes(*version, Message::Directive(d.clone()))?),
        )
        .await?;
    }
    let want = set([&frames[2].1, &frames[3].1].map(directive_key));
    collect(
        "directives of a 1.0 session",
        &mut directives,
        &want,
        &want,
        directive_key,
    )
    .await?;

    // The control plane in a 1.0 session: a 1.1 and a 2.0 observation are discarded.
    let (_directives, mut observations, mut raw) =
        control_plane_session_against_raw(f, SUPPORTED, V1_0).await?;
    let frames = [
        (CURRENT, booted(21)),
        (V2_0, booted(22)),
        (V1_0, booted(23)),
        (V1_0, booted(24)),
    ];
    for (version, o) in &frames {
        step_ok(
            "a raw observation",
            raw.send(frame_bytes(*version, Message::Observation(o.clone()))?),
        )
        .await?;
    }
    let want = set([&frames[2].1, &frames[3].1].map(observation_key));
    collect(
        "observations of a 1.0 session",
        &mut observations,
        &want,
        &want,
        observation_key,
    )
    .await?;
    Ok(())
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
    let (mut directives, observations) =
        ok("the data plane's session", f.data_plane_session(dp_link))?;
    step_ok("an observation", observations.send(capacity_unknown(31))).await?;
    eq(
        "the observation on the wire",
        &read_frame(
            "the observation",
            step("the observation", raw.recv()).await?,
        )?,
        &Frame {
            protocol_version: CURRENT,
            message: Message::Observation(capacity_unknown(31)),
        },
    )?;
    let rich = launch_runner()?;
    step_ok(
        "a raw directive",
        raw.send(frame_bytes(CURRENT, Message::Directive(rich.clone()))?),
    )
    .await?;
    eq(
        "the directive read",
        &step_ok("the directive", directives.recv()).await?,
        &rich,
    )?;

    // The control plane, in a 1.0 session.
    let (directives, mut observations, mut raw) =
        control_plane_session_against_raw(f, SUPPORTED, V1_0).await?;
    step_ok("a directive", directives.send(rich.clone())).await?;
    eq(
        "the directive on the wire",
        &read_frame("the directive", step("the directive", raw.recv()).await?)?,
        &Frame {
            protocol_version: V1_0,
            message: Message::Directive(rich),
        },
    )?;
    step_ok(
        "a raw observation",
        raw.send(frame_bytes(V1_0, Message::Observation(booted(32)))?),
    )
    .await?;
    eq(
        "the observation read",
        &step_ok("the observation", observations.recv()).await?,
        &booted(32),
    )
}

async fn malformed_discarded<F: LinkFactory>(f: &F) -> Check {
    let garbage: [&[u8]; 3] = [
        b"not json",
        br#"{"message":{"hello":{}}}"#,
        br#"{"protocol_version":{"major":1,"minor":1},"message":{"hello":{"nonsense":1}}}"#,
    ];

    let (mut cp_link, mut raw) = step("connecting", f.control_plane_facing_raw(SUPPORTED)).await?;
    for bytes in garbage {
        step_ok("raw garbage", raw.send(bytes.to_vec())).await?;
    }
    let readable = hello(SUPPORTED, "boot-2")?;
    step_ok(
        "a hello",
        raw.send(frame_bytes(CURRENT, Message::Hello(readable.clone()))?),
    )
    .await?;
    eq(
        "the hello after garbage",
        &step_ok("the control plane's next hello", cp_link.next_hello()).await?,
        &readable,
    )?;

    let (mut directives, _observations, mut raw) =
        data_plane_session_against_raw(f, SUPPORTED, CURRENT).await?;
    for bytes in garbage {
        step_ok("raw garbage", raw.send(bytes.to_vec())).await?;
    }
    let sent = directive(41, SESSION_EPOCH);
    step_ok(
        "a directive",
        raw.send(frame_bytes(CURRENT, Message::Directive(sent.clone()))?),
    )
    .await?;
    eq(
        "the directive after garbage",
        &step_ok("the directive", directives.recv()).await?,
        &sent,
    )
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
    let allowed = set(sent.iter().map(directive_key));
    until_closed(
        "the data plane's receive after the close",
        &mut s.dp_directives,
        &allowed,
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
    let allowed = set(sent.iter().map(observation_key));
    until_closed(
        "the control plane's receive after the close",
        &mut s.cp_observations,
        &allowed,
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
    eq("observations the close discarded", &discarded, &unread)?;
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
    eq("directives the close discarded", &discarded, &unread)
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
