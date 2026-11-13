# 0031. Service modes and the internal watchdog

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09)
Amended 2026-11-12 (owner, RT-05): Ctrl-C in a terminal escalates, its first press a graceful
stop of the host's idle VMs (see the amendment below).

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

## Amended 2026-11-12 (owner): terminal mode's escalating Ctrl-C (RT-05a)

Context: decision 1 made Ctrl-C run P24, which leaves every VM running under its shim for the
next start (ADR 0037): right for a service manager's restart, wrong for the owner's primary
deployment, `mayflyd` run by hand from a terminal and stopped with Ctrl-C, where "stop" means
"stop working here" and VMs left running hold the machine's memory until the next start. The
owner decided (2026-11-12) how Ctrl-C stops; the planner's choices below are marked.

1. **The first Ctrl-C (SIGINT) is a graceful stop.** Admission closes and nothing more is
   acquired; idle and Prepared attempts are retired and their VMs stopped and deleted with
   their full ownership proof (ADR 0026), never a base and never a foreign or unrecorded VM;
   Busy attempts are waited for up to `[shutdown] busy_wait`; durable writes finish, the run's
   end is recorded, the state-dir lock is released and the daemon exits 0.
2. **The second Ctrl-C prints a warning** on stderr that the next one forces the stop.
3. **The third Ctrl-C is exactly SIGTERM**: P24, VMs left under their shims, the unsettled
   state recorded for the next start, within the 25 s loop bound and 5 s of runtime shutdown.
4. **SIGTERM stays P24**, directly and unchanged: a service manager's stop never waits.
5. **`[shutdown] busy_wait`** (a duration, default 30 minutes) bounds the graceful stop from
   the first Ctrl-C; when it runs out, the stop takes P24's path as the third Ctrl-C does.
   The key is optional; an older build that does not know it refuses the file as it refuses
   any unknown key (ADR 0046), loudly, rather than stop otherwise than configured.

Decision 1's "no flag changes the daemon's semantics" stands: there is no mode flag; the
signal chooses. Planner's choices (RT-05a):

- **Admission closes in memory, for the stopping run only.** The reconciler plans the host as
  draining (nothing reserved, started or granted there; prepared attempts retired `drain`;
  unstarted reservations released at once) without a drain on the host's stream, so a forced
  stop or a crash leaves nothing the next start must undo (the capacity inputs the stopping
  passes record are re-planned by the next run's passes), and an operator's own `host drain`
  is never cancelled by a stop. No event, field or protocol message is added: an older build
  reading the journal is unaffected.
- **Granted and listening runners wait**, as a busy one does (bounded by `busy_wait`, then
  P24): their runner is deregistered before its VM stops (P11), which needs GitHub; RT-05b
  adds them to the stop's retirements with their deregistration.
- **The stop is settled** when no lease is left on the host but quarantined ones: a lease is
  released only once its VM's deletion or loss is observed, and a quarantined VM is an
  operator's case, never deleted automatically (ADR 0025), so the stop leaves it recorded.
- **Safe mode has no graceful stop**: no destructive effect may run there (I29), so the first
  Ctrl-C is P24 at once, as is a graceful stop that meets a stored kind this build cannot
  decode. Before recovery completes, the stop waits for it (bounded by `busy_wait`).
- **`busy_wait` bounds the whole stop**, idle deletions included: a delete whose outcome stays
  unknown cannot hold the terminal past it.
- Listening and granted runners' retirement with their deregistration (P11) and the Busy wait
  over real jobs are RT-05b's, with GitHub.

Consequences: `formal/recovery.qnt` models the graceful stop (`ctrlC`, `stopRetireWith`,
`gracefulExit`, the invariant `gracefulStopSettles`, two negative controls) and
`formal/lifecycle.qnt` names its retirement and its cut by P24 in two scenarios; pinned by
`crates/mayfly-composition/tests/daemon.rs` (`placement::should_delete_the_idle_vms_and_exit_when_ctrl_c_is_pressed_once`,
`should_force_p24_when_ctrl_c_is_pressed_a_third_time_during_the_wait`,
`should_take_p24_when_the_busy_wait_runs_out`), `supervise::tests::should_escalate_to_p24_when_ctrl_c_is_pressed_three_times`
and `mayfly-control`'s `tests/store/reconciler.rs::should_retire_and_admit_nothing_on_its_host_when_the_run_stops`.
