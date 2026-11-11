//! [`LaunchStore`]: the data plane's JIT configs held for delivery (ADR 0014, ADR 0082), the
//! [`LaunchSecrets`] the guest transport takes them from.
//!
//! Each config is bound when it is put: to the exact VM, the instance id `VmCloned` recorded
//! and a deadline. A take for another VM or instance id is refused and discards the config; a
//! second put for a handle held or already taken is refused (a replayed frame cannot replace a
//! config); [`LaunchStore::expire`] drops what passed its deadline, and the transport discards
//! a handle whatever its launch's outcome, so nothing lingers once the directive has ended. In
//! memory only: a restart empties it. Pinned by the tests below.

use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard, PoisonError};

use mayfly_domain::{Millis, OwnedVmName, ProviderInstanceId, Secret, SecretHandleId};
use mayfly_protocol::SecretHandle;

use crate::ports::LaunchSecrets;

/// What a held config is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchBinding {
    /// The exact VM it may be delivered to.
    pub vm_name: OwnedVmName,
    /// The instance id that VM must have.
    pub instance_id: ProviderInstanceId,
    /// When it is dropped undelivered, on the clock its owner passes to
    /// [`LaunchStore::expire`].
    pub deadline: Millis,
}

/// A put for a handle held or already taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlreadyHeld;

#[derive(Default)]
struct HeldConfigs {
    configs: Vec<(SecretHandle, LaunchBinding, Secret<String>)>,
    used: BTreeSet<SecretHandleId>,
}

/// The configs held, by handle.
#[derive(Default)]
pub struct LaunchStore {
    held: Mutex<HeldConfigs>,
}

impl std::fmt::Debug for LaunchStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchStore")
            .field("held", &self.len())
            .finish_non_exhaustive()
    }
}

impl LaunchStore {
    fn lock(&self) -> MutexGuard<'_, HeldConfigs> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Holds `config` behind `handle` for `binding`.
    ///
    /// # Errors
    /// [`AlreadyHeld`] when the handle is held or was taken before; nothing changes.
    pub fn put(
        &self,
        handle: SecretHandle,
        binding: LaunchBinding,
        config: Secret<String>,
    ) -> Result<(), AlreadyHeld> {
        let mut held = self.lock();
        if !held.used.insert(handle.handle_id) {
            return Err(AlreadyHeld);
        }
        held.configs.push((handle, binding, config));
        Ok(())
    }

    /// Drops every config whose deadline is at or before `now`; returns how many.
    pub fn expire(&self, now: Millis) -> usize {
        let mut held = self.lock();
        let before = held.configs.len();
        held.configs
            .retain(|(_, binding, _)| binding.deadline > now);
        before - held.configs.len()
    }

    /// How many configs it holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().configs.len()
    }

    /// Whether it holds none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn remove(&self, handle: SecretHandle) -> Option<(LaunchBinding, Secret<String>)> {
        let mut held = self.lock();
        let at = held.configs.iter().position(|(h, _, _)| *h == handle)?;
        let (_, binding, config) = held.configs.remove(at);
        Some((binding, config))
    }
}

impl LaunchSecrets for LaunchStore {
    fn take(
        &self,
        handle: SecretHandle,
        vm: &OwnedVmName,
        instance_id: &ProviderInstanceId,
    ) -> Option<Secret<String>> {
        let (binding, config) = self.remove(handle)?;
        (binding.vm_name == *vm && binding.instance_id == *instance_id).then_some(config)
    }

    fn discard(&self, handle: SecretHandle) {
        drop(self.remove(handle));
    }
}

#[cfg(test)]
mod tests {
    use mayfly_domain::{AttemptId, Generation, HostId, PoolName, Sample, Slot};

    use super::*;

    fn vm(n: u32) -> OwnedVmName {
        OwnedVmName::new(
            PoolName::sample(),
            HostId::sample(),
            Slot::new(n),
            Generation::FIRST,
            AttemptId::sample(),
        )
    }

    fn id(text: &str) -> ProviderInstanceId {
        ProviderInstanceId::new(text).expect("an instance id")
    }

    fn handle(n: u128) -> SecretHandle {
        SecretHandle {
            handle_id: SecretHandleId::from_u128(n),
        }
    }

    fn binding(n: u32, deadline: u64) -> LaunchBinding {
        LaunchBinding {
            vm_name: vm(n),
            instance_id: id("0a:00:00:00:00:01"),
            deadline: Millis::new(deadline),
        }
    }

    fn config() -> Secret<String> {
        Secret::new("cfg".to_owned())
    }

    #[test]
    fn should_give_the_config_once_when_taken_for_its_vm_and_instance_id() {
        // given
        let store = LaunchStore::default();
        store
            .put(handle(1), binding(1, 10), config())
            .expect("held");

        // when
        let first = store.take(handle(1), &vm(1), &id("0a:00:00:00:00:01"));
        let second = store.take(handle(1), &vm(1), &id("0a:00:00:00:00:01"));

        // then
        assert_eq!(first.map(|c| c.expose().clone()), Some("cfg".to_owned()));
        assert!(second.is_none());
    }

    #[test]
    fn should_refuse_and_discard_when_taken_for_another_vm_or_instance_id() {
        // given
        let store = LaunchStore::default();
        store
            .put(handle(1), binding(1, 10), config())
            .expect("held");
        store
            .put(handle(2), binding(1, 10), config())
            .expect("held");

        // when
        let other_vm = store.take(handle(1), &vm(2), &id("0a:00:00:00:00:01"));
        let other_id = store.take(handle(2), &vm(1), &id("0a:00:00:00:00:02"));

        // then
        assert!(other_vm.is_none() && other_id.is_none());
        assert!(store.is_empty(), "a mismatched take discards the config");
    }

    #[test]
    fn should_refuse_a_second_put_when_the_handle_is_held_or_was_taken() {
        // given
        let store = LaunchStore::default();
        store
            .put(handle(1), binding(1, 10), config())
            .expect("held");

        // when
        let again = store.put(handle(1), binding(2, 10), config());
        drop(store.take(handle(1), &vm(1), &id("0a:00:00:00:00:01")));
        let after_take = store.put(handle(1), binding(1, 10), config());

        // then
        assert_eq!((again, after_take), (Err(AlreadyHeld), Err(AlreadyHeld)));
        assert!(store.is_empty());
    }

    #[test]
    fn should_drop_a_config_when_its_deadline_passes_or_it_is_discarded() {
        // given
        let store = LaunchStore::default();
        store
            .put(handle(1), binding(1, 10), config())
            .expect("held");
        store
            .put(handle(2), binding(1, 20), config())
            .expect("held");
        store
            .put(handle(3), binding(1, 30), config())
            .expect("held");

        // when
        let expired = store.expire(Millis::new(10));
        store.discard(handle(3));

        // then
        assert_eq!(expired, 1);
        assert_eq!(store.len(), 1);
        assert!(format!("{store:?}").contains("held: 1"));
    }
}
