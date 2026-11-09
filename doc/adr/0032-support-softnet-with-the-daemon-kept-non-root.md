# 0032. Support Softnet with the daemon kept non-root

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09)

## Context

Tart's Softnet isolates a guest's network from the host's LAN. It needs root privileges for its
packet filter, while mayfly's daemon runs as a non-root user (plan §13). DDD-01 recommended no
Softnet in v0.x (`doc/domain-model.md` OD19); the owner decided to support it.

## Decision

mayfly supports Softnet for guests, and the daemon itself stays non-root. The privilege model is
designed by its own task (NET-01) and recorded in its own ADR, choosing between:

- Tart's Softnet started through a narrow `sudoers` rule (no password, one path-pinned binary,
  fixed arguments), installed and checked by `mayfly service install` and `doctor`; or
- a separate privileged helper (a launchd daemon with a minimal, audited interface) that the
  non-root daemon asks to start Softnet for one VM.

Until NET-01 lands, pools run with Tart's default NAT and SEC-01 records the reachability matrix.
Softnet is network isolation, not an egress allowlist.

## Consequences

NET-01 (design, ADR, `doctor` checks), DP-04 (`run --net-softnet` options), SEC-01 (reachability
tests with and without Softnet). An untrusted tier (ADR 0027) will depend on it.

## Alternatives considered

- No Softnet in v0.x (the DDD-01 recommendation).
- Run the daemon as root: every bug becomes a root bug.
