# 0015. Owner-gated real-world lanes

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09)

## Context

Two things cannot be tested honestly without the real world: real Tart on a Mac with
Virtualization.framework, and the live GitHub scale-set service. Agents run in a Linux sandbox
with no Docker, macOS or Tart (ADR 0016), and a pull request from a fork must never reach a
self-hosted Mac. Verification section 2 therefore treats them as a separate, owner-paid level.

## Decision

Real-world lanes are owner-gated. A manual `hardware.yml` workflow runs only on dispatch, on the
owner's self-hosted Mac with a dedicated `TART_HOME`, against a disposable GitHub repository and
App; never on pull requests and never from a fork. Agents deliver everything up to that
boundary (the workflow, the gated tests against the fakes, the docs) and report the `(owner)`
criterion as pending.

## Consequences

Everything else is proven against fakes and models on every commit, and the gated tests are
written before the hardware can run them. A defect only the hardware shows is found late, by the
owner. The owner registers the runner and dispatches the lane; an agent never runs a real `tart`
(the work skill's constraints). Implemented by the hardware workflow and E2E tasks.
