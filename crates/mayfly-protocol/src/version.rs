//! Protocol versions and their negotiation (plan §3.2, ADR 0010).
//!
//! A version is `major.minor`. A peer speaks every minor up to its highest within each major
//! it lists. Within a major, a newer minor only adds optional fields and new kinds, and a frame
//! never uses more than the minor the session accepted, so an older reader that ignores what it
//! does not know stays correct. A new major is a break: a peer refuses a frame of a major it
//! does not speak, before it reads the body.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A protocol version: `major.minor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProtocolVersion {
    /// Incompatible changes.
    pub major: u16,
    /// Additive changes within the major.
    pub minor: u16,
}

impl ProtocolVersion {
    /// A version.
    #[must_use]
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }
}

/// The version this build writes when the session allows it.
pub const CURRENT: ProtocolVersion = ProtocolVersion::new(1, 5);

/// The minor that added what the data plane cannot determine (ADR 0052: the refusal
/// `capacity_unknown`, a capacity report's `unknown` dimensions, a listed VM's facts) and
/// idempotent destructive directives (ADR 0053: the unknown reason `listing_incomplete`). A
/// session below it is written without them ([`crate::ObservationKind::written_in`]).
pub const UNKNOWN_FACTS: ProtocolVersion = ProtocolVersion::new(1, 1);

/// The minor that added the refusal `target_absent` (ADR 0066: a `StopVm` or `StartVm` whose
/// recorded VM a complete listing shows absent, which the control plane quarantines as a desync)
/// and the `Welcome`'s control-plane clock reading (ADR 0020, amendment accepted 2026-11-10). A
/// session below it is written `proof_mismatch` instead, which a stop's older reader quarantines
/// too ([`crate::ObservationKind::written_in`]); the clock reading is a handshake field an older
/// reader ignores.
pub const VANISHED_TARGET: ProtocolVersion = ProtocolVersion::new(1, 2);

/// The minor that added a launch's instance id (ADR 0038 decision 2, ADR 0082): the data plane
/// compares it with the VM's right before it delivers the JIT config. A session below it is
/// written without it ([`crate::DirectiveKind::written_in`]), and the data plane refuses such a
/// launch `proof_mismatch`, which the control plane quarantines (ADR 0038: no delivery
/// unchecked).
pub const GUEST_IDENTITY: ProtocolVersion = ProtocolVersion::new(1, 3);

/// The minor that added a materialised base's measured disk (`ImageMaterialised::disk_bytes`,
/// ADR 0052's amendment of 2026-11-12; CP-07): the disk its image's manifest reconstructs to, so
/// the control plane plans a clone at it when `clone_disk_gib` is not configured. A session
/// below it is written without it ([`crate::ObservationKind::written_in`]); the control plane
/// then reads it as unknown and plans the clone with no disk, and the data plane's gate (I33)
/// refuses what does not fit, as before.
pub const MEASURED_BASE_DISK: ProtocolVersion = ProtocolVersion::new(1, 4);

/// The minor that added the image-store listing beside a complete inventory
/// (`ObservationKind::Inventory::image_store`, DP-08): the data plane's verified images and its
/// `Ready` bases, from which the control plane resolves a pull or a materialisation whose outcome
/// is unknown, with no new directive and no outcome asserted (ADR 0025, P7). A session below it
/// is written without it ([`crate::ObservationKind::written_in`]); the control plane then
/// resolves as before: a materialisation or an eviction only by its base's absence from the
/// inventory, a pull only by its late answer.
pub const IMAGE_LISTING: ProtocolVersion = ProtocolVersion::new(1, 5);

/// What this build speaks: the highest minor of each major, one entry per major.
pub const SUPPORTED: &[ProtocolVersion] = &[CURRENT];

/// Why a version was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionRefusal {
    /// The two peers share no major.
    NoCommonMajor {
        /// What the refusing side speaks.
        supported: Vec<ProtocolVersion>,
    },
    /// A handshake frame of a major the reader does not speak.
    MajorNotSupported {
        /// The frame's major.
        major: u16,
    },
    /// A frame of another major than the session's.
    MajorMismatch {
        /// The session's version.
        session: ProtocolVersion,
        /// The frame's version.
        frame: ProtocolVersion,
    },
    /// A frame of a newer minor than the session accepted.
    MinorAboveSession {
        /// The session's version.
        session: ProtocolVersion,
        /// The frame's version.
        frame: ProtocolVersion,
    },
}

/// Highest minor per major; a major listed twice keeps its highest minor.
fn by_major(versions: &[ProtocolVersion]) -> BTreeMap<u16, u16> {
    let mut majors = BTreeMap::new();
    for v in versions {
        let minor = majors.entry(v.major).or_insert(v.minor);
        if v.minor > *minor {
            *minor = v.minor;
        }
    }
    majors
}

/// The session version for what the data plane offers (its `Hello`) and what the control plane
/// speaks: the highest major both list, at the lower of the two highest minors in it. The
/// result does not depend on the order of either list. Pinned by
/// `should_pick_the_highest_common_major_at_the_lower_minor_when_both_offer_several` and
/// `should_refuse_when_no_major_is_common`.
///
/// # Errors
/// [`VersionRefusal::NoCommonMajor`], naming what `supported` lists, when they share none.
pub fn negotiate(
    offered: &[ProtocolVersion],
    supported: &[ProtocolVersion],
) -> Result<ProtocolVersion, VersionRefusal> {
    let ours = by_major(supported);
    by_major(offered)
        .into_iter()
        .rev()
        .find_map(|(major, minor)| {
            ours.get(&major)
                .map(|own| ProtocolVersion::new(major, minor.min(*own)))
        })
        .ok_or_else(|| VersionRefusal::NoCommonMajor {
            supported: ours
                .into_iter()
                .map(|(major, minor)| ProtocolVersion::new(major, minor))
                .collect(),
        })
}

/// The version a handshake frame (`Hello`, `Welcome`, `HandshakeRefused`) is written in: the
/// oldest major the writer speaks, at its highest minor, so a peer that speaks only that major
/// can still read the handshake and answer it. `None` when the writer speaks nothing.
#[must_use]
pub fn handshake_version(supported: &[ProtocolVersion]) -> Option<ProtocolVersion> {
    by_major(supported)
        .into_iter()
        .next()
        .map(|(major, minor)| ProtocolVersion::new(major, minor))
}

/// Whether a reader admits a frame written in `frame`, read from its header before the body.
/// Before the handshake (`session` is `None`) the major must be one the reader speaks; within a
/// session the major must be the session's and the minor no newer than it. A refused frame is
/// discarded unexecuted; the link model's `foreignMajorDiscardedTest` states the rule and
/// `tests/golden.rs::should_refuse_every_fixture_when_its_major_is_unknown` pins it.
///
/// # Errors
/// The [`VersionRefusal`] that applies.
pub fn admit_frame(
    session: Option<ProtocolVersion>,
    frame: ProtocolVersion,
    supported: &[ProtocolVersion],
) -> Result<(), VersionRefusal> {
    match session {
        None if by_major(supported).contains_key(&frame.major) => Ok(()),
        None => Err(VersionRefusal::MajorNotSupported { major: frame.major }),
        Some(s) if s.major != frame.major => {
            Err(VersionRefusal::MajorMismatch { session: s, frame })
        }
        Some(s) if frame.minor > s.minor => {
            Err(VersionRefusal::MinorAboveSession { session: s, frame })
        }
        Some(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn v(major: u16, minor: u16) -> ProtocolVersion {
        ProtocolVersion::new(major, minor)
    }

    #[test]
    fn should_pick_the_highest_common_major_at_the_lower_minor_when_both_offer_several() {
        // given
        let offered = [v(1, 4), v(2, 1), v(3, 0)];
        let supported = [v(2, 3), v(1, 2)];

        // when
        let session = negotiate(&offered, &supported);

        // then
        assert_eq!(session, Ok(v(2, 1)));
        assert_eq!(
            negotiate(&[v(3, 0), v(2, 1), v(1, 4)], &supported),
            Ok(v(2, 1))
        );
        assert_eq!(negotiate(&[v(1, 9)], &supported), Ok(v(1, 2)));
    }

    #[test]
    fn should_keep_the_highest_minor_when_a_major_is_listed_twice() {
        // given
        let offered = [v(1, 0), v(1, 3)];

        // when
        let session = negotiate(&offered, &[v(1, 5)]);

        // then
        assert_eq!(session, Ok(v(1, 3)));
    }

    #[test]
    fn should_refuse_when_no_major_is_common() {
        // given
        let offered = [v(2, 0)];

        // when
        let refused = negotiate(&offered, SUPPORTED);

        // then
        assert_eq!(
            refused,
            Err(VersionRefusal::NoCommonMajor {
                supported: vec![CURRENT]
            })
        );
        assert!(negotiate(&[], SUPPORTED).is_err());
    }

    #[test]
    fn should_write_the_handshake_in_the_oldest_major_when_several_are_spoken() {
        // given
        let supported = [v(2, 1), v(1, 4)];

        // when
        let written = handshake_version(&supported);

        // then
        assert_eq!(written, Some(v(1, 4)));
        assert_eq!(handshake_version(&[]), None);
    }

    #[test]
    fn should_refuse_an_unknown_major_when_no_session_is_open() {
        // given
        let frame = v(2, 0);

        // when
        let admitted = admit_frame(None, frame, SUPPORTED);

        // then
        assert_eq!(
            admitted,
            Err(VersionRefusal::MajorNotSupported { major: 2 })
        );
        assert_eq!(admit_frame(None, v(1, 7), SUPPORTED), Ok(()));
    }

    #[test]
    fn should_hold_frames_to_the_session_when_one_is_open() {
        // given
        let session = Some(v(1, 2));
        let both = [v(1, 2), v(2, 0)];

        // when
        let admitted =
            [v(1, 0), v(1, 2), v(1, 3), v(2, 0)].map(|frame| admit_frame(session, frame, &both));

        // then
        assert_eq!(
            admitted,
            [
                Ok(()),
                Ok(()),
                Err(VersionRefusal::MinorAboveSession {
                    session: v(1, 2),
                    frame: v(1, 3)
                }),
                Err(VersionRefusal::MajorMismatch {
                    session: v(1, 2),
                    frame: v(2, 0)
                }),
            ]
        );
    }
}
