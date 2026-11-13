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
//! **The JIT config of a launch** (protocol 1.6, LNK-02, `formal/link.qnt`). The control
//! plane's sending half, given a [`LaunchConfigSource`] (`ControlPlaneLink::with_launch_configs`),
//! takes a `LaunchRunner`'s config from it when it sends the launch, in a session of 1.6 or
//! newer, for a launch that names an instance id and is stamped with the session's epoch (what
//! the data plane admits), for the directive's attempt and that epoch, into that one frame;
//! once taken, a resend carries nothing. The data plane's receiving half, given a
//! [`LaunchConfigSink`], empties it when a welcome opens a session, and puts a config it reads
//! into it before it hands the launch up, bound to the launch's VM, instance id and deadline,
//! only for a launch of its own session's epoch that names an instance id, in a frame of 1.6
//! or newer; every other config it reads (another epoch, another kind, no sink, a session or
//! frame below 1.6, a handle already used) is dropped unread and counted in
//! [`Discards::configs`]. This is the one epoch the endpoints judge, and only for the config: the
//! directive itself is handed up whatever its epoch, for the executor to fence. No error or count
//! names a byte of a config. Link contract cases LK-17 to LK-19.
//!
//! Pinned by: `tests/link_contract.rs::should_pass_the_link_contract_when_the_transport_is_inproc`
//! (LK-16), and `should_fail_the_version_cases_when_a_link_does_not_check_versions` for the
//! version count.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mayfly_domain::{Epoch, Secret};
use mayfly_protocol::{
    Directive, DirectiveKind, DirectiveSink, DirectiveSource, Frame, HandshakeInitiator,
    HandshakeRefused, HandshakeResponder, Hello, LAUNCH_CONFIG, LaunchConfigSink,
    LaunchConfigSource, LaunchTarget, LinkError, Message, Observation, ObservationSink,
    ObservationSource, ProtocolVersion, Welcome, handshake_version,
};

use crate::codec::{self, Decoded, Discard};
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
    /// JIT configs read and dropped unread: carried by a frame this end puts none from (another
    /// epoch, another kind, a session below 1.6, no store), or refused by the store (a handle
    /// already used). Link contract case LK-19.
    pub configs: u64,
}

/// The receiving side shared by every end: decode, admit, count.
struct Reader<R> {
    rx: R,
    supported: Vec<ProtocolVersion>,
    discards: Discards,
}

impl<R: WireRx> Reader<R> {
    /// The next admitted message, the version its frame was written in and the config it
    /// carried. Cancel-safe when the wire's `recv` is: nothing awaits after a frame was taken.
    async fn next_carrying(
        &mut self,
        session: Option<ProtocolVersion>,
    ) -> Result<(Message, ProtocolVersion, Option<Secret<String>>), LinkError> {
        loop {
            let bytes = self.rx.recv().await.ok_or(LinkError::Closed)?;
            match codec::decode(&bytes, session, &self.supported) {
                Ok(Decoded { frame, config }) => {
                    return Ok((frame.message, frame.protocol_version, config));
                }
                Err(Discard::Version(_)) => self.discards.version += 1,
                Err(Discard::Malformed) => self.discards.malformed += 1,
            }
        }
    }

    /// The next admitted message; a config its frame carried is dropped and counted, as no end
    /// but the data plane's directive half takes one.
    async fn next(&mut self, session: Option<ProtocolVersion>) -> Result<Message, LinkError> {
        let (message, _, config) = self.next_carrying(session).await?;
        if config.is_some() {
            self.dropped_config();
        }
        Ok(message)
    }

    /// Counts a config dropped unread (LK-19).
    fn dropped_config(&mut self) {
        self.discards.configs += 1;
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
    session: Option<(ProtocolVersion, Epoch)>,
    configs: Option<Arc<dyn LaunchConfigSink>>,
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
            configs: None,
        }
    }

    /// This end, putting the JIT config a launch's frame carries into `store` before it hands
    /// the launch up (the data plane's `LaunchStore`, ADR 0082), and emptying `store` when a
    /// welcome opens a session. Without it, every config read is dropped and counted
    /// ([`Discards::configs`]).
    #[must_use]
    pub fn with_launch_configs(self, store: Arc<dyn LaunchConfigSink>) -> Self {
        Self {
            configs: Some(store),
            ..self
        }
    }

    /// The version of the session the last `Welcome` opened, if any.
    #[must_use]
    pub const fn session(&self) -> Option<ProtocolVersion> {
        match self.session {
            Some((version, _)) => Some(version),
            None => None,
        }
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
        let (version, epoch) = self.session.ok_or_else(no_session)?;
        Ok((
            DirectiveReceiver {
                reader: self.reader,
                version,
                epoch,
                configs: self.configs,
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
                    self.session = Some((welcome.accepted_version, welcome.epoch));
                    // no config held was put for this session (LNK-02)
                    if let Some(store) = &self.configs {
                        store.discard_held();
                    }
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
    epoch: Epoch,
    configs: Option<Arc<dyn LaunchConfigSink>>,
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

impl<R: WireRx> DirectiveReceiver<R> {
    /// Puts `config`, carried by `directive`'s frame written in `written`, into the store when
    /// the directive is a launch of this session's epoch that names an instance id, in a session
    /// and a frame of 1.6 or newer; drops and counts it otherwise, or when the store refuses it
    /// (a handle already used).
    /// `formal/link.qnt`'s `dpDeliverWith` (`configOnlyInItsEpoch`, `configDeliveredOnce`).
    ///
    /// Pinned by:
    /// `tests/link_contract.rs::should_pass_the_link_contract_when_the_transport_is_inproc`
    /// (LK-17, LK-19), `should_fail_the_config_cases_when_a_link_strips_the_config` and
    /// `should_fail_only_the_session_case_when_a_link_puts_every_carried_config`.
    fn hold(&mut self, directive: &Directive, written: ProtocolVersion, config: Secret<String>) {
        let put = match (&directive.kind, &self.configs) {
            (
                DirectiveKind::LaunchRunner {
                    vm_name,
                    launch,
                    instance_id: Some(instance_id),
                },
                Some(store),
            ) if self.version >= LAUNCH_CONFIG
                && written >= LAUNCH_CONFIG
                && directive.epoch == self.epoch =>
            {
                store
                    .put(
                        *launch,
                        LaunchTarget {
                            vm_name: vm_name.clone(),
                            instance_id: instance_id.clone(),
                            deadline: directive.deadline,
                        },
                        config,
                    )
                    .is_ok()
            }
            _ => false,
        };
        if !put {
            self.reader.dropped_config();
        }
    }
}

impl<R: WireRx> DirectiveSource for DirectiveReceiver<R> {
    /// The next directive; a launch's config its frame carries is put into the store first
    /// ([`DirectiveReceiver::hold`]), so the executor never sees a launch before its config.
    async fn recv(&mut self) -> Result<Directive, LinkError> {
        loop {
            match self.reader.next_carrying(Some(self.version)).await? {
                (Message::Directive(directive), written, config) => {
                    if let Some(config) = config {
                        self.hold(&directive, written, config);
                    }
                    return Ok(directive);
                }
                (
                    Message::Hello(_)
                    | Message::Welcome(_)
                    | Message::HandshakeRefused(_)
                    | Message::Observation(_),
                    _,
                    config,
                ) => {
                    if config.is_some() {
                        self.reader.dropped_config();
                    }
                    self.reader.unexpected();
                }
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
    session: Option<(ProtocolVersion, Epoch)>,
    configs: Option<Arc<dyn LaunchConfigSource>>,
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
            configs: None,
        }
    }

    /// This end, taking each launch's JIT config from `broker` when it sends the launch (the
    /// control plane's `JitBroker`, plan §3.1). Without it, no frame carries a config.
    #[must_use]
    pub fn with_launch_configs(self, broker: Arc<dyn LaunchConfigSource>) -> Self {
        Self {
            configs: Some(broker),
            ..self
        }
    }

    /// The version of the session the last `welcome` opened, if any.
    #[must_use]
    pub const fn session(&self) -> Option<ProtocolVersion> {
        match self.session {
            Some((version, _)) => Some(version),
            None => None,
        }
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
        let (version, epoch) = self.session.ok_or_else(no_session)?;
        Ok((
            DirectiveSender {
                tx: self.tx,
                version,
                epoch,
                configs: self.configs,
                lost: Arc::default(),
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
        let accepted = (welcome.accepted_version, welcome.epoch);
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
    epoch: Epoch,
    configs: Option<Arc<dyn LaunchConfigSource>>,
    lost: Arc<AtomicU64>,
}

impl<T> DirectiveSender<T> {
    /// The config `directive` carries: a launch's, taken from the broker for its attempt and
    /// epoch, in a session of 1.6 or newer, only when the data plane would put it (the launch
    /// names an instance id and is stamped with this session's epoch), so a config never
    /// crosses for nothing; `None` otherwise, and on every send after the first (the take is
    /// single). `formal/link.qnt`'s `relaySendWith` (`configSentOnce`).
    fn carried(&self, directive: &Directive) -> Option<Secret<String>> {
        if self.version < LAUNCH_CONFIG || directive.epoch != self.epoch {
            return None;
        }
        let (
            DirectiveKind::LaunchRunner {
                launch,
                instance_id: Some(_),
                ..
            },
            Some(attempt),
            Some(broker),
        ) = (&directive.kind, directive.attempt, &self.configs)
        else {
            return None;
        };
        broker.take(*launch, attempt.attempt_id, directive.epoch)
    }

    /// How many configs this session's sends took from the broker but may not have handed to
    /// the wire: the send failed or was cancelled after the take (its launch then fails
    /// definitely for want of the config, and P13 deregisters the runner by id). Clones share
    /// the count.
    ///
    /// Pinned by: `tests/link_contract.rs::should_count_a_config_lost_when_its_send_fails`.
    #[must_use]
    pub fn configs_lost(&self) -> u64 {
        self.lost.load(Ordering::Relaxed)
    }
}

/// Counts a taken config as lost unless the send that carries it is accepted: dropped by a
/// failed or cancelled send, it counts.
struct Taken<'a> {
    lost: &'a AtomicU64,
    accepted: bool,
}

impl Drop for Taken<'_> {
    fn drop(&mut self) {
        if !self.accepted {
            self.lost.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl<T: WireTx> DirectiveSink for DirectiveSender<T> {
    /// Writes `directive` down to the session's version
    /// (`mayfly_protocol::DirectiveKind::written_in`: a launch's instance id from 1.3, ADR 0082)
    /// and sends it stamped with that version; a launch's frame carries its JIT config when the
    /// broker still holds it for this attempt and epoch (1.6, [`DirectiveSender::carried`]). The
    /// config is taken before the send: a send cancelled or failed after the take loses it
    /// ([`DirectiveSender::configs_lost`]), and the launch then fails definitely for want of it
    /// (single take, plan §3.2). Link contract cases LK-17 and LK-18.
    ///
    /// Pinned by:
    /// `tests/link_contract.rs::should_pass_the_link_contract_when_the_transport_is_inproc` and
    /// `should_fail_the_config_cases_when_a_link_carries_the_config_on_every_send`.
    async fn send(&self, directive: Directive) -> Result<(), LinkError> {
        let directive = Directive {
            kind: directive.kind.written_in(self.version),
            ..directive
        };
        let config = self.carried(&directive);
        let mut taken = config.as_ref().map(|_| Taken {
            lost: &self.lost,
            accepted: false,
        });
        let frame = Frame {
            protocol_version: self.version,
            message: Message::Directive(directive),
        };
        let bytes = match &config {
            Some(config) => codec::encode_carrying(&frame, config)?,
            None => codec::encode(&frame)?,
        };
        drop(config);
        self.tx.send(bytes).await?;
        if let Some(taken) = &mut taken {
            taken.accepted = true;
        }
        Ok(())
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
