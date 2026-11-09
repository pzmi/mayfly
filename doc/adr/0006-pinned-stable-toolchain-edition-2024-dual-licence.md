# 0006. Pinned stable toolchain, edition 2024, dual licence

Date: 2026-11-09

## Status

Proposed

## Context

A runner scheduler's own build must be reproducible: the Quint, nextest and clippy results the
gate relies on change with the compiler. Foundation section 4.1 asks for an exact toolchain pin and
a committed lockfile so that a green gate means the same thing on every machine and in CI. The
plan has no publishing story: mayfly is a daemon the owner deploys, not a library.

## Decision

The toolchain is pinned to stable Rust 1.97.0 in `rust-toolchain.toml`, edition 2024, resolver 3.
`Cargo.lock` is committed and CI builds with `--locked`. The licence is `MIT OR Apache-2.0`.
Crates are not published to crates.io.

## Consequences

Compiler upgrades are deliberate commits (foundation section 8, item 5: a version bump is its
own commit with the gate green), so new lints never break the gate unannounced. `cargo-deny`
checks dependency licences against a permissive allowlist (FND-04). Not publishing removes the
semver obligations of a library. Proposed: the owner may pick a different licence or toolchain.
