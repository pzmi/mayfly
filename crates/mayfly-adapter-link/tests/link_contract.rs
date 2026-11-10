//! The link contract (ver §5) against the in-process transport, and the suite's own tests: a
//! transport that reorders and duplicates frames passes it (the suite assumes no order), and
//! each deliberately broken transport fails the cases named for its fault, reordering included
//! (`ReorderingNoVersionCheckLink`: a case that stopped at the first readable frame would let
//! it through).
//!
//! The doubles are wires (`mayfly_adapter_link::wire::Wire`) under the production endpoints,
//! built here on their own bounded channels; each raw peer is a plain channel end, so a double
//! cannot hide what it puts on the wire. Each broken test pins the exact set of failed cases: a
//! change that lets a broken transport through, or makes it fail for an unrelated reason,
//! shows here.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};

use mayfly_adapter_link::inproc::{self, ChannelRx, ChannelTx, ChannelWire};
use mayfly_adapter_link::wire::{Wire, WireRx, WireTx};
use mayfly_adapter_link::{
    ControlPlaneLink, DataPlaneLink, DirectiveReceiver, DirectiveSender, Discards,
    ObservationReceiver, ObservationSender,
};
use mayfly_protocol::{LinkError, ProtocolVersion};
use mayfly_testkit_shared::link_contract::{
    DiscardCounts, LinkCase, LinkFactory, RawWire, check_link_contract, run_link_contract,
};
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------------------------
// The in-process transport

/// The raw peer of an in-process wire; `finish` drops its sending half.
struct ChannelRaw {
    tx: Option<ChannelTx>,
    rx: ChannelRx,
}

impl RawWire for ChannelRaw {
    async fn send(&mut self, frame: Vec<u8>) -> Result<(), LinkError> {
        match &self.tx {
            Some(tx) => tx.send(frame).await,
            None => Err(LinkError::Closed),
        }
    }

    async fn recv(&mut self) -> Option<Vec<u8>> {
        self.rx.recv().await
    }

    async fn finish(&mut self) {
        self.tx = None;
    }
}

fn raw(wire: ChannelWire) -> ChannelRaw {
    let (tx, rx) = wire.split();
    ChannelRaw { tx: Some(tx), rx }
}

/// The endpoints' counts as the suite reads them.
const fn counts(d: Discards) -> DiscardCounts {
    DiscardCounts {
        version: d.version,
        malformed: d.malformed,
        unexpected: d.unexpected,
    }
}

struct InprocFactory {
    capacity: usize,
}

impl LinkFactory for InprocFactory {
    type DpLink = DataPlaneLink<ChannelWire>;
    type CpLink = ControlPlaneLink<ChannelWire>;
    type DpDirectives = DirectiveReceiver<ChannelRx>;
    type DpObservations = ObservationSender<ChannelTx>;
    type CpDirectives = DirectiveSender<ChannelTx>;
    type CpObservations = ObservationReceiver<ChannelRx>;
    type Raw = ChannelRaw;

    fn capacity(&self) -> usize {
        self.capacity
    }

    async fn connect(
        &self,
        dp: &[ProtocolVersion],
        cp: &[ProtocolVersion],
    ) -> (Self::DpLink, Self::CpLink) {
        inproc::link(self.capacity, dp.to_vec(), cp.to_vec())
    }

    async fn data_plane_facing_raw(&self, dp: &[ProtocolVersion]) -> (Self::DpLink, ChannelRaw) {
        let (ours, theirs) = inproc::wire(self.capacity);
        (DataPlaneLink::new(ours, dp.to_vec()), raw(theirs))
    }

    async fn control_plane_facing_raw(&self, cp: &[ProtocolVersion]) -> (Self::CpLink, ChannelRaw) {
        let (ours, theirs) = inproc::wire(self.capacity);
        (ControlPlaneLink::new(ours, cp.to_vec()), raw(theirs))
    }

    fn data_plane_session(
        &self,
        link: Self::DpLink,
    ) -> Result<(Self::DpDirectives, Self::DpObservations), LinkError> {
        link.split()
    }

    fn control_plane_session(
        &self,
        link: Self::CpLink,
    ) -> Result<(Self::CpDirectives, Self::CpObservations), LinkError> {
        link.split()
    }

    async fn close_data_plane(
        &self,
        directives: Self::DpDirectives,
        observations: Self::DpObservations,
    ) -> usize {
        drop(observations);
        directives.close().await
    }

    async fn close_control_plane(
        &self,
        directives: Self::CpDirectives,
        observations: Self::CpObservations,
    ) -> usize {
        drop(directives);
        observations.close().await
    }

    fn data_plane_discards(&self, link: &Self::DpLink) -> DiscardCounts {
        counts(link.discarded())
    }

    fn data_plane_session_discards(&self, directives: &Self::DpDirectives) -> DiscardCounts {
        counts(directives.discarded())
    }

    fn control_plane_discards(&self, link: &Self::CpLink) -> DiscardCounts {
        counts(link.discarded())
    }

    fn control_plane_session_discards(&self, observations: &Self::CpObservations) -> DiscardCounts {
        counts(observations.discarded())
    }
}

#[test]
fn should_pass_the_link_contract_when_the_transport_is_inproc() {
    // given
    let factory = InprocFactory { capacity: 4 };

    // when
    let report = check_link_contract(&factory);

    // then
    assert!(report.is_green(), "{report}");
}

#[test]
fn should_pass_the_link_contract_when_inproc_has_its_default_capacity() {
    // given
    let factory = InprocFactory {
        capacity: inproc::DEFAULT_CAPACITY,
    };

    // when
    let run = || run_link_contract(&factory);

    // then
    run();
}

// ---------------------------------------------------------------------------------------------
// The doubles

/// What a double does differently from a correct wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// `ReorderingLink` (correct): hands frames up last-in first-out over what has arrived,
    /// and delivers every other frame twice.
    Reorder,
    /// `SilentDropLink`: a send to a full wire drops the frame and reports success.
    SilentDrop,
    /// `NoVersionCheckLink`: rewrites every received frame's version to 1.0, so the endpoint
    /// admits frames of any version.
    NoVersionCheck,
    /// `ReorderingNoVersionCheckLink`: `Reorder` and `NoVersionCheck` at once, so the readable
    /// frame sent last is handed up first and a forbidden one only after it.
    ReorderNoVersionCheck,
    /// `DeafCloseLink`: a close neither closes nor reports; the peer never sees it.
    DeafClose,
    /// `LossyCloseLink`: once every sender of a direction is gone, its reader sees `None` at
    /// once, dropping what was queued instead of draining it first.
    LossyClose,
    /// `CorruptingLink`: puts every frame on the wire with each byte flipped and flips it back
    /// on receipt, so two doubles understand each other and nothing else does.
    Corrupting,
}

const FLIP: u8 = 0x5a;

/// Channels a deaf double keeps open after its close.
#[derive(Default, Clone)]
struct Kept(Arc<Mutex<Vec<Box<dyn Send>>>>);

impl Kept {
    fn keep(&self, value: Box<dyn Send>) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(value);
    }
}

struct DoubleWire {
    fault: Fault,
    tx: mpsc::Sender<Vec<u8>>,
    rx: mpsc::Receiver<Vec<u8>>,
    kept: Kept,
}

#[derive(Clone)]
struct DoubleTx {
    fault: Fault,
    tx: mpsc::Sender<Vec<u8>>,
}

struct DoubleRx {
    fault: Fault,
    rx: mpsc::Receiver<Vec<u8>>,
    /// Frames taken from the channel but not handed up yet, with whether each is a copy.
    held: Vec<(Vec<u8>, bool)>,
    fresh: u64,
    kept: Kept,
}

impl Wire for DoubleWire {
    type Tx = DoubleTx;
    type Rx = DoubleRx;

    fn split(self) -> (DoubleTx, DoubleRx) {
        if self.fault == Fault::DeafClose {
            self.kept.keep(Box::new(self.tx.clone()));
        }
        (
            DoubleTx {
                fault: self.fault,
                tx: self.tx,
            },
            DoubleRx {
                fault: self.fault,
                rx: self.rx,
                held: Vec::new(),
                fresh: 0,
                kept: self.kept,
            },
        )
    }
}

impl WireTx for DoubleTx {
    async fn send(&self, frame: Vec<u8>) -> Result<(), LinkError> {
        match self.fault {
            Fault::SilentDrop => match self.tx.try_send(frame) {
                Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => Ok(()),
                Err(mpsc::error::TrySendError::Closed(_)) => Err(LinkError::Closed),
            },
            Fault::Corrupting => {
                let flipped = frame.into_iter().map(|b| b ^ FLIP).collect();
                self.tx.send(flipped).await.map_err(|_| LinkError::Closed)
            }
            Fault::Reorder
            | Fault::NoVersionCheck
            | Fault::ReorderNoVersionCheck
            | Fault::DeafClose
            | Fault::LossyClose => self.tx.send(frame).await.map_err(|_| LinkError::Closed),
        }
    }
}

/// The frame with its version rewritten to 1.0, or unchanged if it is not a JSON object.
fn version_blind(frame: Vec<u8>) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&frame) else {
        return frame;
    };
    let Some(object) = value.as_object_mut() else {
        return frame;
    };
    object.insert(
        "protocol_version".to_owned(),
        serde_json::json!({"major": 1, "minor": 0}),
    );
    serde_json::to_vec(&value).unwrap_or(frame)
}

impl DoubleRx {
    async fn reordered(&mut self) -> Option<Vec<u8>> {
        if self.held.is_empty() {
            let frame = self.rx.recv().await?;
            self.held.push((frame, false));
        }
        while let Ok(frame) = self.rx.try_recv() {
            self.held.push((frame, false));
        }
        let (frame, copy) = self.held.pop()?;
        if !copy {
            self.fresh += 1;
            if self.fresh.is_multiple_of(2) {
                self.held.insert(0, (frame.clone(), true));
            }
        }
        Some(frame)
    }
}

impl WireRx for DoubleRx {
    async fn recv(&mut self) -> Option<Vec<u8>> {
        match self.fault {
            Fault::Reorder => self.reordered().await,
            Fault::NoVersionCheck => self.rx.recv().await.map(version_blind),
            Fault::ReorderNoVersionCheck => self.reordered().await.map(version_blind),
            Fault::LossyClose if self.rx.is_closed() => None,
            Fault::Corrupting => self
                .rx
                .recv()
                .await
                .map(|frame| frame.into_iter().map(|b| b ^ FLIP).collect()),
            Fault::SilentDrop | Fault::DeafClose | Fault::LossyClose => self.rx.recv().await,
        }
    }

    async fn close(mut self) -> usize {
        if self.fault == Fault::DeafClose {
            self.kept.keep(Box::new(self.rx));
            return 0;
        }
        self.rx.close();
        let mut discarded = self.held.len();
        while self.rx.try_recv().is_ok() {
            discarded += 1;
        }
        discarded
    }
}

/// The raw peer of a double: plain channels, the true encoding; `finish` drops its sender.
struct PlainRaw {
    tx: Option<mpsc::Sender<Vec<u8>>>,
    rx: mpsc::Receiver<Vec<u8>>,
}

impl RawWire for PlainRaw {
    async fn send(&mut self, frame: Vec<u8>) -> Result<(), LinkError> {
        let tx = self.tx.as_ref().ok_or(LinkError::Closed)?;
        tx.send(frame).await.map_err(|_| LinkError::Closed)
    }

    async fn recv(&mut self) -> Option<Vec<u8>> {
        self.rx.recv().await
    }

    async fn finish(&mut self) {
        self.tx = None;
    }
}

struct DoubleFactory {
    fault: Fault,
    kept: Kept,
}

impl DoubleFactory {
    const CAPACITY: usize = 4;

    fn new(fault: Fault) -> Self {
        Self {
            fault,
            kept: Kept::default(),
        }
    }

    fn pair(&self) -> (DoubleWire, DoubleWire) {
        let (a_tx, b_rx) = mpsc::channel(Self::CAPACITY);
        let (b_tx, a_rx) = mpsc::channel(Self::CAPACITY);
        (self.wire(a_tx, a_rx), self.wire(b_tx, b_rx))
    }

    fn wire(&self, tx: mpsc::Sender<Vec<u8>>, rx: mpsc::Receiver<Vec<u8>>) -> DoubleWire {
        DoubleWire {
            fault: self.fault,
            tx,
            rx,
            kept: self.kept.clone(),
        }
    }

    fn facing_raw(&self) -> (DoubleWire, PlainRaw) {
        let (ours_tx, raw_rx) = mpsc::channel(Self::CAPACITY);
        let (raw_tx, ours_rx) = mpsc::channel(Self::CAPACITY);
        (
            self.wire(ours_tx, ours_rx),
            PlainRaw {
                tx: Some(raw_tx),
                rx: raw_rx,
            },
        )
    }
}

impl LinkFactory for DoubleFactory {
    type DpLink = DataPlaneLink<DoubleWire>;
    type CpLink = ControlPlaneLink<DoubleWire>;
    type DpDirectives = DirectiveReceiver<DoubleRx>;
    type DpObservations = ObservationSender<DoubleTx>;
    type CpDirectives = DirectiveSender<DoubleTx>;
    type CpObservations = ObservationReceiver<DoubleRx>;
    type Raw = PlainRaw;

    fn capacity(&self) -> usize {
        Self::CAPACITY
    }

    async fn connect(
        &self,
        dp: &[ProtocolVersion],
        cp: &[ProtocolVersion],
    ) -> (Self::DpLink, Self::CpLink) {
        let (dp_wire, cp_wire) = self.pair();
        (
            DataPlaneLink::new(dp_wire, dp.to_vec()),
            ControlPlaneLink::new(cp_wire, cp.to_vec()),
        )
    }

    async fn data_plane_facing_raw(&self, dp: &[ProtocolVersion]) -> (Self::DpLink, PlainRaw) {
        let (wire, raw) = self.facing_raw();
        (DataPlaneLink::new(wire, dp.to_vec()), raw)
    }

    async fn control_plane_facing_raw(&self, cp: &[ProtocolVersion]) -> (Self::CpLink, PlainRaw) {
        let (wire, raw) = self.facing_raw();
        (ControlPlaneLink::new(wire, cp.to_vec()), raw)
    }

    fn data_plane_session(
        &self,
        link: Self::DpLink,
    ) -> Result<(Self::DpDirectives, Self::DpObservations), LinkError> {
        link.split()
    }

    fn control_plane_session(
        &self,
        link: Self::CpLink,
    ) -> Result<(Self::CpDirectives, Self::CpObservations), LinkError> {
        link.split()
    }

    async fn close_data_plane(
        &self,
        directives: Self::DpDirectives,
        observations: Self::DpObservations,
    ) -> usize {
        drop(observations);
        directives.close().await
    }

    async fn close_control_plane(
        &self,
        directives: Self::CpDirectives,
        observations: Self::CpObservations,
    ) -> usize {
        drop(directives);
        observations.close().await
    }

    fn data_plane_discards(&self, link: &Self::DpLink) -> DiscardCounts {
        counts(link.discarded())
    }

    fn data_plane_session_discards(&self, directives: &Self::DpDirectives) -> DiscardCounts {
        counts(directives.discarded())
    }

    fn control_plane_discards(&self, link: &Self::CpLink) -> DiscardCounts {
        counts(link.discarded())
    }

    fn control_plane_session_discards(&self, observations: &Self::CpObservations) -> DiscardCounts {
        counts(observations.discarded())
    }
}

fn failed_cases(fault: Fault) -> BTreeSet<LinkCase> {
    check_link_contract(&DoubleFactory::new(fault)).failed_cases()
}

#[test]
fn should_pass_the_link_contract_when_frames_arrive_reordered_and_duplicated() {
    // given
    let factory = DoubleFactory::new(Fault::Reorder);

    // when
    let report = check_link_contract(&factory);

    // then
    assert!(report.is_green(), "{report}");
}

#[test]
fn should_fail_every_case_past_the_capacity_when_a_full_link_drops_silently() {
    // given
    let fault = Fault::SilentDrop;

    // when
    let failed = failed_cases(fault);

    // then
    // LK-08 sends every golden message, more than the capacity, before the peer reads.
    assert_eq!(
        failed,
        BTreeSet::from([
            LinkCase::WireEncoding,
            LinkCase::AtLeastOnce,
            LinkCase::Backpressure
        ])
    );
}

#[test]
fn should_fail_the_version_cases_when_a_link_does_not_check_versions() {
    // given
    let fault = Fault::NoVersionCheck;

    // when
    let failed = failed_cases(fault);

    // then
    assert_eq!(
        failed,
        BTreeSet::from([
            LinkCase::UnknownMajorDiscarded,
            LinkCase::SessionVersionHeld,
            LinkCase::DiscardsCounted,
        ])
    );
}

#[test]
fn should_fail_the_version_cases_when_a_reordering_link_does_not_check_versions() {
    // given
    let fault = Fault::ReorderNoVersionCheck;

    // when
    let failed = failed_cases(fault);

    // then
    assert_eq!(
        failed,
        BTreeSet::from([
            LinkCase::UnknownMajorDiscarded,
            LinkCase::SessionVersionHeld,
            LinkCase::DiscardsCounted,
        ])
    );
}

#[test]
fn should_fail_the_close_cases_when_a_link_does_not_report_its_close() {
    // given
    let fault = Fault::DeafClose;

    // when
    let failed = failed_cases(fault);

    // then
    assert_eq!(
        failed,
        BTreeSet::from([
            LinkCase::CloseSeenByDataPlane,
            LinkCase::CloseSeenByControlPlane,
            LinkCase::CloseReportsInFlight,
        ])
    );
}

#[test]
fn should_fail_the_drain_cases_when_a_close_drops_what_was_queued() {
    // given
    let fault = Fault::LossyClose;

    // when
    let failed = failed_cases(fault);

    // then
    assert_eq!(
        failed,
        BTreeSet::from([
            LinkCase::UnknownMajorDiscarded,
            LinkCase::SessionVersionHeld,
            LinkCase::MalformedDiscarded,
            LinkCase::CloseSeenByDataPlane,
            LinkCase::CloseSeenByControlPlane,
            LinkCase::DiscardsCounted,
        ])
    );
}

#[test]
fn should_fail_the_wire_cases_when_a_link_corrupts_its_encoding() {
    // given
    let fault = Fault::Corrupting;

    // when
    let failed = failed_cases(fault);

    // then
    assert!(failed.contains(&LinkCase::WireEncoding), "{failed:?}");
    assert_eq!(
        failed,
        BTreeSet::from([
            LinkCase::UnknownMajorDiscarded,
            LinkCase::SessionVersionHeld,
            LinkCase::WelcomeOutsideOffer,
            LinkCase::HandshakeInOldestMajor,
            LinkCase::WireEncoding,
            LinkCase::MalformedDiscarded,
            LinkCase::DiscardsCounted,
        ])
    );
}
