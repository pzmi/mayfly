# 0040. One scale set per pool at organisation scope

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09)

## Context

A scale set's name is the label workflows use; GitHub gives a job to any idle runner of the set,
and capacity and statistics are per set. Scale sets exist at repository, organisation and
enterprise scope. Deleting a scale set that still has queued jobs has undocumented effects, and
the SDK's example deletes its set on shutdown (`doc/domain-model.md` OD14).

## Decision

One pool is exactly one scale set, and one scale set one pool. Organisation scope is the
supported scope in v0.x; repository scope is allowed for testing; enterprise scope later.
mayfly never deletes a scale set automatically, not on shutdown, decommission or error; a
decommissioned pool leaves its scale set in place for the operator. Extra labels are aliases
only.

## Consequences

`ScaleSet` streams are per pool (`scaleset-<pool>`); KER-06, GH-03 and RT-01 (config scope)
implement it; GH-00 captures an organisation-scope session.

## Alternatives considered

- Several pools behind one scale set: label routing GitHub does not do for JIT runners.
- Delete the scale set on decommission: fails queued workflows.
