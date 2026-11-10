//! The frame: every message with the version it is written in (plan §3.2, ADR 0010).

use serde::{Deserialize, Serialize};

use crate::directive::Directive;
use crate::handshake::{HandshakeRefused, Hello, Welcome};
use crate::observation::Observation;
use crate::version::ProtocolVersion;

/// One message on the link. Transports encode and decode every frame, the in-process one
/// included (plan §3.3), so a field that does not serialise cannot slip through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    /// The version the frame is written in. A reader checks it ([`crate::admit_frame`]) before
    /// it decodes the message.
    pub protocol_version: ProtocolVersion,
    /// The message.
    pub message: Message,
}

/// Every frame's first field, decoded alone: any frame of any version decodes as a header,
/// because unknown fields are ignored, so a reader can refuse an unknown major before it reads
/// a body it may not understand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameHeader {
    /// The version the frame is written in.
    pub protocol_version: ProtocolVersion,
}

/// A message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Message {
    /// Data plane → control plane: open a session.
    Hello(Hello),
    /// Control plane → data plane: the session is open.
    Welcome(Welcome),
    /// Control plane → data plane: no session.
    HandshakeRefused(HandshakeRefused),
    /// Control plane → data plane: do something.
    Directive(Directive),
    /// Data plane → control plane: something was observed.
    Observation(Observation),
}

/// A message kind, named as on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MessageKind {
    /// [`Message::Hello`].
    Hello,
    /// [`Message::Welcome`].
    Welcome,
    /// [`Message::HandshakeRefused`].
    HandshakeRefused,
    /// [`Message::Directive`].
    Directive,
    /// [`Message::Observation`].
    Observation,
}

impl MessageKind {
    /// Every kind.
    pub const ALL: [Self; 5] = [
        Self::Hello,
        Self::Welcome,
        Self::HandshakeRefused,
        Self::Directive,
        Self::Observation,
    ];

    /// The wire name: the message's tag and its golden fixture directory.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Hello => "hello",
            Self::Welcome => "welcome",
            Self::HandshakeRefused => "handshake_refused",
            Self::Directive => "directive",
            Self::Observation => "observation",
        }
    }

    /// Whether the kind belongs to the handshake, whose frames are written in the oldest major
    /// the writer speaks ([`crate::handshake_version`]).
    #[must_use]
    pub const fn is_handshake(self) -> bool {
        match self {
            Self::Hello | Self::Welcome | Self::HandshakeRefused => true,
            Self::Directive | Self::Observation => false,
        }
    }
}

impl Message {
    /// The message's kind.
    #[must_use]
    pub const fn kind(&self) -> MessageKind {
        match self {
            Self::Hello(_) => MessageKind::Hello,
            Self::Welcome(_) => MessageKind::Welcome,
            Self::HandshakeRefused(_) => MessageKind::HandshakeRefused,
            Self::Directive(_) => MessageKind::Directive,
            Self::Observation(_) => MessageKind::Observation,
        }
    }
}

#[cfg(test)]
mod tests {
    use mayfly_domain::Epoch;

    use super::*;
    use crate::handshake::{HandshakeRefusal, Mode};
    use crate::version::CURRENT;

    #[test]
    fn should_tell_the_handshake_kinds_when_messages_are_classified() {
        // given
        let messages = [
            Message::Welcome(Welcome {
                epoch: Epoch::FIRST,
                mode: Mode::Normal,
                accepted_version: CURRENT,
                cp_clock: None,
            }),
            Message::HandshakeRefused(HandshakeRefused {
                reason: HandshakeRefusal::UnsupportedVersion {
                    supported: vec![CURRENT],
                },
            }),
        ];

        // when
        let kinds = messages.each_ref().map(Message::kind);

        // then
        assert_eq!(kinds, [MessageKind::Welcome, MessageKind::HandshakeRefused]);
        let handshake: Vec<MessageKind> = MessageKind::ALL
            .into_iter()
            .filter(|k| k.is_handshake())
            .collect();
        assert_eq!(
            handshake,
            [
                MessageKind::Hello,
                MessageKind::Welcome,
                MessageKind::HandshakeRefused
            ]
        );
    }
}
