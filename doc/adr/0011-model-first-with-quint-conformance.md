# 0011. Model first with Quint conformance

Date: 2026-11-09

## Status

Proposed

## Context

The hardest bugs in this system are lifecycle and recovery bugs: an effect with an unknown
outcome, a stale epoch, a crash between intent and effect. Verification section 6 answers with
Quint models whose invariants are checked by Apalache and whose traces replay against production
code. The sibling project site-preview retrofitted its model after the code; the retrofit found
real divergences, and mayfly can avoid paying for them.

## Decision

**Model first from day one.** The lifecycle, scheduling, recovery, scale-set and link Quint models
precede the Rust code they govern. A change to any of them updates the Quint model, its named
scenarios and the conformance driver before the Rust code. Conformance replays ITF traces against
the production code (verification section 6). A modelled invariant is never removed or weakened
to get green, and every check has a negative control.

## Consequences

Slower first delivery of KER-04 to KER-07 and the data-plane link model, in return for a
specification that is executable and cannot silently drift. It needs Node (Quint) and a JVM
(Apalache) as dev tools, pinned with fail-closed checks (ADR 0016, FND-06). The foundation rule
and the `/work` plan station enforce the ordering; the conformance gate enforces the agreement.
