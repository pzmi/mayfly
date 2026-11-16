//! The runner watch (DP-09): which of our VMs' runners the data plane reads the launcher's
//! status of, and which exits it reported (plan §3.2, §6, §8.3; ADR 0082).
//!
//! The guest's launcher keeps the runner's status in a file that outlives the daemon, so a
//! runner's exit is read from it, never from `tart run` or a process the data plane holds. The
//! watch decides what to read and what an answer means; the executor reads it on its own
//! cadence and sends what [`RunnerWatch::answered`] returns.
//!
//! - **What is read.** Each VM a launch reached in this process (applied or of unknown outcome:
//!   a runner may have started either way), and, after a restart, each of our VMs the first
//!   complete inventory of the process lists running (adopted: whether a launch reached it is
//!   not known, so it is read once and dropped when the guest answers that no launch did). That
//!   inventory may come after this process started VMs of its own (the start's listing was
//!   incomplete): those are adopted too, read once and dropped the same way. A VM started after
//!   it is read only once a launch reached it: its launch goes through this process. Only a VM
//!   the ledger counts as running is read (a stopped guest cannot answer), and a VM a complete
//!   inventory no longer lists is forgotten.
//! - **What an answer means.** `Exited{code}` is `RunnerExited{vm_name, code}`, once per
//!   session; `Running` keeps the VM read; an answer that cannot be read records nothing and the
//!   VM is read again next time (plan §6: never an exit, I10).
//! - **Once per launch, per session.** An unsolicited observation is accepted by the link, not
//!   delivered (`ObservationSink::send`), so an exit reported in one session is sent again, from
//!   memory and without reading the guest, when a new session opens ([`RunnerWatch::resend`]);
//!   the attempt records a repeat as nothing (its `decide` answers a known exit with no event).
//!
//! Plain data with its rules as methods (ADR 0022). Pure: the session's epoch and the ledger's
//! VMs arrive as values.

use std::collections::BTreeMap;

use mayfly_domain::{Epoch, OwnedVmName};
use mayfly_protocol::ObservationKind;

use crate::inventory::Inventory;
use crate::ledger::VmUse;

/// What the launcher's status file in the guest says about the runner (plan §8.3: runner exit
/// after a restart is read from it, never from `tart run`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerStatus {
    /// No launch reached this guest's launcher.
    NotLaunched,
    /// The runner was started and has not exited.
    Running,
    /// The runner exited with `code`.
    Exited {
        /// Its exit code.
        code: i32,
    },
}

/// Why a VM's runner is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Watched {
    /// Listed running by the process's first complete inventory: a runner may have been
    /// launched in it before the restart.
    Adopted,
    /// A launch reached it in this process, applied or of unknown outcome.
    Launched,
    /// Its runner's exit was reported in the session of `epoch`; it is not read again.
    Reported {
        /// The exit code reported.
        code: i32,
        /// The session that reported it.
        epoch: Epoch,
    },
}

/// The VMs whose runners are read, and the exits reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerWatch {
    /// No complete inventory has been applied in this process yet: the first one adopts the
    /// running VMs it lists.
    pub adopting: bool,
    /// The VMs read or reported.
    pub watched: BTreeMap<OwnedVmName, Watched>,
}

impl Default for RunnerWatch {
    fn default() -> Self {
        Self::new()
    }
}

impl RunnerWatch {
    /// The watch of a process that just started: nothing read yet, adopting.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            adopting: true,
            watched: BTreeMap::new(),
        }
    }

    /// A launch reached `vm`, applied or of unknown outcome: its runner is read from now on.
    /// A VM whose exit was reported stays reported. Pinned by
    /// `tests/data_plane/runners.rs::should_read_a_vm_when_a_launch_reached_it`.
    pub fn launched(&mut self, vm: &OwnedVmName) {
        let entry = self.watched.entry(vm.clone()).or_insert(Watched::Launched);
        if *entry == Watched::Adopted {
            *entry = Watched::Launched;
        }
    }

    /// A complete inventory was applied to the ledger, whose VMs are now `vms`: the first one of
    /// the process adopts every VM it lists running, and every one forgets the VMs it no longer
    /// lists. An incomplete inventory changes nothing (I25). Pinned by
    /// `tests/data_plane/runners.rs::should_adopt_only_the_running_vms_when_the_first_complete_inventory_arrives`
    /// and `should_forget_a_vm_when_a_complete_inventory_no_longer_lists_it`.
    pub fn observed(&mut self, inventory: &Inventory, vms: &BTreeMap<OwnedVmName, VmUse>) {
        match inventory {
            Inventory::Complete { .. } => {}
            Inventory::Incomplete { .. } => return,
        }
        if self.adopting {
            self.adopting = false;
            for (name, vm) in vms {
                if vm.running {
                    self.watched.entry(name.clone()).or_insert(Watched::Adopted);
                }
            }
        }
        self.watched.retain(|name, _| vms.contains_key(name));
    }

    /// The VMs to read now: those adopted or launched that the ledger counts as running.
    #[must_use]
    pub fn due(&self, vms: &BTreeMap<OwnedVmName, VmUse>) -> Vec<OwnedVmName> {
        self.watched
            .iter()
            .filter(|(name, watched)| due_now(name, **watched, vms))
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Whether any VM is due a read ([`RunnerWatch::due`] is not empty), without listing them.
    #[must_use]
    pub fn any_due(&self, vms: &BTreeMap<OwnedVmName, VmUse>) -> bool {
        self.watched
            .iter()
            .any(|(name, watched)| due_now(name, *watched, vms))
    }

    /// What the status read of `vm` answered, `None` when it could not be read, in the session
    /// of `epoch`: the observation to send, if any. Pinned by
    /// `tests/data_plane/runners.rs::should_report_an_exit_once_when_the_guest_says_exited`,
    /// `should_record_nothing_when_the_status_cannot_be_read` and
    /// `should_drop_an_adopted_vm_when_no_launch_reached_it`.
    pub fn answered(
        &mut self,
        vm: &OwnedVmName,
        status: Option<RunnerStatus>,
        epoch: Epoch,
    ) -> Option<ObservationKind> {
        let watched = *self.watched.get(vm)?;
        match watched {
            Watched::Adopted | Watched::Launched => {}
            Watched::Reported { .. } => return None,
        }
        match status? {
            RunnerStatus::Exited { code } => {
                self.watched
                    .insert(vm.clone(), Watched::Reported { code, epoch });
                Some(ObservationKind::RunnerExited {
                    vm_name: vm.clone(),
                    code,
                })
            }
            RunnerStatus::Running => {
                self.watched.insert(vm.clone(), Watched::Launched);
                None
            }
            RunnerStatus::NotLaunched => {
                if watched == Watched::Adopted {
                    self.watched.remove(vm);
                }
                None
            }
        }
    }

    /// The session of `epoch` opened: each exit reported in an earlier session, of a VM the
    /// ledger still counts (`vms`), is sent again, since the link may have lost it. Pinned by
    /// `tests/data_plane/runners.rs::should_send_a_reported_exit_again_once_when_a_new_session_opens`.
    pub fn resend(
        &mut self,
        epoch: Epoch,
        vms: &BTreeMap<OwnedVmName, VmUse>,
    ) -> Vec<ObservationKind> {
        let mut out = Vec::new();
        for (name, watched) in &mut self.watched {
            let code = match *watched {
                Watched::Reported {
                    code,
                    epoch: reported,
                } if reported != epoch && vms.contains_key(name) => code,
                Watched::Adopted | Watched::Launched | Watched::Reported { .. } => continue,
            };
            *watched = Watched::Reported { code, epoch };
            out.push(ObservationKind::RunnerExited {
                vm_name: name.clone(),
                code,
            });
        }
        out
    }
}

/// Whether `name`, watched as `watched`, is read now: adopted or launched, and counted running.
fn due_now(name: &OwnedVmName, watched: Watched, vms: &BTreeMap<OwnedVmName, VmUse>) -> bool {
    let readable = match watched {
        Watched::Adopted | Watched::Launched => true,
        Watched::Reported { .. } => false,
    };
    readable && vms.get(name).is_some_and(|vm| vm.running)
}
