//! The wire: a bounded, framed byte pipe in both directions, under the endpoints.
//!
//! A wire carries one encoded frame per `send` and hands back whole frames, in no promised
//! order and possibly more than once (at-least-once is all the protocol assumes, plan §3.2).
//! What it must promise is what the link contract suite checks: a frame `send` accepted is
//! delivered while the link stays open (never dropped silently); a full wire makes `send` wait
//! (backpressure, fnd §11 "bounded everything"); a peer's close is seen, by `recv` returning
//! `None` once what was sent before is drained and by `send` failing with
//! [`LinkError::Closed`]; and `close` reports the frames it discarded unread.
//!
//! Pinned by: `tests/link_contract.rs::should_pass_the_link_contract_when_the_transport_is_inproc`
//! and, for each promise, the broken wires of the same file that fail it (drained before `None`:
//! `should_fail_the_drain_cases_when_a_close_drops_what_was_queued`).

use std::future::Future;

use mayfly_protocol::LinkError;

/// Both directions of one connection, before the endpoints split them.
pub trait Wire: Send {
    /// The sending half.
    type Tx: WireTx;
    /// The receiving half.
    type Rx: WireRx;

    /// The two halves.
    fn split(self) -> (Self::Tx, Self::Rx);
}

/// The sending half of a wire. Clones share the direction; the peer sees the close when the
/// last clone is dropped.
pub trait WireTx: Clone + Send + Sync {
    /// Hands one encoded frame to the wire, waiting while it is full. `Ok` means accepted, not
    /// delivered. Not cancel-safe: a cancelled send may or may not have been accepted.
    ///
    /// # Errors
    /// [`LinkError::Closed`] when the peer has closed its receiving half.
    fn send(&self, frame: Vec<u8>) -> impl Future<Output = Result<(), LinkError>> + Send;
}

/// The receiving half of a wire.
pub trait WireRx: Send {
    /// The next whole frame, or `None` once the peer closed and what it sent before is
    /// drained. Cancel-safe: a frame not yet returned stays queued.
    fn recv(&mut self) -> impl Future<Output = Option<Vec<u8>>> + Send;

    /// Closes this direction: the peer's sends fail from now on, and the frames accepted but not
    /// yet received are discarded. Returns how many were discarded: at least one when any was,
    /// exact where the wire can count them (a socket cannot count what its peer has not written
    /// yet).
    fn close(self) -> impl Future<Output = usize> + Send;
}
