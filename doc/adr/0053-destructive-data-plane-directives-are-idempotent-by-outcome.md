# 0053. Destructive data-plane directives are idempotent by outcome

Date: 2026-11-10

## Status

Accepted (owner, 2026-11-10). Amends ADR 0026 (an absent delete target), marked there.

## Context

A `DeleteVm` or `StopVm` is executed at most once per `(epoch, directive_id)` and a repeat in
the session answers the recorded outcome (`formal/link.qnt`). The relay re-sends a directive
whose answer was lost under the next session, where it executes again: KER-08 re-observed the
target, found it absent because the first delivery had deleted it, and refused
`proof_mismatch`, which the control plane reads as an ownership doubt and quarantines on. A
stop of a VM already stopped ran `tart stop` again. The KER-08 checkpoint put this to the
owner, who answered (2026-11-10): "How about idempotent api? Can we make it idempotent? We
should opt in for idempotent and safe apis. If someone requests the same thing - delete the
same vm, we should say it's done, shouldn't we?"

## Decision

1. **`DeleteVm` means "ensure absent".** When the re-observation right before the effect is a
   **complete** listing that does not list the target, the directive is `Done` with
   `VmDeleted`, without an effect, whether this directive, an earlier delivery or someone else
   removed it.
2. **`StopVm` on a stopped VM is done.** When the re-observation proves the target (exact name,
   generation and instance id, ADR 0026) and shows it stopped, the directive is `Done` with
   `VmStopped`, without `tart stop`.
3. **Safety is unchanged.** A present target that differs in name, generation or instance id
   is refused `proof_mismatch` and never touched; a `StopVm` whose target is absent is still
   `proof_mismatch` (an absent VM is not a stopped one; the inventory reports the absence). The
   proof's own consistency, `protected_target`, safe mode and the deadline are checked first,
   as before.
4. **An incomplete listing is never `Done` (I25).** A re-observation that fails, does not parse
   or does not finish answers `OutcomeUnknown{listing_incomplete}` (new unknown reason, protocol
   1.1; `deadline` in a 1.0 session, ADR 0052): nothing was done now, and what an earlier
   delivery did is not known. It replaces the earlier `effect_failed` refusal, which claimed
   more than the data plane knew.
5. The same directive id keeps answering its first outcome within the session (the table).

## Consequences

- Control plane: a `DeleteVm` whose earlier answer was lost is `Unknown`; the `VmDeleted`
  answer to its re-delivery is an observation of absence on a complete listing, so the
  `Unknown → Deleted` transition is by observation (I10, ADR 0025); nobody asserts an outcome.
  A provider that cannot list completely leaves it `Unknown`, reported as a provider
  limitation.
- `mayfly-data-domain`: `Reobservation` carries `running`; `Stimulus::Satisfied` takes
  `Executing(ReObserve)` to `Done`; `ReObserveFailed` goes to `Unknown{listing_incomplete}`
  (`matrix/directive.txt`). DP-01 passes `None` to `DataPlane::reobserved` only for a complete
  listing and calls `reobserve_failed` otherwise. *(Changed 2026-11-10: `reobserved` takes the
  inventory itself; see the notes below.)*
- `formal/lifecycle.qnt`: a delete's success reply is enabled when the world holds no VM
  (`replySucceededWith`); scenarios `deleteOfAbsentVmIsDoneTest` and
  `deleteLostThenAnsweredDeletedTest`. Its inventory already read absence on a complete listing
  as `Deleted` for a pending delete (`inventoryAbsent`); a stop of a stopped VM was already a
  success. `formal/link.qnt` is unchanged: it models fencing and the table, not the effect.
- Pinned by `tests/data_plane/executor.rs`:
  `should_answer_done_when_a_delete_is_resent_and_a_complete_listing_shows_the_target_absent`,
  `should_answer_unknown_never_done_when_a_delete_reobservation_listing_is_incomplete`,
  `should_refuse_proof_mismatch_when_the_reobserved_name_generation_or_instance_id_differs`,
  `should_answer_done_when_a_stop_finds_its_proven_target_stopped`,
  `should_refuse_proof_mismatch_when_a_stop_finds_its_target_gone`.

## Alternatives considered

- Keep `proof_mismatch` for an absent target: a retried delete quarantines a VM that is gone.
- Answer `Done` on any absence, an incomplete listing included: reads absence from a listing
  that says nothing (I25).
- Treat a stop of an absent VM as done: reports a VM stopped that is not there.

## Notes 2026-11-10 (planner, DEC-04F): who reads absence, and an Unknown sticks to its id

The decision stands; these record how it is carried and one consequence it left implicit.

- **The data plane reads absence, not its caller.** `DataPlane::reobserved` takes the
  provider's `Inventory` and reads it itself (`executor::reobserve`), instead of an
  `Option<&Reobservation>` whose `None` meant "absent from a complete listing" with nothing in
  the type to say the listing was complete. Only a complete listing with nothing that may be
  the target is absence: a VM in the target's slot (host and slot, any pool, generation or
  attempt), the exact name without an instance id or listed twice, or an `mf-` name that does
  not parse is present but unproven, refused `proof_mismatch` and never touched; an incomplete
  listing is `Unknown{listing_incomplete}`. `reobserve_failed` is gone: an incomplete
  inventory says it. Pinned by `tests/data_plane/executor.rs`:
  `should_refuse_proof_mismatch_when_a_listed_target_has_no_instance_id`,
  `should_refuse_proof_mismatch_when_an_unparseable_mf_name_is_listed`,
  `should_refuse_proof_mismatch_when_the_target_name_is_listed_twice`,
  `should_refuse_proof_mismatch_when_the_reobserved_name_generation_or_instance_id_differs`,
  `should_answer_done_when_a_delete_is_resent_and_a_complete_listing_shows_the_target_absent`
  (other VMs listed) and
  `should_answer_unknown_never_done_when_a_delete_reobservation_listing_is_incomplete`.
- **An `Unknown` outcome sticks to its directive id.** Decision 5's table answers a repeat of
  the same id in the session with its first outcome, `Unknown{listing_incomplete}` included, so
  re-sending that id in the session never learns more. The control plane resolves the
  `Unknown` by observation (a complete inventory, ADR 0025) and, if the effect is still wanted,
  issues it again under a new directive id; the next session's table is empty, so a re-delivery
  there executes afresh (decision 1).
