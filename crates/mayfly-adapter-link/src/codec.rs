//! Frames to bytes and back: JSON (ADR 0010), the header read and admitted before the body.
//!
//! A launch's frame may carry its JIT config (protocol 1.6, LNK-02): one more member,
//! [`CONFIG_MEMBER`], beside `protocol_version` and `message`. No type that holds the config is
//! `Serialize` (ADR 0014, `ARCH-secret-serialize`): [`encode_carrying`] writes the member into the
//! bytes of the encoded frame by hand, and [`decode`] reads it through a private struct that is
//! only `Deserialize` and wraps it in [`Secret`] at once. No error built here names a byte of it.

use mayfly_domain::Secret;
use mayfly_protocol::{
    Frame, FrameHeader, LinkError, Message, ProtocolVersion, VersionRefusal, admit_frame, negotiate,
};
use serde::Deserialize;

/// The member of a launch's frame that carries its JIT config (protocol 1.6). Pinned by
/// `tests::should_write_the_config_as_one_member_beside_the_message_when_a_frame_carries_it`.
pub(crate) const CONFIG_MEMBER: &str = "launch_config";

/// A frame's config member, read alone. Only `Deserialize`, and no `Debug`: what it holds goes
/// straight into a [`Secret`].
#[derive(Deserialize)]
struct CarriedConfig {
    #[serde(default)]
    launch_config: Option<String>,
}

/// A decoded frame and the config it carried, if any.
pub(crate) struct Decoded {
    /// The frame.
    pub(crate) frame: Frame,
    /// Its launch's JIT config, when the frame carried one.
    pub(crate) config: Option<Secret<String>>,
}

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

/// Encodes `frame` carrying `config` in [`CONFIG_MEMBER`]: the frame's own encoding, its
/// closing brace replaced by the member, the config written as a JSON string. The error names no
/// byte of the config. Pinned by
/// `tests::should_write_the_config_as_one_member_beside_the_message_when_a_frame_carries_it`.
pub(crate) fn encode_carrying(
    frame: &Frame,
    config: &Secret<String>,
) -> Result<Vec<u8>, LinkError> {
    let failed = || LinkError::Transport("a frame did not encode".to_owned());
    let mut bytes = encode(frame)?;
    if bytes.pop() != Some(b'}') {
        return Err(failed());
    }
    bytes.extend_from_slice(b",\"");
    bytes.extend_from_slice(CONFIG_MEMBER.as_bytes());
    bytes.extend_from_slice(b"\":");
    serde_json::to_writer(&mut bytes, config.expose().as_str()).map_err(|_| failed())?;
    bytes.push(b'}');
    Ok(bytes)
}

/// Decodes a frame when its header is admitted for `session` (`None`: before the handshake
/// completes) by a reader that speaks `supported`, with the config it carries, if any. The body
/// of a refused frame is never decoded: a frame of an unknown major may not parse as anything
/// this build knows. The config member is read from a directive's frame only (on any other it
/// is an unknown field, ignored); one that is not a string makes the frame malformed.
pub(crate) fn decode(
    bytes: &[u8],
    session: Option<ProtocolVersion>,
    supported: &[ProtocolVersion],
) -> Result<Decoded, Discard> {
    let header: FrameHeader = serde_json::from_slice(bytes).map_err(|_| Discard::Malformed)?;
    admit_frame(session, header.protocol_version, supported).map_err(Discard::Version)?;
    let frame: Frame = serde_json::from_slice(bytes).map_err(|_| Discard::Malformed)?;
    let config = match frame.message {
        Message::Directive(_) => {
            let carried: CarriedConfig =
                serde_json::from_slice(bytes).map_err(|_| Discard::Malformed)?;
            carried.launch_config.map(Secret::new)
        }
        Message::Hello(_)
        | Message::Welcome(_)
        | Message::HandshakeRefused(_)
        | Message::Observation(_) => None,
    };
    Ok(Decoded { frame, config })
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
    use mayfly_domain::{
        AttemptId, Deadline, DirectiveId, Epoch, Generation, HostId, Millis, OwnedVmName, PoolName,
        ProviderInstanceId, Sample, SecretHandleId, Slot,
    };
    use mayfly_protocol::{
        AttemptRef, CURRENT, Directive, DirectiveKind, Message, Mode, SecretHandle, Welcome,
    };
    use serde_json::Value;

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
        let decoded = decode(&bytes, None, &[CURRENT]).map(|d| (d.frame, d.config.is_none()));

        // then
        assert_eq!(decoded, Ok((frame, true)));
    }

    #[test]
    fn should_discard_by_header_when_the_version_is_not_admitted() {
        // given
        let foreign = encode(&welcome_frame(v(2, 0))).expect("encodes");
        let newer_minor = encode(&welcome_frame(v(1, 1))).expect("encodes");

        // when
        let before_session = decode(&foreign, None, &[CURRENT]).map(|d| d.frame);
        let in_old_session = decode(&newer_minor, Some(v(1, 0)), &[CURRENT]).map(|d| d.frame);

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
        let decoded = inputs.map(|bytes| decode(bytes, None, &[CURRENT]).map(|d| d.frame));

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

    fn launch_frame() -> Frame {
        Frame {
            protocol_version: CURRENT,
            message: Message::Directive(Directive {
                directive_id: DirectiveId::from_u128(0xd1),
                epoch: Epoch::new(7),
                attempt: Some(AttemptRef {
                    attempt_id: AttemptId::sample(),
                    generation: Generation::FIRST,
                }),
                deadline: Deadline::at(Millis::new(60_000)),
                kind: DirectiveKind::LaunchRunner {
                    vm_name: OwnedVmName::new(
                        PoolName::sample(),
                        HostId::sample(),
                        Slot::new(1),
                        Generation::FIRST,
                        AttemptId::sample(),
                    ),
                    launch: SecretHandle {
                        handle_id: SecretHandleId::from_u128(0x5ec),
                    },
                    instance_id: ProviderInstanceId::new("7e:0b:14:52:9a:c1").ok(),
                },
            }),
        }
    }

    /// A config built at run time, with a quote and a backslash the encoding must escape.
    fn config_text() -> String {
        ["codec", "fixture", "config", "\"q\\"].join("-")
    }

    #[test]
    fn should_write_the_config_as_one_member_beside_the_message_when_a_frame_carries_it() {
        // given
        let frame = launch_frame();
        let config = Secret::new(config_text());

        // when
        let bytes = encode_carrying(&frame, &config).expect("protocol data encodes");

        // then
        let mut written: Value = serde_json::from_slice(&bytes).expect("the frame is JSON");
        let member = written
            .as_object_mut()
            .and_then(|o| o.remove(CONFIG_MEMBER))
            .expect("the member is written");
        assert_eq!(member, Value::from(config_text()));
        let plain: Value =
            serde_json::from_slice(&encode(&frame).expect("encodes")).expect("the frame is JSON");
        assert_eq!(written, plain);
        let decoded = decode(&bytes, Some(CURRENT), &[CURRENT]).expect("decodes");
        assert_eq!(decoded.frame, frame);
        assert_eq!(
            decoded.config.as_ref().map(Secret::expose),
            Some(&config_text())
        );
    }

    #[test]
    fn should_read_no_config_when_a_frame_carries_none() {
        // given
        let bytes = encode(&launch_frame()).expect("encodes");

        // when
        let decoded = decode(&bytes, Some(CURRENT), &[CURRENT]).expect("decodes");

        // then
        assert!(decoded.config.is_none());
        assert!(!String::from_utf8_lossy(&bytes).contains(CONFIG_MEMBER));
    }

    #[test]
    fn should_discard_as_malformed_when_the_config_member_is_not_a_string() {
        // given
        let mut value: Value =
            serde_json::from_slice(&encode(&launch_frame()).expect("encodes")).expect("JSON");
        if let Some(object) = value.as_object_mut() {
            object.insert(CONFIG_MEMBER.to_owned(), Value::from(7));
        }
        let bytes = serde_json::to_vec(&value).expect("encodes");

        // when
        let decoded = decode(&bytes, Some(CURRENT), &[CURRENT]).map(|d| d.frame);

        // then
        assert_eq!(decoded, Err(Discard::Malformed));
    }
}
