# 0017. Serde derive in the shared kernel

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09)

## Context

`mayfly-domain` holds the ids, `OwnedVmName`, `Generation`, `Epoch`, `Fence`, `ImageDigest`,
`Resources` and the time value types. Decision D9 makes the CP-DP protocol serde JSON with an
explicit `protocol_version`, and plan §5 stores events as JSON in the journal. Both carry these
types, so the kernel must be encodable. The alternative, a parallel set of wire types in
`mayfly-protocol` and `mayfly-control-domain` with conversions, doubles every id and lets the
wire form drift from the in-memory one.

`mayfly-domain` is a pure crate (fnd §2.2): no I/O, clock, randomness or runtime. `serde` with its
`derive` feature is a compile-time trait and macro crate and does none of these; `serde_json` is
a format and does I/O-adjacent work on bytes, which is what the pure crates must not name.

## Decision

`mayfly-domain` depends on `serde` with the `derive` feature and on nothing else at runtime. It
never depends on `serde_json` or any other format; encoding stays in `mayfly-protocol`, the
journal adapter and the golden-fixture harness.

A type whose invariant a malformed value would break (`PoolName`, `HostId`, the 128-bit ids,
`OwnedVmName`, `ImageDigest`) serialises as its canonical string and deserialises only through
its validating parser (`#[serde(try_from = "String")]`), so a decoded value is as valid as a
constructed one. `Generation` (1 or more) deserialises through its validating constructor from a bare number.
Plain value types (`Epoch`, `Millis`, `Resources`) use the derive's natural form. `proptest` is a dev-dependency only.

## Consequences

One definition of each kernel type serves the model, the wire and the journal. A decoder can
never produce an unowned `OwnedVmName` or a pool name with a separator in it. The `serde`
derive proc-macro enters the build of every crate; `architecture.toml` lists `serde` for
`mayfly-domain` and nothing format-specific. Later tasks (KER-02, KER-03) add the encoders.
Reversal means a new ADR and wire types elsewhere.
