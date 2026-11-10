//! The resource ledger (plan §9, ADR 0024): what this host has, what our VMs, images, bases and
//! in-flight directives and foreign VMs use, and whether a directive fits (I33).
//!
//! In memory only, rebuilt from observation on start (plan §3.1): the host probe gives the
//! totals, the operator reserve, foreign consumption and the pressure flags; a complete
//! inventory gives our VMs; the image store's listing gives images and bases. Whatever the
//! ledger cannot know it assumes the larger way (fail closed): a VM or base of unknown guest is
//! counted as macOS, a running VM of unknown memory as [`Sizing::vm_memory_bytes`], an
//! outcome that is `Unknown` keeps its reservation until a complete listing shows what exists,
//! and every sum saturates, so an overflow fits nothing.

use std::collections::{BTreeMap, BTreeSet};

use mayfly_domain::{DirectiveId, Epoch, ImageDigest, OwnedVmName, Resources, VmNameClass};
use mayfly_protocol::{
    BaseName, Capacity, CapacityConstraint, CapacityReport, DirectiveKind, GuestOs,
    ResourceDimension,
};

use crate::inventory::Inventory;

/// The footprints the data plane assumes for what a directive creates, from its own
/// configuration, never from the control plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sizing {
    /// The disk ceiling of one pulled image.
    pub image_disk_bytes: u64,
    /// The disk ceiling of one materialised base.
    pub base_disk_bytes: u64,
    /// The disk ceiling of one clone.
    pub clone_disk_bytes: u64,
    /// The memory a running VM of unknown memory is counted with (one adopted after a restart).
    pub vm_memory_bytes: u64,
}

/// What the host probe reports (DP-01's `HostProbe`, fed by inventory and probes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostProbe {
    /// What the host has for VMs: slots, the hypervisor's macOS-guest slots, memory and disk.
    pub totals: Capacity,
    /// What the operator keeps back from VMs.
    pub reserve: Capacity,
    /// What VMs that are not ours use: their slots, running macOS guests, memory and disk.
    pub foreign: Capacity,
    /// Free disk is under the low-water mark: nothing that needs disk starts (F17).
    pub disk_low_water: bool,
    /// The host reports memory pressure: nothing that needs memory starts.
    pub memory_pressure: bool,
    /// The hypervisor refused another macOS guest: no macOS guest starts.
    pub hypervisor_limit: bool,
}

/// One of our VMs, as counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmUse {
    /// Its guest; macOS when unknown.
    pub guest: GuestOs,
    /// Whether it runs (it then uses memory, and a macOS guest slot if macOS).
    pub running: bool,
    /// Memory while running.
    pub memory_bytes: u64,
    /// Its disk ceiling.
    pub disk_bytes: u64,
}

/// One materialised base, as counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BaseUse {
    /// The guest its clones run; macOS when unknown.
    pub guest: GuestOs,
    /// Its disk ceiling.
    pub disk_bytes: u64,
}

/// Which listing shows what a held reservation's directive did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settles {
    /// A complete VM inventory.
    Inventory,
    /// The image store's listing.
    Images,
}

/// Capacity set aside for one directive of one epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reservation {
    /// What it sets aside.
    pub demand: Capacity,
    /// Which listing settles it once held.
    pub settles: Settles,
    /// Its outcome is not known (or its session ended): kept until that listing arrives.
    pub held: bool,
}

/// The ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ledger {
    /// Assumed footprints.
    pub sizing: Sizing,
    /// The last host probe.
    pub probe: HostProbe,
    /// Our VMs: every listed VM whose name has our shape (plan §8.4: shape, not ownership; for
    /// accounting a VM is counted either way).
    pub vms: BTreeMap<OwnedVmName, VmUse>,
    /// Pulled images and their disk.
    pub images: BTreeMap<ImageDigest, u64>,
    /// Materialised bases: also the set a destructive directive may never name (ADR 0026).
    pub bases: BTreeMap<BaseName, BaseUse>,
    /// Capacity set aside per `(epoch, directive)` from admission until its outcome is applied.
    pub reservations: BTreeMap<(Epoch, DirectiveId), Reservation>,
    /// The id of the next capacity report.
    pub next_report_id: u64,
}

fn plus(a: Capacity, b: Capacity) -> Capacity {
    Capacity {
        vm_slots: a.vm_slots.saturating_add(b.vm_slots),
        macos_guests: a.macos_guests.saturating_add(b.macos_guests),
        resources: Resources::new(
            a.resources
                .memory_bytes
                .saturating_add(b.resources.memory_bytes),
            a.resources
                .disk_bytes
                .saturating_add(b.resources.disk_bytes),
        ),
    }
}

fn minus(a: Capacity, b: Capacity) -> Capacity {
    Capacity {
        vm_slots: a.vm_slots.saturating_sub(b.vm_slots),
        macos_guests: a.macos_guests.saturating_sub(b.macos_guests),
        resources: Resources::new(
            a.resources
                .memory_bytes
                .saturating_sub(b.resources.memory_bytes),
            a.resources
                .disk_bytes
                .saturating_sub(b.resources.disk_bytes),
        ),
    }
}

const fn guest_slots(guest: GuestOs) -> u32 {
    match guest {
        GuestOs::Macos => 1,
        GuestOs::Linux => 0,
    }
}

const fn disk(bytes: u64) -> Capacity {
    Capacity {
        vm_slots: 0,
        macos_guests: 0,
        resources: Resources::new(0, bytes),
    }
}

const ZERO: Capacity = disk(0);

/// What a VM directive's target takes once started.
fn running_use(vm: &VmUse) -> Capacity {
    Capacity {
        vm_slots: 1,
        macos_guests: if vm.running { guest_slots(vm.guest) } else { 0 },
        resources: Resources::new(if vm.running { vm.memory_bytes } else { 0 }, vm.disk_bytes),
    }
}

impl Ledger {
    /// An empty ledger over `probe`.
    #[must_use]
    pub const fn new(sizing: Sizing, probe: HostProbe) -> Self {
        Self {
            sizing,
            probe,
            vms: BTreeMap::new(),
            images: BTreeMap::new(),
            bases: BTreeMap::new(),
            reservations: BTreeMap::new(),
            next_report_id: 1,
        }
    }

    /// What the host has for VMs after the operator reserve.
    #[must_use]
    pub fn available(&self) -> Capacity {
        minus(self.probe.totals, self.probe.reserve)
    }

    /// What is used: our VMs, images, bases, every reservation and foreign VMs.
    #[must_use]
    pub fn used(&self) -> Capacity {
        let vms = self
            .vms
            .values()
            .fold(ZERO, |sum, vm| plus(sum, running_use(vm)));
        let images = self
            .images
            .values()
            .fold(ZERO, |sum, bytes| plus(sum, disk(*bytes)));
        let bases = self
            .bases
            .values()
            .fold(ZERO, |sum, base| plus(sum, disk(base.disk_bytes)));
        let reserved = self
            .reservations
            .values()
            .fold(ZERO, |sum, r| plus(sum, r.demand));
        plus(
            plus(plus(vms, images), plus(bases, reserved)),
            self.probe.foreign,
        )
    }

    /// What is free: available minus used, never below zero.
    #[must_use]
    pub fn free(&self) -> Capacity {
        minus(self.available(), self.used())
    }

    /// What `kind` would take, by the data plane's own sizing and records. A target that is
    /// already there takes nothing more (a re-run is a re-observation, ADR 0042); a kind that
    /// creates or starts nothing takes nothing. Pinned by
    /// `tests/data_plane/matrix.rs::should_fit_or_name_the_short_dimension_when_each_creating_kind_meets_each_host`.
    #[must_use]
    pub fn demand(&self, kind: &DirectiveKind) -> Capacity {
        match kind {
            DirectiveKind::PullImage { digest } if self.images.contains_key(digest) => ZERO,
            DirectiveKind::PullImage { .. } => disk(self.sizing.image_disk_bytes),
            DirectiveKind::MaterialiseImage { base_name, .. }
                if self.bases.contains_key(base_name) =>
            {
                ZERO
            }
            DirectiveKind::MaterialiseImage { .. } => disk(self.sizing.base_disk_bytes),
            DirectiveKind::CloneVm { vm_name, .. } if self.vms.contains_key(vm_name) => ZERO,
            DirectiveKind::CloneVm { .. } => Capacity {
                vm_slots: 1,
                macos_guests: 0,
                resources: Resources::new(0, self.sizing.clone_disk_bytes),
            },
            DirectiveKind::StartVm {
                vm_name,
                run_options,
            } => {
                let wanted = run_options.resources;
                match self.vms.get(vm_name) {
                    Some(vm) if vm.running => ZERO,
                    Some(vm) => Capacity {
                        vm_slots: 0,
                        macos_guests: guest_slots(vm.guest),
                        resources: Resources::new(
                            wanted.memory_bytes,
                            wanted.disk_bytes.saturating_sub(vm.disk_bytes),
                        ),
                    },
                    None => Capacity {
                        vm_slots: 0,
                        macos_guests: guest_slots(GuestOs::Macos),
                        resources: wanted,
                    },
                }
            }
            DirectiveKind::EvictImage { .. }
            | DirectiveKind::VerifyGuest { .. }
            | DirectiveKind::LaunchRunner { .. }
            | DirectiveKind::ExportDiagnostics { .. }
            | DirectiveKind::StopVm { .. }
            | DirectiveKind::DeleteVm { .. }
            | DirectiveKind::ReportInventory
            | DirectiveKind::ReportCapacity
            | DirectiveKind::ReportHostFacts => ZERO,
        }
    }

    /// Whether `demand` fits what is free, checked per dimension in [`ResourceDimension`]
    /// order. A dimension the demand does not use always fits; one it uses does not while the
    /// probe flags its pressure (low disk, memory pressure, the hypervisor's limit).
    ///
    /// # Errors
    /// The first [`ResourceDimension`] that does not fit.
    pub fn fits(&self, demand: Capacity) -> Result<(), ResourceDimension> {
        let free = self.free();
        let probe = &self.probe;
        if demand.vm_slots > free.vm_slots {
            return Err(ResourceDimension::VmSlots);
        }
        if demand.macos_guests > 0
            && (probe.hypervisor_limit || demand.macos_guests > free.macos_guests)
        {
            return Err(ResourceDimension::MacosGuests);
        }
        let (memory, free_memory) = (demand.resources.memory_bytes, free.resources.memory_bytes);
        if memory > 0 && (probe.memory_pressure || memory > free_memory) {
            return Err(ResourceDimension::Memory);
        }
        let (bytes, free_disk) = (demand.resources.disk_bytes, free.resources.disk_bytes);
        if bytes > 0 && (probe.disk_low_water || bytes > free_disk) {
            return Err(ResourceDimension::Disk);
        }
        Ok(())
    }

    /// Admits `kind` for `key` if it fits, and sets its demand aside so that a concurrent
    /// directive cannot take the same capacity (I33). Pinned by
    /// `tests/data_plane/executor.rs::should_refuse_the_second_clone_when_the_first_reserved_the_last_slot`.
    ///
    /// # Errors
    /// The first [`ResourceDimension`] that does not fit; nothing is set aside.
    pub fn reserve(
        &mut self,
        key: (Epoch, DirectiveId),
        kind: &DirectiveKind,
    ) -> Result<(), ResourceDimension> {
        let demand = self.demand(kind);
        self.fits(demand)?;
        let settles = settles(kind);
        self.reservations.insert(
            key,
            Reservation {
                demand,
                settles,
                held: false,
            },
        );
        Ok(())
    }

    /// The directive of `key` was applied: its reservation becomes what it created or frees
    /// what it removed.
    pub fn commit(&mut self, key: (Epoch, DirectiveId), kind: &DirectiveKind) {
        self.reservations.remove(&key);
        let sizing = self.sizing;
        match kind {
            DirectiveKind::PullImage { digest } => {
                self.images
                    .entry(digest.clone())
                    .or_insert(sizing.image_disk_bytes);
            }
            DirectiveKind::MaterialiseImage { base_name, .. } => {
                self.bases.entry(base_name.clone()).or_insert(BaseUse {
                    guest: GuestOs::Macos,
                    disk_bytes: sizing.base_disk_bytes,
                });
            }
            DirectiveKind::EvictImage { base_name } => {
                self.bases.remove(base_name);
            }
            DirectiveKind::CloneVm { base_name, vm_name } => {
                let guest = self
                    .bases
                    .get(base_name)
                    .map_or(GuestOs::Macos, |base| base.guest);
                self.vms.entry(vm_name.clone()).or_insert(VmUse {
                    guest,
                    running: false,
                    memory_bytes: 0,
                    disk_bytes: sizing.clone_disk_bytes,
                });
            }
            DirectiveKind::StartVm {
                vm_name,
                run_options,
            } => {
                let vm = self.vms.entry(vm_name.clone()).or_insert(VmUse {
                    guest: GuestOs::Macos,
                    running: false,
                    memory_bytes: 0,
                    disk_bytes: sizing.clone_disk_bytes,
                });
                vm.running = true;
                vm.memory_bytes = run_options.resources.memory_bytes;
                vm.disk_bytes = vm.disk_bytes.max(run_options.resources.disk_bytes);
            }
            DirectiveKind::StopVm { proof, .. } => {
                if let Some(vm) = self.vms.get_mut(&proof.vm_name) {
                    vm.running = false;
                    vm.memory_bytes = 0;
                }
            }
            DirectiveKind::DeleteVm { proof } => {
                self.vms.remove(&proof.vm_name);
            }
            DirectiveKind::VerifyGuest { .. }
            | DirectiveKind::LaunchRunner { .. }
            | DirectiveKind::ExportDiagnostics { .. }
            | DirectiveKind::ReportInventory
            | DirectiveKind::ReportCapacity
            | DirectiveKind::ReportHostFacts => {}
        }
    }

    /// The directive of `key` was refused: nothing happened, its reservation goes.
    pub fn release(&mut self, key: (Epoch, DirectiveId)) {
        self.reservations.remove(&key);
    }

    /// The outcome of `key` is not known: its reservation stays until the listing that shows
    /// what exists (fail closed).
    pub fn hold(&mut self, key: (Epoch, DirectiveId)) {
        if let Some(r) = self.reservations.get_mut(&key) {
            r.held = true;
        }
    }

    /// The session ended: no outcome of it will be recorded, so every reservation is held.
    pub fn hold_all(&mut self) {
        for r in self.reservations.values_mut() {
            r.held = true;
        }
    }

    /// Applies an inventory. A complete one is the truth about our VMs: a listed VM of our
    /// shape is counted (unknown ones the larger way), an unlisted one is gone, and held VM
    /// reservations are settled. An incomplete one changes nothing (I25): no VM is released on
    /// it. Pinned by
    /// `tests/data_plane/ledger.rs::should_release_nothing_when_an_inventory_is_incomplete`.
    pub fn apply_inventory(&mut self, inventory: &Inventory) {
        let listed = match inventory {
            Inventory::Complete { vms } => vms,
            Inventory::Incomplete { .. } => return,
        };
        let mut ours: BTreeMap<OwnedVmName, bool> = BTreeMap::new();
        for vm in listed {
            match mayfly_domain::classify_vm_name(&vm.name) {
                VmNameClass::WellFormed(name) => {
                    ours.insert(name, vm.running);
                }
                VmNameClass::MalformedManaged | VmNameClass::Foreign => {}
            }
        }
        self.vms.retain(|name, _| ours.contains_key(name));
        let sizing = self.sizing;
        for (name, running) in ours {
            let vm = self.vms.entry(name).or_insert(VmUse {
                guest: GuestOs::Macos,
                running: false,
                memory_bytes: 0,
                disk_bytes: sizing.clone_disk_bytes,
            });
            if running && !vm.running {
                vm.memory_bytes = vm.memory_bytes.max(sizing.vm_memory_bytes);
            }
            if !running {
                vm.memory_bytes = 0;
            }
            vm.running = running;
        }
        self.reservations
            .retain(|_, r| !(r.held && r.settles == Settles::Inventory));
    }

    /// Applies the image store's listing: the pulled images and the bases with their guests.
    /// Held image reservations are settled.
    pub fn apply_images(
        &mut self,
        images: &BTreeSet<ImageDigest>,
        bases: &BTreeMap<BaseName, GuestOs>,
    ) {
        let sizing = self.sizing;
        self.images = images
            .iter()
            .map(|digest| (digest.clone(), sizing.image_disk_bytes))
            .collect();
        self.bases = bases
            .iter()
            .map(|(name, guest)| {
                (
                    name.clone(),
                    BaseUse {
                        guest: *guest,
                        disk_bytes: sizing.base_disk_bytes,
                    },
                )
            })
            .collect();
        self.reservations
            .retain(|_, r| !(r.held && r.settles == Settles::Images));
    }

    /// Whether `name` is one of the image bases (ADR 0026: never the target of a destructive
    /// directive).
    #[must_use]
    pub fn is_base(&self, name: &str) -> bool {
        self.bases.keys().any(|base| base.as_str() == name)
    }

    /// The `CapacityReported` content now, under the next report id (ADR 0024). Pinned by
    /// `tests/data_plane/ledger.rs::should_report_totals_used_free_and_foreign_when_capacity_is_reported`.
    pub fn report(&mut self) -> CapacityReport {
        let report_id = self.next_report_id;
        self.next_report_id = self.next_report_id.saturating_add(1);
        let totals = self.available();
        let used = self.used();
        let probe = &self.probe;
        let flags = [
            (probe.disk_low_water, CapacityConstraint::DiskLowWater),
            (probe.memory_pressure, CapacityConstraint::MemoryPressure),
            (probe.foreign != ZERO, CapacityConstraint::ForeignGuests),
            (probe.reserve != ZERO, CapacityConstraint::OperatorReserve),
            (
                probe.hypervisor_limit || used.macos_guests >= totals.macos_guests,
                CapacityConstraint::HypervisorLimit,
            ),
        ];
        CapacityReport {
            report_id,
            totals,
            used,
            free: minus(totals, used),
            foreign: probe.foreign,
            reasons: flags
                .into_iter()
                .filter(|(on, _)| *on)
                .map(|(_, reason)| reason)
                .collect(),
        }
    }
}

const fn settles(kind: &DirectiveKind) -> Settles {
    match kind {
        DirectiveKind::PullImage { .. }
        | DirectiveKind::MaterialiseImage { .. }
        | DirectiveKind::EvictImage { .. } => Settles::Images,
        DirectiveKind::CloneVm { .. }
        | DirectiveKind::StartVm { .. }
        | DirectiveKind::VerifyGuest { .. }
        | DirectiveKind::LaunchRunner { .. }
        | DirectiveKind::ExportDiagnostics { .. }
        | DirectiveKind::StopVm { .. }
        | DirectiveKind::DeleteVm { .. }
        | DirectiveKind::ReportInventory
        | DirectiveKind::ReportCapacity
        | DirectiveKind::ReportHostFacts => Settles::Inventory,
    }
}
