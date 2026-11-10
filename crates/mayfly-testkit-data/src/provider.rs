//! `FakeVmProvider`: an in-memory provider with scripted faults (ver §7.1).
//!
//! VMs live in a map by listed name, foreign names included. Like Tart, it refuses (`Definite`)
//! a clone onto a name that exists and a start of a VM that runs; once a base is registered
//! ([`FakeVmProvider::allow_base`]) it also refuses a clone from any base not registered, as Tart
//! refuses a clone from a base that is not local (ADR 0039). Each call is logged by [`Op`]; the
//! next call of an op can carry a [`Fault`] (failed before acting, or ambiguous with or without
//! its effect applied, or an incomplete listing) and can be held until the test releases it
//! ([`FakeVmProvider::hold_next`]); a held call that is never released stands for a hung
//! provider, which only the caller's deadline ends. A listing is read when the call starts, so
//! one held while an effect runs returns what existed before that effect, as a real listing
//! taken before a clone would.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use mayfly_data::{Ambiguity, VmError, VmProvider};
use mayfly_data_domain::Inventory;
use mayfly_domain::{Millis, OwnedVmName, ProviderInstanceId};
use mayfly_protocol::{BaseName, GuestOs, InventoryGap, OwnershipProof, RunOptions, VmObservation};
use tokio::sync::watch;

/// A provider call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Op {
    /// `inventory`.
    Inventory,
    /// `clone_vm`.
    Clone,
    /// `start`.
    Start,
    /// `stop`.
    Stop,
    /// `delete`.
    Delete,
}

/// How the next call of an op fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// It fails before acting (`VmError::Definite`; a listing is `ListFailed`).
    Definite,
    /// It reports an ambiguous error, having applied its effect or not.
    Ambiguous {
        /// What the error says.
        cause: Ambiguity,
        /// Whether the effect took place.
        applied: bool,
    },
    /// A listing that is incomplete for this reason (an effect fails `Definite`).
    Incomplete(InventoryGap),
}

/// One VM the fake holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeVm {
    /// Its instance id, when the provider reports one.
    pub instance_id: Option<ProviderInstanceId>,
    /// Whether it runs.
    pub running: bool,
    /// What a listing describes of it (ADR 0052): its guest.
    pub guest: Option<GuestOs>,
    /// Its memory.
    pub memory_bytes: Option<u64>,
    /// Its disk.
    pub disk_bytes: Option<u64>,
}

/// A call held until [`Held::release`]. Its stage only moves forward, so each wait completes
/// at once once the stage was reached, however often it is awaited.
#[derive(Debug, Clone)]
pub struct Held(Arc<watch::Sender<Stage>>);

/// Where a held call is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Stage {
    Waiting,
    Started,
    Released,
    Finished,
}

impl Held {
    fn new() -> Self {
        Self(Arc::new(watch::Sender::new(Stage::Waiting)))
    }

    fn reach(&self, stage: Stage) {
        self.0.send_modify(|now| *now = (*now).max(stage));
    }

    async fn until(&self, stage: Stage) {
        let mut reading = self.0.subscribe();
        loop {
            // Read in its own statement: the guard must not live across the await.
            let now = *reading.borrow_and_update();
            if now >= stage || reading.changed().await.is_err() {
                return;
            }
        }
    }

    /// Completes once the held call has started.
    pub async fn started(&self) {
        self.until(Stage::Started).await;
    }

    /// Completes once the held call has done what it does and returned.
    pub async fn finished(&self) {
        self.until(Stage::Finished).await;
    }

    /// Lets the held call go on.
    pub fn release(&self) {
        self.reach(Stage::Released);
    }
}

/// Marks a held call finished when the call returns or is dropped.
struct Finish(Option<Held>);

impl Drop for Finish {
    fn drop(&mut self) {
        if let Some(held) = &self.0 {
            held.reach(Stage::Finished);
        }
    }
}

#[derive(Debug, Default)]
struct State {
    vms: BTreeMap<String, FakeVm>,
    faults: BTreeMap<Op, VecDeque<Fault>>,
    holds: BTreeMap<Op, VecDeque<Held>>,
    log: Vec<Op>,
    cloned: u32,
    bases: Option<BTreeSet<String>>,
    clone_guest: Option<GuestOs>,
    clone_disk_bytes: Option<u64>,
}

/// The fake provider; clones share one state.
#[derive(Debug, Clone, Default)]
pub struct FakeVmProvider {
    state: Arc<Mutex<State>>,
}

impl FakeVmProvider {
    /// An empty provider whose clones are listed with `guest` and `disk_bytes`.
    #[must_use]
    pub fn new(guest: Option<GuestOs>, disk_bytes: Option<u64>) -> Self {
        let fake = Self::default();
        {
            let mut state = fake.lock();
            state.clone_guest = guest;
            state.clone_disk_bytes = disk_bytes;
        }
        fake
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Puts `vm` under `name` (any text: foreign and malformed names too).
    pub fn seed(&self, name: &str, vm: FakeVm) {
        self.lock().vms.insert(name.to_owned(), vm);
    }

    /// Registers `base` as a local base and lists it as a stopped VM, as Tart does; from the
    /// first registration on, a clone from any other base is refused.
    pub fn allow_base(&self, base: &BaseName, instance_id: ProviderInstanceId) {
        let mut state = self.lock();
        state
            .bases
            .get_or_insert_with(BTreeSet::new)
            .insert(base.as_str().to_owned());
        let vm = FakeVm {
            instance_id: Some(instance_id),
            running: false,
            guest: state.clone_guest,
            memory_bytes: None,
            disk_bytes: state.clone_disk_bytes,
        };
        state.vms.insert(base.as_str().to_owned(), vm);
    }

    /// Removes `name` with no check: what a broken provider's own deletes do in the contract's
    /// tests.
    pub fn remove(&self, name: &str) {
        self.lock().vms.remove(name);
    }

    /// Marks `name` running or not with no check: what a broken provider's own stops do in the
    /// contract's tests.
    pub fn set_running(&self, name: &str, running: bool) {
        if let Some(vm) = self.lock().vms.get_mut(name) {
            vm.running = running;
        }
    }

    /// Changes the instance id listed for `name`: another VM now holds the name.
    pub fn set_instance(&self, name: &str, instance_id: Option<ProviderInstanceId>) {
        if let Some(vm) = self.lock().vms.get_mut(name) {
            vm.instance_id = instance_id;
        }
    }

    /// What the provider holds now.
    #[must_use]
    pub fn vms(&self) -> BTreeMap<String, FakeVm> {
        self.lock().vms.clone()
    }

    /// The next call of `op` fails with `fault`.
    pub fn fail_next(&self, op: Op, fault: Fault) {
        self.lock().faults.entry(op).or_default().push_back(fault);
    }

    /// Holds the next call of `op` not yet held until the returned handle releases it; holds
    /// of one op apply to its calls in order.
    #[must_use]
    pub fn hold_next(&self, op: Op) -> Held {
        let held = Held::new();
        self.lock()
            .holds
            .entry(op)
            .or_default()
            .push_back(held.clone());
        held
    }

    /// Every call so far, in order.
    #[must_use]
    pub fn log(&self) -> Vec<Op> {
        self.lock().log.clone()
    }

    /// How many calls of `op` were made.
    #[must_use]
    pub fn calls(&self, op: Op) -> usize {
        self.lock().log.iter().filter(|o| **o == op).count()
    }

    /// Logs a call of `op` and takes its hold and fault.
    fn begin(&self, op: Op) -> (Option<Held>, Option<Fault>) {
        let mut state = self.lock();
        state.log.push(op);
        let held = state.holds.get_mut(&op).and_then(VecDeque::pop_front);
        let fault = state.faults.get_mut(&op).and_then(VecDeque::pop_front);
        (held, fault)
    }

    async fn wait(held: Option<&Held>) {
        if let Some(held) = held {
            held.reach(Stage::Started);
            held.until(Stage::Released).await;
        }
    }

    /// Runs an effect under `op`'s hold and fault.
    async fn effect<T>(
        &self,
        op: Op,
        apply: impl FnOnce(&mut State) -> Result<T, VmError> + Send,
    ) -> Result<T, VmError> {
        let (held, fault) = self.begin(op);
        let finish = Finish(held);
        Self::wait(finish.0.as_ref()).await;
        match fault {
            None => apply(&mut self.lock()),
            Some(Fault::Ambiguous { cause, applied }) => {
                if applied {
                    let _effect = apply(&mut self.lock());
                }
                Err(VmError::Ambiguous(cause))
            }
            Some(Fault::Definite | Fault::Incomplete(_)) => Err(VmError::Definite),
        }
    }
}

fn proven(vm: &FakeVm, proof: &OwnershipProof) -> bool {
    vm.instance_id.as_ref() == Some(&proof.instance_id)
}

impl VmProvider for FakeVmProvider {
    async fn inventory(&self) -> Inventory {
        let (held, fault) = self.begin(Op::Inventory);
        let vms: Vec<VmObservation> = self
            .lock()
            .vms
            .iter()
            .map(|(name, vm)| VmObservation {
                name: name.clone(),
                instance_id: vm.instance_id.clone(),
                running: vm.running,
                guest: vm.guest,
                memory_bytes: vm.memory_bytes,
                disk_bytes: vm.disk_bytes,
            })
            .collect();
        let finish = Finish(held);
        Self::wait(finish.0.as_ref()).await;
        match fault {
            None => Inventory::Complete { vms },
            Some(Fault::Incomplete(reason)) => Inventory::Incomplete { reason },
            Some(Fault::Definite | Fault::Ambiguous { .. }) => Inventory::Incomplete {
                reason: InventoryGap::ListFailed,
            },
        }
    }

    async fn clone_vm(
        &self,
        base: &BaseName,
        name: &OwnedVmName,
    ) -> Result<ProviderInstanceId, VmError> {
        let name = name.to_string();
        let base = base.as_str().to_owned();
        self.effect(Op::Clone, move |state| {
            let local = state
                .bases
                .as_ref()
                .is_none_or(|bases| bases.contains(&base));
            if state.vms.contains_key(&name) || !local {
                return Err(VmError::Definite);
            }
            state.cloned += 1;
            let mac = format!(
                "0a:00:00:00:{:02x}:{:02x}",
                state.cloned >> 8,
                state.cloned & 0xff
            );
            let instance_id = ProviderInstanceId::new(&mac).map_err(|_| VmError::Definite)?;
            let vm = FakeVm {
                instance_id: Some(instance_id.clone()),
                running: false,
                guest: state.clone_guest,
                memory_bytes: None,
                disk_bytes: state.clone_disk_bytes,
            };
            state.vms.insert(name, vm);
            Ok(instance_id)
        })
        .await
    }

    async fn start(&self, name: &OwnedVmName, options: &RunOptions) -> Result<(), VmError> {
        let name = name.to_string();
        let memory = options.resources.memory_bytes;
        self.effect(Op::Start, move |state| {
            let vm = state.vms.get_mut(&name).ok_or(VmError::Definite)?;
            if vm.running {
                return Err(VmError::Definite);
            }
            vm.running = true;
            vm.memory_bytes = Some(memory);
            Ok(())
        })
        .await
    }

    async fn stop(&self, proof: &OwnershipProof, _grace: Millis) -> Result<(), VmError> {
        let proof = proof.clone();
        self.effect(Op::Stop, move |state| {
            match state.vms.get_mut(&proof.vm_name.to_string()) {
                Some(vm) if proven(vm, &proof) => {
                    vm.running = false;
                    Ok(())
                }
                Some(_) | None => Err(VmError::Definite),
            }
        })
        .await
    }

    async fn delete(&self, proof: &OwnershipProof) -> Result<(), VmError> {
        let proof = proof.clone();
        self.effect(Op::Delete, move |state| {
            let name = proof.vm_name.to_string();
            match state.vms.get(&name) {
                Some(vm) if !proven(vm, &proof) => Err(VmError::Definite),
                Some(_) | None => {
                    state.vms.remove(&name);
                    Ok(())
                }
            }
        })
        .await
    }
}
