# 0015. Owner-gated real-world lanes

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09); amended 2026-11-11 (by the owner, 2026-11-11: the real-Tart
lane is `cargo xtask hw01` run from a terminal; `hardware.yml` is optional)

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

## Amended 2026-11-11 (owner): the real-Tart lane is one command run from a terminal

The owner decided (PLAN-01, HW-01X) that the real-Tart lane needs no self-hosted runner. It is
`cargo xtask hw01 [--macos] [--service-mode --service-user <user>] [--report <file>]`, run by the
owner on their Mac from a checkout of the commit to check, with Rust and Tart 2.41.1 installed
(`doc/hw01.md`). The command refuses to start off macOS; creates its own throwaway `TART_HOME`
(`0700`) and never reads or writes `~/.tart` or the caller's `TART_HOME`; pulls only the images
`tools/hw01/images.toml` pins by digest; records everything it makes before making it and deletes
exactly that when it exits, one Ctrl-C included (a crash or a second Ctrl-C leaves it, and
`cargo xtask hw01 --cleanup <run dir>` replays the record later); and writes a versioned,
redacted JSON report the owner sends back. `hardware.yml` becomes optional: should the owner register a self-hosted
runner later, it runs the same command on manual dispatch and uploads the report. The rest of
the decision stands: the lane is owner-gated, never runs on pull requests or from a fork, and no
agent runs a real `tart`; agents build and test the command against `mayfly-fake-tart`, and turn
the report into fixtures and fixes.

Reason: a runner registered on the owner's own Mac costs a standing service, a token and a
workflow to maintain for a lane that runs a few times a year; a terminal command gives the same
evidence with nothing left installed. The D14 row, foundation §7 and verification §2 follow.
