# 0016. Agent sandbox and fail-closed tools

Date: 2026-11-09

## Status

Proposed

## Context

Agents implement tasks in a Linux sandbox without Docker, macOS or Tart. The gate needs Node
(Quint) and a JVM (Apalache), and a gate that silently skipped a missing tool would report green
for work it did not check. Supply-chain attacks through CI actions are a known path, which is why
Scorecard and pinning are in foundation section 7.

## Decision

The agent sandbox is Linux with no Docker, macOS or Tart. Quint (Node) and Apalache (JVM) are
dev-only tools pinned in `tools/versions.toml`; a missing or wrong tool **fails closed**, never a
skipped-but-green check, and the message says how to install it. `cargo xtask doctor` reports
tool state when local and CI disagree. GitHub Actions are pinned by commit SHA (the `action-pins`
stage).

## Consequences

A gate result means the same thing everywhere, at the price of an install step on a fresh
machine (`cargo xtask install-tools`, foundation section 8). The shipped binary never needs Node
or Java. Implemented by FND-02 (`doctor`, `tools/versions.toml`, action pins) and FND-06 (the
formal toolchain); a stage that skips counts as failed in the stage table.
