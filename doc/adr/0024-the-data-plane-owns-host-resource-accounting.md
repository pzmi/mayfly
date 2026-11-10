# 0024. The data plane owns host resource accounting

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09); amended by 0052

## Context

DDD-01 (`doc/domain-model.md` §3, §6.3, hot spot OD4) made the control plane's `Host` aggregate
the authority on a host's physical resource vector (slots, memory, disk ceiling, two macOS
guests) and on external consumption, with the data plane reporting only threshold facts. The
facts that bound a host (free memory, free disk, running macOS guests including foreign ones,
the hypervisor's own limit) are known only where the VMs run. A control-plane copy of them is
stale between reports, and in the multi-host edition every host has its own. The data plane is
already the last line of refusal for ownership (plan §8.4); the owner wants the same for
resources: "we must manage capacity; the data plane should manage resources".

## Decision

1. **The data plane accounts and enforces.** Each host agent keeps an in-memory resource ledger,
   rebuilt from the provider inventory and host probes on start (no durable state, ADR 0007):
   per dimension (VM slots, memory, disk ceilings of live clones and bases, macOS-guest slots,
   operator reserves) the total, what running and in-flight VMs use (ours and foreign) and what
   is free. It **refuses** a `CloneVm`, `StartVm`, `PullImage` or `MaterialiseImage` that does
   not fit, as a definite `DirectiveRefused{insufficient_resources, dimension}`, whatever the
   control plane decided.
2. **The data plane reports capacity**: `CapacityReported{report_id, totals, used, free,
   foreign, reasons}` at the handshake, on every change and on `ReportCapacity`.
3. **The control plane allocates only within reported capacity.** `Host` applies each report
   (`CapacityReportApplied`, a newer `report_id` wins) and keeps leases as *allocation*
   bookkeeping: `ReserveSlot` is refused when the lease does not fit the last report's free
   capacity minus leases the report does not yet show. Pools, admission order, per-scale-set
   carve-outs and the `maxCapacity` sent to GitHub stay control-plane decisions over reported
   capacity. When the two disagree, the data plane's refusal wins and the reconciler re-reads.
4. I03 is held twice: by admission in the control plane (I03, I24) and by enforcement in the
   data plane (I33).

## Consequences

- `Host` loses `RecordExternalConsumption` and the physical vector as its own truth and gains
  `RecordCapacityReport`; `doc/domain-model.md` §3, §6.3 and plan §3, §9 say so. KER-05, KER-08,
  KER-02 (`ReportCapacity`, `CapacityReported`, the refusal reason), DP-01, DP-06 and CP-03
  implement it; mutant 24 (ver §9) and F37, F26 pin it.
- A stale report can make the control plane over-reserve; the cost is a definite refusal and a
  retry on the next pass, never an overcommitted host.
- Two ledgers must be kept honest; the conformance driver for scheduling compares them.

Amended by ADR 0052 (2026-11-10): the ledger assumes nothing it does not know; a dimension it
cannot determine refuses what needs it (`capacity_unknown`) and is reported with its causes and
nothing free.

## Alternatives considered

- Control-plane authority with data-plane thresholds (the DDD-01 proposal): overcommits between
  a change and the next threshold fact.
- The data plane decides admission alone: it would need demand, pools and GitHub reporting,
  which belong to the control plane (plan §3.1).
