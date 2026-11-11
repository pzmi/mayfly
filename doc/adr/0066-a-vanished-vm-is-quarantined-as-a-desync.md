# 0066. A vanished VM is quarantined as a desync

Date: 2026-11-10

## Status

Accepted (owner, 2026-11-10). Amends ADR 0053 (a stop of an absent target), marked there.
Amended 2026-11-11 (DEC-07): a vanished VM is no longer quarantined; the desync is recorded and
the attempt closed, whether a stop, a start or the inventory finds it (accepted by the owner,
2026-11-11). Decisions 2 and 3 are replaced by the amendment below; 1, 4, 5 and 6 stand.

## Context

ADR 0053 made a `DeleteVm` whose target a complete listing shows absent `Done` (`VmDeleted`):
"delete" means "ensure absent", whoever removed it. It left a `StopVm` of an absent target refused
`proof_mismatch` ("an absent VM is not a stopped one"), and DP-01 refused a `StartVm` of an
absent VM the same way. The DEC-04 card left the stop open as an owner decision, with the
planner's proposal of a distinct, non-quarantining refusal `target_absent`. But the two cases are
not alike: the control plane issues a stop or a start only for a VM it recorded cloning
(`VmCloned` with its instance id), and mayfly removes a VM only by a `DeleteVm`, which a stop or
start never follows. So a stop or start that finds its VM gone means the provider and the journal
disagree, and nothing mayfly did explains it. The owner answered (2026-11-10): "If we know it was
there and we didn't do it it should be quarantined for review- this is invalid state, we
desynced somehow and we must inform user about it and us to ship eventual fix."

## Decision

1. **A new definite refusal, `target_absent`.** The data plane answers a `StopVm` or `StartVm`
   whose target a **complete** listing does not list (nothing in the target's slot for a stop,
   the exact name for a start) with `DirectiveRefused{target_absent}`, never touching anything.
   A listed but unproven target stays `proof_mismatch` (ADR 0026, ADR 0053's notes); an
   incomplete listing stays `Unknown{listing_incomplete}` (I25); a `DeleteVm` of an absent
   target stays `VmDeleted` (ADR 0053).
2. *(Replaced 2026-11-11, see the amendment.)* **The control plane quarantines it as a desync.** `target_absent` for the pending stop or
   start quarantines the attempt with the distinct reason `vm_vanished_externally`, the VM
   recorded not running (`vm: stopped`), so no stop is issued for it. It is never read as
   stopped, lost or a stage failure, and never retried.
3. *(Replaced 2026-11-11, see the amendment.)* **It stays quarantined for review.** Unlike the other quarantines, the inventory's absence
   does not release it to `Lost`: the absence is its reason. Only an operator's
   `ResolveQuarantine` ends it, with a delete that ADR 0053 answers `VmDeleted`. No timer, no
   cap (ADR 0025).
4. **The operator is told.** `mayfly status` and `mayfly quarantine list|inspect` name the reason
   in plain words (the VM vanished outside mayfly: a desync to report), and the diagnostics
   bundle (ADR 0036) carries the case with its evidence, so the operator can send it to us and
   the defect behind it can be fixed. Quarantine opening already logs at ERROR and counts in a
   metric (CP-06, RT-04).
5. **A start refused for its proof quarantines too.** A `StartVm` refused `proof_mismatch` (its
   exact name listed without an instance id or twice; or, from a data plane older than protocol
   1.2, its VM absent) now quarantines `provider_identity_mismatch`, as a stop, a delete or a
   launch already did (ADR 0026), instead of backing off.
6. **Protocol 1.2, compatible.** `target_absent` is written from protocol 1.2
   (`mayfly_protocol::VANISHED_TARGET`); a session below it gets `proof_mismatch`
   (`ObservationKind::written_in`). Is an older build that ignores this still correct? It is
   safe: a definite refusal either way, nothing touched, no outcome asserted. A control plane of
   1.1 or older quarantines the stop (`provider_identity_mismatch`), but it reads a start's
   `proof_mismatch` as a stage failure: it backs off, retires the attempt and deletes it, which
   ADR 0053 answers `VmDeleted`, so that one case is not reported as a desync by an older
   control plane. A 1.2 control plane quarantines a 1.1 data plane's answer to either
   (decision 5). In v0.x both planes run from one build (`inproc`, or `uds` from one
   installation), so the mixed case lasts only as long as an upgrade of one role.

## Consequences

- `mayfly-protocol`: `RefusalReason::TargetAbsent`, `VANISHED_TARGET`, `CURRENT` 1.2; fixture
  `observation/v1.refused-target-absent.json`. Pinned by
  `observation.rs::should_write_proof_mismatch_when_the_session_is_older_than_the_vanished_target`.
- `mayfly-data-domain`: `Stimulus::TargetAbsent` takes `Executing(ReObserve)` to
  `Refused(target_absent)` (`matrix/directive.txt`); `DataPlane::reobserved` gives it for an
  absent stop or start target. Pinned by `tests/data_plane/executor.rs`:
  `should_refuse_target_absent_when_a_stop_finds_its_target_gone`,
  `should_refuse_target_absent_when_a_start_finds_its_target_gone`,
  `should_write_proof_mismatch_for_a_vanished_target_when_the_session_is_older_than_1_2`.
- *(The quarantine items below were replaced on 2026-11-11 by the amendment's tests, model and
  fixtures; the refusal, its fixture and protocol 1.2 stand.)*
- `mayfly-control-domain`: `RefusalReason::TargetAbsent`, `QuarantineReason::VmVanishedExternally`
  (event fixtures `directive_refused/v1.target-absent.json`,
  `attempt_quarantined/v1.vm-vanished-externally.json`), the determinism case
  `vanished_vm_quarantines`, the matrix state `quarantined/vanished` and stimulus
  `directive_refused_absent`. Pinned by `tests/attempt/lifecycle.rs`:
  `should_quarantine_as_vanished_when_a_stop_or_start_finds_its_vm_absent`,
  `should_keep_a_vanished_vm_quarantined_when_the_inventory_shows_it_absent`,
  `should_quarantine_when_the_data_plane_refuses_the_proof_of_a_start`.
- `formal/lifecycle.qnt` (model first): `replyTargetAbsent`, the attempt's `vanished` flag,
  `inventoryAbsent` keeping a vanished quarantine, `replyProofRefused` for a start, invariant
  `vanishedStaysQuarantined`, scenarios `stopOfVanishedVmQuarantinesTest` and
  `startOfVanishedVmQuarantinesTest`. A scratch mutation that lets the inventory release it is
  found by simulation.
- The new enum variants are additions to existing v1 event payloads. No release has been cut,
  so no journal holds the old shape; after the first release such a change ships as prepare
  then activate (`doc/event-store.md` §6).
- Later work: CP-04's link mapping reads `target_absent`; CP-06 opens the case; RT-03 and OPS-07
  show and bundle it (notes on their cards in `doc/tasks.md`).
- Open, for the owner: the inventory still reads a recorded VM absent from a complete listing,
  outside any stop or start, as `Lost` (ADR 0025, dm §6.4). By the same reasoning it may be a
  desync too; that is not changed here. *(Decided 2026-11-11: it is, see the amendment.)*

## Alternatives considered

- A distinct non-quarantining refusal (the DEC-04 proposal): it records the absence but tells no
  one, and a desync would pass as routine.
- Keep `proof_mismatch`: it quarantines a stop as an identity mismatch, which misnames the
  fault, and leaves a start as a stage failure.
- Read the absence as `Lost` and move on: hides the defect that caused it.

## Amended 2026-11-11 (DEC-07): a vanished VM converges like a controller's (accepted by the owner 2026-11-11)

**Context.** Decision 3 kept a vanished VM's attempt quarantined until an operator resolved it,
and the inventory's absence outside a stop or start was still read as a silent `Lost` (the open
item above). Asked about both, the owner answered: "maybe we should treat it like a k8s
controller? we want desired state and try to restore it? keep log/audit/journal for
debugging?", choosing option A: one rule for every way the absence is found.

**Decision.** A VM the attempt recorded as existing (`VmCloned`, its instance id) and mayfly did
not delete, found absent from a **complete** listing, whether by a `StopVm` or `StartVm`
re-observation (`target_absent`, decision 1, unchanged) or by the periodic inventory, is
handled the same way:

1. **Recorded.** The attempt records the fact `VmVanished { discovered_by, instance_id, vm }`
   (`discovered_by`: `inventory`, `stop_vm` or `start_vm`; the recorded instance id; whether the
   VM was last known running): a new event type, kept in the journal for debugging.
2. **Closed, never resurrected.** The attempt is lost (`Lost`; its slot released on that
   evidence, `ReleaseEvidence::AttemptLost`) and closed. No directive is issued for it again;
   the reconciler provisions a fresh attempt and VM for the demand that remains, as a controller
   restores desired state.
3. **Surfaced.** The `attempts` view carries the desync (`desync`, spec version 2), which
   `mayfly status` and `mayfly vm inspect` show (RT-03) and the diagnostics bundle carries
   (OPS-07); a metric counts it (RT-04: no metrics port exists in the control plane yet).
4. **Counted.** A vanish is a pool failure for the breaker (P15, CP-06), so repeated vanishes of
   one pool's image open it.
5. **Quarantine is for what is present.** It remains for a VM that is present and does not match
   the record (another instance id under our name, ADR 0026; an unknown `mf-*` VM). Such a VM
   stays quarantined while it is listed; once the recorded VM's absence is observed on a
   complete listing, it is a vanish like any other. A quarantined VM of our name that the
   attempt never recorded is released by its absence as `AttemptLost` (it was never known to be
   ours: no desync).
6. **Absence only from a complete listing** (I25). An incomplete listing (`InventoryIncomplete`,
   `Unknown{listing_incomplete}`) concludes nothing: an answer leaves the pending outcome
   `Unknown`, and nothing is recorded vanished, lost or deleted.

The data plane is unchanged: `target_absent` stays the honest observation; only the control
plane's reaction changes. `QuarantineReason::VmVanishedExternally` stays in the enum so a
journal that holds it decodes, but no build writes it; an attempt quarantined so converges when
the inventory shows its VM absent.

**Consequences.**

- `formal/lifecycle.qnt` (model first): `vanish` replaces `quarantineVanished`;
  `replyTargetAbsent` and `inventoryAbsent` record it, quarantined or not; `listingIncomplete`
  is the incomplete listing. Invariants `mismatchStaysQuarantined`, `vanishedClosedForGood` and
  `noAbsenceFromIncomplete` replace `vanishedStaysQuarantined`, each with a negative-control
  scenario that forces the transition it forbids. Regression traces
  `stopOfVanishedVmIsClosedAsDesync`, `startOfVanishedVmIsClosedAsDesync`,
  `inventoryFindsVanishedVm`, `quarantinedVmVanishedIsClosed`, `mismatchedVmStaysQuarantined`,
  `incompleteListingConcludesNothing`; `requarantineAfterVanished` becomes
  `requarantineAfterResolution`.
- `mayfly-control-domain`: event `vm_vanished` v1 (fixtures `vm_vanished/v1.json`,
  `v1.stop-vm.json`, `v1.start-vm.json`), `VanishDiscovery`, `AttemptState::vanished`; the
  determinism cases `vanished_vm_closes` and `inventory_vanished_vm_closes`. Pinned by
  `tests/attempt/lifecycle.rs`: `should_close_as_vanished_when_a_stop_or_start_finds_its_vm_absent`,
  `should_close_as_vanished_when_the_inventory_shows_a_recorded_vm_absent`,
  `should_keep_a_mismatched_vm_quarantined_when_it_is_listed_until_it_is_absent`,
  `should_conclude_no_absence_when_a_stop_is_reobserved_on_an_incomplete_listing` and the
  negative control `should_record_no_vanish_when_the_refusal_is_not_a_recorded_vm_s_absence`;
  by `mayfly-conformance`'s `should_catch_a_forgotten_desync_at_the_first_divergent_step_when_the_inventory_finds_a_vm_gone`.
- `mayfly-control`: `attempts` v2 (`desync`), `pending_effects` v2 (subscribes `VmVanished`).
- Recovery (KER-07) is consistent and unchanged in its rows: `MarkLost` reads a complete
  listing's absence, and the attempt it reaches records `VmVanished`; a quarantined record is
  kept by recovery and converges through the attempt's own reading of the inventory.
- Compatibility: `vm_vanished` is a new event type. An older build that meets it cannot decode
  it and starts in safe mode (foundation.md §6 rule 8): safe, nothing destructive. No release
  has been cut, so no deployed build meets it; after the first release such a change ships as
  prepare then activate (`doc/event-store.md` §6).
- Later work: CP-06 counts `VmVanished` in the breaker; RT-03 shows the desync in `status` and
  `vm inspect`; RT-04 adds its metric; OPS-07 bundles it (notes on their cards).

**Alternatives considered.** Keep quarantining (decision 3): every vanish waits on an operator
while its capacity is lost, and the inventory's absence stays a second, silent path. Read every
absence as a plain `Lost`: converges, but hides the desync the owner wants kept and surfaced.
