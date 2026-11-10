# 0052. No defaults for unknown facts: the data plane fails safe

Date: 2026-11-10

## Status

Accepted (owner, 2026-11-10). Amends ADR 0024 (the ledger's unknown facts), marked there.

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
6. **Configured values are not defaults.** `Sizing` is what the operator set: the disk
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
