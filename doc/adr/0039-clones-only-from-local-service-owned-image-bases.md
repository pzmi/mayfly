# 0039. Clones only from local service-owned image bases

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09)

## Context

`tart clone <oci-ref>` reuses Tart's OCI cache without checking the registry, and Tart prunes
that cache automatically (least recently used, by size) unless `TART_NO_AUTO_PRUNE` is set; local
VMs are never pruned. Pulling at job time makes job latency and success depend on a registry and
on cache state mayfly does not own (`doc/domain-model.md` OD13).

## Decision

1. **Never pull at job time.** `CloneVm` clones only from a local, service-owned image base
   (`mfb-…`) on the host; an OCI reference never reaches a clone.
2. **`mayfly image refresh <pool>`** prepares a base: pull by digest and record the digest;
   clone a base named by digest and runner version; boot it once and install the pinned,
   SHA-checked runner through `tart exec`; verify it; move the pool's base pointer in the journal,
   keeping the previous one for rollback; garbage-collect bases no pool or live clone references.
3. `TART_NO_AUTO_PRUNE` is set for every Tart call; eviction is explicit (P17).
4. `status` warns about runner age; an optional daily runner-only refresh is a pool setting.

## Consequences

Image placement is journaled (`ImagePlacement`, KER-11) and executed by data-plane directives
(DP-07); CP-07 manages placement; OPS-06 builds the command; ADR 0030's refresh is this command.
A host needs disk for one base per pinned digest plus the previous one.

## Alternatives considered

- Clone straight from the OCI reference: registry and cache at job time.
- Bases materialised by hand, presence only verified: a manual step per image.
