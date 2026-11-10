//! The endpoints: the link ports of `mayfly_protocol` over any [`Wire`].
//!
//! Each side starts as a handshake end ([`DataPlaneLink`], [`ControlPlaneLink`]) and, once a
//! session is open, splits into a receiving half and a cloneable sending half, so a side can
//! receive and send at once. Frames are encoded on every send and decoded on every receive,
//! in process too (plan §3.3), so a field that does not round-trip cannot slip through.
//!
//! What a receiving end may not hand up it discards and counts in [`Discards`]: a frame of a
//! version not admitted, bytes that are not a frame, and a message of the wrong kind for the
//! phase (a directive before the welcome is of a session not entered yet, discarded unanswered
//! per ADR 0042; a duplicated `Welcome` or `Hello` after it). Nothing a peer sends closes the
//! link; at-least-once delivery re-sends what was lost (plan §3.2). Link contract case LK-16
//! sends each of these to both ends and reads the counts back.
//!
//! Pinned by: `tests/link_contract.rs::should_pass_the_link_contract_when_the_transport_is_inproc`
//! (LK-16), and `should_fail_the_version_cases_when_a_link_does_not_check_versions` for the
//! version count.

use mayfly_protocol::{
    Directive, DirectiveSink, DirectiveSource, Frame, HandshakeInitiator, HandshakeRefused,
    HandshakeResponder, Hello, LinkError, Message, Observation, ObservationSink, ObservationSource,
    ProtocolVersion, Welcome, handshake_version,
};

use crate::codec::{self, Discard};
use crate::wire::{Wire, WireRx, WireTx};

/// What a receiving end discarded instead of handing up, by cause. Each count is a lower bound
/// on what the peer sent of that kind once the frames arrived (at-least-once may deliver one
/// twice); link contract case LK-16.
///
/// Pinned by: `tests/link_contract.rs::should_pass_the_link_contract_when_the_transport_is_inproc`
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Discards {
    /// Frames of a version not admitted (`mayfly_protocol::admit_frame`).
    pub version: u64,
    /// Bytes that are not a frame of an admitted version.
    pub malformed: u64,
    /// Admitted frames of a kind this end does not take at that point.
    pub unexpected: u64,
}

/// The receiving side shared by every end: decode, admit, count.
struct Reader<R> {
    rx: R,
    supported: Vec<ProtocolVersion>,
    discards: Discards,
}

impl<R: WireRx> Reader<R> {
    /// The next admitted message. Cancel-safe when the wire's `recv` is: nothing awaits after a
    /// frame was taken.
    async fn next(&mut self, session: Option<ProtocolVersion>) -> Result<Message, LinkError> {
        loop {
            let bytes = self.rx.recv().await.ok_or(LinkError::Closed)?;
            match codec::decode(&bytes, session, &self.supported) {
                Ok(frame) => return Ok(frame.message),
                Err(Discard::Version(_)) => self.discards.version += 1,
                Err(Discard::Malformed) => self.discards.malformed += 1,
            }
        }
    }

    /// Counts an admitted message of the wrong kind for the phase, which the caller then drops
    /// unanswered (a directive before the welcome: ADR 0042). Link contract case LK-16.
    ///
    /// Pinned by: `tests/link_contract.rs::should_pass_the_link_contract_when_the_transport_is_inproc`
    fn unexpected(&mut self) {
        self.discards.unexpected += 1;
    }
}

/// Encodes and sends one frame.
async fn send_frame<T: WireTx>(
    tx: &T,
    protocol_version: ProtocolVersion,
    message: Message,
) -> Result<(), LinkError> {
    let bytes = codec::encode(&Frame {
        protocol_version,
        message,
    })?;
    tx.send(bytes).await
}

/// The version handshake frames are written in: the oldest major spoken, so the peer can read
/// and answer them whatever it speaks of ours.
fn handshake_frame_version(supported: &[ProtocolVersion]) -> Result<ProtocolVersion, LinkError> {
    handshake_version(supported)
        .ok_or_else(|| LinkError::Transport("this end speaks no protocol version".to_owned()))
}

fn no_session() -> LinkError {
    LinkError::Transport("no session is open on this link".to_owned())
}

// ---------------------------------------------------------------------------------------------
// The data plane's side

/// The data plane's halves of a session over a wire `W`: directives in, observations out.
pub type DataPlaneSession<W> = (
    DirectiveReceiver<<W as Wire>::Rx>,
    ObservationSender<<W as Wire>::Tx>,
);

/// The control plane's halves of a session over a wire `W`: directives out, observations in.
pub type ControlPlaneSession<W> = (
    DirectiveSender<<W as Wire>::Tx>,
    ObservationReceiver<<W as Wire>::Rx>,
);

/// The data plane's end of a link, before and during the handshake.
pub struct DataPlaneLink<W: Wire> {
    tx: W::Tx,
    reader: Reader<W::Rx>,
    session: Option<ProtocolVersion>,
}

impl<W: Wire> DataPlaneLink<W> {
    /// The data plane's end over `wire`, speaking `supported` (normally
    /// `mayfly_protocol::SUPPORTED`).
    pub fn new(wire: W, supported: Vec<ProtocolVersion>) -> Self {
        let (tx, rx) = wire.split();
        Self {
            tx,
            reader: Reader {
                rx,
                supported,
                discards: Discards::default(),
            },
            session: None,
        }
    }

    /// The version of the session the last `Welcome` opened, if any.
    #[must_use]
    pub const fn session(&self) -> Option<ProtocolVersion> {
        self.session
    }

    /// What this end discarded so far (link contract case LK-16).
    ///
    /// Pinned by: `tests/link_contract.rs::should_pass_the_link_contract_when_the_transport_is_inproc`
    #[must_use]
    pub const fn discarded(&self) -> Discards {
        self.reader.discards
    }

    /// The session's halves: directives in, observations out.
    ///
    /// # Errors
    /// [`LinkError::Transport`] when no welcome opened a session; the link is dropped (closed).
    pub fn split(self) -> Result<DataPlaneSession<W>, LinkError> {
        let version = self.session.ok_or_else(no_session)?;
        Ok((
            DirectiveReceiver {
                reader: self.reader,
                version,
            },
            ObservationSender {
                tx: self.tx,
                version,
            },
        ))
    }

    /// Closes the link before a session was split off; returns how many frames were discarded
    /// unread.
    pub async fn close(self) -> usize {
        self.reader.rx.close().await
    }
}

impl<W: Wire> HandshakeInitiator for DataPlaneLink<W> {
    /// Sends `hello` in the oldest major this end speaks and waits for the answer. A `Welcome`
    /// in a version this end does not speak is refused with [`LinkError::Version`] and opens
    /// nothing. Whether its epoch may be entered is the data plane's decision
    /// (`mayfly_protocol::enters_session`), not the transport's. Link contract cases LK-01,
    /// LK-03, LK-04, LK-06 and LK-07.
    ///
    /// Pinned by: `tests/link_contract.rs::should_pass_the_link_contract_when_the_transport_is_inproc`
    async fn hello(&mut self, hello: Hello) -> Result<Welcome, LinkError> {
        self.session = None;
        let version = handshake_frame_version(&self.reader.supported)?;
        send_frame(&self.tx, version, Message::Hello(hello)).await?;
        loop {
            match self.reader.next(None).await? {
                Message::Welcome(welcome) => {
                    codec::speaks(welcome.accepted_version, &self.reader.supported)
                        .map_err(LinkError::Version)?;
                    self.session = Some(welcome.accepted_version);
                    return Ok(welcome);
                }
                Message::HandshakeRefused(refused) => return Err(LinkError::Refused(refused)),
                Message::Hello(_) | Message::Directive(_) | Message::Observation(_) => {
                    self.reader.unexpected();
                }
            }
        }
    }
}

/// The data plane's receiving half of a session.
pub struct DirectiveReceiver<R> {
    reader: Reader<R>,
    version: ProtocolVersion,
}

impl<R: WireRx> DirectiveReceiver<R> {
    /// The session's version.
    #[must_use]
    pub const fn version(&self) -> ProtocolVersion {
        self.version
    }

    /// What this half and the handshake before it discarded so far (link contract case
    /// LK-16).
    ///
    /// Pinned by: `tests/link_contract.rs::should_pass_the_link_contract_when_the_transport_is_inproc`
    #[must_use]
    pub const fn discarded(&self) -> Discards {
        self.reader.discards
    }

    /// Closes the session's inbound direction: the control plane's sends fail from now on.
    /// Returns how many frames were discarded unread (link contract case LK-14: at least one
    /// when any was).
    ///
    /// Pinned by: `tests/link_contract.rs::should_pass_the_link_contract_when_the_transport_is_inproc`
    pub async fn close(self) -> usize {
        self.reader.rx.close().await
    }
}

impl<R: WireRx> DirectiveSource for DirectiveReceiver<R> {
    async fn recv(&mut self) -> Result<Directive, LinkError> {
        loop {
            match self.reader.next(Some(self.version)).await? {
                Message::Directive(directive) => return Ok(directive),
                Message::Hello(_)
                | Message::Welcome(_)
                | Message::HandshakeRefused(_)
                | Message::Observation(_) => self.reader.unexpected(),
            }
        }
    }
}

/// The data plane's sending half of a session; clones share it.
#[derive(Clone)]
pub struct ObservationSender<T> {
    tx: T,
    version: ProtocolVersion,
}

impl<T: WireTx> ObservationSink for ObservationSender<T> {
    /// Writes `observation` down to the session's version
    /// (`mayfly_protocol::ObservationKind::written_in`) and sends it stamped with that version.
    /// Link contract case LK-02.
    ///
    /// Pinned by: `tests/link_contract.rs::should_pass_the_link_contract_when_the_transport_is_inproc`
    async fn send(&self, observation: Observation) -> Result<(), LinkError> {
        let observation = Observation {
            kind: observation.kind.written_in(self.version),
            ..observation
        };
        send_frame(&self.tx, self.version, Message::Observation(observation)).await
    }
}

// ---------------------------------------------------------------------------------------------
// The control plane's side

/// The control plane's end of a link, before and during the handshake.
pub struct ControlPlaneLink<W: Wire> {
    tx: W::Tx,
    reader: Reader<W::Rx>,
    session: Option<ProtocolVersion>,
}

impl<W: Wire> ControlPlaneLink<W> {
    /// The control plane's end over `wire`, speaking `supported`.
    pub fn new(wire: W, supported: Vec<ProtocolVersion>) -> Self {
        let (tx, rx) = wire.split();
        Self {
            tx,
            reader: Reader {
                rx,
                supported,
                discards: Discards::default(),
            },
            session: None,
        }
    }

    /// The version of the session the last `welcome` opened, if any.
    #[must_use]
    pub const fn session(&self) -> Option<ProtocolVersion> {
        self.session
    }

    /// What this end discarded so far (link contract case LK-16).
    ///
    /// Pinned by: `tests/link_contract.rs::should_pass_the_link_contract_when_the_transport_is_inproc`
    #[must_use]
    pub const fn discarded(&self) -> Discards {
        self.reader.discards
    }

    /// The session's halves: directives out, observations in.
    ///
    /// # Errors
    /// [`LinkError::Transport`] when no welcome opened a session; the link is dropped (closed).
    pub fn split(self) -> Result<ControlPlaneSession<W>, LinkError> {
        let version = self.session.ok_or_else(no_session)?;
        Ok((
            DirectiveSender {
                tx: self.tx,
                version,
            },
            ObservationReceiver {
                reader: self.reader,
                version,
            },
        ))
    }

    /// Closes the link before a session was split off; returns how many frames were discarded
    /// unread.
    pub async fn close(self) -> usize {
        self.reader.rx.close().await
    }
}

impl<W: Wire> HandshakeResponder for ControlPlaneLink<W> {
    async fn next_hello(&mut self) -> Result<Hello, LinkError> {
        loop {
            match self.reader.next(None).await? {
                Message::Hello(hello) => return Ok(hello),
                Message::Welcome(_)
                | Message::HandshakeRefused(_)
                | Message::Directive(_)
                | Message::Observation(_) => self.reader.unexpected(),
            }
        }
    }

    /// Opens the session at `welcome.accepted_version`, which the caller negotiated
    /// (`mayfly_protocol::negotiate`); a version this end does not speak is refused with
    /// [`LinkError::Version`] and nothing is sent (link contract case LK-06).
    ///
    /// Pinned by: `tests/link_contract.rs::should_pass_the_link_contract_when_the_transport_is_inproc`
    async fn welcome(&mut self, welcome: Welcome) -> Result<(), LinkError> {
        codec::speaks(welcome.accepted_version, &self.reader.supported)
            .map_err(LinkError::Version)?;
        let version = handshake_frame_version(&self.reader.supported)?;
        let accepted = welcome.accepted_version;
        send_frame(&self.tx, version, Message::Welcome(welcome)).await?;
        self.session = Some(accepted);
        Ok(())
    }

    async fn refuse(&mut self, refused: HandshakeRefused) -> Result<(), LinkError> {
        self.session = None;
        let version = handshake_frame_version(&self.reader.supported)?;
        send_frame(&self.tx, version, Message::HandshakeRefused(refused)).await
    }
}

/// The control plane's sending half of a session; clones share it.
#[derive(Clone)]
pub struct DirectiveSender<T> {
    tx: T,
    version: ProtocolVersion,
}

impl<T: WireTx> DirectiveSink for DirectiveSender<T> {
    async fn send(&self, directive: Directive) -> Result<(), LinkError> {
        send_frame(&self.tx, self.version, Message::Directive(directive)).await
    }
}

/// The control plane's receiving half of a session.
pub struct ObservationReceiver<R> {
    reader: Reader<R>,
    version: ProtocolVersion,
}

impl<R: WireRx> ObservationReceiver<R> {
    /// The session's version.
    #[must_use]
    pub const fn version(&self) -> ProtocolVersion {
        self.version
    }

    /// What this half and the handshake before it discarded so far (link contract case
    /// LK-16).
    ///
    /// Pinned by: `tests/link_contract.rs::should_pass_the_link_contract_when_the_transport_is_inproc`
    #[must_use]
    pub const fn discarded(&self) -> Discards {
        self.reader.discards
    }

    /// Closes the session's inbound direction: the data plane's sends fail from now on.
    /// Returns how many frames were discarded unread (link contract case LK-14: at least one
    /// when any was).
    ///
    /// Pinned by: `tests/link_contract.rs::should_pass_the_link_contract_when_the_transport_is_inproc`
    pub async fn close(self) -> usize {
        self.reader.rx.close().await
    }
}

impl<R: WireRx> ObservationSource for ObservationReceiver<R> {
    async fn recv(&mut self) -> Result<Observation, LinkError> {
        loop {
            match self.reader.next(Some(self.version)).await? {
                Message::Observation(observation) => return Ok(observation),
                Message::Hello(_)
                | Message::Welcome(_)
                | Message::HandshakeRefused(_)
                | Message::Directive(_) => self.reader.unexpected(),
            }
        }
    }
}
