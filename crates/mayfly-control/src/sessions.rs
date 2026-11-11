//! The data-plane sessions the control plane holds: per host, the run epoch its last
//! `Welcome` opened and the session's mode (plan §3.2, ADR 0026).
//!
//! Start-up's handshake writes it (`crate::recovery::Startup::open`; a reconnect, RT-02); the
//! attempt saga reads the epoch its commands are issued under (I18), and the directive relay's
//! sink stamps it on every frame at send time and holds the kinds safe mode refuses (I29). The
//! epoch is never stored with a directive: a directive decided once is sent under whichever
//! session is current when it is sent (`doc/event-store.md` §5.3).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use mayfly_domain::{Epoch, HostId};
use mayfly_protocol::Mode;

/// One host's current session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostSession {
    /// The run epoch the session's `Welcome` carried.
    pub epoch: Epoch,
    /// Whether it runs every directive kind or holds effects (safe mode).
    pub mode: Mode,
}

/// The table of current sessions by host. Clones share one table.
#[derive(Debug, Clone, Default)]
pub struct HostSessions {
    table: Arc<Mutex<BTreeMap<HostId, HostSession>>>,
}

impl HostSessions {
    /// An empty table: no host has a session, so nothing is sent and the saga issues nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the session a `Welcome` opened for `host`, replacing an older one.
    pub fn open(&self, host: HostId, session: HostSession) {
        self.table
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(host, session);
    }

    /// Changes the mode of `host`'s session, if it has one.
    pub fn set_mode(&self, host: &HostId, mode: Mode) {
        if let Some(session) = self
            .table
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(host)
        {
            session.mode = mode;
        }
    }

    /// Forgets `host`'s session (its data plane left): nothing is sent or issued for it until
    /// the next welcome.
    pub fn close(&self, host: &HostId) {
        self.table
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(host);
    }

    /// `host`'s current session.
    #[must_use]
    pub fn get(&self, host: &HostId) -> Option<HostSession> {
        self.table
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(host)
            .copied()
    }
}
