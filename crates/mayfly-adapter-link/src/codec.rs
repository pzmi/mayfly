//! Frames to bytes and back: JSON (ADR 0010), the header read and admitted before the body.

use mayfly_protocol::{
    Frame, FrameHeader, LinkError, ProtocolVersion, VersionRefusal, admit_frame, negotiate,
};

/// Why a received frame was discarded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Discard {
    /// Its version is not admitted ([`admit_frame`]).
    Version(VersionRefusal),
    /// It is not a frame: no header, or a body that does not decode in an admitted version.
    Malformed,
}

/// Encodes a frame. Serialising protocol data fails only on a type that cannot serialise, so
/// the error carries no detail (and so no field value).
pub(crate) fn encode(frame: &Frame) -> Result<Vec<u8>, LinkError> {
    serde_json::to_vec(frame).map_err(|_| LinkError::Transport("a frame did not encode".to_owned()))
}

/// Decodes a frame when its header is admitted for `session` (`None`: before the handshake
/// completes) by a reader that speaks `supported`. The body of a refused frame is never
/// decoded: a frame of an unknown major may not parse as anything this build knows.
pub(crate) fn decode(
    bytes: &[u8],
    session: Option<ProtocolVersion>,
    supported: &[ProtocolVersion],
) -> Result<Frame, Discard> {
    let header: FrameHeader = serde_json::from_slice(bytes).map_err(|_| Discard::Malformed)?;
    admit_frame(session, header.protocol_version, supported).map_err(Discard::Version)?;
    serde_json::from_slice(bytes).map_err(|_| Discard::Malformed)
}

/// Whether this end can hold a session at `accepted`: its major is spoken and its minor no
/// newer than the highest this end speaks in it.
pub(crate) fn speaks(
    accepted: ProtocolVersion,
    supported: &[ProtocolVersion],
) -> Result<(), VersionRefusal> {
    match negotiate(&[accepted], supported) {
        Ok(spoken) if spoken == accepted => Ok(()),
        Ok(spoken) => Err(VersionRefusal::MinorAboveSession {
            session: spoken,
            frame: accepted,
        }),
        Err(_) => Err(VersionRefusal::MajorNotSupported {
            major: accepted.major,
        }),
    }
}

#[cfg(test)]
mod tests {
    use mayfly_domain::Epoch;
    use mayfly_protocol::{CURRENT, Message, Mode, Welcome};

    use super::*;

    const fn v(major: u16, minor: u16) -> ProtocolVersion {
        ProtocolVersion::new(major, minor)
    }

    fn welcome_frame(version: ProtocolVersion) -> Frame {
        Frame {
            protocol_version: version,
            message: Message::Welcome(Welcome {
                epoch: Epoch::FIRST,
                mode: Mode::Normal,
                accepted_version: CURRENT,
                cp_clock: None,
            }),
        }
    }

    #[test]
    fn should_decode_what_it_encoded_when_the_version_is_admitted() {
        // given
        let frame = welcome_frame(CURRENT);
        let bytes = encode(&frame).expect("protocol data encodes");

        // when
        let decoded = decode(&bytes, None, &[CURRENT]);

        // then
        assert_eq!(decoded, Ok(frame));
    }

    #[test]
    fn should_discard_by_header_when_the_version_is_not_admitted() {
        // given
        let foreign = encode(&welcome_frame(v(2, 0))).expect("encodes");
        let newer_minor = encode(&welcome_frame(v(1, 1))).expect("encodes");

        // when
        let before_session = decode(&foreign, None, &[CURRENT]);
        let in_old_session = decode(&newer_minor, Some(v(1, 0)), &[CURRENT]);

        // then
        assert_eq!(
            before_session,
            Err(Discard::Version(VersionRefusal::MajorNotSupported {
                major: 2
            }))
        );
        assert_eq!(
            in_old_session,
            Err(Discard::Version(VersionRefusal::MinorAboveSession {
                session: v(1, 0),
                frame: v(1, 1),
            }))
        );
    }

    #[test]
    fn should_discard_as_malformed_when_the_bytes_are_not_a_frame() {
        // given
        let inputs: [&[u8]; 3] = [
            b"not json",
            br#"{"message":{"hello":{}}}"#,
            br#"{"protocol_version":{"major":1,"minor":0},"message":{"hello":{"x":1}}}"#,
        ];

        // when
        let decoded = inputs.map(|bytes| decode(bytes, None, &[CURRENT]));

        // then
        assert_eq!(decoded, [const { Err(Discard::Malformed) }; 3]);
    }

    #[test]
    fn should_hold_a_session_only_in_a_spoken_version_when_one_is_accepted() {
        // given
        let supported = [v(1, 1)];

        // when
        let held =
            [v(1, 0), v(1, 1), v(1, 2), v(2, 0)].map(|accepted| speaks(accepted, &supported));

        // then
        assert_eq!(
            held,
            [
                Ok(()),
                Ok(()),
                Err(VersionRefusal::MinorAboveSession {
                    session: v(1, 1),
                    frame: v(1, 2),
                }),
                Err(VersionRefusal::MajorNotSupported { major: 2 }),
            ]
        );
    }
}
