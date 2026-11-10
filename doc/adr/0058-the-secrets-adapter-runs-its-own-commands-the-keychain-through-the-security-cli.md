# 0058. The secrets adapter runs its own commands; the Keychain through the security CLI

Date: 2026-11-10

## Status

Proposed. Amends D1 (ADR 0002) as plan §3.4 states it: one control-plane module may spawn.

## Context

ADR 0033 gives the `SecretProvider` an `exec:` scheme: the stdout of an operator command, argv
only, no shell, a deadline, bounded output. Running a command is process authority. D1, as
plan §3.4 enforces it, says the control plane names no process API (`std::process` only in the
Tart and guest adapters, composition and test tools), and foundation.md §2.3 grants it only to
the crates in `[purity] process_authority`. The `exec:` adapter lives in
`mayfly-adapter-secrets` (control plane), so either a control-plane module gains the authority
or the spawn moves to a crate that has it:

- the Tart and guest adapters are data plane; the control plane may not reach them (D1);
- composition is wiring only, and the runner a secret provider needs (no shell, no inherited
  environment, no stderr, output refused rather than truncated, executable checked) is the
  adapter's own logic, not wiring.

ADR 0033 also leaves the `keychain:` binding to SEC-02 (foundation.md §2.4: "macOS-only
Keychain binding chosen by its task with an ADR"). Two ways are open: a Security.framework
binding crate (`security-framework`, a safe wrapper over `unsafe` FFI and a tree of
`core-foundation` crates), or `/usr/bin/security find-generic-password -s <service> -a
<account> -w`, which ships with every macOS and prints the item's password on stdout.

ADR 0033 says an `env:` value is "never passed to a child". The variable stays in the daemon's
own environment (Rust 2024 makes removing it `unsafe`), so the promise holds only if no child
anywhere in the product inherits that environment.

The port is a native `async fn` trait (D8) and no blocking work may run on a runtime worker
(foundation.md §4.4 rule 3).

## Decision

1. **One module of the control plane holds process authority: `mayfly-adapter-secrets::command`.**
   Not the crate: the checker's `PROCESS_MODULES` grants the module alone, the crate's other
   modules stay under `ARCH-process-authority` (fixture `secrets-process-outside-command`), and
   `[purity] process_authority` is unchanged. The module spawns exactly two kinds of child: the
   absolute path of an `exec:` reference and `/usr/bin/security` for `keychain:`. The runner
   gives the child argv only (never `sh -c`), a cleared environment plus only what the caller
   passes, `/` as its working directory, a null stdin, a null stderr, its own process group,
   and a deadline. The whole group is killed at the deadline, as soon as stdout passes the
   port's limit (refused, never truncated), and when the call is dropped. The same module also
   holds the adapter's one blocking hand-off (`spawn_blocking`, by `SPAWN_MODULES`) for the
   file and executable checks.
2. **Every child in the product gets a cleared environment plus an explicit allowlist.** This
   is what keeps an `env:` secret out of every child (ADR 0033), so it binds the Tart and guest
   adapters and composition as much as this runner: `ARCH-child-environment` counts, per file of
   production code, `Command::new` against `.env_clear()` (fixture `spawn-without-env-clear`),
   and `mayfly-testkit-shared::child_env` holds the probe a data-plane contract reuses (the
   secrets contract's EX-04). For this runner the allowlist is `PATH`, `HOME`, `USER`,
   `LOGNAME`, `LANG` and `TMPDIR`, less every name an `env:` reference reads, which
   `UriSecretProvider::new` strips whatever it is given.
3. **The executable is checked like a secret file, a little wider.** The command, its directory
   and every directory above it must belong to root or the daemon's user and be writable by
   neither group nor others (an ancestor may be world-writable with the sticky bit, as `/tmp`
   is); the command and its directory are not followed through a symlink; an ancestor symlink is
   followed only when root or the daemon's user owns it (`/var` on macOS). The check is on the
   path, then the path is run: the gap between them is closed by the directories being writable
   only by root or the daemon's user.
4. **Stderr is discarded, not captured.** A failing secret command's stderr may carry the
   secret or the secret manager's own session token; a refusal reports the exit status only,
   and the operator runs the command by hand to see why.
5. **The Keychain is read through the `security` CLI**, by the same runner, with no new
   dependency and no FFI, given the same few variables as an `exec:` command. Exit 44
   (`errSecItemNotFound`) is "absent"; 36 and 51 (interaction not allowed, authorisation
   failed) are "locked". `security -w` ends the password with a line break of its own, which is
   dropped; then the port's one rule applies as to every source (one trailing line break of the
   value dropped), so an item whose value itself ends in a line break resolves without it,
   exactly as a file or a command output would. The provider is portable code over a program
   path, so its tests run on Linux over a fake `security` script; only the system constructor
   (`/usr/bin/security`) is `#[cfg(target_os = "macos")]`. On any other system a `keychain:`
   reference is refused as unsupported.
6. **The adapter uses Tokio** (`rt`, `process`, `io-util`, `time`) to await the child and its
   deadline and to hand the file checks and reads to the blocking pool, and `rustix` (`fs`,
   `process`) for the `O_NOFOLLOW` walk, the daemon's user id and the process-group kill. Both
   are allowlisted for this crate only.

## Consequences

The control plane's process authority is one module, reviewed as such; plan §3.4 names the
exception. `exec:` and `keychain:` share one bounded, shell-free runner, and the Keychain needs
no dependency of its own. A Keychain item whose password is binary is printed in hex by
`security -w` and resolves to that hex text: Keychain secrets are text. A Keychain prompt
cannot appear (no terminal, no GUI session for a daemon); a locked keychain is refused, and
anything that waits is cut by the deadline. An executable installed group-writable, or behind a
symlink, is refused: the operator points the reference at the real file.

`CLAUDE.md` names the process-authority crates; it is updated when this record is accepted.
SEC-02 implements it; DP-04, DP-05 and RT-02 follow rule 2; the real Keychain runs in the
hardware lane (D14).

## Alternatives considered

- The whole crate in `[purity] process_authority`: wider than the one runner needs.
- `security-framework`: no child process, but a dependency tree of FFI crates for one call,
  and a binding the hardware lane would be the only place to exercise.
- Spawning in composition behind a port: puts the adapter's refusal logic in the wiring crate.
- Capturing stderr with redaction: the runner cannot know every shape a secret takes.
- Removing `env:` variables from the daemon's environment after capture: `unsafe` in Rust 2024
  (`std::env::remove_var`), and still racy with a thread that spawns.
