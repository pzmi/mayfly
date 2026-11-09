# 0007. Event sourcing and CQRS in the control plane only

Date: 2026-11-09

## Status

Proposed (amended by 0024, 0026, 0037)

## Context

Plan section 5 makes every decision an event (goal G7) and section 3.1 gives the data plane the
job of observing and executing, not remembering. Event sourcing and CQRS buy audit, replay of the
lifecycle model against production code (ADR 0011) and a rebuildable read side, but they also
cost write-side ceremony. Putting them everywhere would force the data plane to persist state
and name a journal type, which contradicts plan section 3.4.

## Decision

Event sourcing and CQRS live in the **control plane only**. The data plane keeps no durable state
and re-observes the provider on start (plan section 3.1); what it knows reaches the control plane
as observations, which are facts recorded and deduplicated in the control plane's inbox. Aggregates
export pure `decide` and `evolve`, and `AggregateStore::save` is the only caller of
`Journal::append` (plan section 5.2).

## Consequences

A data-plane crate names no journal type (`ARCH-layer-isolation`, ADR 0019), so a data-plane host
can be killed and restarted without a recovery of its own: it reports inventory and the control
plane reconciles (plan section 10). The control plane carries the whole durability burden, which
ADR 0008 and ADR 0021 shape; the codec and registry are ADR 0018, and time enters as a value
(ADR 0020). Implemented by KER-03, ES-01 to ES-04.

Amended by later records: the data plane keeps an in-memory resource ledger rebuilt from
observation (ADR 0024), owns one durable artefact, the epoch-floor marker in `TART_HOME`, a fence
rather than state (ADR 0026), and leaves a per-VM shim's lock and `exit.json` beside each VM
(ADR 0037). None of these is a journal or a record of decisions.
