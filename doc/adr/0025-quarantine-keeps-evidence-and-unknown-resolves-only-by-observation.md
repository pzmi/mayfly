# 0025. Quarantine keeps evidence and Unknown resolves only by observation

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09)

## Context

DDD-01 (hot spots OD9, OD10) recommended a per-pool quarantine cap that trips the pool, automatic
deletion of `forensic_retention` cases after seven days, and an operator command that asserts an
effect's outcome (`OutcomeAsserted`) so that a slot held by an `Unknown` effect can be freed.
Each trades evidence or certainty for capacity. The owner's rule is "never assume".

**Why outcomes become `Unknown`.** A directive was sent and its effect may or may not have
happened: the daemon was killed mid-call (SIGKILL, crash, power loss, upgrade), a hung provider
command was killed at its deadline after it may have acted, a reply was lost on the CP↔DP link,
the host slept across a deadline, or the data plane restarted mid-directive. Provider queries
(`tart list`, `tart get`, the guest launcher's status) are idempotent reads, so asking again is
always safe; only the answer can be missing.

## Decision

1. **Quarantine whenever ownership or an outcome cannot be proven** (closed reason set,
   `doc/domain-model.md` §6.10). The VM is kept (stopped unless a job is live), its lease keeps
   its resources and it is never reused.
2. **A quarantined VM is never deleted automatically**: no retention timer, no cap-driven
   deletion, no deletion to make room. It is deleted only by an operator's resolution, with
   ownership proof re-checked by the data plane, or released when a complete inventory observes
   it absent. Evidence is preserved.
3. **Capacity loss is accepted.** Quarantined leases count against capacity; a pool whose
   capacity is consumed admits nothing and `AdmissionExplain` names quarantine as the cause.
4. **Escalation is loud**: an ERROR event at opening, `mayfly_quarantined{pool,reason}`,
   `status`, and escalation levels at T_q1 and T_q2. Escalation never acts on the VM.
5. **`Unknown` leaves only on an observation** (inventory, launcher status, GitHub runner state).
   There is no operator assertion of an outcome; I10 stays strict. Automation re-observes at
   T_u1 and T_u2; when the provider cannot answer, the effect stays `Unknown` and is reported as
   a **provider limitation** (command, last error, age), and its slot stays held.

## Consequences

- Removed from the domain model and plan: `AssertOutcome`, `OutcomeAsserted`, `mayfly attempt
  assert`, the recovery-plan action `assert_outcome`, automatic `forensic_retention` deletion,
  the quarantine cap as a deletion trigger. I28 is rewritten; P16 and P19 change; CP-06
  implements the policies; F46 and mutants 26, 27 (ver §9) pin them.
- A host with a misbehaving provider can lose capacity until the operator repairs the provider
  or resolves cases; the operator sees exactly why.

## Alternatives considered

- Auto-delete after a retention period: loses evidence exactly when the doubt was real.
- A quarantine cap that deletes or trips: hides the cause behind a number.
- Operator assertion of outcomes: a guess becomes a fact for automation; a wrong one deletes a
  VM that is in use or double-books a slot.
