# 0091. The daemon's composition: one run per session, its configuration and identity

Date: 2026-11-12

## Status

Proposed

## Context

RT-02 wires both planes into `mayflyd --role all` (plan §3, §10). Five questions are left open
by the plan and the ADRs it builds on:

- **The host's identity.** Every stream, VM name and ownership proof names a `HostId`, and the
  image store records a `deployment_id` on every base (ADR 0087); the configuration has neither.
- **Where images come from.** `TartImageStore` needs one OCI repository, the pinned runner
  checksums and the token realm hosts (DP-07); RT-01's schema has none of them.
- **A reconnect's new epoch** (CP-04). Plan §3.2 mints an epoch per data-plane session, but
  `HostRun` mints once per run (`AlreadyMinted` after the first).
- **The first inventory.** Recovery waits on an inventory, and the data plane's observer lists
  only every `observe_every`. ADR 0034 makes directives events of the journal, with no outbox.
- **Where state lives by default.** RT-01's schema requires `state_dir` and `tart_home`; the
  owner asked for a quick start that needs no research (2026-11-12).

## Decision

1. **The host id is generated once and never replaced.** It is kept in `<state_dir>/host-id`
   (`0600`): `h` and 15 hex digits from the operating system's generator, written on the first
   start (synced, then the directory synced) and read on every later one. A file that exists
   and does not hold an id is refused. A missing file beside an existing journal is refused too:
   a new id would orphan everything the journal records. The same id is the image store's
   `deployment_id`, since one state directory is one host and one deployment.
2. **`[images]` is required by `mayflyd`.** The table (`repository`, `runners`, `realm_hosts`)
   is optional in the schema, so every existing file still validates, and `mayflyd` exits 78
   naming `images` without it. Signatures stay `digest_only` until SIG-01.
3. **A session that ends is a run that ends.** When the in-process link ends after recovery
   completed, the daemon starts a new run in the same process: `StartRun` (recording the previous
   run unclean), `MintEpoch` above it, a new handshake and a full recovery, with the data plane's
   ledger kept. `formal/link.qnt` (a reconnect enters a newer epoch) and `formal/recovery.qnt`
   (a restart) already model this, so no model changes. A link that ends before recovery
   completed stops the daemon (exit 1) instead of looping. An in-run reconnect would need
   `HostRun` to mint twice; LNK-01, where a data plane really disconnects, may revisit it.
4. **Start-up asks for an inventory outside the journal**, an explicit exception to ADR 0034's
   "directives are events": right after the welcome, the control plane sends `ReportInventory`
   straight on the link. It is a read with no effect on any VM, attempt or host, so it has
   nothing to recover, retry or fence: a lost request costs only the observer's next pass, and
   journaling it would add an event that no aggregate decides. Every directive with an effect
   still goes through the journal and the relay.

5. **Default places (decided by the owner, 2026-11-12: "the system should be usable and have
   reasonable defaults out of the box").** `state_dir` and `[provider] tart_home` are optional;
   without them `mayflyd` uses the platform's standard places for terminal mode, creates them
   `0700` and then runs the usual permission checks (ADR 0087):
   - macOS: the state in `~/Library/Application Support/mayfly`, `TART_HOME` at `<state>/tart`;
   - Linux: the state (journal, host id, base registry, shims) in `$XDG_STATE_HOME/mayfly`
     (`~/.local/state/mayfly`), `TART_HOME` in `$XDG_DATA_HOME/mayfly/tart`
     (`~/.local/share/mayfly/tart`), since VM disks and image bases are large user data. The
     epoch floor stays in `TART_HOME` (ADR 0026), so it lives beside the VMs it fences.

   A configured `state_dir` alone puts `TART_HOME` at `<state_dir>/tart`; the configuration
   overrides every path. Nothing goes in a cache directory, which the system may purge. With no
   absolute `HOME` there is no default, and `mayflyd` asks for `state_dir` (exit 78). Service mode
   (launchd or systemd) sets `/Library/Application Support/mayfly` or `/var/lib/mayfly` in its
   configuration (OPS-01). The socket's default (RT-03) is `$XDG_RUNTIME_DIR/mayfly.sock` on
   Linux and `<state>/mayfly.sock` on macOS; logs' (RT-04) are `~/Library/Logs/mayfly` and
   `$XDG_STATE_HOME/mayfly/logs`.

## Consequences

- RT-02 implements it (`mayfly-composition::{daemon, identity, paths}`). Pinned by
  `crates/mayfly-composition/tests/daemon.rs` (restart and link-loss tests, the refusals),
  `tests/config.rs` (`full.toml`, `defaults.toml`, `images_invalid.toml`), `identity`'s and
  `paths`' unit tests and `tests/daemon.rs::should_create_the_platform_places_private_when_the_config_names_none`.
- RT-03's `config validate` reports a file without `[images]` valid; `mayflyd` and `doctor`
  (OPS-01) are where it is required.

## Alternatives considered

- The host name as the host id: an operator may change it, and it may not fit the id's shape.
- `[images]` required by the schema: every existing configuration would stop validating.
- A reconnect inside one run by minting again: a `HostRun` change, model first, for a case the
  in-process link meets only on a bug.
- A journaled inventory request: an event with no aggregate to decide it, for a read.
- The `directories` crate for decision 5: it reads the process environment itself, so a test
  could only exercise it by mutating that environment; the rules are a dozen lines of pure code
  over an injected environment instead.
- `TART_HOME` inside the Linux state directory: simpler, but VM disks are large user data, which
  XDG puts in the data directory, and the floor needs no change either way.
