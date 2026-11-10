# 0026. Provider instance identity in the ownership proof and an external epoch floor

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09); amended by 0053

## Context

Ownership today is journal record + exact `OwnedVmName` + generation (plan §8.4, G4). A VM
re-created under our name (by a human, a restored state directory or a zombie daemon) passes
that test. Tart keeps a clone's MAC address unless it collides, so the MAC alone does not tell
two clones apart. Run epochs fence directives (I09), but a journal restored from a backup mints
epochs that were already used, and nothing outside the journal remembers the highest one
(`doc/domain-model.md` OD6, OD7).

## Decision

1. **Provider instance id.** `CloneVm` clones and then gives the clone a fresh random MAC
   (`tart set <vm> --random-mac`); the data plane reads it back and reports it in
   `VmCloned{instance_id}`, and the attempt records it. A VM is owned only if the journal record,
   the exact name, the slot generation **and** the instance id match. The data plane
   re-observes the instance id immediately before `DeleteVm` (and before a proof-carrying
   `StopVm`) and refuses on any mismatch; the attempt is then quarantined
   (`provider_identity_mismatch`). The data plane also refuses any destructive directive naming
   an image base (`mfb-*`) or any VM in its base set.
2. **External epoch floor.** The dedicated `TART_HOME` carries a deployment marker (deployment id
   and epoch floor) owned by the data plane. Before a new epoch is used, the floor is raised to
   it and fsynced (file and directory); the data plane refuses a `Welcome` whose epoch is below
   the floor. At start-up a journal whose highest minted epoch is below the floor was rolled
   back: safe mode (I17). A deployment-id mismatch fails `doctor` and closes admission.

## Consequences

- The marker is the data plane's only durable artefact: a fence that lives with the VMs it
  fences, not state about them (amends ADR 0007). It is not restored with the state directory,
  which is the point.
- KER-01 (`OwnershipProof`, `ProviderInstanceId`), KER-02 (`Hello` carries the floor), KER-08,
  DP-03 (fake-tart `set --random-mac`, `get`), DP-04, CP-04 implement it; F35, F39 and mutant 25
  pin it. HW-01 verifies `--random-mac` and MAC read-back on the pinned Tart versions.

Amended by ADR 0053 (2026-11-10): a `DeleteVm` whose target a complete re-observation shows
absent is done (`VmDeleted`), not refused; a present target that differs is still refused.

## Alternatives considered

- Name and generation only: a re-created VM with our name is deletable.
- A marker file written into the VM's directory: reads provider internals.
- Floor in the Keychain (headless access), a sidecar in the state directory (restored with it),
  or no floor (ghost writers after a restore).

## Implementation note (2026-11-10), not an amendment

Recorded from the KER-08 checkpoint review (KER-08F); the decision and its status are unchanged.

- A `MaterialiseImage` whose runner version is malformed (empty, longer than 64 bytes, or not
  inert: anything outside `[A-Za-z0-9.+_-]`) is refused `effect_failed` before it reaches argv
  (`mayfly-data-domain` `executor::admission`; pinned by
  `tests/data_plane/executor.rs::should_refuse_a_runner_version_when_it_is_empty_too_long_or_not_inert`).
- A VM directive naming a VM of another host, and a `StopVm` or `DeleteVm` whose proof names
  another host, is refused `protected_target` (pinned by
  `tests/data_plane/executor.rs::should_refuse_a_protected_target_when_a_delete_names_another_host`
  and the `other_host` column of `tests/fixtures/matrix/admission.txt`).
- Decision 1's base refusal holds by type: a VM directive names an `OwnedVmName` (`mf-`), which
  a base name (`mfb-`) never parses as (I05), so the executor has no runtime base check (pinned
  by `tests/data_plane/executor.rs::should_never_parse_a_base_name_as_a_vm_name_when_a_vm_directive_could_name_it`).
- The re-observation before a destructive effect compares the exact name, the generation and
  the instance id with the proof (`DataPlane::reobserved`; pinned by
  `tests/data_plane/executor.rs::should_refuse_proof_mismatch_when_the_reobserved_name_generation_or_instance_id_differs`).
