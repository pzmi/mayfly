# 0013. Tart through its CLI by argv; guests by tart exec or ssh

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09); partly superseded by 0038 (guests: no SSH)

## Context

Tart is a CLI, and its source is licensed FSL-1.1-ALv2 (plan section 1.3): mayfly executes the
binary and never incorporates its code. Reaching the guest needs either `tart exec` or SSH. A Rust
SSH library would add a large dependency and a protocol implementation to a security-sensitive
path; shelling out through a shell would invite injection (plan section 13).

## Decision

Tart is driven only through its CLI by **argv**, never `sh -c`, with a deadline, its own process
group, bounded captured output and redaction (plan section 8.2). Guests are reached by `tart exec`
where the image supports it, else by the system `ssh` binary with a pinned per-image host key
(never `StrictHostKeyChecking no`). There is no Rust SSH library in v0.x.

## Consequences

Process authority is confined to the Tart and guest adapters, composition and test tools
(`ARCH-process-authority`, foundation section 2.2), and each call is one argv vector a test can
assert. The system `ssh` binary must exist on the host and its behaviour is outside our control.
An image that supports neither path cannot be used. Implemented by DP-04 and the guest adapter
task; the guest launcher and JIT delivery are ADR 0014.

Partly superseded by ADR 0038 (2026-11-09, owner): guests are reached only by `tart exec`; the
SSH path and the pinned per-image host key are gone. Tart by argv stands.
