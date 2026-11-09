# 0012. Independent fault lab and fakes

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09)

## Context

A fault lab that shares code with production shares its bugs: if both parse a Tart JSON the same
wrong way, the test agrees with the defect. Verification section 7.4 asks that the instrument
cannot be fooled by the thing it measures. The same holds for the fakes that stand in for Tart and
GitHub in integration tests.

## Decision

The fault lab (`tools/faultlab`) imports **no** production crate. It drives the binaries, reads
SQLite with its own SQL, reads the state of `mayfly-fake-tart` and `mayfly-fake-github` with its
own parsers, and has its own checker. `mayfly-fake-tart` and `mayfly-fake-github` likewise
implement their wire behaviour independently of production. The `independent` role in
`[purity]` of `architecture.toml` carries this.

## Consequences

Duplicated parsing code, on purpose: a disagreement between the lab and production is a finding,
not a compile error. The lab reads the journal with its own SQL, so a journal adapter other than
SQLite (ADR 0008) needs its own reader there. Enforced by `ARCH` independence rules
(foundation section 2.6, ADR 0019); implemented by the fake-tart and fault-lab tasks.
