# 0037. A per-VM shim owns each tart run process

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09)

## Context

A Tart VM lives inside its `tart run` process; there is no detach flag, and killing the process
kills the VM. `tart run` treats SIGINT as a hard stop (`tart stop` sends SIGINT, then SIGKILL
after its timeout) and does not handle SIGTERM. While it runs, it holds an `fcntl` lock on the
VM's `config.json`, which names its PID and is never stale. If `mayflyd` were the parent, a
daemon crash or upgrade would kill every VM, a busy job included, and a pipe to a dead daemon
would kill a VM by SIGPIPE. containerd solves the same problem with a per-container shim that
survives daemon restarts (`doc/domain-model.md` OD1).

## Decision

1. Each VM is started by a **shim**: the `mayfly vm-shim` subcommand of the service binary,
   started in its own session (`setsid`) and process group. The shim runs `tart run` as its
   child with stdio to bounded files in the state directory, never a pipe; it holds its own lock
   file for the VM and, when the child exits, writes `exit.json` (exit status, signal, times)
   atomically (temporary file, fsync, rename).
2. **On start the daemon re-adopts**: it reconciles the journal, `tart list`, each shim's lock and
   `exit.json`. A running VM whose shim is alive is adopted under the new epoch (and, for a
   Listening or Busy attempt, kept while GitHub shows its runner online); a stopped one is
   finalised from `exit.json`; anything else follows the recovery table.
3. **A VM is stopped only through `tart stop`** by name with proof; mayfly never sends SIGTERM or
   SIGKILL to `tart run` or a shim.

## Consequences

A daemon crash or upgrade leaves VMs running and adoptable, so "never delete Busy" holds across
restarts (F13). The shim is the only long-lived child, so process authority stays in
`mayfly-adapter-tart` and composition. DP-04 (shim, adoption inputs), KER-08 (supervisor gains
`ProcessLost` and shim states), CP-04 and KER-07 (recovery rows) implement it; HW-01 verifies
survival and `tart stop` semantics on real Tart.

## Alternatives considered

- Detached `tart run` with no shim: no exit status after a restart, no owner of the stdio files.
- A launchd job per VM: plist churn for every attempt.
- VMs die with the daemon: upgrades need a drain, and a crash loses the running job.
