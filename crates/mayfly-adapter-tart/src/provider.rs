//! `TartProvider`: the `VmProvider` port over the `tart` CLI and per-VM shims (plan §8.1, §8.2).
//!
//! Every call is bounded by its own deadlines (`config::Deadlines`), so every future completes:
//! a hung `tart` child is killed and reaped and the call answers `Ambiguous(Deadline)`, and the
//! executor re-inspects. How each effect reads its outcome:
//!
//! - **inventory**: `tart list` then one `tart get` per local VM, which describes its guest,
//!   memory, disk and MAC (ADR 0052, ADR 0026); any failed, unparseable or late call makes the
//!   whole listing `Incomplete`, never a shorter list (I25).
//! - **clone_vm**: only from a service-owned base (ADR 0039, ADR 0087), under the registry's
//!   admission lock: the registry ([`crate::bases`]) names it `Ready`, and right before the clone
//!   it is revalidated ([`crate::proof::ready_for_clone`]: stopped, the same file identity and
//!   final fingerprint, the same MAC); a base that fails is quarantined, any other local `mfb-*`
//!   VM is refused `Definite`. The clone's name is recorded under its base as an unresolved
//!   intent before the clone (a reference that blocks eviction), never `--overwrite`; then
//!   `tart set --random-mac` and the MAC read back, which must differ from the base's: that MAC
//!   is the instance id (ADR 0026), recorded against the intent. A clone whose new MAC cannot be
//!   confirmed answers `Ambiguous` and its intent stays: the VM may exist without a proven
//!   identity.
//! - **start**: refused when Tart lists the VM running; then `tart set --memory` and the shim
//!   (ADR 0037). It returns once the shim is settled: `Ok` when the shim recorded this run and
//!   Tart lists the VM running while the shim holds its lock; `Definite` (or `HypervisorLimit`)
//!   when the run exited non-zero or the shim refused before spawning it; `Ambiguous` when the
//!   run ended cleanly before it was seen running or the shim died. At the deadline it issues
//!   `tart stop`, waits up to the settle bound for the run's end, and answers
//!   `Ambiguous(Deadline)`: never a signal to `tart run` or the shim.
//! - **stop** / **delete**: the MAC is read back right before the effect and compared with the
//!   proof's instance id; a mismatch is refused. `stop` is `tart stop` only; `delete` of a VM
//!   Tart does not list is done (ensure absent, ADR 0053), and removes the shim directory once
//!   its shim is gone.
//!
//! Pinned by the `VmProvider` contract (`tests/tart/contract.rs`, `mayfly-testkit-data`'s
//! `provider_contract`), `tests/tart/shim.rs` and `tests/tart/calls.rs`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use mayfly_data::{
    Ambiguity, BaseName, Inventory, InventoryGap, OwnershipProof, RunOptions, Shim, VmError,
    VmObservation, VmProvider,
};
use mayfly_domain::{Millis, OwnedVmName, ProviderInstanceId};
use tokio::time::Instant;

use crate::bases::BaseRecords;
use crate::config::TartConfig;
use crate::parse::{self, VmInfo};
use crate::process::launch::Shims;
use crate::process::vm_shim::{ShimArgs, ShimTarget};
use crate::proof::{self, NotCloneable};
use crate::shim::{self, ExitRecord, RUN_STDERR, RunRecord, ShimReading};
use crate::tart::{CallError, TartCli};

/// Why the Tart version probe failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeError {
    /// `tart --version` failed or did not parse.
    Failed,
    /// Tart answered a version this adapter was not pinned for: admission stays closed (R12).
    Unsupported(String),
}

/// The Tart `VmProvider`.
#[derive(Debug)]
pub struct TartProvider {
    tart: TartCli,
    shims: Shims,
    runs: AtomicU64,
    bases: Arc<BaseRecords>,
}

const MIB: u64 = 1024 * 1024;

/// A read before the effect failed: whatever it was, nothing was done yet.
pub(crate) const fn before_effect(_: &CallError) -> VmError {
    VmError::Definite
}

/// What a failed effect means.
pub(crate) fn effect_failed(err: &CallError) -> VmError {
    match err {
        CallError::Refused { .. } | CallError::SpawnFailed => VmError::Definite,
        CallError::TimedOut => VmError::Ambiguous(Ambiguity::Deadline),
        CallError::Lost | CallError::Unparseable => VmError::Ambiguous(Ambiguity::ProcessLost),
    }
}

/// What a failure after the effect means: it happened, but its outcome is not confirmed.
pub(crate) fn after_effect(err: &CallError) -> VmError {
    match err {
        CallError::TimedOut => VmError::Ambiguous(Ambiguity::Deadline),
        _ => VmError::Ambiguous(Ambiguity::ProcessLost),
    }
}

fn gap(err: &CallError) -> InventoryGap {
    match err {
        CallError::TimedOut => InventoryGap::Deadline,
        CallError::Unparseable => InventoryGap::Unparseable,
        CallError::Refused { .. } | CallError::Lost | CallError::SpawnFailed => {
            InventoryGap::ListFailed
        }
    }
}

fn proven(info: &VmInfo, proof: &OwnershipProof) -> bool {
    info.instance_text() == proof.instance_id.as_str()
}

/// A proof whose name and generation disagree names no VM this data plane made: refused before
/// anything is read or done. The control plane builds both from one record, so this never
/// fires there (pinned by `tests::should_refuse_a_proof_when_its_generation_differs_from_the_name`).
fn consistent(proof: &OwnershipProof) -> Result<(), VmError> {
    if proof.generation == proof.vm_name.generation() && proof.slot == proof.vm_name.slot() {
        Ok(())
    } else {
        Err(VmError::Definite)
    }
}

/// Tells the registry that a `start` has returned, however it returns.
struct Returned<'a> {
    shims: &'a Shims,
    run_id: &'a str,
}

impl Drop for Returned<'_> {
    fn drop(&mut self) {
        self.shims.returned(self.run_id);
    }
}

/// How a settled start ended, read from its shim.
enum Settled {
    Running,
    Ended(VmError),
}

impl TartProvider {
    /// A provider over `config` that clones only from the bases `bases` records (ADR 0039,
    /// ADR 0087); the image store that materialises them shares the same record.
    #[must_use]
    pub fn new(config: TartConfig, bases: Arc<BaseRecords>) -> Self {
        Self {
            tart: TartCli::new(config),
            shims: Shims::default(),
            runs: AtomicU64::new(0),
            bases,
        }
    }

    fn config(&self) -> &TartConfig {
        self.tart.config()
    }

    /// Probes Tart's version against the pinned set (plan §8.2).
    ///
    /// # Errors
    /// [`ProbeError`].
    pub async fn probe(&self) -> Result<String, ProbeError> {
        let version = self.tart.version().await.map_err(|_| ProbeError::Failed)?;
        if parse::SUPPORTED_VERSIONS.contains(&version.as_str()) {
            Ok(version)
        } else {
            Err(ProbeError::Unsupported(version))
        }
    }

    /// What `vm`'s shim directory says now: the re-adoption input of ADR 0037.
    #[must_use]
    pub fn shim(&self, vm: &str) -> ShimReading {
        shim::read(&shim::dir(&self.config().shims, vm))
    }

    /// Every shim directory and what it says, by VM name.
    #[must_use]
    pub fn shims(&self) -> Vec<(String, ShimReading)> {
        let Ok(entries) = std::fs::read_dir(&self.config().shims) else {
            return Vec::new();
        };
        let mut out: Vec<(String, ShimReading)> = entries
            .filter_map(Result::ok)
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .map(|vm| {
                let reading = self.shim(&vm);
                (vm, reading)
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Removes the shim directory of a VM that is gone, unless a shim still holds it, so
    /// [`TartProvider::shims`] stops reporting it.
    pub(crate) fn forget_shim(&self, vm: &str) {
        if self.shim(vm).shim != Shim::Alive {
            let _removed = std::fs::remove_dir_all(shim::dir(&self.config().shims, vm));
        }
    }

    /// How many shims this provider started and still holds a process entry for: one per VM
    /// whose shim lives, none once each has exited and been reaped.
    #[must_use]
    pub fn shim_children(&self) -> usize {
        self.shims.reap();
        self.shims.len()
    }

    fn next_run_id(&self) -> String {
        let n = self.runs.fetch_add(1, Ordering::Relaxed);
        format!("{}-{}-{n}", std::process::id(), shim::now_ms())
    }

    async fn describe(&self, name: String) -> Result<VmObservation, InventoryGap> {
        match self.tart.get(&name).await {
            Ok(Some(info)) => Ok(VmObservation {
                instance_id: ProviderInstanceId::new(&info.instance_text()).ok(),
                running: info.running,
                guest: info.guest(),
                memory_bytes: info.memory_bytes(),
                disk_bytes: info.disk_bytes(),
                name,
            }),
            // Listed a moment ago and gone now: the listing is not a snapshot any more.
            Ok(None) => Err(InventoryGap::ListFailed),
            Err(err) => Err(gap(&err)),
        }
    }

    /// Reads how this start's run settled, if it has.
    fn settled(&self, vm: &str, run_id: &str, running: bool) -> Option<Settled> {
        let dir = shim::dir(&self.config().shims, vm);
        let reading = shim::read(&dir);
        if let Some(exit) = reading.exit.as_ref().filter(|e| e.run_id == run_id) {
            return Some(Settled::Ended(classify_exit(exit, &dir)));
        }
        let recorded = reading
            .run
            .as_ref()
            .is_some_and(|r: &RunRecord| r.run_id == run_id);
        if recorded && running && reading.shim == Shim::Alive {
            return Some(Settled::Running);
        }
        self.shims.exited(run_id)?;
        // The shim exited: it writes `exit.json` before it exits, so read it once more.
        let reading = shim::read(&dir);
        if let Some(exit) = reading.exit.as_ref().filter(|e| e.run_id == run_id) {
            return Some(Settled::Ended(classify_exit(exit, &dir)));
        }
        Some(Settled::Ended(if recorded {
            VmError::Ambiguous(Ambiguity::ProcessLost)
        } else {
            // It refused before spawning `tart run` (another shim holds the lock, I/O).
            VmError::Definite
        }))
    }

    async fn running(&self, vm: &str) -> bool {
        matches!(self.tart.get(vm).await, Ok(Some(info)) if info.running)
    }

    async fn settle(&self, vm: &str, run_id: &str) -> Result<(), VmError> {
        let poll = self.config().poll;
        let due = Instant::now() + self.config().deadlines.start;
        loop {
            let running = self.running(vm).await;
            match self.settled(vm, run_id, running) {
                Some(Settled::Running) => return Ok(()),
                Some(Settled::Ended(err)) => return Err(err),
                None => {}
            }
            if Instant::now() >= due {
                break;
            }
            tokio::time::sleep(poll).await;
        }
        // Past the deadline: only `tart stop` may end the run (ADR 0037). Whatever it answers,
        // the run's end is waited for up to the settle bound, and the start is ambiguous.
        let _stopped = self.tart.stop(vm, Duration::ZERO).await;
        let due = Instant::now() + self.config().deadlines.settle;
        while Instant::now() < due {
            let reading = self.shim(vm);
            let ended = reading.exit.as_ref().is_some_and(|e| e.run_id == run_id)
                || self.shims.exited(run_id).is_some();
            if ended {
                break;
            }
            tokio::time::sleep(poll).await;
        }
        Err(VmError::Ambiguous(Ambiguity::Deadline))
    }
}

impl TartProvider {
    /// Runs `target` under a new shim and returns its run id once the shim is settled, as
    /// `start` does after its checks (ADR 0037); the image store boots a base this way.
    pub(crate) async fn boot(&self, target: ShimTarget) -> Result<String, VmError> {
        let vm = target.to_string();
        let run_id = self.next_run_id();
        let args = ShimArgs {
            tart: self.config().tart.clone(),
            vm: target,
            dir: shim::dir(&self.config().shims, &vm),
            run_id: run_id.clone(),
            log_limit: self.config().run_log_limit,
        };
        self.shims
            .spawn(&self.config().shim, &args, self.tart.environment())
            .map_err(|_| VmError::Definite)?;
        let _returned = Returned {
            shims: &self.shims,
            run_id: &run_id,
        };
        Box::pin(self.settle(&vm, &run_id)).await?;
        Ok(run_id.clone())
    }

    /// Stops `vm`, booted by [`TartProvider::boot`] as `run_id`, with `tart stop` only (ADR
    /// 0037), waits up to the settle bound for its shim to record the run's end, and confirms
    /// that Tart lists it stopped.
    pub(crate) async fn halt(
        &self,
        vm: &str,
        run_id: &str,
        grace: Duration,
    ) -> Result<(), VmError> {
        if let Err(err) = self.tart.stop(vm, grace).await
            && !matches!(err, CallError::Refused { .. })
        {
            return Err(effect_failed(&err));
        }
        let poll = self.config().poll;
        let due = Instant::now() + self.config().deadlines.settle;
        loop {
            let ended = self
                .shim(vm)
                .exit
                .as_ref()
                .is_some_and(|e| e.run_id == run_id)
                || self.shims.exited(run_id).is_some();
            if ended {
                break;
            }
            if Instant::now() >= due {
                return Err(VmError::Ambiguous(Ambiguity::Deadline));
            }
            tokio::time::sleep(poll).await;
        }
        match self.tart.get(vm).await {
            Ok(Some(info)) if !info.running => Ok(()),
            Ok(_) => Err(VmError::Ambiguous(Ambiguity::ProcessLost)),
            Err(err) => Err(after_effect(&err)),
        }
    }
}

/// What a run that ended before it was seen settled means.
fn classify_exit(exit: &ExitRecord, dir: &std::path::Path) -> VmError {
    match (exit.error.as_ref(), exit.code, exit.signal) {
        // `tart run` never ran.
        (Some(error), _, _) if error.starts_with(shim::SPAWN_FAILED) => VmError::Definite,
        // The shim lost its child: how the run ended is not known.
        (Some(_), _, _) => VmError::Ambiguous(Ambiguity::ProcessLost),
        (None, Some(0), _) | (None, None, _) => VmError::Ambiguous(Ambiguity::ProcessLost),
        (None, Some(_), _) => {
            let stderr = std::fs::read(dir.join(RUN_STDERR)).unwrap_or_default();
            if parse::is_hypervisor_limit(&String::from_utf8_lossy(&stderr)) {
                VmError::HypervisorLimit
            } else {
                VmError::Definite
            }
        }
    }
}

impl VmProvider for TartProvider {
    async fn inventory(&self) -> Inventory {
        self.shims.reap();
        let entries = match self.tart.list().await {
            Ok(entries) => entries,
            Err(err) => return Inventory::Incomplete { reason: gap(&err) },
        };
        let mut vms = Vec::with_capacity(entries.len());
        for entry in entries {
            match self.describe(entry.name).await {
                Ok(vm) => vms.push(vm),
                Err(reason) => return Inventory::Incomplete { reason },
            }
        }
        Inventory::Complete { vms }
    }

    async fn clone_vm(
        &self,
        base: &BaseName,
        name: &OwnedVmName,
    ) -> Result<ProviderInstanceId, VmError> {
        let name = name.to_string();
        let _held = self.bases.hold(base.as_str()).await;
        let _admitted = self.bases.admit().await;
        let entry = self
            .bases
            .read()
            .map_err(|_| VmError::Definite)?
            .bases
            .remove(base.as_str())
            .ok_or(VmError::Definite)?;
        let base_info = match self.tart.get(base.as_str()).await {
            Ok(Some(info)) => info,
            Ok(None) => return Err(VmError::Definite),
            Err(err) => return Err(before_effect(&err)),
        };
        let home = &self.config().tart_home;
        match proof::ready_for_clone(&entry, &base_info, home, base.as_str()) {
            Ok(()) => {}
            Err(NotCloneable::NotReady) => return Err(VmError::Definite),
            Err(NotCloneable::Changed(reason)) => {
                proof::quarantine(&self.bases, base.as_str(), reason);
                return Err(VmError::Definite);
            }
        }
        self.bases
            .update(|record| {
                if let Some(entry) = record.bases.get_mut(base.as_str()) {
                    entry.clones.insert(name.clone(), None);
                }
            })
            .map_err(|_| VmError::Definite)?;
        self.tart
            .clone(base.as_str(), &name)
            .await
            .map_err(|err| effect_failed(&err))?;
        self.tart
            .set_random_mac(&name)
            .await
            .map_err(|err| after_effect(&err))?;
        let info = match self.tart.get(&name).await {
            Ok(Some(info)) => info,
            Ok(None) => return Err(VmError::Ambiguous(Ambiguity::ProcessLost)),
            Err(err) => return Err(after_effect(&err)),
        };
        if info.instance_text() == base_info.instance_text() {
            return Err(VmError::Ambiguous(Ambiguity::ProcessLost));
        }
        let id = ProviderInstanceId::new(&info.instance_text())
            .map_err(|_| VmError::Ambiguous(Ambiguity::ProcessLost))?;
        // Unrecorded, the intent stays and keeps blocking eviction: the safe side.
        let _resolved = self.bases.update(|record| {
            if let Some(entry) = record.bases.get_mut(base.as_str()) {
                entry
                    .clones
                    .insert(name.clone(), Some(id.as_str().to_owned()));
            }
        });
        Ok(id)
    }

    async fn start(&self, name: &OwnedVmName, options: &RunOptions) -> Result<(), VmError> {
        let vm = name.to_string();
        self.shims.reap();
        match self.tart.get(&vm).await {
            Ok(Some(info)) if !info.running => {}
            Ok(_) => return Err(VmError::Definite),
            Err(err) => return Err(before_effect(&err)),
        }
        let mib = options.resources.memory_bytes.div_ceil(MIB);
        self.tart
            .set_memory(&vm, mib)
            .await
            .map_err(|err| before_effect(&err))?;
        self.boot(ShimTarget::Vm(name.clone())).await.map(drop)
    }

    async fn stop(&self, proof: &OwnershipProof, grace: Millis) -> Result<(), VmError> {
        consistent(proof)?;
        let vm = proof.vm_name.to_string();
        match self.tart.get(&vm).await {
            Ok(Some(info)) if proven(&info, proof) => {}
            Ok(_) => return Err(VmError::Definite),
            Err(err) => return Err(before_effect(&err)),
        }
        let grace = Duration::from_millis(grace.get());
        if let Err(err) = self.tart.stop(&vm, grace).await {
            if !matches!(err, CallError::Refused { .. }) {
                return Err(effect_failed(&err));
            }
            // Tart refused, but the VM may have stopped since it was re-observed ("is not
            // running"): a proven, stopped VM is what the stop asked for (ADR 0053).
            return match self.tart.get(&vm).await {
                Ok(Some(info)) if proven(&info, proof) && !info.running => Ok(()),
                Ok(_) => Err(VmError::Definite),
                Err(err) => Err(after_effect(&err)),
            };
        }
        match self.tart.get(&vm).await {
            Ok(Some(info)) if !info.running => Ok(()),
            Ok(_) => Err(VmError::Ambiguous(Ambiguity::ProcessLost)),
            Err(err) => Err(after_effect(&err)),
        }
    }

    async fn delete(&self, proof: &OwnershipProof) -> Result<(), VmError> {
        consistent(proof)?;
        let vm = proof.vm_name.to_string();
        match self.tart.get(&vm).await {
            Ok(Some(info)) if proven(&info, proof) => {}
            Ok(Some(_)) => return Err(VmError::Definite),
            Ok(None) => {
                self.forget_shim(&vm);
                return Ok(());
            }
            Err(err) => return Err(before_effect(&err)),
        }
        if let Err(err) = self.tart.delete(&vm).await {
            if !matches!(err, CallError::Refused { .. }) {
                return Err(effect_failed(&err));
            }
            // Tart refused, but it may have removed the VM before it failed: absence is what the
            // delete asked for (ADR 0053); a VM still there was not deleted.
            return match self.tart.get(&vm).await {
                Ok(None) => {
                    self.forget_shim(&vm);
                    Ok(())
                }
                Ok(Some(_)) => Err(VmError::Definite),
                Err(err) => Err(after_effect(&err)),
            };
        }
        self.forget_shim(&vm);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use mayfly_domain::{AttemptId, Generation, HostId, PoolName, Sample, Slot};

    use super::*;

    fn proof(slot: u32, generation: u32) -> OwnershipProof {
        let name = OwnedVmName::new(
            PoolName::sample(),
            HostId::sample(),
            Slot::new(1),
            Generation::FIRST,
            AttemptId::from_u128(1),
        );
        OwnershipProof {
            attempt_id: AttemptId::from_u128(1),
            host_id: HostId::sample(),
            slot: Slot::new(slot),
            generation: Generation::new(generation).expect("a generation"),
            vm_name: name,
            instance_id: ProviderInstanceId::new("0a:00:00:00:00:01").expect("an instance id"),
        }
    }

    #[test]
    fn should_refuse_a_proof_when_its_generation_differs_from_the_name() {
        // given
        let matching = proof(1, 1);
        let other_generation = proof(1, 2);
        let other_slot = proof(2, 1);

        // when
        let checked = [
            consistent(&matching),
            consistent(&other_generation),
            consistent(&other_slot),
        ];

        // then
        assert_eq!(
            checked,
            [Ok(()), Err(VmError::Definite), Err(VmError::Definite)]
        );
    }
}
