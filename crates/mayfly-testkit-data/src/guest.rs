//! The in-memory guest world: [`MemoryGuests`], a `GuestTransport` over guests held in memory
//! that keeps the transport's contract (ADR 0038, ADR 0082), [`MemorySecrets`], the data
//! plane's JIT-config store (`mayfly_data::LaunchStore`) shared by handle, and
//! [`UncheckedLockTransport`], the deliberately broken transport of ver §5 that skips the
//! lock-PID and instance-id check before delivery.
//!
//! Each VM has a shim child pid, a run-lock holder, an instance id and a guest that answers its
//! `tart exec` (another VM's guest when a test misroutes it). Each guest has a MAC it reports
//! in its claims and in the launch handshake, a launcher status, the configs its launcher
//! received on stdin and a runner log the diagnostics export reads, redacted by the same
//! function the Tart transport uses. Nothing here spawns, sleeps or reads a clock.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use mayfly_data::{
    AlreadyHeld, Ambiguity, Diagnostics, GuestAnswer, GuestTransport, LaunchAnswer, LaunchBinding,
    LaunchSecrets, LaunchStore, RunnerStatus, VmError, redact_log,
};
use mayfly_domain::{Millis, OwnedVmName, ProviderInstanceId, Secret};
use mayfly_protocol::{DiagnosticsLoss, GuestFacts, GuestOs, GuestRejection, Nonce, SecretHandle};
use sha2::{Digest, Sha256};

fn lock<T>(state: &Mutex<T>) -> MutexGuard<'_, T> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The data plane's JIT-config store, shared: clones hold the same configs.
#[derive(Debug, Clone, Default)]
pub struct MemorySecrets {
    store: Arc<LaunchStore>,
}

impl MemorySecrets {
    /// Holds `config` behind `handle` for `vm` with `instance_id`, with no deadline.
    ///
    /// # Errors
    /// [`AlreadyHeld`] when the handle is held or was taken before.
    pub fn put(
        &self,
        handle: SecretHandle,
        vm: &OwnedVmName,
        instance_id: &ProviderInstanceId,
        config: &str,
    ) -> Result<(), AlreadyHeld> {
        self.store.put(
            handle,
            LaunchBinding {
                vm_name: vm.clone(),
                instance_id: instance_id.clone(),
                deadline: Millis::new(u64::MAX),
            },
            Secret::new(config.to_owned()),
        )
    }

    /// How many configs it holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.store.len()
    }

    /// Whether it holds none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.store.is_empty()
    }
}

impl LaunchSecrets for MemorySecrets {
    fn take(
        &self,
        handle: SecretHandle,
        vm: &OwnedVmName,
        instance_id: &ProviderInstanceId,
    ) -> Option<Secret<String>> {
        self.store.take(handle, vm, instance_id)
    }

    fn discard(&self, handle: SecretHandle) {
        self.store.discard(handle);
    }
}

/// One VM as the host sees it.
#[derive(Debug, Clone)]
struct HostVm {
    instance_id: ProviderInstanceId,
    shim_child: u32,
    lock_holder: Option<u32>,
    /// The VM whose guest answers this VM's exec.
    answered_by: String,
    agent: bool,
}

/// One guest.
#[derive(Debug, Clone)]
struct Guest {
    mac: ProviderInstanceId,
    delivered: Vec<String>,
    status: RunnerStatus,
    /// The runner's exit code once launched; `None` keeps it running.
    job_exit: Option<i32>,
    /// The launcher fails after reading a config: the delivery's outcome is unknown.
    fails_after_config: bool,
    log: String,
}

#[derive(Debug, Default)]
struct World {
    vms: BTreeMap<String, HostVm>,
    guests: BTreeMap<String, Guest>,
    artefacts: BTreeMap<String, Vec<u8>>,
    next_pid: u32,
}

/// A `GuestTransport` over guests in memory; clones share them.
#[derive(Debug, Clone)]
pub struct MemoryGuests {
    world: Arc<Mutex<World>>,
    secrets: MemorySecrets,
    diagnostics_limit: usize,
    checked: bool,
}

/// Discards a launch's handle however the launch ends (ADR 0082).
struct Discard<'a> {
    secrets: &'a MemorySecrets,
    handle: SecretHandle,
}

impl Drop for Discard<'_> {
    fn drop(&mut self) {
        self.secrets.discard(self.handle);
    }
}

impl MemoryGuests {
    /// No VMs, taking configs from `secrets`, exporting at most `diagnostics_limit` bytes.
    #[must_use]
    pub fn new(secrets: MemorySecrets, diagnostics_limit: usize) -> Self {
        Self {
            world: Arc::new(Mutex::new(World {
                next_pid: 1000,
                ..World::default()
            })),
            secrets,
            diagnostics_limit,
            checked: true,
        }
    }

    /// A transport over the same guests and secrets, as a restarted daemon builds it.
    #[must_use]
    pub fn reopened(&self) -> Self {
        self.clone()
    }

    /// Boots `vm` under its shim with `instance_id`; its guest runs a launcher and reports that
    /// MAC. Its run lock is held by its shim's child.
    pub fn boot(&self, vm: &OwnedVmName, instance_id: &ProviderInstanceId) {
        let mut world = lock(&self.world);
        world.next_pid += 1;
        let child = world.next_pid;
        let name = vm.to_string();
        world.vms.insert(
            name.clone(),
            HostVm {
                instance_id: instance_id.clone(),
                shim_child: child,
                lock_holder: Some(child),
                answered_by: name.clone(),
                agent: true,
            },
        );
        world.guests.insert(
            name,
            Guest {
                mac: instance_id.clone(),
                delivered: Vec::new(),
                status: RunnerStatus::NotLaunched,
                job_exit: None,
                fails_after_config: false,
                log: String::new(),
            },
        );
    }

    fn with_vm(&self, vm: &OwnedVmName, change: impl FnOnce(&mut HostVm)) {
        if let Some(found) = lock(&self.world).vms.get_mut(&vm.to_string()) {
            change(found);
        }
    }

    fn with_guest(&self, vm: &OwnedVmName, change: impl FnOnce(&mut Guest)) {
        if let Some(found) = lock(&self.world).guests.get_mut(&vm.to_string()) {
            change(found);
        }
    }

    /// The run lock of `vm` is held by a process that is not its shim's child.
    pub fn foreign_lock(&self, vm: &OwnedVmName) {
        self.with_vm(vm, |v| v.lock_holder = Some(v.shim_child + 7_000));
    }

    /// `vm` gets a new instance id (its MAC changes) without a new name.
    pub fn change_mac(&self, vm: &OwnedVmName, to: &ProviderInstanceId) {
        self.with_vm(vm, |v| v.instance_id = to.clone());
        self.with_guest(vm, |g| g.mac = to.clone());
    }

    /// An exec addressed to `vm` reaches the guest of `to`.
    pub fn misroute(&self, vm: &OwnedVmName, to: &OwnedVmName) {
        self.with_vm(vm, |v| v.answered_by = to.to_string());
    }

    /// The guest of `vm` has no guest agent: no exec reaches it.
    pub fn without_agent(&self, vm: &OwnedVmName) {
        self.with_vm(vm, |v| v.agent = false);
    }

    /// The runner of `vm` exits with `code` once launched.
    pub fn runner_exits(&self, vm: &OwnedVmName, code: i32) {
        self.with_guest(vm, |g| g.job_exit = Some(code));
    }

    /// The launcher of `vm` fails after it has read a config.
    pub fn fails_after_config(&self, vm: &OwnedVmName) {
        self.with_guest(vm, |g| g.fails_after_config = true);
    }

    /// The runner log of `vm`'s guest.
    pub fn write_log(&self, vm: &OwnedVmName, text: &str) {
        self.with_guest(vm, |g| g.log = text.to_owned());
    }

    /// The configs the launcher of `vm` received on its stdin.
    #[must_use]
    pub fn delivered(&self, vm: &OwnedVmName) -> Vec<String> {
        lock(&self.world)
            .guests
            .get(&vm.to_string())
            .map(|g| g.delivered.clone())
            .unwrap_or_default()
    }

    /// The last diagnostics artefact exported for `vm`.
    #[must_use]
    pub fn artefact(&self, vm: &OwnedVmName) -> Option<Vec<u8>> {
        lock(&self.world).artefacts.get(&vm.to_string()).cloned()
    }

    /// The guest an exec to `vm` reaches, when it reaches one.
    fn answering(world: &World, vm: &OwnedVmName) -> Result<Option<String>, VmError> {
        let host = world.vms.get(&vm.to_string()).ok_or(VmError::Definite)?;
        Ok(host.agent.then(|| host.answered_by.clone()))
    }
}

impl GuestTransport for MemoryGuests {
    async fn verify(&self, vm: &OwnedVmName, _nonce: Nonce) -> Result<GuestAnswer, VmError> {
        let world = lock(&self.world);
        let Some(answering) = Self::answering(&world, vm)? else {
            return Ok(GuestAnswer::Rejected(GuestRejection::AgentMissing));
        };
        let host = &world.vms[&vm.to_string()];
        let claimed = world.guests.get(&answering).map(|g| &g.mac);
        if claimed != Some(&host.instance_id) {
            return Ok(GuestAnswer::Rejected(GuestRejection::ChallengeFailed));
        }
        Ok(GuestAnswer::Verified(GuestFacts {
            os: GuestOs::Linux,
            uptime: Millis::new(0),
        }))
    }

    async fn launch(
        &self,
        vm: &OwnedVmName,
        instance_id: &ProviderInstanceId,
        launch: SecretHandle,
    ) -> Result<LaunchAnswer, VmError> {
        let _discard = Discard {
            secrets: &self.secrets,
            handle: launch,
        };
        let mut world = lock(&self.world);
        let host = world
            .vms
            .get(&vm.to_string())
            .cloned()
            .ok_or(VmError::Definite)?;
        if self.checked
            && (host.lock_holder != Some(host.shim_child) || host.instance_id != *instance_id)
        {
            return Ok(LaunchAnswer::IdentityRefused);
        }
        if !host.agent {
            return Err(VmError::Definite);
        }
        let guest = world
            .guests
            .get_mut(&host.answered_by)
            .ok_or(VmError::Definite)?;
        // the handshake on the launch itself: the answering guest names its MAC
        if self.checked && guest.mac != *instance_id {
            return Ok(LaunchAnswer::IdentityRefused);
        }
        if guest.status != RunnerStatus::NotLaunched {
            return Ok(LaunchAnswer::Delivered);
        }
        let config = self
            .secrets
            .take(launch, vm, instance_id)
            .ok_or(VmError::Definite)?;
        guest.delivered.push(config.expose().clone());
        if guest.fails_after_config {
            return Err(VmError::Ambiguous(Ambiguity::ProcessLost));
        }
        guest.status = guest
            .job_exit
            .map_or(RunnerStatus::Running, |code| RunnerStatus::Exited { code });
        Ok(LaunchAnswer::Delivered)
    }

    async fn runner_status(&self, vm: &OwnedVmName) -> Result<RunnerStatus, VmError> {
        let world = lock(&self.world);
        let answering = Self::answering(&world, vm)?.ok_or(VmError::Definite)?;
        world
            .guests
            .get(&answering)
            .map(|g| g.status)
            .ok_or(VmError::Definite)
    }

    async fn export_diagnostics(&self, vm: &OwnedVmName) -> Result<Diagnostics, VmError> {
        let mut world = lock(&self.world);
        let Some(answering) = Self::answering(&world, vm)? else {
            return Ok(Diagnostics::Lost(DiagnosticsLoss::GuestUnreachable));
        };
        let guest = world.guests.get(&answering).ok_or(VmError::Definite)?;
        if guest.log.len() > self.diagnostics_limit {
            return Ok(Diagnostics::Lost(DiagnosticsLoss::OverLimit));
        }
        let artefact = redact_log(guest.log.as_bytes(), &[]).into_bytes();
        let sha256: [u8; 32] = Sha256::digest(&artefact).into();
        let bytes = u64::try_from(artefact.len()).unwrap_or(u64::MAX);
        world.artefacts.insert(vm.to_string(), artefact);
        Ok(Diagnostics::Exported { bytes, sha256 })
    }
}

/// The broken transport of ver §5: [`MemoryGuests`] that delivers without checking the VM's
/// run lock, its instance id or the answering guest's MAC. The `GuestTransport` contract fails
/// it (GT-05, GT-06, GT-12).
#[derive(Debug, Clone)]
pub struct UncheckedLockTransport(pub MemoryGuests);

impl UncheckedLockTransport {
    /// The unchecked twin of `guests`, over the same world.
    #[must_use]
    pub fn new(guests: &MemoryGuests) -> Self {
        Self(MemoryGuests {
            checked: false,
            ..guests.clone()
        })
    }
}

impl GuestTransport for UncheckedLockTransport {
    async fn verify(&self, vm: &OwnedVmName, nonce: Nonce) -> Result<GuestAnswer, VmError> {
        self.0.verify(vm, nonce).await
    }

    async fn launch(
        &self,
        vm: &OwnedVmName,
        instance_id: &ProviderInstanceId,
        launch: SecretHandle,
    ) -> Result<LaunchAnswer, VmError> {
        self.0.launch(vm, instance_id, launch).await
    }

    async fn runner_status(&self, vm: &OwnedVmName) -> Result<RunnerStatus, VmError> {
        self.0.runner_status(vm).await
    }

    async fn export_diagnostics(&self, vm: &OwnedVmName) -> Result<Diagnostics, VmError> {
        self.0.export_diagnostics(vm).await
    }
}
