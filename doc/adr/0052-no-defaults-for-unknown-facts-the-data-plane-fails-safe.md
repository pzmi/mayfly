# 0052. No defaults for unknown facts: the data plane fails safe

Date: 2026-11-10

## Status

Accepted (owner, 2026-11-10). Amends ADR 0024 (the ledger's unknown facts), marked there.
Amended 2026-11-12 (owner, RT-06): a measured fact is not an assumption; the host's sizing
keys are optional and measured when absent (see the amendment below).
Amended 2026-11-12 (owner, CP-07): the control plane plans a pool without `clone_disk_gib` at
its placed base's measured disk, carried by protocol 1.4.

## Context

The KER-08 data-plane ledger (ADR 0024) filled every fact it did not have with a value: a VM
adopted after a restart, or a malformed `mf-*` VM, was counted as a macOS guest with the
configured memory and a clone's disk; a base's guest was macOS until listed; a `StartVm` of a
VM the ledger did not know was counted as macOS; and an empty ledger before the first listing
read as "nothing exists". A larger guess can still undercount (a VM started with more memory
than the configured sizing), and any guess hides that the host does not know. The KER-08
checkpoint put this to the owner, who answered (2026-11-10): "What do you mean unknown? Why
they would be unknown? We should not default to anything I think, if something is not
specified we should fail safe fail early".

## Decision

1. **Nothing unknown is assumed.** A fact the data plane does not have is unknown, not a
   default: a listed VM's guest, its memory while it runs, its disk; our VMs before the first
   complete inventory (every dimension); images and bases before the first image listing
   (disk); a foreign VM the host probe could not size (`HostProbe::foreign_unsized`).
2. **Unknown makes a dimension indeterminate.** The ledger sums only what it knows (a lower
   bound) and lists each unknown dimension with its causes (`Ledger::unknown`).
3. **Fail early.** A directive that needs an unknown dimension, or whose own demand cannot be
   determined (a `StartVm` of a VM whose guest or disk is not known, or that the ledger does
   not know), is refused with the new definite reason `capacity_unknown{dimension}`; a known
   shortage in the same dimension is named first (`insufficient_resources`).
4. **Report it.** `CapacityReported` carries `unknown: [{dimension, causes}]`, the causes
   naming the VMs to describe (`vm_fact_unknown{vm_name}`) or the listing not read yet, and
   reports `free` zero in each unknown dimension, so the control plane allocates nothing there
   and `mayfly status`/`doctor` can say why. *(Corrected 2026-11-10: the control plane does not
   read `free`; see the correction below.)*
5. **Capacity returns on description.** A complete inventory whose `VmObservation`s carry
   `guest`, `memory_bytes` and `disk_bytes` (new optional fields), or the data plane's own
   record of what it created, makes the dimension known again. An incomplete one describes
   nothing (I25).
6. **Configured values are not defaults.** *(Extended 2026-11-12 by the owner's amendment
   below: where nothing is configured, a measured value applies.)* `Sizing` is what the operator set: the disk
   ceilings of an image, a base and a clone the data plane creates, and the memory floor of a
   `StartVm` (KER-08F). They count what this data plane creates, never a VM it did not.
7. **Protocol: minor 1.1, compatible.** `capacity_unknown`, `listing_incomplete` (ADR 0053),
   `CapacityReport::unknown` and the `VmObservation` facts are protocol 1.1 (`UNKNOWN_FACTS`).
   A frame never uses more than the session's minor (`ObservationKind::written_in`): in a 1.0
   session `capacity_unknown` is written `insufficient_resources` (a definite refusal either
   way) and the new fields are left out, while `free` is already zero. An older build that
   ignores this is still correct, so it lands in one release. *(Corrected 2026-11-10: it is
   correct because the data plane refuses, not because `free` is zero; see below.)*

## Consequences

- Removed defaults in `mayfly-data-domain` (`ledger.rs`): adopted VM guest macOS, memory
  `vm_memory_bytes`, disk `clone_disk_bytes`; malformed VM the same; a materialised base's
  guest macOS; a clone of an unlisted base macOS; a `StartVm` of an unknown VM macOS with a
  clone's disk; a stopped VM's memory zeroed (it is kept, and counted only while running); an
  unlisted ledger read as empty. Kept, as facts and not defaults: absence on a complete listing
  (`Inventory::seen`), `Unobserved` before a complete inventory (`supervisor`), and the
  unreachable `step` fallback in `DataPlane::receive`, which refuses (`effect_failed`).
- DP-06's probe sizes foreign VMs or names the unsized dimensions; DP-04's inventory describes
  each VM (`tart get`: OS, memory, disk) or leaves the fact out; DP-01 reads a complete
  inventory and the image store before its first capacity report. The control plane's
  `RefusalReason` gains `capacity_unknown` where the link adapter maps it (KER-05's `Host`
  already allocates within `free` *(corrected 2026-11-10: it does not; see below)*); the
  operator interface shows `unknown`.
- `formal/scheduling.qnt`: `dp.memUnknown`, `vmFactsChange`, `dpFits` and `dpReport` follow,
  invariant `noStartWhileCapacityUnknown`, scenario `unknownMemoryStartsNothingTest`.
- Pinned by `tests/data_plane/ledger.rs`:
  `should_refuse_capacity_unknown_and_never_undercount_when_a_running_vm_has_unknown_memory`,
  `should_refuse_capacity_unknown_when_a_start_targets_a_vm_of_unknown_guest`,
  `should_return_capacity_when_a_complete_listing_describes_every_vm`,
  `should_refuse_capacity_unknown_for_every_fit_when_nothing_has_been_listed_since_the_start`,
  `should_report_unknown_dimensions_with_free_zero_and_the_vms_to_describe_when_facts_are_missing`;
  the `matrix/ledger.txt` rows; `executor.rs::should_refuse_insufficient_resources_in_a_1_0_session_when_capacity_is_unknown`;
  and `mayfly-protocol`'s
  `should_write_only_what_the_session_accepted_when_it_is_older_than_the_unknown_facts`.
- A host with an undescribable VM delivers nothing in that dimension until an operator acts;
  that capacity loss is accepted, as for quarantine (ADR 0025).

## Alternatives considered

- Count unknown memory at the pool maximum and unknown guests as macOS (the KER-08 proposal): a
  larger guess is still a guess, and a VM started outside the pools exceeds it.
- Reuse `insufficient_resources` for unknown capacity: the operator could not tell a full host
  from one that does not know, and the fix differs.

## Correction 2026-11-10 (planner, DEC-04F): what keeps an unknown dimension unallocated

The decision stands; three sentences above claimed more than the code does. The control
plane's admission (`mayfly-control-domain`'s `capacity::admissible`, KER-05) reads a report's
totals, foreign and base use, not `free`, and nothing in the control plane reads `unknown` yet.
So a zero `free` does not by itself stop the control plane from allocating in an unknown
dimension, in a 1.1 session or a 1.0 one. What is true today:

- **The data plane refuses.** A directive that needs an unknown dimension is refused
  `capacity_unknown` (`insufficient_resources` in a 1.0 session), a definite refusal that
  executes nothing (I33's data-plane half; pinned by `tests/data_plane/ledger.rs` and
  `executor.rs::should_refuse_insufficient_resources_in_a_1_0_session_when_capacity_is_unknown`).
  An older build that ignores the new fields is still correct for this reason.
- **The control plane may propose and be refused.** Until CP-04's unit that maps
  `CapacityReported` into the `Host`'s `ReportedCapacity` zeroes what each unknown dimension
  can deliver, the control plane can issue a directive there and read the refusal (CP-03
  re-reads it next pass). The note on CP-04's card in `doc/tasks.md` records that work.

## Amended 2026-11-12 (owner): measured facts are not assumptions (RT-06)

The owner (2026-11-12): "The system should be usable and have reasonable defaults out of the
box, so users don't have to research everything for a quick start", with host sizing
**measured, not guessed**, keeping decision 1. RT-02 had made `[host] vm_slots`,
`macos_guests`, `image_disk_gib` and `clone_disk_gib` required (exit 78). They are optional
now (the keys keep their names), and a configured value still overrides. Where none is
configured, each value is a fact with a source, never a guess:

| Value | When not configured | Source |
|:--|:--|:--|
| `macos_guests` | 2; a configured value is capped at 2. Linux guests are not bound by it. | Apple's limit: the macOS licence allows two virtual macOS instances per Mac, and Virtualization.framework refuses a third running macOS guest |
| `image_disk_gib`, `clone_disk_gib` | Per image: the sum of the disk layers' `org.cirruslabs.tart.uncompressed-size` in the image's manifest (the disk Tart reconstructs); a base is its image's, a clone its base's | The manifest fetched by digest and checked to hash to it before any layer is pulled (`ImageStore::measure`, contract case IC-15), and the manifest kept at the pull for the listing |
| `vm_slots` | min((RAM − memory reserve) ÷ the largest memory of any pool, (CPUs − CPU reserve) ÷ the most CPUs of any pool): each divisor the largest any pool asks, the safer choice since the ledger does not track CPUs | The host probe's physical memory and CPU count (on Linux bounded by the process's cgroup v2 `memory.max` and CPU quota), read on every probe |
| `reserve_memory_mib` | max(25 % of RAM, 8 GiB), for the person using the machine; it is also the memory-pressure mark, as before | The host probe's physical memory |
| `reserve_cpus` (new key) | 2 | The owner's default |

What cannot be measured stays unknown (decision 1): a manifest not measured yet (or that does
not read) makes a pull, a base or a clone of it `capacity_unknown{disk}`, and a listed image or
base of unknown size makes disk unknown (`images_not_read`, whose meaning now includes "a
listed image's or base's disk not measured": no new protocol cause; `mayflyd` names each such
image on stderr, `unmeasured_image=<digest> reason=manifest_unreadable`). The data plane
measures such an image again on every observer pass, so a lost kept manifest blocks disk work
only until the registry answers. It is not dropped from the listing to plan a re-pull: its files
are still on disk, and counting it as absent would undercount (decision 1). CPUs that do not
read make
`vm_slots` unknown (reported through the probe's existing `foreign_vm_unsized` cause, which the
executor already uses for a failed probe; a dedicated cause would be protocol 1.2; stderr says
`reason=cpus_unreadable`); memory that
does not read fails the probe, as before. Until the control plane places a pool's image (CP-07),
a pool without `clone_disk_gib` plans its VMs with no disk, and the data plane's gate (I33)
refuses what does not fit. `mayflyd` writes each value and its source to stderr at start-up
(`mayfly_composition::sizing`, the seam RT-03's status will return).

Compatibility, both ways a rollback can differ: (a) with `reserve_memory_mib` absent, the
default memory reserve, and with it the memory-pressure mark, moves from a fixed 4 GiB to
max(25 % of RAM, 8 GiB), so the same file keeps more memory back on this build than on an older
one (the older one stays correct, only less conservative); (b) an older build refuses a file
that sets `reserve_cpus` (unknown keys are refused) or omits a sizing key (exit 78), which fails
loudly rather than run another sizing. A file that sets every sizing key and not
`reserve_cpus` means the same to both, apart from (a) when `reserve_memory_mib` is absent. No Quint model changes: the scheduling model takes slots and disk as
inputs, and an unknown dimension refuses through the `capacity_unknown` path it already models.

Pinned by `mayfly-adapter-host`'s `tests/probe.rs`
(`should_measure_slots_from_memory_and_cpus_when_none_are_configured`,
`should_keep_the_configured_reserve_when_slots_are_measured`,
`should_take_the_configured_slots_without_reading_cpus_when_slots_are_configured`,
`should_leave_slots_unknown_when_the_cpus_cannot_be_read`), `mayfly-data-domain`'s
`tests/data_plane/ledger.rs`
(`should_refuse_capacity_unknown_when_a_pull_needs_a_disk_not_measured_yet`,
`should_size_each_creation_by_its_manifest_when_no_ceiling_is_configured`,
`should_size_by_the_configured_ceiling_when_one_is_configured`), `mayfly-data`'s
`tests/executor/effects.rs::should_measure_the_manifest_before_admitting_a_pull_when_no_ceiling_is_configured`,
the `ImageStore` contract's IC-15, and `mayfly-composition`'s `tests/sizing.rs`,
`tests/config.rs::should_cap_macos_guests_at_apples_limit_when_more_are_configured` and
`tests/daemon.rs::should_start_and_report_each_sizing_source_when_no_sizing_key_is_configured`;
the re-measure by `mayfly-data`'s
`tests/executor/effects.rs::should_measure_a_listed_image_again_when_its_size_did_not_read` and
`mayfly-data-domain`'s `should_size_a_base_by_its_image_when_its_own_size_did_not_read`; no
registry fetch for a fenced pull by `should_fetch_no_manifest_when_a_pull_is_stale` and
`should_measure_only_a_fresh_admissible_pull_when_its_disk_is_unknown`; and IC-15's broken
stores (`GuessingSizeStore`, `TrustingMeasureStore`).

## Amended 2026-11-12 (owner): the control plane plans at the placed base's measured disk (CP-07)

The RT-06 amendment above says that until the control plane places a pool's image (CP-07), a
pool without `clone_disk_gib` plans its VMs with no disk. The owner decided (2026-11-12, CP-07's
F3) how that ends: the data plane answers a materialisation with the disk the base reconstructs
to, measured from its image's verified manifest (the size this ADR's RT-06 amendment already
measures), in protocol 1.4's optional `disk_bytes` on `ImageMaterialised`; the control plane
records it on the placement (`ImageMaterialised` v2 in the journal) and plans each clone of a pool
without `clone_disk_gib` at its placed base's disk on that host. A configured `clone_disk_gib`
still wins.

Decision 1 holds: a base whose disk was not reported (a data plane of protocol 1.3 or older, a
manifest that did not measure, a listing that failed) is unknown, never a default; its clones are
planned with no disk, and the data plane's gate (I33) refuses what does not fit, as before. An
older control plane ignores the field (an optional field of a newer minor) and plans with no disk,
which is correct by the same gate; a 1.3 session gets no field. Pinned by
`mayfly-control-domain`'s
`tests/reconcile.rs::should_cap_reservations_by_the_hosts_disk_when_the_placed_base_is_measured`,
`mayfly-control`'s `tests/store/placement.rs::should_cap_reservations_by_the_hosts_disk_when_the_placed_base_reports_its_disk`
and `should_plan_no_disk_when_the_materialised_answer_carries_none`, `mayfly-protocol`'s
`should_leave_the_base_disk_out_when_the_session_is_older_than_measured_disk` and `mayfly-data`'s
`tests/executor/effects.rs::should_answer_the_bases_measured_disk_when_a_materialisation_is_applied`.
