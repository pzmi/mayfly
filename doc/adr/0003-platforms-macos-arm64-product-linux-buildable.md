# 0003. Platforms: macOS arm64 product, Linux-buildable

Date: 2026-11-09

## Status

Proposed

## Context

The product is a macOS VM runner: Tart drives Virtualization.framework, which exists only on
Apple silicon. Agents and CI, however, run on Linux (D15, ADR 0016), and the repository is
public, so contributors will not all have a Mac. Foundation section 2.2 evaluates the edges for all three targets, and section 3.3 forbids runtime `cfg!` branches (`ARCH-platform-branch`)
because a branch decided at run time is compiled and untested on the other platform.

## Decision

The product targets `aarch64-apple-darwin`. Every crate also builds and tests on Linux
`x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`, which are what CI and the agent
sandbox run. Platform-specific code uses `#[cfg(...)]` items or blocks, never a `cfg!(target_os
...)` runtime branch. Windows is out of scope.

## Consequences

Everything but real Tart and live GitHub is testable on Linux against fakes (ADR 0012, ADR 0015
for what is not). CI adds a macOS arm64 build and test job and a `cargo check --target
aarch64-apple-darwin` from Linux (FND-04), so a platform item that does not compile on one side
fails in CI. Windows contributors are unsupported. Proposed: the owner may widen or narrow the
list.
