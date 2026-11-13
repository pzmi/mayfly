//! [`LaunchStore`]: the data plane's JIT configs held for delivery (ADR 0014, ADR 0082), the
//! [`LaunchSecrets`] the guest transport takes them from.
//!
//! Each config is bound when it is put: to the exact VM, the instance id `VmCloned` recorded
//! and a deadline. The link's data-plane end puts the config a launch's frame carries through
//! the store's [`LaunchConfigSink`] (protocol 1.6, LNK-02), bound to that launch, before the
//! executor sees the directive. A take for another VM or instance id is refused and discards the config; a
//! second put for a handle held or already taken is refused (a replayed frame cannot replace a
//! config) until [`USED_GRACE`] after its deadline, when the handle is forgotten so the store's
//! memory stays bounded; [`LaunchStore::expire`] drops what passed its deadline, and the transport discards
//! a handle whatever its launch's outcome, so nothing lingers once the directive has ended. In
//! memory only: a restart empties it. Pinned by the tests below.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use mayfly_domain::{Millis, OwnedVmName, ProviderInstanceId, Secret, SecretHandleId};
use mayfly_protocol::{LaunchConfigSink, LaunchTarget, PutRefused, SecretHandle};

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

/// How long after its deadline a used handle is still refused a second put. Forgetting it then
/// is safe: a frame replayed later is put again, but its launch is past its deadline, which the
/// executor refuses `deadline_expired` before the guest transport is asked (DP-01), and the
/// store drops the config at the next [`LaunchStore::expire`]; a replay of an earlier session's
/// frame is never put at all (the link's data-plane end checks the epoch, LNK-02).
pub const USED_GRACE: Millis = Millis::new(10 * 60 * 1000);

#[derive(Default)]
struct HeldConfigs {
    configs: Vec<(SecretHandle, LaunchBinding, Secret<String>)>,
    /// Every handle put, with the deadline it was put for.
    used: BTreeMap<SecretHandleId, Millis>,
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
        if held.used.contains_key(&handle.handle_id) {
            return Err(AlreadyHeld);
        }
        held.used.insert(handle.handle_id, binding.deadline);
        held.configs.push((handle, binding, config));
        Ok(())
    }

    /// Drops every config whose deadline is at or before `now`, and forgets every handle whose
    /// deadline is [`USED_GRACE`] or more before `now`; returns how many configs it dropped.
    pub fn expire(&self, now: Millis) -> usize {
        let mut held = self.lock();
        let before = held.configs.len();
        held.configs
            .retain(|(_, binding, _)| binding.deadline > now);
        held.used
            .retain(|_, deadline| deadline.get().saturating_add(USED_GRACE.get()) > now.get());
        before - held.configs.len()
    }

    /// Drops every config held, keeping the handles used: a new session opened (LNK-02).
    pub fn discard_held(&self) {
        self.lock().configs.clear();
    }

    /// How many handles it remembers, held or used.
    #[must_use]
    pub fn remembered(&self) -> usize {
        self.lock().used.len()
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

/// The link's data-plane end puts a launch's config here (LNK-02), bound to the launch's VM,
/// instance id and deadline (the directive's, in the control plane's clock, which is the clock
/// the store's owner passes to [`LaunchStore::expire`]). Pinned by
/// `tests::should_hold_a_config_from_the_link_bound_to_its_launch_when_it_is_put_once`.
impl LaunchConfigSink for LaunchStore {
    fn put(
        &self,
        handle: SecretHandle,
        target: LaunchTarget,
        config: Secret<String>,
    ) -> Result<(), PutRefused> {
        let binding = LaunchBinding {
            vm_name: target.vm_name,
            instance_id: target.instance_id,
            deadline: target.deadline.instant(),
        };
        Self::put(self, handle, binding, config).map_err(|AlreadyHeld| PutRefused)
    }

    fn discard_held(&self) {
        Self::discard_held(self);
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

    #[test]
    fn should_hold_a_config_from_the_link_bound_to_its_launch_when_it_is_put_once() {
        // given
        let store = LaunchStore::default();
        let link: &dyn LaunchConfigSink = &store;
        let target = LaunchTarget {
            vm_name: vm(1),
            instance_id: id("0a:00:00:00:00:01"),
            deadline: mayfly_domain::Deadline::at(Millis::new(40)),
        };

        // when
        let first = link.put(handle(4), target.clone(), config());
        let again = link.put(handle(4), target, config());

        // then
        assert_eq!((first, again), (Ok(()), Err(PutRefused)));
        assert_eq!(store.expire(Millis::new(39)), 0);
        let taken = store.take(handle(4), &vm(1), &id("0a:00:00:00:00:01"));
        assert_eq!(taken.map(|c| c.expose().clone()), Some("cfg".to_owned()));
        store
            .put(handle(5), binding(1, 40), config())
            .expect("held");
        assert_eq!(store.expire(Millis::new(40)), 1);
    }

    #[test]
    fn should_drop_what_it_holds_and_keep_its_handles_used_when_a_session_opens() {
        // given
        let store = LaunchStore::default();
        store
            .put(handle(1), binding(1, 10), config())
            .expect("held");

        // when
        LaunchConfigSink::discard_held(&store);

        // then
        assert!(store.is_empty());
        assert_eq!(
            store.put(handle(1), binding(1, 10), config()),
            Err(AlreadyHeld)
        );
    }

    #[test]
    fn should_forget_a_used_handle_only_when_its_grace_after_the_deadline_passed() {
        // given
        let store = LaunchStore::default();
        store
            .put(handle(1), binding(1, 10), config())
            .expect("held");
        drop(store.take(handle(1), &vm(1), &id("0a:00:00:00:00:01")));

        // when
        store.expire(Millis::new(10 + USED_GRACE.get() - 1));
        let before_grace = store.remembered();
        store.expire(Millis::new(10 + USED_GRACE.get()));

        // then
        assert_eq!((before_grace, store.remembered()), (1, 0));
    }
}
