//! [`JitBroker`]: the control plane's minted JIT configs held for their one launch (plan §3.1,
//! §3.2): in memory, single take, each bound when it is put to the attempt it was minted for
//! and the run epoch it was minted under.
//!
//! The registration manager puts a config here right after the mint answered and records its
//! handle on the attempt (`RecordJitHandle`); the journal and the delivery ledger see only the
//! handle. The link's control-plane end takes it at send time, through the broker's
//! [`LaunchConfigSource`] (the launch's attempt and the epoch its frame is stamped with), into
//! the one frame that carries it to the data plane's `LaunchStore` (protocol 1.6, LNK-02; ADR
//! 0082); a resend finds nothing and carries nothing. A take for another
//! attempt or epoch is refused and discards the config, a second put for a handle held or
//! already taken is refused, and a restart empties the broker: the attempt's handle is then
//! orphaned and its registration presumed leaked (P13), never minted again (I19, I22).
//! Nothing here logs, journals or persists a config (ADR 0014). Pinned by the tests below.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard, PoisonError};

use mayfly_domain::{AttemptId, Epoch, Secret, SecretHandleId};
use mayfly_protocol::{LaunchConfigSource, SecretHandle};

/// What a held config is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitScope {
    /// The attempt whose registration minted it.
    pub attempt: AttemptId,
    /// The run epoch it was minted under.
    pub epoch: Epoch,
}

/// A put for a handle held or already taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlreadyHeld;

#[derive(Default)]
struct BrokerConfigs {
    configs: BTreeMap<SecretHandleId, (JitScope, Secret<String>)>,
    used: BTreeSet<SecretHandleId>,
}

/// The JIT configs held, by handle.
#[derive(Default)]
pub struct JitBroker {
    held: Mutex<BrokerConfigs>,
}

impl std::fmt::Debug for JitBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JitBroker")
            .field("held", &self.len())
            .finish_non_exhaustive()
    }
}

impl JitBroker {
    /// An empty broker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, BrokerConfigs> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Holds `config` behind `handle` for `scope`.
    ///
    /// # Errors
    /// [`AlreadyHeld`] when the handle is held or was taken before; nothing changes.
    pub fn put(
        &self,
        handle: SecretHandleId,
        scope: JitScope,
        config: Secret<String>,
    ) -> Result<(), AlreadyHeld> {
        let mut held = self.lock();
        if !held.used.insert(handle) {
            return Err(AlreadyHeld);
        }
        held.configs.insert(handle, (scope, config));
        Ok(())
    }

    /// Takes the config behind `handle` once, for `scope` only: a take for another attempt or
    /// epoch discards it and answers `None`, as does a second take.
    pub fn take(&self, handle: SecretHandleId, scope: JitScope) -> Option<Secret<String>> {
        let (held_for, config) = self.lock().configs.remove(&handle)?;
        (held_for == scope).then_some(config)
    }

    /// Whether a config is held behind `handle`.
    #[must_use]
    pub fn holds(&self, handle: SecretHandleId) -> bool {
        self.lock().configs.contains_key(&handle)
    }

    /// Drops the config behind `handle`, if held: its launch will never come.
    pub fn discard(&self, handle: SecretHandleId) {
        self.lock().configs.remove(&handle);
    }

    /// Forgets `handle` altogether, the config and that it was ever used: for a registration
    /// that settled and never mints again (I19), so the broker's memory stays bounded by the
    /// registrations still open.
    pub fn forget(&self, handle: SecretHandleId) {
        let mut held = self.lock();
        held.configs.remove(&handle);
        held.used.remove(&handle);
    }

    /// How many handles the broker remembers, held or used.
    #[must_use]
    pub fn remembered(&self) -> usize {
        self.lock().used.len()
    }

    /// How many configs are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().configs.len()
    }

    /// Whether no config is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The link's control-plane end takes a launch's config here (LNK-02): once, for the launch's
/// attempt and the run epoch its frame is stamped with ([`JitBroker::take`]). Pinned by
/// `tests::should_hand_the_config_to_the_link_once_when_its_launch_is_sent_in_its_epoch`.
impl LaunchConfigSource for JitBroker {
    fn take(
        &self,
        handle: SecretHandle,
        attempt: AttemptId,
        epoch: Epoch,
    ) -> Option<Secret<String>> {
        Self::take(self, handle.handle_id, JitScope { attempt, epoch })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(attempt: u128, epoch: u64) -> JitScope {
        JitScope {
            attempt: AttemptId::from_u128(attempt),
            epoch: Epoch::new(epoch),
        }
    }

    fn config() -> Secret<String> {
        Secret::new(["broker", "test", "config"].join("-"))
    }

    #[test]
    fn should_hand_a_config_out_once_when_it_is_taken_for_its_scope() {
        // given
        let broker = JitBroker::new();
        let handle = SecretHandleId::from_u128(1);
        broker
            .put(handle, scope(1, 1), config())
            .unwrap_or_else(|_| panic!("the first put"));

        // when
        let first = broker.take(handle, scope(1, 1));
        let second = broker.take(handle, scope(1, 1));

        // then
        assert_eq!(
            first.map(|c| c.expose().clone()),
            Some(config().expose().clone())
        );
        assert!(second.is_none());
        assert!(broker.is_empty());
    }

    #[test]
    fn should_discard_the_config_when_it_is_taken_for_another_scope() {
        // given
        let broker = JitBroker::new();
        let handle = SecretHandleId::from_u128(2);
        broker
            .put(handle, scope(1, 1), config())
            .unwrap_or_else(|_| panic!("the first put"));

        // when
        let other_attempt = broker.take(handle, scope(2, 1));
        let afterwards = broker.take(handle, scope(1, 1));

        // then
        assert!(other_attempt.is_none());
        assert!(afterwards.is_none());
        assert!(!broker.holds(handle));
    }

    #[test]
    fn should_refuse_a_second_put_when_the_handle_was_held_or_taken() {
        // given
        let broker = JitBroker::new();
        let handle = SecretHandleId::from_u128(3);
        broker
            .put(handle, scope(1, 1), config())
            .unwrap_or_else(|_| panic!("the first put"));

        // when
        let while_held = broker.put(handle, scope(1, 1), config());
        let _ = broker.take(handle, scope(1, 1));
        let after_take = broker.put(handle, scope(1, 1), config());

        // then
        assert_eq!(while_held, Err(AlreadyHeld));
        assert_eq!(after_take, Err(AlreadyHeld));
        assert!(broker.is_empty());
    }

    #[test]
    fn should_remember_nothing_of_a_handle_when_it_is_forgotten() {
        // given
        let broker = JitBroker::new();
        let handle = SecretHandleId::from_u128(5);
        broker
            .put(handle, scope(1, 1), config())
            .unwrap_or_else(|_| panic!("the first put"));
        let _ = broker.take(handle, scope(1, 1));

        // when
        broker.forget(handle);

        // then
        assert_eq!(broker.remembered(), 0);
        assert!(broker.is_empty());
    }

    #[test]
    fn should_not_show_the_config_when_the_broker_is_debug_printed() {
        // given
        let broker = JitBroker::new();
        broker
            .put(SecretHandleId::from_u128(4), scope(1, 1), config())
            .unwrap_or_else(|_| panic!("the first put"));

        // when
        let shown = format!("{broker:?}");

        // then
        assert!(!shown.contains(config().expose().as_str()), "{shown}");
        assert!(shown.contains("held: 1"), "{shown}");
    }

    #[test]
    fn should_hand_the_config_to_the_link_once_when_its_launch_is_sent_in_its_epoch() {
        // given
        let broker = JitBroker::new();
        let link: &dyn LaunchConfigSource = &broker;
        let handle = SecretHandle {
            handle_id: SecretHandleId::from_u128(6),
        };
        let stale = SecretHandle {
            handle_id: SecretHandleId::from_u128(7),
        };
        for held in [handle, stale] {
            broker
                .put(held.handle_id, scope(1, 2), config())
                .unwrap_or_else(|_| panic!("the first put"));
        }

        // when
        let sent = link.take(handle, AttemptId::from_u128(1), Epoch::new(2));
        let resent = link.take(handle, AttemptId::from_u128(1), Epoch::new(2));
        let next_session = link.take(stale, AttemptId::from_u128(1), Epoch::new(3));

        // then
        assert_eq!(
            sent.map(|c| c.expose().clone()),
            Some(config().expose().clone())
        );
        assert!(resent.is_none());
        assert!(next_session.is_none());
        assert!(
            broker.is_empty(),
            "a take for another epoch discards the config"
        );
    }
}
