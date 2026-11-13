# 0091. The daemon's composition: one run per session, its configuration and identity

Date: 2026-11-12

## Status

Proposed. Extended 2026-11-12 (OPS-02) by the section "Image manifests and pinning",
proposed with it.

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
   configuration (OPS-01). The operator socket (RT-03, as built 2026-11-13) is
   `<state_dir>/mayfly.sock` when the configuration names `state_dir`, so one setting moves it
   with everything else and two daemons with their own state directories never share one; with
   no `state_dir`, it is `$XDG_RUNTIME_DIR/mayfly.sock` on Linux when that variable is absolute,
   else `<state>/mayfly.sock`, and `<state>/mayfly.sock` on macOS. A socket address holds at
   most 103 bytes of path; on macOS a default place longer than that (a long `HOME`) falls back
   to `$TMPDIR/mayfly-<uid>.sock`, the per-user private temporary directory, so the default
   still works (RT-03F, 2026-11-13). The fallback reads `TMPDIR`, so a CLI run without it finds
   the daemon only through `--socket`, and macOS may purge an unused file there after days (a
   limitation of this rare case). A configured `state_dir` is never moved, and one too long is
   refused with the fix. A live socket already at the path (another daemon answering) refuses
   the start; only a stale one of the same user is replaced. Logs' (RT-04) are
   `~/Library/Logs/mayfly` and `$XDG_STATE_HOME/mayfly/logs`.

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

## Image manifests and pinning (OPS-02, 2026-11-12)

Proposed with this record: the owner accepts these decisions with it.

### Context

Each pool names an image manifest (`image_manifest`, plan §11), and the control plane pins a
pool only to a digest whose `Image` stream is `Verified` (`doc/domain-model.md` §6.2, §6.6).
OPS-02 writes the manifest's schema, `mayfly image verify` and the path from a pool's manifest
to its pin under `mayflyd`. The documents leave open:

- **What "Verified" means.** The timeline (dm §2.1, steps 7 and 10) verifies an image before
  any host pulls it, and the data plane already verifies the content at the pull against the
  OCI manifest fetched by digest (ADR 0087 decision 7). A rejection is permanent for its digest
  (`ImageRejected`: never verified again).
- **Where a runner pin comes from.** DEC-01 puts the runner version and its SHA-256 in the
  manifest; decision 2 above put the pins in `[images.runners]`, optional in the schema, and
  the image store refuses a platform with no pin (HW-01X). The owner asked for reasonable defaults out of
  the box (2026-11-12).
- **What the journal records of a manifest.** KER-11 left the launcher hash and the required
  host to OPS-02 "as optional fields" of `ImageRegistered`'s manifest.
- **The repository.** The image store pulls from one repository per deployment
  (`[images] repository`, decision 2); a manifest names its image's own.

### Decisions

Numbered on from the decisions above.

6. **The manifest is a TOML file of schema 1**, read like the configuration (unknown keys
   refused, every problem named with its field and its fix): `schema`, `provider = "tart"`,
   `os`, `arch = "arm64"` (the host requirement of schema 1: Tart runs arm64 guests on Apple
   silicon), `source = "<repository>@sha256:<64 hex>"`, `revision`, `[runner] version sha256`,
   `[launcher] sha256`, `[capabilities] guest_agent = true`. A tag-only source, a tag beside the
   digest and any other digest form are refused (ADR 0039: never by tag); an image without the
   guest agent is refused (ADR 0038). A later requirement is a new key, which an older build
   refuses.
7. **Verified means the manifest passed this build's checks against this deployment**:
   `mayflyd` reads every pool's manifest at start and refuses to start (exit 78, each problem
   named under its pool) when one does not read, comes from another repository than
   `[images] repository`, names another guest than the pool's `guest_os`, pins a runner package
   otherwise than another pin of it, or describes a digest otherwise than another pool's
   manifest. Every run then registers each pool's image, records `RecordVerification{passed}`
   and pins the pool (`mayfly_control::images`). A problem of the deployment is a start-up
   refusal the operator fixes, never a permanent `ImageRejected`. The content is verified by the
   data plane at the pull: a drifted pull fails there (the placement `Failed`, the pool not
   admissible on that host); recording `ImageRejected` on a drift is P17's pending branch
   (planned, no task yet).
8. **A manifest's runner pin joins the data plane's pin table** for its guest's platform
   (`osx-arm64` or `linux-arm64`), so a manifest is enough to run its image and
   `[images.runners]` stays optional; a configured pin of the same package must agree.
   Manifests are operator-owned files with the configuration's trust, and their integrity now
   carries the runner checksum. **Open question for the owner:** whether `mayflyd` should refuse
   a manifest that is writable by group or others (today it is read whatever its mode, as the
   configuration is).
9. **The journal records `{os, runner_version, revision}`** (`ImageRegistered` v1, unchanged):
   what the control plane decides with. The rest stays in the file: the runner's checksum joins
   the data plane's pin table, which checks the package at the materialisation (ADR 0087
   decision 10); the architecture is checked at every start; the launcher's checksum is
   recorded and format-checked only, and comparing it with the image's launcher is planned (no
   task yet). Recording them in the journal would be a new event version that nothing reads.
10. **A refusal at run time leaves the pin as it was**: a digest registered with another
    manifest (`manifest_immutable`), a rejected or withdrawn image. It is logged at ERROR for
    the operator (`image_registration_refused`, `image_pin_refused`), and the run goes on. The
    levels differ on purpose from `ConfigurePool`'s refusal (WARNING, `pool_configuration_refused`):
    a refused pin leaves the pool on an image the operator did not ask for, which needs their
    attention, while a store fault in any of these steps is WARNING (`image_pin_failed` and the
    like), handled by the next run.

### Consequences

- OPS-02 implements it: `mayfly-composition::manifest`, `mayfly_control::images`,
  `mayfly-composition::daemon`'s `load` and `pin_images`, `mayfly image verify`. Pinned by
  `mayfly-composition`'s `tests/manifest.rs` (valid and invalid fixtures with golden reports,
  the deployment checks), `tests/daemon.rs::should_pin_each_pool_to_its_manifest_image_when_the_daemon_starts`
  and `should_refuse_to_start_when_a_pool_manifest_names_a_tag`, `mayfly-control`'s
  `tests/store/images.rs`, and `mayfly-cli`'s `tests/image_verify.rs`.
- Every configuration's pools now need a readable manifest under `mayflyd`; `config validate`
  (RT-03) still checks the path's shape only.
- The pin reads the image stream and refuses a withdrawn image, narrowing the window
  `withdraw_image` leaves; a pin that commits first, of a pool created after the withdrawal
  listed the pools, is not refused (OPS-02's card records the two-step withdrawal that would).

### Alternatives considered

- Verification as a boot of the image (a canary): needs a placement, which needs the pin; the
  data plane's materialisation already boots the base and checks the runner and the guest.
- `[images.runners]` required for every manifest's runner: the same checksum written twice.
- Recording the launcher hash and the host requirement in `ImageRegistered` v2: nothing in the
  control plane reads them.
- Several repositories per deployment: the image store and its directives name one; a protocol
  change for a case no operator has asked for.
