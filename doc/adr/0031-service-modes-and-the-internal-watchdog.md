# 0031. Service modes and the internal watchdog

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09)

## Context

mayfly runs on a developer's Mac and on unattended servers. Tart and Virtualization.framework
need a user session, the Keychain is per user, and a wedged daemon that still holds its lock is
invisible to a service manager that only restarts exited processes (`doc/domain-model.md` OD16,
OD17).

## Decision

1. **Two ways to run, one binary and one config.** Locally, `mayflyd` runs in the foreground,
   started by hand (logs to stderr; Ctrl-C runs the shutdown policy P24). On a server, launchd
   starts it automatically: a LaunchAgent in an auto-login session for v0.x, with `KeepAlive`,
   `ThrottleInterval` and `ExitTimeOut`, installed by `mayfly service install`. No flag changes
   the daemon's semantics between the two.
2. **Internal watchdog plus `KeepAlive`.** Every supervised loop (listener, delivery workers,
   reconciler, store writer) heartbeats; a stall longer than T_wd exits with a distinct watchdog
   code and launchd restarts the daemon; a crash loop starts straight into safe mode. Exit codes
   distinguish lock refused, watchdog and invalid config, so a refused second instance is not
   restarted forever. `mayfly_heartbeat_timestamp_seconds` is served on the loopback metrics
   endpoint and the runbook documents an external dead-man alert.
3. `doctor` fails when the host may sleep; host wake is handled anyway (P22).

## Consequences

OPS-01 (plist, install, doctor), OPS-05 (watchdog, exit codes, heartbeat), RT-02 and RT-04
implement it; F40 pins it. A LaunchDaemon is a later ADR after a prototype.

## Alternatives considered

- `KeepAlive` only: misses hangs.
- A LaunchDaemon now: Keychain and GUI-session risks before a prototype.
