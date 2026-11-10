//! The data plane's side of a session (plan §3.2, ADR 0026, ADR 0042; `formal/link.qnt`'s
//! `dpHello`, `dpWelcome` and `dpRestart`).
//!
//! A `Hello` ends the open session and its outcome table; a `Welcome` is entered only while a
//! hello waits, only strictly above the epoch floor ([`mayfly_protocol::enters_session`]) and
//! only in a version this build speaks; the floor is raised (and fsynced by the caller) before
//! the session opens, so no frame of it runs first. A restart is a new [`crate::DataPlane`] built
//! from the persisted floor: everything else is gone. The table is pinned by
//! `tests/data_plane/matrix.rs::should_define_every_pair_when_each_session_state_meets_each_stimulus`
//! (fixture `matrix/link.txt`).

use std::collections::BTreeMap;

use mayfly_domain::{DirectiveId, Epoch};
use mayfly_protocol::{Mode, ProtocolVersion, SUPPORTED, Welcome, enters_session};

use crate::executor::Entry;

/// The open session: its epoch, mode, version and outcome table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// The run epoch it was welcomed at, now the floor.
    pub epoch: Epoch,
    /// Normal or safe (I29).
    pub mode: Mode,
    /// The negotiated version: no frame of the session uses more.
    pub version: ProtocolVersion,
    /// Every directive id this session received, with its phase and outcome: a repeat answers
    /// what the first delivery recorded (`executesAtMostOncePerEpoch`).
    pub table: BTreeMap<DirectiveId, Entry>,
}

/// Where the session stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// No hello sent yet (a fresh start).
    Idle,
    /// A hello was sent; a welcome is awaited.
    Awaiting,
    /// A welcome was accepted; its epoch is being made the floor before the session opens.
    Entering(Welcome),
    /// The session is open.
    Open(Session),
}

/// One stimulus of the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkStimulus {
    /// The data plane says hello (a start or a reconnect).
    Hello,
    /// A welcome arrived.
    Welcome(Welcome),
    /// The caller made `Epoch` the floor and fsynced it.
    FloorRaised(Epoch),
}

/// What a stimulus led to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkOutcome {
    /// A hello is to be sent; the open session, if any, ended.
    SendHello,
    /// Raise the floor to this epoch (fsync) and report [`LinkStimulus::FloorRaised`].
    RaiseFloor(Epoch),
    /// The session is open at this epoch, now the floor.
    Entered(Epoch),
    /// Ignored, and why.
    Ignored(LinkIgnored),
}

/// Why a stimulus was ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkIgnored {
    /// A welcome while no hello waits (a duplicate, or one of an earlier hello).
    NotAwaiting,
    /// A welcome at or below the floor: that session was entered before and its table is
    /// gone, so it is never entered again (ADR 0042).
    AtOrBelowFloor,
    /// A welcome in a version this build does not speak.
    Version,
    /// A floor report that is not for the welcome being entered.
    NotEntering,
}

/// Whether this build speaks `version` (its major, at a minor no newer than its own).
fn speaks(version: ProtocolVersion) -> bool {
    SUPPORTED
        .iter()
        .any(|own| own.major == version.major && version.minor <= own.minor)
}

/// The session's transition for `stimulus` with the floor at `floor`.
#[must_use]
pub fn step(link: &Link, floor: Epoch, stimulus: LinkStimulus) -> (Link, LinkOutcome) {
    match stimulus {
        LinkStimulus::Hello => (Link::Awaiting, LinkOutcome::SendHello),
        LinkStimulus::Welcome(welcome) => match link {
            Link::Awaiting if !enters_session(floor, welcome.epoch) => (
                link.clone(),
                LinkOutcome::Ignored(LinkIgnored::AtOrBelowFloor),
            ),
            Link::Awaiting if !speaks(welcome.accepted_version) => {
                (link.clone(), LinkOutcome::Ignored(LinkIgnored::Version))
            }
            Link::Awaiting => {
                let epoch = welcome.epoch;
                (Link::Entering(welcome), LinkOutcome::RaiseFloor(epoch))
            }
            Link::Idle | Link::Entering(_) | Link::Open(_) => {
                (link.clone(), LinkOutcome::Ignored(LinkIgnored::NotAwaiting))
            }
        },
        LinkStimulus::FloorRaised(epoch) => match link {
            Link::Entering(welcome) if welcome.epoch == epoch => (
                Link::Open(Session {
                    epoch,
                    mode: welcome.mode,
                    version: welcome.accepted_version,
                    table: BTreeMap::new(),
                }),
                LinkOutcome::Entered(epoch),
            ),
            Link::Idle | Link::Awaiting | Link::Entering(_) | Link::Open(_) => {
                (link.clone(), LinkOutcome::Ignored(LinkIgnored::NotEntering))
            }
        },
    }
}
