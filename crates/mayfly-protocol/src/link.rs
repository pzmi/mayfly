//! The link ports: transport-agnostic, native async traits used through generics (ADR 0009).
//!
//! Each direction has a sending and a receiving half. A transport (`inproc`, `uds`: DP-02,
//! LNK-01) encodes and decodes every frame, stamps the session's version, refuses a frame of
//! another version before decoding it ([`crate::admit_frame`]) and never hands it up. It
//! promises neither delivery nor order: delivery is at-least-once because the relay re-sends a
//! directive until its answer is definite and the data plane re-reports its facts, and both
//! ends are idempotent (`formal/link.qnt`). The link contract suite (ver §5) pins these
//! promises against every transport.

use std::fmt;
use std::future::Future;

use crate::directive::Directive;
use crate::handshake::{HandshakeRefused, Hello, Welcome};
use crate::observation::Observation;
use crate::version::VersionRefusal;

/// Why a link operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// The link is closed: the peer left or the transport shut down. A frame accepted before
    /// may or may not have been delivered.
    Closed,
    /// The peer refused the handshake.
    Refused(HandshakeRefused),
    /// A frame's version was refused.
    Version(VersionRefusal),
    /// The transport failed. The text is for diagnostics; a transport must never put a secret in
    /// it.
    Transport(String),
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("link closed"),
            Self::Refused(refused) => write!(f, "handshake refused: {:?}", refused.reason),
            Self::Version(refusal) => write!(f, "version refused: {refusal:?}"),
            Self::Transport(detail) => write!(f, "transport failed: {detail}"),
        }
    }
}

impl std::error::Error for LinkError {}

/// The data plane's side of the handshake.
pub trait HandshakeInitiator: Send {
    /// Sends `hello` and waits for the answer. Not cancel-safe: a cancelled handshake is
    /// abandoned and a new one starts with a new `Hello`.
    ///
    /// # Errors
    /// [`LinkError::Refused`] when the control plane refuses, [`LinkError::Closed`] or
    /// [`LinkError::Transport`] when the link fails.
    fn hello(&mut self, hello: Hello) -> impl Future<Output = Result<Welcome, LinkError>> + Send;
}

/// The control plane's side of the handshake.
pub trait HandshakeResponder: Send {
    /// The next `Hello`. Cancel-safe: a hello not yet returned stays queued.
    ///
    /// # Errors
    /// [`LinkError::Closed`] when no data plane will connect any more.
    fn next_hello(&mut self) -> impl Future<Output = Result<Hello, LinkError>> + Send;

    /// Opens the session the last hello asked for. Not cancel-safe: on cancellation the data
    /// plane may or may not have the welcome, and says hello again if not.
    ///
    /// # Errors
    /// [`LinkError::Closed`] or [`LinkError::Transport`].
    fn welcome(&mut self, welcome: Welcome) -> impl Future<Output = Result<(), LinkError>> + Send;

    /// Refuses the last hello.
    ///
    /// # Errors
    /// [`LinkError::Closed`] or [`LinkError::Transport`].
    fn refuse(
        &mut self,
        refused: HandshakeRefused,
    ) -> impl Future<Output = Result<(), LinkError>> + Send;
}

/// The control plane sends directives.
pub trait DirectiveSink: Send + Sync {
    /// Hands `directive` to the transport. `Ok` means accepted, not delivered: the relay keeps
    /// re-sending until an answer is definite. Waits while the transport's bound is reached
    /// (backpressure). Not cancel-safe: a cancelled send may or may not have been accepted,
    /// which the relay's re-send covers.
    ///
    /// # Errors
    /// [`LinkError::Closed`] or [`LinkError::Transport`].
    fn send(&self, directive: Directive) -> impl Future<Output = Result<(), LinkError>> + Send;
}

/// The data plane receives directives.
pub trait DirectiveSource: Send {
    /// The next directive of an admitted version, in no promised order. Cancel-safe: a frame
    /// not yet returned stays queued.
    ///
    /// # Errors
    /// [`LinkError::Closed`] when the session ended.
    fn recv(&mut self) -> impl Future<Output = Result<Directive, LinkError>> + Send;
}

/// The data plane sends observations.
pub trait ObservationSink: Send + Sync {
    /// Hands `observation` to the transport; `Ok` means accepted, not delivered. Waits while
    /// the transport's bound is reached. Not cancel-safe, like [`DirectiveSink::send`].
    ///
    /// # Errors
    /// [`LinkError::Closed`] or [`LinkError::Transport`].
    fn send(&self, observation: Observation) -> impl Future<Output = Result<(), LinkError>> + Send;
}

/// The control plane receives observations.
pub trait ObservationSource: Send {
    /// The next observation of an admitted version, in no promised order and possibly a
    /// duplicate (the inbox deduplicates by `observation_id`). Cancel-safe.
    ///
    /// # Errors
    /// [`LinkError::Closed`] when the session ended.
    fn recv(&mut self) -> impl Future<Output = Result<Observation, LinkError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handshake::HandshakeRefusal;

    #[test]
    fn should_say_what_failed_when_a_link_error_is_shown() {
        // given
        let errors = [
            LinkError::Closed,
            LinkError::Refused(HandshakeRefused {
                reason: HandshakeRefusal::UnsupportedVersion {
                    supported: Vec::new(),
                },
            }),
            LinkError::Version(VersionRefusal::MajorNotSupported { major: 2 }),
            LinkError::Transport("reset".to_owned()),
        ];

        // when
        let shown = errors.map(|e| e.to_string());

        // then
        assert_eq!(
            shown,
            [
                "link closed",
                "handshake refused: UnsupportedVersion { supported: [] }",
                "version refused: MajorNotSupported { major: 2 }",
                "transport failed: reset",
            ]
        );
    }
}
