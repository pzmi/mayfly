//! The handshake and the run-epoch rules of a session (plan §3.2, ADR 0026, `formal/link.qnt`).
//!
//! The data plane says `Hello` on every start and reconnect; the control plane answers with a
//! `Welcome` that opens a session under a freshly minted run epoch, or refuses the version. The
//! rules both peers apply to epochs are the pure functions here; the link model states them as
//! `i09NoStaleEpochExecutes`, `i17EpochsIncrease` and `executesAtMostOncePerEpoch`.

use std::collections::BTreeSet;

use mayfly_domain::{Epoch, HostId, Millis};
use serde::{Deserialize, Serialize};

use crate::version::ProtocolVersion;

/// Data plane → control plane, first frame of every session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The host the data plane runs on.
    pub host_id: HostId,
    /// The data plane process's boot id: opaque, compared only for equality, so the control
    /// plane can tell a reconnect of the same process from a restart.
    pub boot_id: String,
    /// The data plane's build version, for status and diagnostics only.
    pub dp_version: String,
    /// The protocol versions the data plane speaks, the highest minor of each major.
    pub supported: Vec<ProtocolVersion>,
    /// Named optional features the data plane offers; an unknown name is ignored.
    pub capabilities: BTreeSet<String>,
    /// The epoch floor kept in `TART_HOME` (ADR 0026): the highest epoch ever entered. Zero
    /// when none was.
    pub epoch_floor: Epoch,
}

/// Whether the session may execute effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Every directive kind runs.
    Normal,
    /// Containment: the kinds [`crate::DirectiveKind::held_in_safe_mode`] names are refused
    /// (I29); observation continues.
    Safe,
}

/// Control plane → data plane: the session is open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// The run epoch minted for this session ([`mint_epoch`]).
    pub epoch: Epoch,
    /// The session's mode.
    pub mode: Mode,
    /// The negotiated version ([`crate::negotiate`]): no frame of the session uses more.
    pub accepted_version: ProtocolVersion,
    /// The control plane's `Clock` reading (wall-clock milliseconds since the Unix epoch, ADR
    /// 0020) when it wrote this welcome: the clock every directive deadline of the session is
    /// written in. The data plane records its offset to its own monotonic clock when it enters
    /// the session and compares every deadline of the session in monotonic time, so a step of
    /// either wall clock after the handshake moves no deadline (ADR 0020, amendment accepted
    /// 2026-11-10). `None` from a control plane older than protocol 1.2, which leaves it out:
    /// the data plane then reads its own wall clock at receipt, as before. A handshake field,
    /// so an older data plane ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cp_clock: Option<Millis>,
}

/// Control plane → data plane: no session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeRefused {
    /// Why.
    pub reason: HandshakeRefusal,
}

/// Why a handshake was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandshakeRefusal {
    /// The peers share no major.
    UnsupportedVersion {
        /// What the control plane speaks.
        supported: Vec<ProtocolVersion>,
    },
}

/// The run epoch for a new session: one above both the highest the journal minted and the
/// floor the data plane reported, so it is above every epoch either side has seen (I17). `None`
/// when the epochs are exhausted. Pinned by
/// `should_mint_above_the_journal_and_the_floor_when_a_session_opens` and the link model's
/// `epochMintedAboveFloorTest`.
#[must_use]
pub fn mint_epoch(minted: Epoch, floor: Epoch) -> Option<Epoch> {
    minted.max(floor).next()
}

/// Whether the data plane enters a session on this `Welcome`: only strictly above its floor, so
/// a session it already left (whose per-epoch table is gone after a restart) is never entered
/// again, and a duplicated welcome cannot reopen it. The floor is raised to the epoch and
/// fsynced before anything of the session runs. Pinned by
/// `should_enter_only_above_the_floor_when_a_welcome_arrives` and the link model's
/// `oldWelcomeAfterRestartIgnoredTest`.
#[must_use]
pub fn enters_session(floor: Epoch, welcome: Epoch) -> bool {
    welcome > floor
}

/// How the data plane treats a directive frame by its epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpochCheck {
    /// The current session's frame: it may execute, at most once per directive id.
    Current,
    /// An older session's frame: never executed, answered
    /// [`crate::ObservationKind::OutcomeUnknown`] with
    /// [`crate::UnknownReason::StaleEpoch`], never a refusal (ADR 0042: an earlier delivery of
    /// the same directive may have taken effect before its answer was lost).
    Stale,
    /// No session is open, or the frame is of a session not entered yet: discarded unanswered;
    /// the relay re-sends under the current session.
    NotEntered,
}

/// Classifies a directive frame's epoch against the open session (I09). Pinned by
/// `should_fence_frames_by_the_session_epoch_when_a_directive_arrives` and the link model's
/// `staleFrameIsNeverRefusedTest` and `frameBeforeWelcomeDiscardedTest`.
#[must_use]
pub fn check_epoch(session: Option<Epoch>, frame: Epoch) -> EpochCheck {
    match session {
        Some(current) if frame == current => EpochCheck::Current,
        Some(current) if frame < current => EpochCheck::Stale,
        Some(_) | None => EpochCheck::NotEntered,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn e(value: u64) -> Epoch {
        Epoch::new(value)
    }

    #[test]
    fn should_mint_above_the_journal_and_the_floor_when_a_session_opens() {
        // given
        let cases = [
            ((0, 0), Some(1)),
            ((4, 2), Some(5)),
            ((2, 7), Some(8)),
            ((u64::MAX, 0), None),
        ];

        // when
        let minted: Vec<Option<Epoch>> = cases
            .iter()
            .map(|((minted, floor), _)| mint_epoch(e(*minted), e(*floor)))
            .collect();

        // then
        let expected: Vec<Option<Epoch>> = cases.iter().map(|(_, want)| want.map(e)).collect();
        assert_eq!(minted, expected);
    }

    #[test]
    fn should_read_a_welcome_without_the_clock_when_an_older_control_plane_wrote_it() {
        // given
        let older = r#"{"epoch":7,"mode":"normal","accepted_version":{"major":1,"minor":1}}"#;
        let newer = Welcome {
            epoch: e(7),
            mode: Mode::Normal,
            accepted_version: crate::CURRENT,
            cp_clock: Some(Millis::new(1_700_000_000_000)),
        };

        // when
        let read: Welcome = serde_json::from_str(older).expect("an older welcome decodes");
        let written = serde_json::to_string(&Welcome {
            cp_clock: None,
            ..newer.clone()
        })
        .expect("encodes");

        // then
        assert_eq!(read.cp_clock, None);
        assert!(!written.contains("cp_clock"), "{written}");
        assert!(
            serde_json::to_string(&newer)
                .expect("encodes")
                .contains("\"cp_clock\":1700000000000")
        );
    }

    #[test]
    fn should_enter_only_above_the_floor_when_a_welcome_arrives() {
        // given
        let floor = e(3);

        // when
        let entered = [e(4), e(3), e(2)].map(|w| enters_session(floor, w));

        // then
        assert_eq!(entered, [true, false, false]);
    }

    #[test]
    fn should_fence_frames_by_the_session_epoch_when_a_directive_arrives() {
        // given
        let session = Some(e(5));

        // when
        let checks = [
            check_epoch(session, e(5)),
            check_epoch(session, e(4)),
            check_epoch(session, e(6)),
            check_epoch(None, e(5)),
        ];

        // then
        assert_eq!(
            checks,
            [
                EpochCheck::Current,
                EpochCheck::Stale,
                EpochCheck::NotEntered,
                EpochCheck::NotEntered
            ]
        );
    }
}
