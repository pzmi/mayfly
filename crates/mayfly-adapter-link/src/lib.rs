//! `mayfly-adapter-link` (shared plane): the transports that carry `mayfly_protocol` frames
//! between the control plane and the data plane (plan §3.3).
//!
//! A transport is two layers. A [`wire::Wire`] moves one encoded frame at a time, bounded, in
//! both directions, and says when the peer left; it knows nothing of the protocol. The
//! endpoints ([`DataPlaneLink`], [`ControlPlaneLink`] and the session halves they split into)
//! implement the link ports of `mayfly_protocol` over any wire: they encode and decode every
//! frame (JSON, ADR 0010), write the handshake in the oldest major they speak, stamp session
//! frames with the negotiated version and write observations down to it
//! (`ObservationKind::written_in`, ADR 0052, ADR 0053), admit a frame by its header before
//! decoding it (`admit_frame`), and discard and count what they may not hand up. Epochs are
//! not theirs to judge: a directive of any epoch is handed up, and the data plane's executor
//! fences it (I09, ADR 0042).
//!
//! - [`inproc`]: the wire of the all-in-one `mayflyd`, bounded Tokio channels (DP-02).
//! - `uds`: length-prefixed frames over a Unix socket (LNK-01), a second [`wire::Wire`].
//!
//! Every transport passes the link contract suite of `mayfly-testkit-shared`
//! (`tests/link_contract.rs`, ver §5).

mod codec;
mod endpoint;
pub mod inproc;
pub mod wire;

pub use endpoint::{
    ControlPlaneLink, ControlPlaneSession, DataPlaneLink, DataPlaneSession, DirectiveReceiver,
    DirectiveSender, Discards, ObservationReceiver, ObservationSender,
};
