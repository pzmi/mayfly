# 0010. Serde JSON link protocol with an explicit version

Date: 2026-11-09

## Status

Proposed

## Context

Plan section 3.2 defines the control-plane / data-plane contract as versioned,
transport-agnostic messages, with three transports (`inproc` now, `uds` for the fault lab and
split roles, `tls` later; plan section 3.3). Both directions are at-least-once and a lossy
mismatch between two builds of one daemon is possible during upgrades. The kernel types that
cross the link must therefore be encodable (ADR 0017), and an in-process transport that skipped
encoding would let a non-serialisable field through.

## Decision

The CP-DP protocol is serde JSON. Every message carries `protocol_version` and a peer refuses an
unsupported major version. Even the in-process transport encodes and decodes every message. Secrets
cross only as a `SecretHandle` (plan section 3.2, ADR 0014), resolved by the link adapter at send
time. `serde` derive in `mayfly-domain` is the subject of ADR 0017; `serde_json` and encoding stay
out of the pure crates.

## Consequences

One definition of each message serves all transports, and one link contract suite runs against
each of them. JSON is bigger and slower than a binary format, which is irrelevant at this volume
and keeps frames readable in the fault lab and in failures. A wire change is a version change, so
messages are golden-tested (verification section 4). Implemented by KER-02 and DP-02.
