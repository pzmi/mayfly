//! The in-process transport (plan §3.3): the wire of the all-in-one `mayflyd`, two bounded
//! Tokio channels of encoded frames.
//!
//! The endpoints encode and decode every frame as a socket transport would, so the in-process
//! link exercises the same encoding. A full channel makes `send` wait; nothing is dropped while
//! the link is open. Closing a receiving half (or dropping it) makes the peer's sends fail with
//! `Closed` and discards, and reports, what it had not received; dropping every sending half
//! lets the peer drain what was sent and then see `Closed`.
//!
//! Pinned by: `tests/link_contract.rs::should_pass_the_link_contract_when_the_transport_is_inproc`
//! (every link contract case).

use mayfly_protocol::{LinkError, ProtocolVersion};
use tokio::sync::mpsc;

use crate::endpoint::{ControlPlaneLink, DataPlaneLink};
use crate::wire::{Wire, WireRx, WireTx};

/// Frames one direction holds before a send waits, unless the caller names another bound. The
/// relay re-sends what has no definite answer, so a deep queue buys nothing; 64 covers one
/// burst of a pool's worth of directives (a reconcile pass, a report of every VM) without
/// stalling the sender.
pub const DEFAULT_CAPACITY: usize = 64;

/// One end of an in-process wire.
pub struct ChannelWire {
    tx: ChannelTx,
    rx: ChannelRx,
}

/// The sending half of a [`ChannelWire`].
#[derive(Clone)]
pub struct ChannelTx(mpsc::Sender<Vec<u8>>);

/// The receiving half of a [`ChannelWire`].
pub struct ChannelRx(mpsc::Receiver<Vec<u8>>);

/// Two connected wire ends; each direction holds at most `capacity` frames (at least one).
#[must_use]
pub fn wire(capacity: usize) -> (ChannelWire, ChannelWire) {
    let bound = capacity.max(1);
    let (a_tx, b_rx) = mpsc::channel(bound);
    let (b_tx, a_rx) = mpsc::channel(bound);
    (
        ChannelWire {
            tx: ChannelTx(a_tx),
            rx: ChannelRx(a_rx),
        },
        ChannelWire {
            tx: ChannelTx(b_tx),
            rx: ChannelRx(b_rx),
        },
    )
}

/// A connected pair of endpoints: the data plane's speaking `dp_supported`, the control
/// plane's speaking `cp_supported` (both normally `mayfly_protocol::SUPPORTED`).
#[must_use]
pub fn link(
    capacity: usize,
    dp_supported: Vec<ProtocolVersion>,
    cp_supported: Vec<ProtocolVersion>,
) -> (DataPlaneLink<ChannelWire>, ControlPlaneLink<ChannelWire>) {
    let (dp, cp) = wire(capacity);
    (
        DataPlaneLink::new(dp, dp_supported),
        ControlPlaneLink::new(cp, cp_supported),
    )
}

impl Wire for ChannelWire {
    type Tx = ChannelTx;
    type Rx = ChannelRx;

    fn split(self) -> (ChannelTx, ChannelRx) {
        (self.tx, self.rx)
    }
}

impl WireTx for ChannelTx {
    async fn send(&self, frame: Vec<u8>) -> Result<(), LinkError> {
        self.0.send(frame).await.map_err(|_| LinkError::Closed)
    }
}

impl WireRx for ChannelRx {
    async fn recv(&mut self) -> Option<Vec<u8>> {
        self.0.recv().await
    }

    async fn close(mut self) -> usize {
        self.0.close();
        let mut discarded = 0;
        while self.0.try_recv().is_ok() {
            discarded += 1;
        }
        discarded
    }
}
