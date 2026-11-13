# 0091. The daemon's composition: one run per session, its configuration and identity

Date: 2026-11-12

## Status

Proposed. Extended 2026-11-12 (OPS-02) by the section "Image manifests and pinning",
proposed with it. Extended 2026-11-13 (RT-07) by the section "The GitHub side", whose
configuration keys and scale-set names the owner agreed (2026-11-13); proposed with it.

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

## The GitHub side (RT-07, 2026-11-13)

Proposed with this record. Decisions 11 and 12 are the owner's (2026-11-13).

### Context

GH-02 and GH-03 built the client, CP-05 the registration manager and the reaper, GH-04 the
listener and the job facts, and `mayflyd` built none of them. `[github]` names the App, its
installation and its key, but not where GitHub is: the client needs a REST base and a web base
(`AppConfig`), GHES lives elsewhere, and a test must point the daemon at `mayfly-fake-github`.
The listener needs each pool's scale-set name, runner group and labels, which no pool key gives.

### Decisions

Numbered on from the decisions above.

11. **`[github]` takes two optional keys, `api_url` and `web_url`** (owner, 2026-11-13). Absent,
    they are github.com's (`https://api.github.com`, `https://github.com`); GHES sets both
    (`https://<host>/api/v3`, `https://<host>`); a test sets `api_url` to the fake's loopback
    base. Each must be `https`, or plain `http` to a loopback host, with no query, fragment or
    user info (`mayfly_adapter_github::parse_base`, the rule of `credential_base_problem`): the
    App's credentials are sent there. A refused value is not repeated in the report. An older
    build refuses the keys like any unknown key. The decision test (`doc/philosophy.md` §8)
    holds: nothing is sent anywhere but where the operator points, and the default is the
    service the operator already chose by installing the App there (ADR 0036).
12. **No new pool key for the scale set** (owner, 2026-11-13): a pool's scale set is named after
    the pool, in the runner group `Default`, with the pool's name as its one label
    (`crate::github::spec_of`; ADR 0040: the name is the label workflows use).
13. **The GitHub side never holds a run back.** One client serves every pool's listener, the
    registration manager and the reaper. Each run starts, once its recovery reported: one
    listener per pool under a local lease (closed on the run's end, bounded at 10 s), the job
    facts and the registration manager as durable consumers on their own workers, the manager's
    pass once a minute, and the reaper's sweeps (at run start once every pool's scale set is
    journaled, daily, when a session opens after 15 minutes or more without one, and on
    `doctor`), at most one listing an hour whatever asks (P14), counted across the process's
    runs: each listing asked, answered or not, is kept beside the reaper's report and seeds the
    next run's manager, so a link that flaps never lists once per run. The manager's busy
    backoffs and online deadlines start afresh each run, on purpose (a new run waits anew). A
    removal a sweep asks for, `doctor`'s included, is journaled under the automation actor
    `registration_manager`. None of this is part of the ready line's promise, which names the
    host side's consumers (the attempt saga, the directive relay, the placement relay). The
    run's end drops a GitHub call in flight rather than wait out its deadline: every save is
    one unit, an unacked message comes again, and an unanswered mint is an unknown issue, never
    minted again (I19); a `handle` so dropped keeps its delivery claim until the lease lapses
    (about four minutes), so the next run's manager consumer may lag that long.
14. **The manager's handle outlasts a mint**: its worker's handle deadline is the call budget
    (two minutes, `Retry-After` waits included) and a minute more, under a lease a third
    longer (CP-05's note).
15. **The pass reads only what may still need it**: the new `open_registrations` projection (a
    row per registration until it is `Deregistered` or `Absent`) and the `attempts` rows in a
    phase whose retirement may still wait on its registration, instead of folding every
    registration stream ever written once a minute (CP-05's note). The sweep still reads every
    registration stream, at most once an hour.
16. **`doctor` asks the run for a sweep** over the operator socket (`reaper`, an additive request
    of API 1; an older daemon answers `unknown_command`, which `doctor` reports), waits at most
    15 s, and shows the last report: the runners of mayfly's name shape that no registration
    recorded, never removed (ADR 0029).

### Consequences

- RT-07 implements it: `mayfly-composition::github`, `config` (`api_url`, `web_url`),
  `daemon`, `operator` (`reaper`, the `doctor` check); `mayfly-control`'s
  `open_registrations` and the pass's read of it. Pinned by `tests/config.rs`
  (`should_accept_every_key_when_the_full_fixture_is_parsed`,
  `should_not_echo_a_refused_base_url_when_it_may_carry_a_credential`), `tests/daemon.rs`
  (`github::should_open_a_session_and_report_an_unrecognised_runner_when_github_serves_the_pool`,
  `placement::should_mint_once_and_deliver_the_config_to_the_guest_when_an_acquired_job_activates_an_attempt`,
  `github::should_list_once_when_a_second_run_starts_within_the_hour`),
  `tests/operator.rs::should_answer_the_last_sweep_when_doctor_asks_and_no_run_answers` and
  `mayfly-control`'s `tests/store/registration.rs`
  (`should_skip_a_settled_registration_when_its_attempt_needs_nothing`,
  `should_list_at_most_once_an_hour_when_a_new_manager_is_seeded`).
- Left open: recovery is still handed no GitHub view (every runner `unreachable`, RT-08); on
  github.com's direct flow nothing is granted until GH-08 and GH-10 classify assignments; the
  runner's removal on retirement is pinned at the component level only (nothing retires a
  listening attempt under `mayflyd` within a test's bound yet: DP-09 observes a runner's exit); the assigned-jobs gap and the
  link's lost configs have no gauge until RT-04's metrics.

### Alternatives considered

- One `server_url` key, the API base derived from it (the runner's own convention): one key
  fewer, but a loopback fake or a GHES with an unusual path needs a special case.
- A test-only override in the daemon's options: reaches neither an end-to-end run of the binary
  nor GHES.
- Pool keys `scale_set`, `runner_group` and `labels`: no operator has asked; addable later as
  optional keys.
- The reaper's first sweep at once: before the listeners journal the scale sets, our own
  runners would read as foreign.
