//! The resource ledger (plan §9, ADR 0024): what this host has, what our VMs, images, bases and
//! in-flight directives and foreign VMs use, and whether a directive fits (I33).
//!
//! In memory only, rebuilt from observation on start (plan §3.1): the host probe gives the
//! totals, the operator reserve, foreign consumption and the pressure flags; a complete
//! inventory gives our VMs; the image store's listing gives images and bases. An outcome that
//! is `Unknown` keeps its reservation until a complete listing shows what exists, and every sum
//! saturates, so an overflow fits nothing.
//!
//! **Nothing unknown is assumed (ADR 0052).** A fact the ledger does not have (a listed VM's
//! guest, its memory while it runs, its disk; our VMs before the first complete inventory;
//! images and bases before the first image listing; a foreign VM the probe could not size) is
//! never replaced by a default: it makes the dimensions it bears on unknown ([`Ledger::unknown`]),
//! a directive that needs one of them is refused `capacity_unknown` ([`Ledger::admits`]), the
//! capacity report says which and why with `free` zero there ([`Ledger::report`]), and the
//! dimension comes back when a listing describes it. The configured [`Sizing`] is not such a
//! default: the operator set it, and it bounds what this data plane creates. Nor is a measured
//! disk ceiling (ADR 0052's amendment of 2026-11-12): where no ceiling is configured, an image's
//! and a base's ceiling is its verified manifest's disk size and a clone's its base's
//! ([`MeasuredDisks`], [`Ledger::learn_image`]); one not measured yet is unknown, so a directive
//! that would create it is refused `capacity_unknown{disk}`.
//!
//! What a VM takes is the data plane's own sizing, never only what the control plane sent
//! (ADR 0024, mutant 24): a `StartVm` is counted at the larger of its run options and
//! [`Sizing`] ([`Ledger::demand`]), so a zero or tiny request cannot skip the memory fit.
//!
//! **DP-01 must serialise inventory listing against effects.** [`Ledger::apply_inventory`]
//! takes a complete listing as the truth and removes every VM absent from it: a listing taken
//! before a clone was applied and applied after it would forget that clone and free its
//! capacity. DP-01 lists only while no VM effect runs, or discards a listing that overlapped
//! one. Likewise [`HostProbe::foreign`] counts only VMs whose names are not `mf-*`: the ledger
//! counts a malformed `mf-*` VM itself from the inventory ([`Ledger::malformed`]), and a listed
//! base VM (`mfb-*`) from the inventory and the image store together ([`Ledger::base_vms`]).

use std::collections::{BTreeMap, BTreeSet};

use mayfly_domain::{DirectiveId, Epoch, ImageDigest, OwnedVmName, Resources, VmNameClass};
use mayfly_protocol::{
    BaseName, Capacity, CapacityConstraint, CapacityReport, DirectiveKind, GuestOs, RefusalReason,
    ResourceDimension, RunOptions, UnknownCapacity, UnknownCause, VmObservation,
};

use crate::inventory::Inventory;

/// The footprints of what a directive creates, from the data plane's own configuration (values
/// the operator set, ADR 0052), never from the control plane. A disk ceiling left `None` is
/// measured per image ([`MeasuredDisks`]), never defaulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sizing {
    /// The disk ceiling of one pulled image; `None`: its manifest's disk size.
    pub image_disk_bytes: Option<u64>,
    /// The disk ceiling of one materialised base; `None`: its image's manifest's disk size.
    pub base_disk_bytes: Option<u64>,
    /// The disk ceiling of one clone; `None`: its base's measured disk size.
    pub clone_disk_bytes: Option<u64>,
    /// The memory of one running VM by the data plane's own configuration: the floor of a
    /// `StartVm`'s memory demand whatever its run options ask (ADR 0024). Never what a VM of
    /// unknown memory is counted with: that memory is unknown (ADR 0052).
    pub vm_memory_bytes: u64,
}

impl Sizing {
    /// Whether any disk ceiling is left to the manifests.
    #[must_use]
    pub const fn measures_disks(&self) -> bool {
        self.image_disk_bytes.is_none()
            || self.base_disk_bytes.is_none()
            || self.clone_disk_bytes.is_none()
    }
}

/// Disk ceilings measured from verified image manifests (the sum of the disk layers'
/// uncompressed sizes, the disk Tart reconstructs): what the image store's listing says of each
/// image and base it holds. A size never changes for a digest, so what was measured is kept.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MeasuredDisks {
    /// Each image's disk, by digest.
    pub images: BTreeMap<ImageDigest, u64>,
    /// Each base's disk: its image's.
    pub bases: BTreeMap<BaseName, u64>,
    /// Each listed base's image, so a base whose own size did not read takes its image's once
    /// that is measured again ([`Ledger::unmeasured_listed`]).
    pub base_images: BTreeMap<BaseName, ImageDigest>,
}

/// What the host probe reports (DP-01's `HostProbe`, fed by inventory and probes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostProbe {
    /// What the host has for VMs: slots, the hypervisor's macOS-guest slots, memory and disk.
    pub totals: Capacity,
    /// What the operator keeps back from VMs.
    pub reserve: Capacity,
    /// What VMs that are not ours use: their slots, running macOS guests, memory and disk. Only
    /// VMs whose names are not `mf-*` ([`VmNameClass::Foreign`]): a malformed `mf-*` VM is
    /// counted by the ledger from the inventory ([`Ledger::malformed`]), never here too; nor a
    /// base's `mfb-*`, counted from the inventory and the image store's listing
    /// ([`Ledger::base_vms`], [`foreign_use`]).
    pub foreign: Capacity,
    /// The dimensions in which the probe could not size a VM that is not ours (`foreign` is
    /// then only what it could size), or could not measure the host itself (slots it could not
    /// derive; every dimension when the probe failed): the dimension is unknown (ADR 0052).
    pub foreign_unsized: BTreeSet<ResourceDimension>,
    /// Free disk is under the low-water mark: nothing that needs disk starts (F17).
    pub disk_low_water: bool,
    /// The host reports memory pressure: nothing that needs memory starts.
    pub memory_pressure: bool,
    /// The hypervisor refused another macOS guest: no macOS guest starts.
    pub hypervisor_limit: bool,
}

/// One of our VMs, as counted. `None` is a fact the ledger does not have, never a default
/// (ADR 0052): it makes the dimensions it bears on unknown ([`gaps`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmUse {
    /// Its guest: while it runs, a macOS guest takes a macOS-guest slot.
    pub guest: Option<GuestOs>,
    /// Whether it runs (it then uses memory, and a macOS guest slot if macOS).
    pub running: bool,
    /// The memory it runs with; counted only while it runs.
    pub memory_bytes: Option<u64>,
    /// Its disk ceiling.
    pub disk_bytes: Option<u64>,
}

/// One materialised base, as counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BaseUse {
    /// The guest its clones run; `None` until the image store's listing names it (a
    /// `MaterialiseImage`'s answer does not).
    pub guest: Option<GuestOs>,
    /// Its disk ceiling; `None` while not measured (ADR 0052).
    pub disk_bytes: Option<u64>,
}

/// What the VMs that are not ours use, by one complete provider listing: the host probe's
/// [`HostProbe::foreign`], [`HostProbe::foreign_unsized`] and the running count `HostFacts`
/// reports (DP-06). Read by [`foreign_use`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignUse {
    /// What they are known to take, counted as the ledger counts one of ours.
    pub used: Capacity,
    /// The dimensions a listed foreign VM's missing facts make unknown ([`gaps`]).
    pub unsized_in: BTreeSet<ResourceDimension>,
    /// How many listed VMs run that are not of our well-formed `mf-*` shape: the foreign ones,
    /// and malformed `mf-*` and base `mfb-*` VMs too, which the ledger sizes itself but which
    /// the hypervisor counts as running guests all the same (`HostFacts::external_guests`, the
    /// safe side: never fewer than the hypervisor sees beside ours).
    pub running: u32,
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
    /// Listed VMs with the `mf-` prefix whose names do not parse ([`VmNameClass::MalformedManaged`]):
    /// neither ours nor in [`HostProbe::foreign`], so counted here by what the listing says of
    /// them (what it does not say is unknown), and rebuilt by each complete inventory. Never a
    /// target: no directive can name one (plan §8.4: reported, never deleted).
    pub malformed: BTreeMap<String, VmUse>,
    /// Listed VMs whose names are a base's (`mfb-*`), by what the last complete inventory
    /// describes. [`foreign_use`] leaves them out, so the ledger counts each here (ADR 0052,
    /// fail safe): one the image store lists ([`Ledger::bases`]) is counted there for its disk
    /// and adds only what it takes while it runs; one the image store does not list is counted
    /// by the inventory in full, as a malformed `mf-*` VM is, so a base VM is never counted in
    /// neither ([`Ledger::base_vm`]).
    pub base_vms: BTreeMap<BaseName, VmUse>,
    /// Pulled images and their disk; `None` while not measured (ADR 0052).
    pub images: BTreeMap<ImageDigest, Option<u64>>,
    /// Materialised bases. No VM directive can name one (ADR 0026): it names an
    /// [`OwnedVmName`] (`mf-`), which a base name (`mfb-`) never parses as (I05).
    pub bases: BTreeMap<BaseName, BaseUse>,
    /// Disk ceilings measured from verified manifests: listed, or measured before a pull
    /// ([`Ledger::learn_image`]).
    pub measured: MeasuredDisks,
    /// Capacity set aside per `(epoch, directive)` from admission until its outcome is applied.
    pub reservations: BTreeMap<(Epoch, DirectiveId), Reservation>,
    /// A complete inventory has been applied since the start: until then our VMs are not known
    /// and every dimension is unknown.
    pub inventory_read: bool,
    /// The image store has been listed since the start: until then disk is unknown.
    pub images_read: bool,
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

/// What a VM is known to take: a slot, its disk, and while it runs its memory and a
/// macOS-guest slot if macOS. A fact it lacks adds nothing here and makes its dimension
/// unknown instead ([`gaps`]); the sum is then a lower bound that admits nothing there.
fn known_use(vm: &VmUse) -> Capacity {
    let macos_guests = match vm.guest {
        Some(guest) if vm.running => guest_slots(guest),
        Some(_) | None => 0,
    };
    let memory = match vm.memory_bytes {
        Some(bytes) if vm.running => bytes,
        Some(_) | None => 0,
    };
    Capacity {
        vm_slots: 1,
        macos_guests,
        // an unknown disk adds nothing here: `gaps` makes the dimension unknown instead
        resources: Resources::new(memory, vm.disk_bytes.unwrap_or(0)),
    }
}

/// The dimensions a VM's missing facts make unknown: disk without its disk; memory and the
/// macOS-guest slots while it runs without its memory or its guest. Pinned by
/// `tests/data_plane/ledger.rs::should_refuse_capacity_unknown_and_never_undercount_when_a_running_vm_has_unknown_memory`.
#[must_use]
pub fn gaps(vm: &VmUse) -> Vec<ResourceDimension> {
    let mut out = Vec::new();
    if vm.running && vm.guest.is_none() {
        out.push(ResourceDimension::MacosGuests);
    }
    if vm.running && vm.memory_bytes.is_none() {
        out.push(ResourceDimension::Memory);
    }
    if vm.disk_bytes.is_none() {
        out.push(ResourceDimension::Disk);
    }
    out
}

/// A listed VM as counted: what the listing describes, else what the ledger already knew of
/// it, else nothing (ADR 0052). The memory it knew is kept only while it was already known to
/// run as listed: a VM that started behind our back may run with anything.
fn described(listed: &VmObservation, before: Option<&VmUse>) -> VmUse {
    let memory = before.and_then(|vm| {
        if vm.running || !listed.running {
            vm.memory_bytes
        } else {
            None
        }
    });
    VmUse {
        guest: listed.guest.or(before.and_then(|vm| vm.guest)),
        running: listed.running,
        memory_bytes: listed.memory_bytes.or(memory),
        disk_bytes: listed.disk_bytes.or(before.and_then(|vm| vm.disk_bytes)),
    }
}

/// What the VMs of a complete listing that are not ours use: only names that are neither
/// `mf-*` ([`VmNameClass::Foreign`]; the ledger counts a malformed `mf-*` VM itself) nor a
/// base's `mfb-*` (counted by the ledger, [`Ledger::base_vms`], so never twice). Each is
/// counted by what the listing describes, as [`Ledger::apply_inventory`] counts one of ours,
/// and a fact it lacks makes its dimension unknown, never a default (ADR 0052). The running
/// count is wider ([`ForeignUse::running`]): every running VM not of our well-formed shape.
/// Pinned by
/// `tests/data_plane/ledger.rs::should_count_only_foreign_vms_and_name_the_unsized_dimensions_when_a_listing_mixes_them`.
#[must_use]
pub fn foreign_use(listed: &[VmObservation]) -> ForeignUse {
    let mut out = ForeignUse {
        used: ZERO,
        unsized_in: BTreeSet::new(),
        running: 0,
    };
    for vm in listed {
        let (ours, foreign) = match mayfly_domain::classify_vm_name(&vm.name) {
            VmNameClass::Foreign => (false, BaseName::new(&vm.name).is_err()),
            VmNameClass::MalformedManaged => (false, false),
            VmNameClass::WellFormed(_) => (true, false),
        };
        if vm.running && !ours {
            out.running = out.running.saturating_add(1);
        }
        if foreign {
            let counted = described(vm, None);
            out.used = plus(out.used, known_use(&counted));
            out.unsized_in.extend(gaps(&counted));
        }
    }
    out
}

impl Ledger {
    /// An empty ledger over `probe`.
    #[must_use]
    pub const fn new(sizing: Sizing, probe: HostProbe) -> Self {
        Self {
            sizing,
            probe,
            vms: BTreeMap::new(),
            malformed: BTreeMap::new(),
            base_vms: BTreeMap::new(),
            images: BTreeMap::new(),
            bases: BTreeMap::new(),
            measured: MeasuredDisks {
                images: BTreeMap::new(),
                bases: BTreeMap::new(),
                base_images: BTreeMap::new(),
            },
            reservations: BTreeMap::new(),
            next_report_id: 1,
            inventory_read: false,
            images_read: false,
        }
    }

    /// What the host has for VMs after the operator reserve.
    #[must_use]
    pub fn available(&self) -> Capacity {
        minus(self.probe.totals, self.probe.reserve)
    }

    /// What a listed base VM adds, and the dimensions its missing facts make unknown. One the
    /// image store lists is counted there for its disk and takes no VM slot (no job runs in a
    /// base), so only its running memory and macOS-guest slot are added and only their gaps
    /// count; one the image store does not list is counted by the inventory in full
    /// ([`known_use`], [`gaps`]), the larger way (ADR 0052). Pinned by
    /// `tests/data_plane/ledger.rs::should_count_a_listed_base_vm_when_the_image_store_does_not_list_it`.
    #[must_use]
    pub fn base_vm(&self, name: &BaseName, vm: &VmUse) -> (Capacity, Vec<ResourceDimension>) {
        if !self.bases.contains_key(name) {
            return (known_use(vm), gaps(vm));
        }
        let mut running = known_use(vm);
        running.vm_slots = 0;
        running.resources.disk_bytes = 0;
        let mut unknown = gaps(vm);
        unknown.retain(|dimension| *dimension != ResourceDimension::Disk);
        (running, unknown)
    }

    /// What is known to be used: our VMs, malformed `mf-*` VMs, listed base VMs
    /// ([`Ledger::base_vm`]), images, bases, every reservation and foreign VMs; a lower bound
    /// in every [`Ledger::unknown`] dimension. Pinned by
    /// `tests/data_plane/ledger.rs::should_count_a_malformed_mf_vm_when_a_complete_inventory_lists_it`.
    #[must_use]
    pub fn used(&self) -> Capacity {
        let listed_bases = self
            .base_vms
            .iter()
            .fold(ZERO, |sum, (name, vm)| plus(sum, self.base_vm(name, vm).0));
        let vms = self
            .vms
            .values()
            .chain(self.malformed.values())
            .fold(listed_bases, |sum, vm| plus(sum, known_use(vm)));
        let images = self
            .images
            .values()
            .fold(ZERO, |sum, bytes| plus(sum, disk(bytes.unwrap_or(0))));
        let bases = self.bases.values().fold(ZERO, |sum, base| {
            plus(sum, disk(base.disk_bytes.unwrap_or(0)))
        });
        let reserved = self
            .reservations
            .values()
            .fold(ZERO, |sum, r| plus(sum, r.demand));
        plus(
            plus(plus(vms, images), plus(bases, reserved)),
            self.probe.foreign,
        )
    }

    /// What is free: available minus used, never below zero; meaningless in an unknown
    /// dimension, which admits nothing ([`Ledger::fits`]) and is reported zero.
    #[must_use]
    pub fn free(&self) -> Capacity {
        minus(self.available(), self.used())
    }

    /// The dimensions the ledger cannot determine, each with every cause (ADR 0052): all four
    /// before the first complete inventory, disk before the first image listing, the probe's
    /// unsized foreign dimensions, each listed `mf-` VM's [`gaps`] and each listed base VM's
    /// ([`Ledger::base_vm`]). Empty when everything
    /// is known. Pinned by
    /// `tests/data_plane/ledger.rs::should_return_capacity_when_a_complete_listing_describes_every_vm`.
    #[must_use]
    pub fn unknown(&self) -> BTreeMap<ResourceDimension, BTreeSet<UnknownCause>> {
        let mut out: BTreeMap<ResourceDimension, BTreeSet<UnknownCause>> = BTreeMap::new();
        let mut add = |dimension: ResourceDimension, cause: UnknownCause| {
            out.entry(dimension).or_default().insert(cause);
        };
        if !self.inventory_read {
            for dimension in ResourceDimension::ALL {
                add(dimension, UnknownCause::InventoryNotRead);
            }
        }
        let unmeasured = self.images.values().any(Option::is_none)
            || self.bases.values().any(|base| base.disk_bytes.is_none());
        if !self.images_read || unmeasured {
            add(ResourceDimension::Disk, UnknownCause::ImagesNotRead);
        }
        for dimension in &self.probe.foreign_unsized {
            add(*dimension, UnknownCause::ForeignVmUnsized);
        }
        let ours = self.vms.iter().map(|(name, vm)| (name.to_string(), vm));
        let malformed = self.malformed.iter().map(|(name, vm)| (name.clone(), vm));
        let listed_bases = self
            .base_vms
            .iter()
            .map(|(name, vm)| (name.to_string(), self.base_vm(name, vm).1));
        let described = ours
            .chain(malformed)
            .map(|(name, vm)| (name, gaps(vm)))
            .chain(listed_bases);
        for (vm_name, dimensions) in described {
            for dimension in dimensions {
                add(
                    dimension,
                    UnknownCause::VmFactUnknown {
                        vm_name: vm_name.clone(),
                    },
                );
            }
        }
        out
    }

    /// What `kind` would take, by the data plane's own sizing and records. A target that is
    /// already there takes nothing more (a re-run is a re-observation, ADR 0042); a kind that
    /// creates or starts nothing takes nothing. A `StartVm` takes at least the configured
    /// sizing ([`Ledger::started`]), whatever its run options ask. Pinned by
    /// `tests/data_plane/matrix.rs::should_fit_or_name_the_short_dimension_when_each_creating_kind_meets_each_host`
    /// and `tests/data_plane/ledger.rs::should_count_a_start_at_the_configured_sizing_when_its_run_options_ask_for_less`.
    ///
    /// # Errors
    /// The dimension the demand itself cannot be determined in (ADR 0052): a `StartVm` of a
    /// stopped VM whose guest is not known (or that the ledger does not know at all) may or may
    /// not take a macOS-guest slot, and one whose disk is not known may or may not grow it.
    /// Pinned by `tests/data_plane/ledger.rs::should_refuse_capacity_unknown_when_a_start_targets_a_vm_of_unknown_guest`.
    pub fn demand(&self, kind: &DirectiveKind) -> Result<Capacity, ResourceDimension> {
        Ok(match kind {
            DirectiveKind::PullImage { digest } if self.images.contains_key(digest) => ZERO,
            DirectiveKind::PullImage { digest } => {
                disk(self.image_ceiling(digest).ok_or(ResourceDimension::Disk)?)
            }
            DirectiveKind::MaterialiseImage { base_name, .. }
                if self.bases.contains_key(base_name) =>
            {
                ZERO
            }
            DirectiveKind::MaterialiseImage { digest, .. } => {
                disk(self.base_ceiling(digest).ok_or(ResourceDimension::Disk)?)
            }
            DirectiveKind::CloneVm { vm_name, .. } if self.vms.contains_key(vm_name) => ZERO,
            DirectiveKind::CloneVm { base_name, .. } => Capacity {
                vm_slots: 1,
                macos_guests: 0,
                resources: Resources::new(
                    0,
                    self.clone_ceiling(base_name)
                        .ok_or(ResourceDimension::Disk)?,
                ),
            },
            DirectiveKind::StartVm {
                vm_name,
                run_options,
            } => {
                let wanted = self.started(run_options);
                match self.vms.get(vm_name) {
                    Some(vm) if vm.running => ZERO,
                    Some(vm) => {
                        let guest = vm.guest.ok_or(ResourceDimension::MacosGuests)?;
                        let disk_bytes = vm.disk_bytes.ok_or(ResourceDimension::Disk)?;
                        Capacity {
                            vm_slots: 0,
                            macos_guests: guest_slots(guest),
                            resources: Resources::new(
                                wanted.memory_bytes,
                                wanted.disk_bytes.saturating_sub(disk_bytes),
                            ),
                        }
                    }
                    None => return Err(ResourceDimension::MacosGuests),
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
        })
    }

    /// The disk ceiling of the image `digest`: configured, else measured from its manifest;
    /// `None` while neither (ADR 0052). Pinned by
    /// `tests/data_plane/ledger.rs::should_refuse_capacity_unknown_when_a_pull_needs_a_disk_not_measured_yet`.
    #[must_use]
    pub fn image_ceiling(&self, digest: &ImageDigest) -> Option<u64> {
        self.sizing
            .image_disk_bytes
            .or_else(|| self.measured.images.get(digest).copied())
    }

    /// The disk ceiling of a base made from `digest`: configured, else its image's measured.
    #[must_use]
    pub fn base_ceiling(&self, digest: &ImageDigest) -> Option<u64> {
        self.sizing
            .base_disk_bytes
            .or_else(|| self.measured.images.get(digest).copied())
    }

    /// The disk ceiling of a clone of `base`: configured, else its base's measured. Pinned by
    /// `tests/data_plane/ledger.rs::should_size_each_creation_by_its_manifest_when_no_ceiling_is_configured`.
    #[must_use]
    pub fn clone_ceiling(&self, base: &BaseName) -> Option<u64> {
        self.sizing
            .clone_disk_bytes
            .or_else(|| self.measured_base(base))
    }

    /// A base's measured disk: its own, else its image's.
    fn measured_base(&self, base: &BaseName) -> Option<u64> {
        self.measured.bases.get(base).copied().or_else(|| {
            self.measured
                .base_images
                .get(base)
                .and_then(|digest| self.measured.images.get(digest))
                .copied()
        })
    }

    /// The images whose disk is still unknown although listed, by digest: a listed image of
    /// no measured size, and the image of a listed base of none. The executor measures them
    /// again on each observer pass, and `mayflyd` reports them; until then disk is unknown,
    /// reported `images_not_read` (ADR 0052's amendment: no new protocol cause). Pinned by
    /// `tests/data_plane/ledger.rs::should_size_a_base_by_its_image_when_its_own_size_did_not_read`.
    #[must_use]
    pub fn unmeasured_listed(&self) -> BTreeSet<ImageDigest> {
        let images = self
            .images
            .iter()
            .filter(|(_, bytes)| bytes.is_none())
            .map(|(digest, _)| digest.clone());
        let bases = self
            .bases
            .iter()
            .filter(|(_, base)| base.disk_bytes.is_none())
            .filter_map(|(name, _)| self.measured.base_images.get(name).cloned());
        images.chain(bases).collect()
    }

    /// The image a `PullImage` would pull whose disk ceiling is neither configured nor measured
    /// yet: the executor measures its manifest before admitting it ([`Ledger::learn_image`]).
    #[must_use]
    pub fn unmeasured<'k>(&self, kind: &'k DirectiveKind) -> Option<&'k ImageDigest> {
        match kind {
            DirectiveKind::PullImage { digest }
                if !self.images.contains_key(digest) && self.image_ceiling(digest).is_none() =>
            {
                Some(digest)
            }
            DirectiveKind::PullImage { .. }
            | DirectiveKind::MaterialiseImage { .. }
            | DirectiveKind::EvictImage { .. }
            | DirectiveKind::CloneVm { .. }
            | DirectiveKind::StartVm { .. }
            | DirectiveKind::VerifyGuest { .. }
            | DirectiveKind::LaunchRunner { .. }
            | DirectiveKind::ExportDiagnostics { .. }
            | DirectiveKind::StopVm { .. }
            | DirectiveKind::DeleteVm { .. }
            | DirectiveKind::ReportInventory
            | DirectiveKind::ReportCapacity
            | DirectiveKind::ReportHostFacts => None,
        }
    }

    /// Keeps the disk size measured from the verified manifest of `digest` (a size never
    /// changes for a digest).
    pub fn learn_image(&mut self, digest: &ImageDigest, disk_bytes: u64) {
        self.measured.images.insert(digest.clone(), disk_bytes);
    }

    /// Whether `demand` fits what is free, checked per dimension in [`ResourceDimension`]
    /// order. A dimension the demand does not use always fits; one it uses does not while the
    /// probe flags its pressure (low disk, memory pressure, the hypervisor's limit), nor while
    /// it is unknown ([`Ledger::unknown`], ADR 0052): a known shortage is named first.
    ///
    /// # Errors
    /// The first dimension that does not fit (`insufficient_resources`) or is not known
    /// (`capacity_unknown`).
    pub fn fits(&self, demand: Capacity) -> Result<(), RefusalReason> {
        let free = self.free();
        let probe = &self.probe;
        let unknown = self.unknown();
        let checks = [
            (
                ResourceDimension::VmSlots,
                u64::from(demand.vm_slots),
                false,
                u64::from(free.vm_slots),
            ),
            (
                ResourceDimension::MacosGuests,
                u64::from(demand.macos_guests),
                probe.hypervisor_limit,
                u64::from(free.macos_guests),
            ),
            (
                ResourceDimension::Memory,
                demand.resources.memory_bytes,
                probe.memory_pressure,
                free.resources.memory_bytes,
            ),
            (
                ResourceDimension::Disk,
                demand.resources.disk_bytes,
                probe.disk_low_water,
                free.resources.disk_bytes,
            ),
        ];
        for (dimension, wanted, pressure, free) in checks {
            if wanted == 0 {
                continue;
            }
            if pressure || wanted > free {
                return Err(RefusalReason::InsufficientResources { dimension });
            }
            if unknown.contains_key(&dimension) {
                return Err(RefusalReason::CapacityUnknown { dimension });
            }
        }
        Ok(())
    }

    /// Whether `kind` fits now: its [`Ledger::demand`] and then [`Ledger::fits`]. Pure: nothing
    /// is set aside. Pinned by
    /// `tests/data_plane/matrix.rs::should_fit_or_name_the_short_dimension_when_each_creating_kind_meets_each_host`.
    ///
    /// # Errors
    /// `capacity_unknown` when the demand or a dimension it uses is not known,
    /// `insufficient_resources` when a dimension it uses is short.
    pub fn admits(&self, kind: &DirectiveKind) -> Result<Capacity, RefusalReason> {
        let demand = self
            .demand(kind)
            .map_err(|dimension| RefusalReason::CapacityUnknown { dimension })?;
        self.fits(demand)?;
        Ok(demand)
    }

    /// Admits `kind` for `key` if it fits, and sets its demand aside so that a concurrent
    /// directive cannot take the same capacity (I33). Pinned by
    /// `tests/data_plane/executor.rs::should_refuse_the_second_clone_when_the_first_reserved_the_last_slot`.
    ///
    /// # Errors
    /// The refusal of [`Ledger::admits`]; nothing is set aside.
    pub fn reserve(
        &mut self,
        key: (Epoch, DirectiveId),
        kind: &DirectiveKind,
    ) -> Result<(), RefusalReason> {
        let demand = self.admits(kind)?;
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
        match kind {
            DirectiveKind::PullImage { digest } => {
                let ceiling = self.image_ceiling(digest);
                self.images.entry(digest.clone()).or_insert(ceiling);
            }
            DirectiveKind::MaterialiseImage {
                base_name, digest, ..
            } => {
                let ceiling = self.base_ceiling(digest);
                if let Some(measured) = self.measured.images.get(digest).copied() {
                    self.measured.bases.insert(base_name.clone(), measured);
                }
                self.measured
                    .base_images
                    .insert(base_name.clone(), digest.clone());
                self.bases.entry(base_name.clone()).or_insert(BaseUse {
                    guest: None,
                    disk_bytes: ceiling,
                });
            }
            DirectiveKind::EvictImage { base_name } => {
                self.bases.remove(base_name);
            }
            DirectiveKind::CloneVm { base_name, vm_name } => {
                let guest = self.bases.get(base_name).and_then(|base| base.guest);
                let disk_bytes = self.clone_ceiling(base_name);
                self.vms.entry(vm_name.clone()).or_insert(VmUse {
                    guest,
                    running: false,
                    memory_bytes: None,
                    disk_bytes,
                });
            }
            DirectiveKind::StartVm {
                vm_name,
                run_options,
            } => {
                let started = self.started(run_options);
                let vm = self.vms.entry(vm_name.clone()).or_insert(VmUse {
                    guest: None,
                    running: false,
                    memory_bytes: None,
                    disk_bytes: None,
                });
                vm.running = true;
                vm.memory_bytes = Some(started.memory_bytes);
                vm.disk_bytes = vm.disk_bytes.map(|bytes| bytes.max(started.disk_bytes));
            }
            DirectiveKind::StopVm { proof, .. } => {
                if let Some(vm) = self.vms.get_mut(&proof.vm_name) {
                    vm.running = false;
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

    /// What a `StartVm` runs with by the ledger: its run options, raised to the configured
    /// sizing (the memory of one VM, the disk of one clone when configured; a measured clone's
    /// disk is the VM's own, recorded at its clone). The control plane may ask for
    /// more and is counted at that; it never gets less counted than the data plane's own sizing
    /// (ADR 0024, mutant 24). Raising rather than refusing: the data plane has no per-pool
    /// ceiling to refuse against, and counting the larger way never over-commits.
    #[must_use]
    pub fn started(&self, run_options: &RunOptions) -> Resources {
        let wanted = run_options.resources;
        Resources::new(
            wanted.memory_bytes.max(self.sizing.vm_memory_bytes),
            wanted
                .disk_bytes
                .max(self.sizing.clone_disk_bytes.unwrap_or(0)),
        )
    }

    /// Applies an inventory. A complete one is the truth about our VMs: a listed VM of our
    /// shape is counted by what the listing describes, else by what the ledger already knew of
    /// it, and what neither says stays unknown (ADR 0052); an unlisted one is gone, and held VM
    /// reservations are settled; a listed malformed `mf-*` VM is counted in
    /// [`Ledger::malformed`] and a listed base VM in [`Ledger::base_vms`], by what the listing
    /// describes. An incomplete one changes nothing
    /// (I25): no VM is released on it, and nothing becomes known. The caller serialises listing
    /// against effects (module doc). Pinned by
    /// `tests/data_plane/ledger.rs::should_release_nothing_when_an_inventory_is_incomplete` and
    /// `should_return_capacity_when_a_complete_listing_describes_every_vm`.
    pub fn apply_inventory(&mut self, inventory: &Inventory) {
        let listed = match inventory {
            Inventory::Complete { vms } => vms,
            Inventory::Incomplete { .. } => return,
        };
        let mut ours: BTreeMap<OwnedVmName, VmUse> = BTreeMap::new();
        self.malformed.clear();
        self.base_vms.clear();
        for vm in listed {
            match mayfly_domain::classify_vm_name(&vm.name) {
                VmNameClass::WellFormed(name) => {
                    let counted = described(vm, self.vms.get(&name));
                    ours.insert(name, counted);
                }
                VmNameClass::MalformedManaged => {
                    self.malformed.insert(vm.name.clone(), described(vm, None));
                }
                VmNameClass::Foreign => {
                    if let Ok(base) = BaseName::new(&vm.name) {
                        self.base_vms.insert(base, described(vm, None));
                    }
                }
            }
        }
        self.vms = ours;
        self.inventory_read = true;
        self.reservations
            .retain(|_, r| !(r.held && r.settles == Settles::Inventory));
    }

    /// Applies the image store's listing: the pulled images and the bases with their guests,
    /// each counted at its configured ceiling, else at what `disks` measured of it (kept for
    /// later: a digest's size never changes), else unknown (ADR 0052). Held image reservations
    /// are settled. Pinned by
    /// `tests/data_plane/ledger.rs::should_count_listed_images_and_bases_at_their_measured_disks_when_no_ceiling_is_configured`.
    pub fn apply_images(
        &mut self,
        images: &BTreeSet<ImageDigest>,
        bases: &BTreeMap<BaseName, GuestOs>,
        disks: &MeasuredDisks,
    ) {
        self.measured
            .images
            .extend(disks.images.iter().map(|(d, bytes)| (d.clone(), *bytes)));
        self.measured
            .bases
            .extend(disks.bases.iter().map(|(b, bytes)| (b.clone(), *bytes)));
        self.measured.base_images.extend(
            disks
                .base_images
                .iter()
                .map(|(b, digest)| (b.clone(), digest.clone())),
        );
        let sizing = self.sizing;
        self.images = images
            .iter()
            .map(|digest| (digest.clone(), self.image_ceiling(digest)))
            .collect();
        self.bases = bases
            .iter()
            .map(|(name, guest)| {
                let disk_bytes = sizing.base_disk_bytes.or_else(|| self.measured_base(name));
                (
                    name.clone(),
                    BaseUse {
                        guest: Some(*guest),
                        disk_bytes,
                    },
                )
            })
            .collect();
        self.images_read = true;
        self.reservations
            .retain(|_, r| !(r.held && r.settles == Settles::Images));
    }

    /// The `CapacityReported` content now, under the next report id (ADR 0024). Every unknown
    /// dimension is listed with its causes and reported free zero (ADR 0052). `foreign` is the
    /// probe's foreign use plus the malformed `mf-*` VMs and the listed base VMs the image store
    /// does not list: none of them is ours to run a job in. What keeps the
    /// host safe is this ledger's own gate, not the report: a directive that needs an unknown
    /// dimension is refused `capacity_unknown` ([`Ledger::admits`]); the control plane's
    /// admission reads totals, foreign and base use rather than `free`, so until it zeroes an
    /// unknown dimension (CP-04's note) it may propose there and be refused. Pinned by
    /// `tests/data_plane/ledger.rs::should_report_totals_used_free_and_foreign_when_capacity_is_reported`
    /// and `should_report_unknown_dimensions_with_free_zero_and_the_vms_to_describe_when_facts_are_missing`.
    pub fn report(&mut self) -> CapacityReport {
        let report_id = self.next_report_id;
        self.next_report_id = self.next_report_id.saturating_add(1);
        let totals = self.available();
        let used = self.used();
        let unknown = self.unknown();
        let probe = &self.probe;
        let unstored_bases = self
            .base_vms
            .iter()
            .filter(|(name, _)| !self.bases.contains_key(*name))
            .map(|(_, vm)| vm);
        let foreign = self
            .malformed
            .values()
            .chain(unstored_bases)
            .fold(probe.foreign, |sum, vm| plus(sum, known_use(vm)));
        let mut free = minus(totals, used);
        for dimension in unknown.keys() {
            match dimension {
                ResourceDimension::VmSlots => free.vm_slots = 0,
                ResourceDimension::MacosGuests => free.macos_guests = 0,
                ResourceDimension::Memory => free.resources.memory_bytes = 0,
                ResourceDimension::Disk => free.resources.disk_bytes = 0,
            }
        }
        let flags = [
            (probe.disk_low_water, CapacityConstraint::DiskLowWater),
            (probe.memory_pressure, CapacityConstraint::MemoryPressure),
            (foreign != ZERO, CapacityConstraint::ForeignGuests),
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
            free,
            foreign,
            reasons: flags
                .into_iter()
                .filter(|(on, _)| *on)
                .map(|(_, reason)| reason)
                .collect(),
            unknown: unknown
                .into_iter()
                .map(|(dimension, causes)| UnknownCapacity { dimension, causes })
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
