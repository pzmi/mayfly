# 0040. One scale set per pool at organisation scope

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09). Amended 2026-11-11 (owner, GH-00R): enterprise scope is
unsupported; several organisations are the next step.

## Context

A scale set's name is the label workflows use; GitHub gives a job to any idle runner of the set,
and capacity and statistics are per set. Scale sets exist at repository, organisation and
enterprise scope. Deleting a scale set that still has queued jobs has undocumented effects, and
the SDK's example deletes its set on shutdown (`doc/domain-model.md` OD14).

## Decision

One pool is exactly one scale set, and one scale set one pool. Organisation scope is the
supported scope in v0.x; repository scope is allowed for testing; enterprise scope later
(*unsupported: see the amendment of 2026-11-11*).
mayfly never deletes a scale set automatically, not on shutdown, decommission or error; a
decommissioned pool leaves its scale set in place for the operator. Extra labels are aliases
only.

## Consequences

`ScaleSet` streams are per pool (`scaleset-<pool>`); KER-06, GH-03 and RT-01 (config scope)
implement it; GH-00 captures an organisation-scope session.

## Alternatives considered

- Several pools behind one scale set: label routing GitHub does not do for JIT runners.
- Delete the scale set on decommission: fails queued workflows.

## Amended 2026-11-11 (owner, GH-00R): organisation scope only

**Context.** GitHub's documentation says "You cannot authenticate using a GitHub App for runners
at the enterprise level" (Authenticating ARC to the GitHub API, read 2026-11-11). An enterprise
scale set would need a classic personal access token, a long-lived user credential with wider
reach than the App (ADR 0045).

**Decision.** Organisation scope is the supported scope; repository scope stays for tests.
Enterprise-level scale sets are unsupported: config refuses an enterprise scope with a message
saying why. Several organisations, one scale set per pool in each, with the same GitHub App
installed in each organisation, are the planned next step (GH-07). True enterprise scope is
added only on demand, by a new ADR that weighs the classic token.

**Consequences.** `doc/github-scaleset.md` §1 and plan §7.2 say so; GH-07 carries the
multi-organisation work.
