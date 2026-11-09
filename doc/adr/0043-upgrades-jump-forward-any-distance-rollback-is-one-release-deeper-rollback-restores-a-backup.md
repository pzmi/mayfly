# 0043. Upgrades jump forward any distance; rollback is one release, deeper rollback restores a backup

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09). Decides dm §10.1 OD2.

## Context

An operator may skip releases, and may want to go back after a bad one. A forward-only journal
with "unknown event means safe mode" makes every binary rollback a safe-mode incident, and safe
mode must not be the only answer when the journal itself is the doubt (dm OD2, SR H1/H4).

## Decision

1. **Forward: any jump of any number of versions is supported.** Every event stays readable for
   ever; every upcaster and every migration is kept. A CI **upgrade matrix** opens the journal of
   every released version and upgrades it straight to the current build (ES-11).
2. **Rollback by one version is supported**, by the two-release rule (event-store §6): a change an
   older build must not ignore is *prepared* in release N+1 and *activated* in N+2.
3. **Deeper rollback restores the automatic pre-migration backup** (`mayfly restore <backup>`).
   A backup is taken before every migration. The restored store sits below the external epoch
   floor, which is detected (I17).
4. **An older build meeting newer data refuses to write the affected streams and says why**
   (`StreamEpochUnsupported`, event-store §6.3); it still reads them. An unknown event kind is
   safe mode, as before.
5. **A recovery after a deep rollback quarantines the VMs it cannot prove** (`ownership_doubt`,
   ADR 0025). Ownership is never asserted by the operator and never assumed, so nothing is
   deleted on a doubt.

## Consequences

- Upcasters, golden fixtures and migrations are never retired; a released event shape costs
  maintenance for ever. A rename or removal is refused (ARCH rules, golden harness).
- ES-11 gains the matrix from every released version; OPS gains `mayfly restore`.
- Capacity can be lost after a deep rollback (quarantined VMs wait for the operator); accepted,
  the same trade as ADR 0025.

## Alternatives considered

- Drain-before-upgrade mandatory and no rollback: forces downtime and leaves a bad release
  without an exit.
- Rebuild from inventory and GitHub only: nothing could be proven deletable, so hosts fill up.
- Operator-asserted ownership in a salvage plan: forbidden by ADR 0025.
