# mayfly: task breakdown

This file cuts the build of [`plan.md`](plan.md) (product and architecture), [`foundation.md`](foundation.md)
(engineering rules) and [`verification.md`](verification.md) (test levels, formal models, fault lab)
into small tasks in dependency order. The three documents own *what*; a citation "plan §5.2",
"fnd §3" or "ver §4" points to a section of one of them. This file owns *order* and *size*. Each
task is one focused commit that a fresh agent can finish after reading only what the task names.

**Live gate:** the fallback gate (`cargo fmt --all --check && cargo clippy --workspace
--all-targets --locked -- -D warnings && cargo test --workspace --locked`) until FND-02 lands,
then `cargo xtask check` (and `cargo xtask quick` while iterating).

## Decisions

D1–D15 were the plan's first decisions; D*n* is ADR *n*+1 and stays a valid alias. From ADR 0017
on, the ADR number is the decision id and no new D-id is minted (ADR 0001, amended 2026-11-09);
rows below list every ADR that binds tasks. **Proposed** decisions are planner defaults the owner
may override; until then tasks follow them as written (ADR 0091 is Proposed; the owner accepted
ADRs 0003, 0004, 0006-0020, 0023, 0041 and 0042 on 2026-11-09). Only the owner flips a decision to
Accepted. The owner's answers to `doc/domain-model.md` §10 (OD1–OD20) and `doc/event-store.md`
§8.3 (E1–E9) are ADRs 0021 and 0024–0040; the answers on upgrades, distribution and the App's permissions are ADRs 0043–0045.

| D | Decision | Status |
|---|---|---|
| [D1](adr/0002-control-plane-and-data-plane-in-a-multi-crate-workspace.md) | The multi-crate workspace of plan §4, with the control plane / data plane split of plan §3, is the code structure. The split is enforced by `architecture.toml` edges, not by convention. | Accepted |
| [D2](adr/0003-platforms-macos-arm64-product-linux-buildable.md) | Platforms: the product targets `aarch64-apple-darwin`; every crate also builds and tests on Linux x86_64/arm64 (CI and agent sandbox). Platform code uses `#[cfg]` items, never `cfg!` branches. Windows is out of scope. | Accepted (owner, 2026-11-09) |
| [D3](adr/0004-port-xtask-from-site-preview.md) | `tools/xtask` is ported from `pzmi/site-preview` (same owner, MIT OR Apache-2.0) and adapted, not rewritten: architecture checker with negative fixtures, check catalog, `doctor` with `tools/versions.toml`, `adr`, `land`, action-pin check. Site-preview-specific stages (bench, docs, release specifics) are dropped until needed. | Accepted (owner, 2026-11-09) |
| [D4](adr/0005-integrate-on-main-one-signed-commit-per-task.md) | The whole plan is integrated directly on `main`: one DCO-signed commit per task, subject `<ID>: <summary>`, landed by the orchestrator with `cargo xtask land` after a green gate on the merged tree; implementing agents never push. ⛳ checkpoints and CI on `main` are the quality net. | Accepted |
| [D5](adr/0006-pinned-stable-toolchain-edition-2024-dual-licence.md) | Toolchain pinned to stable 1.97.0, edition 2024, resolver 3; `Cargo.lock` committed; CI uses `--locked`. License `MIT OR Apache-2.0`; no crates.io publishing. | Accepted (owner, 2026-11-09) |
| [D6](adr/0007-event-sourcing-and-cqrs-in-the-control-plane-only.md) | Event sourcing and CQRS live in the **control plane only** (plan §5). The data plane keeps no durable state and re-observes the provider on start (plan §3.1). | Accepted (owner, 2026-11-09) |
| [D7](adr/0008-sqlite-through-rusqlite-as-the-first-journal-adapter.md) | Persistence is behind the storage-neutral `Journal` port (ADR 0021: the stream-scoped event store design, no global order). SQLite through `rusqlite` (bundled) is the first adapter, with WAL, `synchronous=FULL` and forward-only raw-SQL migrations; a PostgreSQL adapter follows. A single writer thread is an implementation detail of the SQLite adapter, never a premise of the port; `sqlx` is rejected (compile-time DB, two SQLite stacks). | Accepted (owner, 2026-11-09); amended by 0021 |
| [D8](adr/0009-native-async-ports-used-through-generics.md) | Ports are native `async fn`/RPITIT traits used through generics; `async_trait` and `dyn` ports only where object safety is proven necessary (with a note in the ADR). | Accepted (owner, 2026-11-09) |
| [D9](adr/0010-serde-json-link-protocol-with-an-explicit-version.md) | The CP↔DP protocol is serde JSON with an explicit `protocol_version`; the in-process transport still encodes and decodes every message. Secrets cross only as `SecretHandle` (plan §3.2). `serde` derive in `mayfly-domain` is ADR 0017. | Accepted (owner, 2026-11-09) |
| [D10](adr/0011-model-first-with-quint-conformance.md) | **Model first from day one** (unlike site-preview's retrofit): the lifecycle, scheduling, recovery, scaleset and link Quint models precede the Rust code they govern; conformance replays ITF traces against production code (ver §6). | Accepted (owner, 2026-11-09) |
| [D11](adr/0012-independent-fault-lab-and-fakes.md) | The fault lab (`tools/faultlab`) imports **no** production crate: it drives the binaries, reads SQLite with its own SQL and fake-tart/fake-github state with its own parsers, and has its own checker (ver §8). `mayfly-fake-tart` and `mayfly-fake-github` likewise implement their wire behaviour independently. | Accepted (owner, 2026-11-09) |
| [D12](adr/0013-tart-through-its-cli-by-argv-guests-by-tart-exec-or-ssh.md) | Tart is driven only through its CLI by argv. Guests are reached by `tart exec` where the image supports it, else the system `ssh` binary with a pinned per-image host key; no Rust SSH library in v0.x. | Accepted (owner, 2026-11-09) |
| [D13](adr/0014-secrets-are-never-persisted-by-mayfly.md) | Secrets are never persisted by mayfly: `Secret<T>` with redacting `Debug`/`Display`, no `Serialize`; JIT configs live in memory and go to the guest launcher over stdin. A redaction test and the secret scan guard it. | Accepted (owner, 2026-11-09) |
| [D14](adr/0015-owner-gated-real-world-lanes.md) | Real-world lanes (real Tart on macOS, live GitHub scale set) are owner-gated: the owner runs `cargo xtask hw01` from a terminal on their Mac (`hardware.yml` optional) and uses a disposable GitHub repository/App. Agents deliver everything up to them. | Accepted (owner, 2026-11-09); amended by the owner, 2026-11-11 |
| [D15](adr/0016-agent-sandbox-and-fail-closed-tools.md) | Agent sandbox: Linux, no Docker, no macOS, no Tart. Quint (Node) and Apalache (JVM) are dev-only tools pinned in `tools/versions.toml`; a missing tool fails closed (never a skipped-but-green check). Actions are pinned by commit SHA. | Accepted (owner, 2026-11-09) |
| [ADR 0017](adr/0017-serde-derive-in-the-shared-kernel.md) | `mayfly-domain` depends on `serde` with `derive` and on no format crate; encoding stays in `mayfly-protocol`, the journal adapter and the golden-fixture harness. | Accepted (owner, 2026-11-09) |
| [ADR 0018](adr/0018-event-codec-and-aggregate-registry.md) | Events implement `DomainEvent`; an aggregate's events form one `event_set!` enum with a codec and a registry, so every kind has its schema version and golden fixture. | Accepted (owner, 2026-11-09) |
| [ADR 0019](adr/0019-plane-separation-over-dev-closures-and-policy-driven-planes.md) | Plane membership and the purity and independence roles are data in `architecture.toml` (`[planes]`, `[purity]`), pinned by a repository test, instead of dev-dependency closures. | Accepted (owner, 2026-11-09) |
| [ADR 0020](adr/0020-control-plane-time-and-the-journal-s-recorded-at.md) | The control plane's `Clock` port reads wall-clock milliseconds since the Unix epoch; every persisted instant is such a reading, and the journal records `recorded_at`. Amended 2026-11-10: the data plane reads directive deadlines in the control plane's clock by an offset to its monotonic clock taken at the handshake (`Welcome.cp_clock`), falling back to its wall clock at receipt without it. | Accepted (owner, 2026-11-09; amendment 2026-11-10) |
| [ADR 0021](adr/0021-a-storage-neutral-stream-scoped-event-store.md) | The `Journal` port follows the stream-scoped event store design (`doc/event-store.md`): no global order, readers enumerate streams and read each after a revision. Amends ADR 0008; amended by 0034. | Accepted |
| [ADR 0022](adr/0022-strategic-ddd-with-a-data-oriented-implementation.md) | DDD is strategic (contexts, language, consistency boundaries in `doc/domain-model.md`); code is data-oriented: plain data, free pure functions and tables. | Accepted |
| [ADR 0023](adr/0023-fake-tool-binaries-may-wait-in-real-time.md) | Fake tool binaries may wait in real time (DP-03). | Accepted (owner, 2026-11-09) |
| [ADR 0024](adr/0024-the-data-plane-owns-host-resource-accounting.md) | The data plane accounts for and enforces host resources (memory, disk, macOS-guest slots incl. foreign VMs), refuses directives that do not fit and reports capacity; the control plane allocates only within reported capacity (OD4). Amended by 0052. | Accepted |
| [ADR 0025](adr/0025-quarantine-keeps-evidence-and-unknown-resolves-only-by-observation.md) | Quarantine whenever ownership or an outcome cannot be proven; never auto-delete a quarantined VM; capacity loss accepted; `Unknown` resolves only by re-observation, no operator assertion (OD9, OD10). | Accepted |
| [ADR 0026](adr/0026-provider-instance-identity-in-the-ownership-proof-and-an-external-epoch-floor.md) | Ownership proof includes the provider instance id (random MAC per clone, re-observed before delete); external epoch floor in `TART_HOME` (OD6, OD7). Amended by 0053. | Accepted |
| [ADR 0027](adr/0027-trusted-only-trust-tier-in-v0-x.md) | Trusted-only tier in v0.x; untrusted offers declined before acquire; `allow_untrusted_prs` refused (OD3). Partly superseded by 0090: classify-before-acquire on GHES only. | Accepted |
| [ADR 0028](adr/0028-scale-sets-only-with-prepared-unregistered-vms.md) | Scale sets only, no classic mode; only prepared, unregistered VMs ahead of demand, no registered standby (OD18, OD8). Amended by 0048 (registration token as an auth-chain step only; accepted by the owner, 2026-11-10). | Accepted |
| [ADR 0029](adr/0029-registration-reaper-and-the-wire-behaviour-of-pause-drain-and-safe-mode.md) | Registration aggregate and reaper; `maxCapacity` and acquisition under pause, drain and safe mode (OD11, OD15). | Accepted |
| [ADR 0030](adr/0030-runner-baked-into-the-image-with-refresh-tooling.md) | Runner baked in, self-update off, breaker on expiry, `mayfly image refresh` (OD12). | Accepted |
| [ADR 0031](adr/0031-service-modes-and-the-internal-watchdog.md) | `mayflyd` in the foreground by hand or under launchd; internal watchdog plus `KeepAlive` (OD16, OD17). | Accepted |
| [ADR 0032](adr/0032-support-softnet-with-the-daemon-kept-non-root.md) | Support Softnet; privilege model by NET-01's ADR; the daemon stays non-root (OD19). | Accepted |
| [ADR 0033](adr/0033-secret-providers-selected-by-uri.md) | `SecretProvider` port, backends selected per secret by URI (`keychain:`, `file:`, `env:`, `exec:`) (OD20). | Accepted |
| [ADR 0034](adr/0034-event-store-choices-for-directives-receipts-epochs-snapshots-and-rebuild.md) | Directives as events (no outbox table), refusal receipts bound to a revision, epochs now, model before delivery, snapshots, online projection rebuild (E2, E3, E6–E9). | Accepted |
| [ADR 0035](adr/0035-postgresql-contract-tests-in-docker-and-knossos-as-the-store-lab-checker.md) | PostgreSQL contract tests in Docker everywhere, failing closed; Knossos checks the store lab (E4, E5). | Accepted |
| [ADR 0036](adr/0036-no-telemetry-and-a-local-diagnostics-bundle.md) | No telemetry ever; a local, redacted `mayfly diagnostics bundle` sent by hand (P2). | Accepted |
| [ADR 0037](adr/0037-a-per-vm-shim-owns-each-tart-run-process.md) | A per-VM shim (`mayfly vm-shim`) owns each `tart run`; re-adoption on start; stop only via `tart stop` (OD1). | Accepted |
| [ADR 0038](adr/0038-jit-delivery-through-tart-exec-with-a-checked-guest-identity.md) | JIT over `tart exec -i` stdin to the exact VM after the lock-PID and instance-id checks; GitHub confirms the runner online; no SSH (OD5). | Accepted |
| [ADR 0039](adr/0039-clones-only-from-local-service-owned-image-bases.md) | Never pull at job time; clones only from local service-owned bases; `mayfly image refresh <pool>` (OD13). | Accepted |
| [ADR 0040](adr/0040-one-scale-set-per-pool-at-organisation-scope.md) | One scale set per pool, organisation scope, never deleted automatically (OD14). Amended 2026-11-11 (owner, GH-00R): enterprise scope unsupported (no App auth there); several organisations next (GH-07). | Accepted |
| [ADR 0041](adr/0041-aggregatestore-one-write-path-intent-digests-and-refusals-bound-to-every-read.md) | `AggregateStore`'s write path; refusal receipts bound to every stream the decision read; `CommandBody` with a canonical intent digest; derived event ids; `ARCH-single-append`. Amends 0034 (E3). | Accepted (owner, 2026-11-09) |
| [ADR 0042](adr/0042-a-frame-of-an-older-session-is-answered-unknown-never-refused.md) | A frame of an older session is answered `Unknown`, never refused (amends plan §3.2's `stale_epoch`). | Accepted (owner, 2026-11-09) |
| [ADR 0043](adr/0043-upgrades-jump-forward-any-distance-rollback-is-one-release-deeper-rollback-restores-a-backup.md) | Forward upgrades of any distance (upgrade matrix from every release); rollback one release; deeper rollback restores the pre-migration backup (`mayfly restore`); older build refuses writes on newer streams; deep-rollback recovery quarantines what it cannot prove (OD2). | Accepted (owner, 2026-11-09) |
| [ADR 0044](adr/0044-homebrew-distributes-and-updates-mayfly-mayfly-uninstall-purge-removes-what-it-created.md) | Homebrew distributes and updates; no self-update; `brew uninstall` removes the program; `mayfly uninstall --purge` removes what mayfly created, with ownership proof (P1, P3). | Accepted (owner, 2026-11-09) |
| [ADR 0045](adr/0045-the-github-app-permission-list-is-a-published-commitment.md) | The minimal GitHub App permission list and the reason for each is published; widening needs docs and an ADR first; `doctor` warns on excess (P4; list to be verified by GH-00/HW-02). Amended 2026-11-12 (owner, GH-00S): the list, "Self-hosted runners: Read and write" and "Actions: Read", optionally "Actions: Write" and "Administration: Read". (`doc/github-scaleset.md` §13). | Accepted (owner, 2026-11-09) |
| [ADR 0048](adr/0048-our-own-scale-set-client-informed-by-reading-actions-scaleset.md) | mayfly writes its own idiomatic Rust Scale Set client, informed by reading `actions/scaleset` (MIT) and copying no code or tests; the protocol is specified in `doc/github-scaleset.md` and pinned by our own fixtures; the fake is built from that spec and GH-00; any copied upstream fragment ships in `THIRD-PARTY-NOTICES`. Amends 0028 (registration token as an auth-chain step only; accepted 2026-11-10). Amended 2026-11-11 (owner, PLAN-01, GH-00R): every fact carries its evidence strength; "until GH-00's capture" reads "until it is server-observed". | Accepted (owner, 2026-11-10) |
| [ADR 0052](adr/0052-no-defaults-for-unknown-facts-the-data-plane-fails-safe.md) | The data plane assumes no fact it does not have: an unknown VM size or guest, or a listing not read yet, makes the dimension unknown; what needs it is refused `capacity_unknown`; `CapacityReported` lists the unknown dimensions with their causes and nothing free; protocol 1.1, compatible. Amends 0024. | Accepted (owner, 2026-11-10) |
| [ADR 0053](adr/0053-destructive-data-plane-directives-are-idempotent-by-outcome.md) | `DeleteVm` is "ensure absent" (a complete re-observation without the target is `Done`), `StopVm` of a proven stopped VM is `Done`; a mismatching present target is still refused; an incomplete re-observation is `Unknown{listing_incomplete}`, never `Done`. Amends 0026; amended by 0066. | Accepted (owner, 2026-11-10) |
| [ADR 0066](adr/0066-a-vanished-vm-is-quarantined-as-a-desync.md) | A `StopVm` or `StartVm` whose recorded VM a complete listing shows absent is refused `target_absent` and the attempt quarantined `vm_vanished_externally` for review (a desync, never read as stopped or lost, not released by the inventory); status and the diagnostics bundle name it; a start refused `proof_mismatch` quarantines too; protocol 1.2, written `proof_mismatch` below. Amends 0053. Amended 2026-11-11 (DEC-07): a recorded VM absent from a complete listing, found by a stop, a start or the inventory, records `VmVanished` and closes the attempt (no quarantine); the `attempts` view's `desync`, the breaker counts it; quarantine only for a present, mismatched VM. | Accepted (owner, 2026-11-10); amended 2026-11-11 (owner) |
| [ADR 0064](adr/0064-github-client-crypto-and-tls-aws-lc-rs-and-rustls-no-jwt-crate.md) | The GitHub client's crypto and TLS: `aws-lc-rs` as the `rustls` provider and for RS256, `rustls-platform-verifier`, `reqwest` 0.13, the App JWT built by hand (no JWT crate). Amends foundation.md §2.4. | Accepted (owner, 2026-11-10) |
| [ADR 0069](adr/0069-the-capacity-carve-keeps-the-headroom-and-what-quarantines-owe.md) | The capacity carve: an open pool keeps its headroom and what its quarantines owe ahead of later pools (`EXACT`); per pool, `capacity_policy = "demand"` keeps only for the VMs it waits for (`DEMAND`), default `"exact"` until GH-00 answers whether `maxCapacity` caps `totalAssignedJobs`. Refines 0024. Amended 2026-11-11 (owner, GH-00R): public evidence says it caps it at 0; `demand` stays an explicit, experimental opt-in. | Accepted (owner, 2026-11-10) |
| [ADR 0076](adr/0076-quarantine-stops-deregister-first.md) | The automatic stop of a quarantined VM whose runner may be registered waits for P11's deregistration (I21): confirmed, it stops; busy, the VM runs until the runner exits; unanswered, it waits. Amends 0025. | Accepted (owner, 2026-11-10) |
| [ADR 0087](adr/0087-service-owned-image-bases-a-durable-record-a-booted-base-and-pull-verification.md) | Service-owned image bases: mayfly's own private `TART_HOME`; a durable, fail-closed registry of the bases it made, kept on the data plane as host evidence; the Reserved → … → Ready → Retiring state machine with recovery by observation; clone only from a revalidated Ready base, never delete a quarantined one; full verification from a raw manifest fetched by digest; signature policy recorded. Amends 0039. | Accepted (owner, 2026-11-11) |
| [ADR 0090](adr/0090-trust-on-github-com-every-assigned-job-s-run-looked-up-before-a-runner.md) | Trust on github.com, where jobs are assigned with no offer: any plan, public repositories allowed, no runner group required; every assigned job's repository must be configured and its workflow run is looked up ("Actions: Read"), and a runner starts only for a job classified trusted (the pool's `allowed_events`, a default list); the hard boundary is every image's in-guest job-started hook, which fails an untrusted job before any step; refusals are journaled and logged (`job_refused`); a refused job holds nothing and goes to a refusal sink (a disposable Linux VM whose hook fails it, `maxCapacity` pinned meanwhile) or, with `cancel_refused_runs`, is cancelled; `check_fork_pr_setting` is an option; GHES keeps 0027. Partly supersedes 0027. | Accepted (owner, 2026-11-11 and 2026-11-12) |
| [ADR 0091](adr/0091-the-daemon-s-composition-one-run-per-session-its-configuration-and-identity.md) | The daemon's composition (RT-02): the host id generated once and never replaced, `[images]` required by `mayflyd`, a link that ends is a run that ends (a new run mints the new epoch), the start-up `ReportInventory` sent outside the journal (an exception to ADR 0034), the platform's standard places when the configuration names none (owner, 2026-11-12). | Proposed |

Decided by the owner on 2026-11-09 after the rows above: **OD2** upgrade and rollback (ADR 0043),
**P1/P3** Homebrew and `mayfly uninstall --purge` (ADR 0044), **P4** the published App permission
list (ADR 0045). Decided on 2026-11-10: inspect `actions/scaleset`, never port it (ADR 0048,
DEC-03); at the KER-08 checkpoint, no defaults for unknown facts (ADR 0052), idempotent
destructive directives (ADR 0053) and ADR 0048's amendment of ADR 0028 accepted (DEC-04); a
vanished VM quarantined as a desync (ADR 0066), deadlines by the handshake's clock offset (ADR
0020's amendment) and the Apalache lane weekly only (DEC-05); ADR 0064 accepted, the event
store's `storeOnlyStep` kept at depth 4 (weekly), ADR 0069 accepted with a per-pool `DEMAND`
option (default `EXACT`) and quarantine stops deregistering first (ADR 0076) (DEC-06). Decided on 2026-11-11 (DEC-07):
ADR 0058 accepted (the secrets adapter's command runner); a vanished VM converges like a
controller's, its desync recorded and its attempt closed (ADR 0066's amendment). Decided on
2026-11-11 (PLAN-01): the scale-set spec is settled from public evidence first (GH-00R; read,
never port, ADR 0048), GH-00 shrinks to the live remainder recorded in HW-02's session, and the
real-Tart lane becomes one command the owner runs from a terminal (HW-01X, `cargo xtask hw01`;
`hardware.yml` optional); GH-00R and HW-01X record the dated amendments of ADRs 0048 and 0015.
Decided on 2026-11-11 (GH-00R): `capacity_policy = "demand"` stays, experimental, since public
evidence says `maxCapacity` 0 gets nothing assigned (ADR 0069 amended); organisation scope only,
enterprise unsupported, several organisations next (ADR 0040 amended, GH-07). Decided on
2026-11-11 and 2026-11-12 (GH-00R, ADR 0090): on github.com a runner starts only for a job whose
workflow run was looked up and classified trusted; any plan and public repositories work;
cancelling refused runs and the fork-PR setting check are options, default off. Decided on
2026-11-12 (GH-00S, ADR 0090 amended, ADR 0045 amended): every image's in-guest job-started hook
is the hard trust boundary (IMG-02); the allowed events are a per-pool default list; refused jobs
hold nothing and go to a refusal sink (IMG-03) or, optionally, are cancelled; the hold, its
release window and the operator release are withdrawn (GH-09); the App permission list is
accepted.
Still planner defaults: the remaining sub-items of **OD20** (socket
authorisation, audit). Owner items still pending: the branch ruleset (FND-04), the live remainder
of the scale-set spec (GH-00, in HW-02's session) and the real-Tart report (HW-01).

## Rules for every task

1. **Read first:** this file's Decisions, then the sections the task cites, then existing files it
   changes. Do not read the spec documents whole.
2. **Green after every task:** the live gate passes on the exact tree you commit.
3. **Commit:** exactly one DCO-signed commit per task, subject `<ID>: <summary>`, body says why and
   carries an `Impact:` line, trailer block last (`Signed-off-by`, `Co-Authored-By`,
   `Claude-Session`, written by hand). Implementing agents never push, tag or change remote settings.
4. **Tests:** `should_<outcome>_when_<condition>`, `// given` / `// when` / `// then`. No sleeps in
   deterministic tests (fake clock, `tokio::time::pause`, channels); no mocking frameworks; tempdirs
   and port 0. A test lives at the lowest level that can fail for the reason it exists (ver §2).
5. **No silencing:** no `#[ignore]`, no `#[allow]`/`#[expect]` without an `architecture.toml`
   exception citing an ADR, no production `todo!`/`unimplemented!`/`dbg!`. Never delete a failing
   test, weaken a modelled invariant or rewrite a golden fixture to get green.
6. **New crate or dependency:** `members` + `architecture.toml` in the same commit; third-party
   crates added with `cargo add` (latest stable, locked) and listed in the crate's external
   allowlist; named with its reason in the commit message.
7. **Model first:** a change to lifecycle, scheduling, recovery, the scale-set session or the link
   updates the Quint model, its scenarios and the conformance driver before the Rust code.
8. **Decisions:** a new or reversed architectural decision adds an ADR (`cargo xtask adr new`,
   with `--number N` when reserved), or amends or supersedes one, in the same commit, together
   with the tests and rules that pin it. An ADR is a record, not a rule: when a better option
   appears, propose the change on the record instead of following or ignoring the old one.
   Agents never flip Proposed to Accepted; an agent's amendment of an Accepted ADR is marked
   proposed until the owner accepts it.
9. **Scope:** do not edit `CLAUDE.md` or `.claude/` (maintained by the orchestrator); report a
   needed change instead.
10. **Owner-only criteria** start with `(owner)`: deliver everything up to them and report them as
    pending.
11. **Parallelism:** tasks run concurrently only in separate worktrees with disjoint file sets; the
    shared files (`Cargo.toml`, `Cargo.lock`, `architecture.toml`, `tools/xtask/`, the model
    registry) follow the orchestrator's serialisation table.

**Model key:** `opus` — design, security boundaries, concurrency, protocols, formal models, the
architecture checker. `sonnet` — well-specified implementation. `haiku` —
mechanical docs and config.

**Review checkpoints (⛳):** after a ⛳ task lands green, the orchestrator runs `/review` plus an
adversarial second review over the range since the previous `checkpoint-*` tag, lands the fixes,
tags `checkpoint-<ID>` locally and reports to the owner. Checkpoints: **FND-04** (gate + CI), **DDD-01** (domain model),
**KER-08** (kernel and models), **ES-04** (journal), **ES-09** (the store on two engines),
**E2E-01** (walking skeleton), **E2E-02**
(scale-set demand), **VER-04** (fault lab), **OPS-03** (release).

## Overview

| ID | Title | Depends on | Model | ⛳ |
|---|---|---|---|---|
| FND-01 | Workspace, toolchain, crate stubs, licences, repo hygiene | — | sonnet | |
| FND-02 | Port xtask core and architecture law from site-preview | FND-01 | opus | |
| FND-03 | ADRs for D1–D15 and the ADR index | FND-02 | haiku | |
| FND-04 | CI, security and supply-chain workflows | FND-02 | sonnet | ⛳ |
| FND-05 | nextest profiles, testkit poll helper, coverage stage | FND-04 | sonnet | |
| FND-06 | Formal toolchain: Quint and Apalache pins, `formal` stages | FND-04 | sonnet | |
| FND-07 | ADR parser: `Rejected` status and decider annotations | FND-02 | haiku | |
| ARCH-01 | Align the architecture policy with fnd §2–§3 | FND-04 | opus | |
| ARCH-02 | Close the checker bypasses of the second FND-04 review | ARCH-01 | opus | |
| ARCH-03 | Close the third FND-04 review's bypasses; checker threat model | ARCH-02 | opus | |
| ARCH-04 | Architecture rules behind the event shape tracer; KER-03G leftovers | ARCH-03, KER-03 | opus | |
| KER-01 | Shared kernel: ids, VM names, generations, epochs, fences, ownership proof | FND-02 | sonnet | |
| DDD-01 | Event storming and domain model: contexts, language, aggregates | KER-01 | opus | ⛳ |
| DEC-01 | Record the owner's decisions as ADRs and apply them to the documents | DDD-01 | opus | |
| DEC-02 | Record the owner's upgrade, distribution and ADR decisions (ADRs 0043-0045, batch acceptance) | DEC-01 | opus | |
| DEC-03 | Specify the Scale Set protocol (`doc/github-scaleset.md`); ADR 0048, our own client | DEC-02 | opus | |
| KER-02 | CP↔DP protocol messages, versions, SecretHandle, link ports | KER-01, FND-06, DEC-01 | opus | |
| KER-03 | Event-sourcing kernel: aggregate, event registry, goldens, determinism | KER-01 | opus | |
| KER-04 | Lifecycle model and `Attempt` aggregate | KER-03, FND-06, DEC-01 | opus | |
| KER-05 | Scheduling model, `Host`/`Pool` aggregates, capacity policy | KER-03, FND-06, DEC-01 | opus | |
| KER-06 | Scale-set model, `ScaleSet` aggregate, demand, trust | KER-03, FND-06, DEC-01 | opus | |
| KER-09 | Registration model and `RunnerRegistration` aggregate | KER-03, FND-06, DEC-01 | opus | |
| KER-07 | Recovery model and decision table | KER-04, KER-05, KER-09, DEC-01 | opus | |
| KER-10 | Operations aggregates: `HostRun`, `RecoveryPlan`, `QuarantineCase`, effective mode | KER-03, KER-07 | opus | |
| KER-10F | Review fixes: per-stream plan heads, fenced plan epoch, plan exit restarts, crash loop at mint, resolution proof | KER-10 | opus | |
| KER-10G | Review fixes: typed plan reads, current-revision plan units, plan exit only while recovering | KER-10F | opus | |
| KER-11 | Image catalog: `Image`, `ImagePlacement` | KER-03, KER-02 | sonnet | |
| KER-08 | Data-plane domain: executor, supervisor, resource ledger, fencing | KER-02, DEC-01 | opus | ⛳ |
| DEC-04 | Record and implement the KER-08 owner decisions (ADRs 0052, 0053; 0028 amended) | KER-08 | opus | |
| DEC-05 | Record and implement the owner decisions of 2026-11-10: ADR 0066, ADR 0020's amendment, a weekly formal lane | DEC-04, DP-01 | opus | |
| DEC-06 | Record and implement the owner decisions of 2026-11-10 (round 7): ADRs 0064, 0069 with `DEMAND`, 0076; store-only depth 4 | DEC-05, KER-05, KER-04, KER-09 | opus | |
| DEC-07 | Record and implement the owner decisions of 2026-11-11: ADR 0058 accepted; a vanished VM converges (ADR 0066 amended) | DEC-06, ES-04 | opus | |
| ES-01 | Storage-neutral journal port, memory journal, journal contract (landed) | KER-03 | opus | |
| ES-01F | Align the landed journal port with the owner's decisions and its review | ES-01, DEC-01 | opus | |
| ES-02 | SQLite journal adapter: schema, triggers, classification, five modes | ES-01F | opus | |
| ES-03 | `AggregateStore`: single write path, receipts, epochs, retry, reactors | ES-01F | opus | |
| ES-03R | Remaining store rules: `ARCH-no-global-order`, `ARCH-journal-sql`, `ARCH-migration-frozen`, `fixtures.lock` | ES-02, ES-03 | opus | |
| ES-04 | Projections: stream-local inline specs, generations, rebuild, first read models | ES-02, ES-03 | sonnet | ⛳ |
| ES-06 | Event-store Quint model and scenarios | FND-06, ES-01F | opus | |
| ES-05 | Durable delivery ledger, worker and `catch_up`; the directive relay | ES-02, ES-03, ES-06 | opus | |
| ES-07 | Event-store conformance driver over memory and SQLite | ES-06, ES-05, VER-01 | opus | |
| ES-08 | Snapshots: port, memory and SQLite, worker, snapshot + tail load | ES-03, ES-07 | opus | |
| ES-09 | PostgreSQL adapter in Docker, schema parity, `contract-postgres` stage and CI job | ES-04, ES-05, ES-08 | opus | ⛳ |
| ES-10 | Store lab: worker, store workloads, Knossos checker, mutants | VER-03, ES-05, ES-09 | opus | |
| ES-11 | Compatibility harness N → N+1 → N and upgrade matrix | OPS-03, ES-09 | sonnet | |
| ES-12 | Online projection rebuild: shadow generations, dual writes, cutover fence | ES-04, ES-05, ES-07 | opus | |
| CP-01 | Commands and queries (CQRS rule live), operator use cases | ES-04, KER-05 | sonnet | |
| CP-02 | Attempt saga and directive relay as durable consumers | ES-05, KER-04, KER-02 | opus | |
| CP-03 | Reconciler: desired vs reported capacity, admission, retirement | CP-02, KER-07 | opus | |
| CP-04 | Observation inbox and startup recovery / safe mode | CP-03, KER-10 | opus | |
| CP-04B | Our non-VM use in the capacity report | CP-04, DP-07 | sonnet | |
| CP-04C | Hold an attempt's cleanup after adoption | CP-04 | opus | |
| CP-05 | Registration manager and reaper | KER-09, GH-03, CP-02 | opus | |
| CP-06 | Breaker, quarantine and Unknown-escalation policies | KER-10, CP-03 | sonnet | |
| CP-07 | Image placement manager | KER-11, DP-07 | sonnet | |
| CP-08 | Terms policy P4: a pool's terms applied on every host | CP-07, RT-02 | sonnet | |
| DP-08 | Image listing on the link: resolve `Unknown` placements after a restart | CP-07, VER-09 | opus | |
| DP-01 | Data-plane executor and observer, resource ledger, fake provider | KER-08 | opus | |
| DP-02 | In-process link transport and link contract suite | KER-02 | sonnet | |
| DP-03 | `mayfly-fake-tart` binary with fault knobs (landed) | FND-02 | sonnet | |
| DP-04 | Process runner, per-VM shim and Tart adapter, VmProvider contract suite | DP-01, DP-03 | opus | |
| DP-05 | Guest transport (`tart exec`) and runner launcher | DP-04 | opus | |
| DP-06 | Host resource probes | DP-01 | sonnet | |
| DP-07 | Image cache directives and `ImageStore` port | DP-04, KER-11 | opus | |
| SIG-01 | Image signature verification for pools that require it | DP-07, SEC-02 | opus | |
| NET-01 | Softnet privilege model: design, ADR, `doctor` checks | DP-04 | opus | |
| RT-01 | Config schema, validation, state-dir lock | KER-01 | sonnet | |
| SEC-02 | `SecretProvider` port and backends by URI | RT-01 | opus | |
| RT-02 | Composition: wiring, supervision tree, startup, `mayflyd` | CP-04, DP-02, DP-04, DP-06, ES-05, RT-01, SEC-02 | opus | |
| RT-03 | Operator socket API and `mayfly` CLI | RT-02 | sonnet | |
| RT-04 | Observability: tracing JSON, metrics, redaction tests | RT-02 | sonnet | |
| RT-05 | Terminal mode: a clean stop on Ctrl-C | RT-02, CP-05 | opus | |
| RT-06 | Measured host sizing | RT-02, DP-06, DP-07 | opus | |
| E2E-01 | Walking skeleton: static demand, fake-tart, one job end to end | RT-03, DP-05 | opus | ⛳ |
| GH-00R | Settle the scale-set spec from public evidence | DEC-03 | opus | |
| GH-01 | `mayfly-fake-github`: Scale Set and REST fake server | FND-02, GH-00R, DEC-03 | opus | |
| GH-02 | GitHub App auth, HTTP client, rate limits | RT-01, SEC-02, DEC-03 | sonnet | |
| GH-03 | Scale Set client and recorded-fixture contract | GH-01, GH-02 | opus | |
| GH-02H | GitHub transport hardening: service and queue host rule, HTTP/2 health, JIT throttling | GH-02 | sonnet | |
| KER-06G | `ScaleSet` aggregate follows GH-00R's model: assigned-job classification, numbering restart, scale set re-created | KER-06, GH-00R | opus | |
| GH-04 | Listener use case: inbox, statistics demand, acquire policy, ack after commit | GH-03, KER-06G, CP-04 | opus | |
| GH-10 | Workflow-run lookup of assigned jobs and the cancel of refused runs | GH-02, KER-06G | sonnet | |
| GH-08 | Trust on github.com: the trust config, the start-up checks, refusals wired | GH-03, GH-10, KER-06G, RT-01 | opus | |
| GH-09 | (Withdrawn by GH-00S) Operator release of a refusal and the release window | — | — | |
| E2E-02 | Scale-set end to end, 100-job backlog on 2 slots | GH-04, GH-08, CP-05, E2E-01 | sonnet | ⛳ |
| GH-07 | Several organisations: one scale set per pool per organisation, one App | E2E-02, GH-08 | opus | |
| LNK-01 | UDS transport and split control/data roles | DP-02, RT-02 | sonnet | |
| VER-01 | Conformance crate: ITF replay, lifecycle driver | KER-04, FND-06 | opus | |
| VER-02 | Conformance drivers: scheduling, recovery, scaleset, link, registration | VER-01, CP-04, GH-04, KER-06G, LNK-01, KER-09 | opus | |
| VER-03 | Fault lab: controller, history, verdicts, guardrails | E2E-01 | opus | |
| VER-04 | Fault lab: nemesis, checker, core scenarios (ver §8.4) | VER-03, E2E-02, LNK-01 | opus | ⛳ |
| VER-05 | Curated mutants (negative controls) and mutation lane | VER-02, VER-04 | sonnet | |
| VER-06 | Fuzz targets and nightly fuzz lane | DP-04, GH-03, RT-01 | sonnet | |
| VER-07 | Apalache invariant verification lane | KER-07, KER-06, KER-09 | sonnet | |
| VER-08 | Conformance driver for the image store model | VER-01, DP-07 | opus | |
| VER-09 | Local OCI registry for daemon tests | CP-08 | sonnet | |
| SEC-01 | Threat model and secret-handling audit | RT-04, DP-05 | opus | |
| OPS-01 | launchd service and foreground mode, install/uninstall, real `doctor` checks | RT-03 | sonnet | |
| OPS-02 | Image manifest, `mayfly image verify`, digest pinning | RT-01 | sonnet | |
| IMG-02 | In-guest job-started hook: the hard trust boundary (ADR 0090) | DP-05, GH-08 | opus | |
| IMG-03 | Refusal sink: a disposable Linux runner VM whose hook fails its job | IMG-02, GH-10, KER-06G | opus | |
| OPS-03 | Release workflow: macOS binaries, checksums, provenance, SBOM | FND-04, OPS-01 | sonnet | ⛳ |
| OPS-04 | Runbooks and operator docs | OPS-01, VER-04 | haiku | |
| OPS-05 | Watchdog, heartbeat metric, distinct exit codes | RT-02, RT-04 | sonnet | |
| OPS-06 | `mayfly image refresh <pool>` and runner-age warnings | CP-07, DP-05, OPS-02 | sonnet | |
| OPS-07 | `mayfly diagnostics bundle` | RT-03, RT-04 | sonnet | |
| OPS-08 | `mayfly restore <backup>` and `mayfly uninstall --purge` | RT-03, OPS-01, CP-07 | opus | |
| TART-01 | Tart 2.28.0 → 2.41.1: re-check the CLI, fake-tart parity | DP-07 | opus | |
| DEP-01 | Dependency and tool refresh | — | sonnet | |
| HW-01X | `cargo xtask hw01`: real-Tart checks from a terminal | DP-05, DP-07, SEC-02 | opus | |
| HW-01 | (owner) Run `cargo xtask hw01` on the owner's Mac and send the report | HW-01X | sonnet | |
| GH-00 | (owner) Live remainder of the scale-set spec, folded into HW-02 | GH-00R, GH-03 | sonnet | |
| HW-02 | (owner) Live GitHub scale set on a disposable organisation | GH-04, HW-01, GH-00 | sonnet | |
| HW-03 | (owner) Canary next to Tartelet, then cutover | E2E-02, HW-02, OPS-03 | sonnet | |

## Tasks

### Phase 0 — foundation

#### FND-01 Workspace, toolchain, crate stubs, licences, repo hygiene
Cites: plan §4, fnd §2, D1, D2, D5. (Landed; `rust-toolchain.toml` was already present.)
Create the Cargo workspace with every crate of plan §4 as a stub (`lib.rs` with a crate doc
comment stating its role and plane; `mayfly-cli` with two `[[bin]]`s `mayflyd` and `mayfly` that
print the version; `mayfly-fake-tart` with a `tart` binary stub). Copy `[workspace.lints]`,
`[workspace.package]` shape, `rust-toolchain.toml`, `deny.toml`, `.cargo/config.toml` (xtask alias)
and `.gitignore` (`/target`, `/.work/`) from site-preview, adapted. README (what, status, design
links), CHANGELOG, SECURITY, CONTRIBUTING (DCO), CODE_OF_CONDUCT.
Accept: fallback gate green; `cargo metadata` lists all crates; `publish = false` everywhere;
`unsafe_code = "forbid"`; edition 2024, resolver 3; no third-party dependency yet.

#### FND-02 Port xtask core and architecture law from site-preview
Cites: fnd §2–§3, §9, D3, D11.
Port `tools/xtask` from [site-preview](https://github.com/pzmi/site-preview) `tools/xtask` (and its tests and fixtures): cli,
process, tools/doctor + `tools/versions.toml`, catalog + `check`/`quick`/`list-checks`,
architecture (graph, policy, external allowlist, inventory, semantic rules for purity, hygiene and
CQRS adapted to mayfly's crate names), adr, land, action pins, secret scan. Drop bench, docs,
release, coverage, mutation, fuzz, conformance, formal for now (later tasks bring their own
stages). Write mayfly's `architecture.toml`: production / test_only / tool_only crates, edges of
plan §4, **CP↔DP separation**, pure crates (`mayfly-domain`, `mayfly-protocol`, `*-domain`),
`std::process` allowed only in `mayfly-adapter-tart`, `mayfly-composition`
and test tools, faultlab/fake binaries importing no production crate. Negative fixtures: a
control crate depending on a data crate, the data plane naming a journal type, a domain crate using
`std::fs`, faultlab importing `mayfly-control-domain`, a test without `should_` naming.
Accept: `cargo xtask check` and `quick` green; `cargo xtask architecture` fails each negative
fixture with a named rule; `cargo xtask doctor` reports pinned tools; `cargo xtask adr new` and
`land --no-push` work on a scratch repo test; CLAUDE.md commands all exist.

#### FND-03 ADRs for D1–D15 and the ADR index
Cites: this file's Decisions. Create ADR 0001 (record decisions) and 0002–0016 for D1–D15 with
`cargo xtask adr new`, Nygard format, Status as in the table. Accept: `cargo xtask adr index`
clean; every D row links its ADR.

#### FND-04 ⛳ CI, security and supply-chain workflows
Cites: fnd §7, ver §10, D2, D15.
`ci.yml` (push to `main`, PR, manual): static (fmt, clippy, architecture, action pins, deny), Linux
tests (nextest), macOS arm64 build + tests (`macos-latest`), `cargo check --target
aarch64-apple-darwin` from Linux. `security.yml` (gitleaks, cargo-deny advisories, scheduled),
`codeql.yml`, `scorecard.yml`, `.github/dependabot.yml` (cargo, actions, npm in `formal/`).
`doc/required-checks.md`. Every action pinned by SHA (the pin check enforces it).
Accept: all workflows green on `main`; actionlint clean; (owner) branch ruleset requiring the
listed checks, if the owner wants one (pending: the owner will add it later).

#### FND-05 nextest profiles, testkit poll helper, coverage stage
Cites: ver §2, §10, ADR 0019. `.config/nextest.toml` (default, ci, serial profiles), the `poll`
helper (the only place a test may wait on a real-OS condition, bounded, with a reason) in the
plane-neutral shared test kit `mayfly-testkit-shared` (test_only, plane `shared`, no workspace
edge), which this task creates (ADR 0019: data-plane tests cannot use the control plane's
`mayfly-testkit`; `ARCH-test-sleep`'s selector moves with it and `POLL_HELPER_REQUIRED` is on),
coverage stage
with `cargo-llvm-cov` and `coverage-floor.toml` (start 85% overall, 90% changed lines, ratcheted).
Accept: `check` runs nextest; a sleeping test fails the hygiene rule; coverage stage in CI.

#### FND-06 Formal toolchain
Cites: ver §6, D10, D15. `formal/package.json` + lockfile pinning Quint; Apalache pinned with
SHA-256 in `tools/versions.toml`; xtask stages `formal-typecheck` (all `.qnt`), `formal-test`
(Quint tests), the `MODELS` registry (empty), `formal.yml` workflow. A trivial `formal/smoke.qnt`.
Accept: stages fail closed when Quint is missing; CI runs them.

#### FND-07 ADR parser: `Rejected` status and decider annotations
Cites: ADR 0001 (amended 2026-11-09), `doc/adr/README.md`, fnd §10.3. `tools/xtask/src/adr_files.rs`
`parse_status` accepts `Rejected` (with the same annotation rules as the other kinds) and the
index shows it; the template's status placeholder lists it (`Proposed | Accepted | Rejected |
Superseded by NNNN`, kept in step with `TEMPLATE_STATUS`); fixtures for `Rejected (owner,
YYYY-MM-DD)`, `Accepted (owner, …); amended …` and a bad status.
Accept: `cargo xtask adr index` clean; the new fixtures pass; a misspelt status still fails.
Landed in the ARCH-04 commit (a small change beside the ADR 0018 amendment): `StatusKind::Rejected`,
`TEMPLATE_STATUS` and `doc/adr/template.md` list `Rejected`, which already ends in the optional
`## Alternatives considered`; `Accepted (owner, …)` already parsed as annotation.

#### ARCH-01 Align the architecture policy with fnd §2–§3
Cites: fnd §2.2, §3.1, §3.3. FND-02 ported site-preview's checker with the edges of the FND-02 card;
foundation.md, written in parallel, is stricter. Bring code and document into one agreement:
(1) edges per fnd §2.2 (`mayfly-data-domain` → `mayfly-protocol`; CP adapters reach only
`mayfly-domain`, `mayfly-control-domain` where needed and `mayfly-control`; DP adapters likewise on
the data side; conformance may drive the Tart adapter); (2) either data-driven `[planes]` and
`[purity]` tables in `architecture.toml` as fnd §3.1 describes, or amend fnd §3.1 to the
code-named design, whichever keeps the checker simpler (say which, and why, in the commit);
(3) the rules fnd §3.3 assigns to FND-02 that are missing: `ARCH-anyhow-in-library`,
`ARCH-unbounded-channel`, `ARCH-detached-spawn`, `ARCH-secret-serialize`, each with a negative
fixture; (4) the fault lab's package name (`mayfly-faultlab`) in fnd.
Accept: `cargo xtask check` green; each new rule fails its fixture; fnd §2–§3 describe the code exactly.

#### ARCH-03 Close the third FND-04 review's bypasses; checker threat model
Cites: fnd §2.3, §3.6, ADR 0019 (2026-11-09 amendment). Landed: lint levels through macro
metavariables refused (ARCH-lint-suppression), build-steering cargo config and environment refused
(ARCH-lint-config, xtask preflight), ARCH-nightly-features, wider pure-crate clippy layer
(`disallowed-macros`, Unix sockets, DNS), CI lockstep refuses failure-swallowing steps and YAML
merge keys, non-literal include paths refused, and fnd §3.6's threat model: the checker defends
against honest mistakes, shortcuts and drift, not a determined insider; review is the backstop.
Accept: each bypass has a negative fixture or test; `cargo xtask check` green.

#### ARCH-02 Close the checker bypasses of the second FND-04 review
Cites: fnd §2–§3, ADR 0019. The adversarial re-review of ARCH-01 bypassed the checker with a renamed
`include!`, `include!` inside macros, clock and fs reads through aliases, `Self` and associated
types, cross-crate `pub type` aliases, exported macros and declarations of the other plane's
vocabulary. Landed: ARCH-include-macro, ARCH-pure-clippy-config (semantic `clippy.toml` layer per
pure crate), ARCH-macro-export, declaration checks in ARCH-plane-vocabulary, YAML-parsed CI
lockstep, the xtask checkout preflight, shared-crate dev edges (ADR 0019 amended).
Accept: each bypass has a negative fixture failing for its rule; `cargo xtask check` green.

#### ARCH-04 Architecture rules behind the event shape tracer; KER-03G leftovers
Cites: ADR 0018 (amended 2026-11-09, ARCH-04), fnd §3.3. The tracer passes, by design, what serde
does not show it: an enum coded as a string or integer whose conversion accepts any value, a
`#[serde(other)]` catch-all, an alias. Landed: `ARCH-event-payload-serde` (`mayfly-control-domain`
and `mayfly-domain` production code writes no `#[serde(..)]` with `try_from`, `from`, `into`, `with`,
`deserialize_with`, `serialize_with`, `alias`, `other`, `untagged`, `tag`, `content`, `flatten`
or unparsable arguments, on containers, fields or variants, through `cfg_attr` and in macro
tokens, and no hand-written `Serialize`/`Deserialize`; a `mayfly-domain` struct may convert, an
enum may not; fixtures `event-payload-try-from-enum`,
`event-payload-alias-field`, `event-payload-flatten`); `ARCH-sample-coverage` (every
`mayfly-domain` type decoded through a non-integer container `try_from`, macro-declared ones
resolved through their invocations, implements `Sample` and is in `shape.rs`'s `texts()`; fixture
`validated-type-without-sample`); `ARCH-event-set-macro` (`impl EventSet for` only in the body
of `macro_rules! event_set` in `src/event.rs`, or in test code; fixture `event-set-by-hand`).
KER-03G leftovers: the field and variant names of the tracer's container identity pinned by a
test (same name, length and visitor, different field list); `DirectiveId`, `EventId` and
`ObservationId` in the value-type leaf test; the catch-all limitation in ADR 0018 and the shape
module doc. FND-07 rides along.
Accept: each rule fails its fixture and passes the workspace; the identity test fails when the
names are dropped from `Identity`; `cargo xtask check` green.

### Phase 1 — kernel (pure crates, models first)

#### KER-01 Shared kernel
Cites: plan §4, §8.4. `mayfly-domain`: `PoolName` and `HostId` restricted to `[a-z0-9]{1,16}` so the name separators are unambiguous, `AttemptId`, `HostId`, `PoolName`, `DirectiveId`,
`ObservationId`, `CommandId`, `Generation`, `Epoch`, `Fence`, `OwnedVmName` (format and strict
parse of `mf-<pool>-<host>-s<slot>-g<gen>-<short>`; rejects anything else), `ImageDigest`,
`Resources`, `Millis`/`Deadline` value types (no clock), error types. No serde in the domain unless
the ADR for D9 allows `serde` derive here (decide and record).
Accept: proptest round-trip format/parse; rejection table for malformed names; foreign names never
parse as owned.
DEC-01 adds: `OwnershipProof {attempt_id, host_id, run_epoch, slot, generation, vm_name,
provider_instance_id}`, `ProviderInstanceId` (a MAC for Tart, ADR 0026), `TrustTier` (only
`Trusted` accepted in v0.x, ADR 0027), `GuestOs`, `ImageRevision`, `SlotGeneration` (rename of
`Generation`), the image-base name codec (`mfb-<digest12>-r<runner>`; never parses as owned),
`RunEpoch` semantics (dm §5.3), `Resources` per dimension (memory, disk, macOS slot).

#### DDD-01 ⛳ Event storming and domain model
Cites: plan §2–§10, ver §6.1. Domain-driven design pass before any real aggregate exists. Big
Picture and Process Level event storming from four perspectives (GitHub demand, VM fleet, SRE/ops,
security), merged into `doc/domain-model.md`: event timeline with pivotal events and swimlanes,
bounded contexts and context map (upstream/downstream, anti-corruption layers for GitHub and the VM
provider, published language for the CP↔DP protocol), ubiquitous language glossary, aggregates with
their invariants (mapped to I01–I16), commands, policies (→ process managers), read models
(→ projections), external systems, and hot spots. Proposes the changes to plan.md and to KER-02..08
it implies; the orchestrator applies them after the ⛳ owner review.
Accept: every KER-04..08 aggregate and every policy traces to the model; (owner) hot spots decided.
(Landed: `doc/domain-model.md`; the owner reviewed it on 2026-11-09, applied by DEC-01. The ⛳ checkpoint is satisfied by the owner's decisions: every hot spot is decided and recorded as ADRs 0024-0045 by DEC-01 and DEC-02.)

#### DEC-01 Record the owner's decisions as ADRs and apply them
Cites: dm §10–§11, event-store §8, philosophy, ADR 0001. ADRs 0024–0040 for OD1, OD3–OD20, E2–E9,
P2; ADR 0021 Accepted; ADR 0001 amended (record-not-rule, dated amendments, `Rejected`, ADR number
as id); plan, domain model, event store, verification, philosophy, foundation, CLAUDE.md and this
file follow; GH-05 removed. Open then: OD2, P1/P3, P4 (decided by DEC-02).
Accept: `cargo xtask check --only docs` green; every decided OD/E names its ADR.

#### DEC-02 Record the owner's upgrade, distribution and ADR decisions
Cites: ADR 0001, dm §10.1 OD2, philosophy P1/P3/P4. New ADRs 0043 (upgrades and rollback), 0044
(Homebrew and `uninstall --purge`), 0045 (the App permission list); ADRs 0041 and 0042 and the
batch 0003, 0004, 0006-0020, 0023 Accepted (owner, 2026-11-09), with the amendments marked
proposed inside them; dm OD2, event-store §6, plan, philosophy and this file follow. Docs only.
Accept: `cargo xtask check --only docs` green; no open owner item left in philosophy.

#### DEC-03 Specify the Scale Set protocol; our own client
Cites: plan §7, ADRs 0028, 0029, 0033, 0038, 0040, 0045. The owner decided to inspect
`actions/scaleset`, not port it (ADR 0048, Accepted (owner, 2026-11-10)). New
`doc/github-scaleset.md`: mayfly's specification of the auth chain, endpoints, session, long
poll, messages and ack, statistics and capacity, acquire, JIT and the guest side, runner removal,
retry and rate limits, mayfly's differences from the reference listener, attribution, and the
open questions GH-00 answers; each fact cites the upstream file and function at a pinned commit
or is marked unverified. ADR 0028 carries a proposed amendment (registration token as an
auth-chain step only); GH-00..GH-04, plan §7 and ver §7.3 follow. Docs only.
Accept: `cargo xtask check --only docs` green; no upstream code or test in the repository.

#### DEC-04 Record and implement the KER-08 checkpoint's owner decisions
Cites: plan §3.2, §6, §8.4, §9; ADRs 0024, 0025, 0026, 0028, 0048. The owner decided on
2026-11-10: no defaults for unknown facts (ADR 0052), destructive directives idempotent by
outcome (ADR 0053), and ADR 0048's amendment of ADR 0028 accepted. Model first:
`formal/scheduling.qnt` (`noStartWhileCapacityUnknown`) and `formal/lifecycle.qnt` (a delete of
an absent VM is done). `mayfly-protocol` 1.1 (`capacity_unknown`, `listing_incomplete`,
`CapacityReport::unknown`, the `VmObservation` facts; a 1.0 session is written without them);
`mayfly-data-domain`'s ledger and executor follow, and the KER-08 notes are removed.
Accept: the named tests of ADRs 0052 and 0053; the regenerated `matrix/ledger.txt`,
`directive.txt` and `link.txt`; the affected Apalache relations at their bounded depths.
Owner decision (2026-11-10, DEC-05, ADR 0066): a `StopVm` (or `StartVm`) whose VM a complete
listing shows absent is refused `target_absent` and quarantined as a desync
(`vm_vanished_externally`), not the proposed non-quarantining refusal. Amended by the owner on
2026-11-11 (DEC-07): the desync is recorded and the attempt closed instead.

#### DEC-05 Record and implement the owner decisions of 2026-11-10
Cites: plan §3.2, §6, §8.4; ver §6; ADRs 0020, 0025, 0026, 0042, 0052, 0053. The owner decided on
2026-11-10: (a) "If we know it was there and we didn't do it it should be quarantined for
review- this is invalid state, we desynced somehow and we must inform user about it and us to
ship eventual fix." (ADR 0066); (b) ADR 0020's amendment, option 2: the clock offset at the
handshake; (c) "We want to run it on a weekly ci only": the Apalache lane. Model first:
`formal/lifecycle.qnt` (`replyTargetAbsent`, `vanishedStaysQuarantined`). `mayfly-protocol` 1.2
(`target_absent`, `Welcome.cp_clock`; a 1.1 session is written `proof_mismatch`);
`mayfly-data-domain`, `mayfly-data` and the `Attempt` aggregate follow. `formal.yml` loses its
push and pull-request triggers and runs one job per model (`cargo xtask formal-verify --model
<name>`); the per-push typecheck and Quint tests stay in `ci.yml`.
Accept: the named tests of ADR 0066 and of ADR 0020's amendment; the regenerated
`matrix/directive.txt`, `link.txt`, `matrix/attempt.txt` and `decisions/attempt.txt`; the
lifecycle relations at their bounded depths; `actionlint` and `action-pins` green.

Amended 2026-11-11 (DEC-07): the vanished VM's quarantine and `vanishedStaysQuarantined` are
replaced; a vanish closes the attempt (ADR 0066's amendment).

#### DEC-06 Record and implement the owner decisions of 2026-11-10 (round 7)
Cites: plan §6, §9, §11; ver §6; dm §6.3.1, §6.4; ADRs 0024, 0025, 0046, 0064, 0069;
`doc/github-scaleset.md` §12 item 6 (§5 and §12 item 3 since GH-00R). The owner decided on 2026-11-10: (a) ADR 0064: "accept";
(b) the event store's `storeOnlyStep` depth: "depth 4" (`BOUNDED` unchanged, weekly runs only;
recorded in `doc/formal-models.md`); (c) ADR 0069 and its `DEMAND` option: "implement both now":
a per-pool `capacity_policy = "exact" | "demand"` (optional, default `"exact"` until GH-00
answers item 6; an older build refuses the key), `capacity::CapacityPolicy` in `capacity::plan`;
(d) the quarantine stop: "deregister" (ADR 0076). Model first: `formal/scheduling.qnt` (the
policy and the waiting jobs as `init` parameters, `carveMonotoneDemand`,
`demandFixesThe13GiBCaseTest`, `demandStarvesWhenDemandIsInvisibleTest`,
`demandLetsTheLaterPoolInTest`), `formal/lifecycle.qnt` (`i21DeregisterBeforeStop` covers the
quarantine stop; the attempt's `dereg`) and `formal/registration.qnt` (`requestQuarantineStop`).
The `Attempt` aggregate gains `StopDeregistrationRequested`, `StopDeregistrationConfirmed` and
`StopDeregistrationRefusedBusy`; the conformance driver compares `dereg`.
Accept: the named tests of ADRs 0069 (DEC-06's) and 0076; KER-05H's and KER-05D's capacity tests
unchanged in what they assert; the regenerated `matrix/attempt.txt`, `decisions/attempt.txt`,
`intents/attempt.txt`, the lifecycle regression traces and two new ones; the conformance replay
and a 1000-trace sweep of lifecycle green; the lifecycle and scheduling relations at their
bounded depths.

#### DEC-07 Record and implement the owner decisions of 2026-11-11
Cites: plan §3.4, §6, §10; dm §6.4, P15; ver §6; ADRs 0025, 0058, 0066, 0076. The owner decided on
2026-11-11: (a) "If we need secrets in control plane that's ok, just need to do it reasonably.":
ADR 0058 accepted (plan §3.4 names the exception; `CLAUDE.md` is the orchestrator's); (b) "maybe
we should treat it like a k8s controller? we want desired state and try to restore it? keep
log/audit/journal for debugging?", option A: a VM the attempt recorded and mayfly did not
delete, absent from a complete listing, whether a stop's or start's re-observation
(`target_absent`) or the inventory finds it, records `VmVanished{discovered_by, instance_id,
vm}` and closes the attempt (never resurrected; the reconciler provisions afresh), surfaced in
the `attempts` view (`desync`, spec 2) and counted by the breaker (P15, CP-06); quarantine only
for a present VM that does not match the record, whose stop deregisters first (ADR 0076;
a vanish ends any such wait, nothing being left to stop); an incomplete listing concludes nothing (I25).
Model first: `formal/lifecycle.qnt` (`vanish`, `listingIncomplete`; `mismatchStaysQuarantined`,
`vanishedClosedForGood`, `noAbsenceFromIncomplete` replace `vanishedStaysQuarantined`).
Accept: the named tests of ADR 0066's amendment; the regenerated lifecycle regression traces,
`matrix/attempt.txt`, `decisions/attempt.txt` and `intents/attempt.txt`; conformance replay and
two seeded sweeps; the lifecycle relations at their bounded depths.

#### KER-02 CP↔DP protocol
Cites: plan §3.2, D9, D13. Model first: `formal/link.qnt` (at-least-once, duplicates,
reordering, epoch fencing; invariant: no directive from an older epoch executes; every directive
id executes at most once per epoch). `mayfly-protocol`: `Hello`/`Welcome`, `Directive`,
`Observation`, `DirectiveOutcome` (`Refused` definite vs `Unknown`), `SecretHandle`,
`ProtocolVersion` negotiation (pure fn), link port traits (`DirectiveSink`, `ObservationSink`, and
their receiving halves) as transport-agnostic async traits.
Accept: golden JSON fixture per message kind and version; unknown-major refusal test; model typechecks and its tests pass.
DEC-01 adds (dm §11.1 §3.2, ADRs 0024, 0026, 0029, 0037–0039): `Hello` carries the epoch floor,
`Welcome{epoch, mode}`; directive kinds `PullImage`, `MaterialiseImage`, `EvictImage`,
`CloneVm{base_name, vm_name}`, `VerifyGuest{nonce}`, `ExportDiagnostics`, `StopVm{proof}`,
`DeleteVm{proof}`, `ReportCapacity`, `ReportHostFacts`; observations `VmCloned{instance_id}`,
`GuestVerified`, `GuestRejected`, `RunnerLaunchUnknown`, `InventoryIncomplete`,
`CapacityReported`, `HostFacts`, `HypervisorLimitHit`, `ImagePulled`, `ImageMaterialised`;
`DirectiveRefused` reasons include `insufficient_resources`, `proof_mismatch`, `safe_mode`;
stored vs wire form of `LaunchRunner` (R14). The link model adds I18 and "safe mode holds
create/destroy kinds" (I29). Accept adds: a refused-reason golden per reason.

#### KER-03 Event-sourcing kernel
Cites: plan §5.1–§5.3. In `mayfly-control-domain`: `Aggregate` trait (`decide`, `evolve`,
`initial`), `DomainEvent` (`EVENT_TYPE`, `SCHEMA_VERSION`), event registry and upcaster hook,
`Command` envelope (`command_id`, `issued_at` supplied by the caller), `Rejection`. In
`mayfly-testkit`: the golden-fixture harness (every registered event has a fixture under
`crates/mayfly-control-domain/tests/fixtures/events/<type>/v<n>.json`, round-trips, and an
unregistered fixture fails) and the `DETERMINISM_CASES` harness (each case decided twice from
equal state gives equal results, refusals included; every aggregate module must contribute
accepted and refused cases).
Accept: harnesses fail on a missing fixture and on an aggregate without cases (tests prove it).
DEC-01 adds: the command envelope carries `run_epoch` and `actor`; the run-epoch fence (I18) is a
kernel rule with accepted and refused determinism cases (F43); `Aggregate::SUPPORTED_EPOCH` and
`EventSet::required_epoch` (event-store §6.2) if ES-03 has not added them; "one command, one stream"
as an architecture rule.

#### KER-04 Lifecycle model and `Attempt` aggregate
Cites: plan §6, ver §6, D10. Model first: `formal/lifecycle.qnt` with invariants I06, I10, I13, I14
(names in ver §6.1), scenario tests. Adds the `ARCH-exhaustive-transition` rule (fnd §3.3). Then the `Attempt` aggregate: states and stimuli of plan §6,
exhaustive State × Stimulus matrix (no wildcard arms; an architecture or unit test proves every
pair is listed), events with goldens, determinism cases.
Accept: matrix test covers every pair; model tests pass; register the model in `MODELS`.
DEC-01: the lifecycle of dm §6.4 replaces plan §6's draft (Verifying, Prepared, Listening, Lost,
`pending` overlay, no Draining state); invariants add I22 (attempt half), I30, model-local
`quarantineNeverAutoDeleted`; there is **no** `AssertOutcome` (ADR 0025): `Unknown` leaves only on
an observation. Directives are `DirectiveIssued` events (ADR 0034). Accept adds: activation without
a grant ref or a fresh verification is refused; no stimulus but an observation leaves `Unknown`
(mutant 27).

#### KER-05 Scheduling model, `Host`/`Pool`, capacity policy
Cites: plan §9, §5.1. Model first: `formal/scheduling.qnt` (I01, I02, I03, I07, I08, I13). `Host`
aggregate (epoch, slots, monotonic generations, reservations, drain, safe mode), `Pool` aggregate,
pure `capacity::plan(inputs) -> CapacityReport + decisions`.
Accept: proptest "admitted ≤ every limit" over random inputs; two macOS guests max; drain admits
nothing; generations strictly increase per slot.
DEC-01 (ADR 0024): `Host` allocates within the last applied `CapacityReported` (dm §6.3) and keeps
leases as allocation bookkeeping; pool terms, gate and drain as dm §6.3; I24, I26, I27, I28 (no
cap-driven deletion, quarantined leases hold capacity), I33's control-plane half (never allocate
beyond a report); `Pool` with breaker. The scheduling model adds `externalConsumptionShrinksAdmissible`,
`noGrantWhileClosed`, `quarantineHoldsCapacity`, `allocationWithinReport`. Accept adds: a stale
report never allocates beyond the newer one; `min_prepared` defaults to 0.

#### KER-06 Scale-set model, `ScaleSet` aggregate, demand
Cites: plan §7.2. Model first: `formal/scaleset.qnt` (redelivery, ack loss, >50 messages,
reassignment; invariants: no double reservation per request id, desired derives from latest
statistics only, ack only after durable record; I12, shared with recovery). `ScaleSet` aggregate (session FSM, cursor,
statistics, acquired ids) and pure `demand::desired(stats, deliverable)` (no standby: DEC-01).
Accept: tests for the mutants "count messages", "add TotalRunningJobs", "ack before record" fail
the model or the unit tests (ver §9 items 4–7).
DEC-01 (ADRs 0027–0029, 0040): `PlanAcquire` (I23: acquire only while assigned < deliverable),
trust classification (I32: trusted only; `pull_request_target`, `workflow_run`, `issue_comment`
and unclassifiable offers declined), per-scale-set carve-out input with a revision, `desired =
min(deliverable, assigned)` (no standby), initial statistics authoritative, unknown message types
(R7), the wire behaviour of pause, drain and safe mode; stream `scaleset-<pool>`. Mutants add
"acquire every offer" (19).
Note (KER-06F, the KER-06 review): "initial statistics authoritative" holds until the session
records a message above its floor. No id tells a message emitted before the open from a newer
one, so its older statistics apply until the backlog is consumed (A6, `doc/github-scaleset.md`
§12): within I23 by A2, and the model now checks desired against GitHub's own count once the
stream has seen everything (keeping the opening's statistics never converges, mutant
`OPEN_WINS`). A message at or below the floor applies nothing; one A1 does not explain degrades
the session (`message_ids_regressed`); the fifth 409 in a row is escalated
(`SESSION_CONFLICT_ESCALATION`, owner-tunable); unclassifiable offers are declined
`unclassifiable`; a degraded or closing session plans no offer.

#### KER-07 Recovery model and decision table
Cites: plan §10, dm §11.3, ADRs 0025, 0026, 0037. Model first: `formal/recovery.qnt` (crash at every
await, epochs and the external floor, unknown outcomes, incomplete inventories; I04, I05, I09,
I10, I11, I12, I17, I25, I29, I31; model-local `noAssertion`). Pure
`operations::recovery::classify(journal_view, provider_view, github_view) -> Decision` covering
every row of plan §10's table (shim alive or gone, `exit.json`, instance-id mismatch → quarantine,
inventory incomplete → wait) plus safe mode. No row asserts an outcome or deletes a quarantined VM.
Accept: exhaustive table test; `mf-*` VM unknown to the journal is never `Delete`; an instance-id
mismatch is never `Delete`; an incomplete inventory releases nothing.
Note (DEC-07, ADR 0066 amended): the table is unchanged and consistent with the new rule:
`MarkLost` reads a complete listing's absence, and the attempt it reaches records `VmVanished`
(a desync) rather than a silent loss; a quarantined record is kept by recovery and converges
through the attempt's own reading of the inventory.

#### KER-08 ⛳ Data-plane domain
Cites: plan §3.1, §6 (DP FSMs), §8.4, §9; ADRs 0024, 0026, 0037. `mayfly-data-domain`: directive
executor FSM (`Received → Executing → Done | Refused | Unknown`), idempotency table semantics (same
`directive_id` → same outcome), epoch fencing and the floor, safe-mode refusals (I29),
ownership-proof check for `StopVm`/`DeleteVm` **after re-observing the instance id** and refusal of
any base name, VM supervisor FSM derived from observations (shim alive, `exit.json`,
`ProcessLost`), inventory `Complete | Incomplete` (I25), and the **resource ledger** (pure: totals,
used by ours and foreign VMs, in-flight reservations, fit check per directive, `CapacityReported`
content) with refusal `insufficient_resources` (I33).
Accept: exhaustive matrices; stale epoch never executed (answered `Unknown{stale_epoch}`,
ADR 0042); delete without matching proof or with a changed instance id refused; a directive that
does not fit is refused whatever the CP sent (mutant 24).
Owner decisions of the KER-08 review, decided 2026-11-10 and implemented by DEC-04: (a) unknown
sizes are not assumed: an unknown VM size or guest makes the dimension unknown and what needs it
is refused `capacity_unknown` (ADR 0052); (b) a `DeleteVm` whose target a complete listing shows
absent answers `VmDeleted`, a `StopVm` of a proven stopped VM `VmStopped`, an incomplete
re-observation `Unknown{listing_incomplete}` (ADR 0053).

#### KER-09 Registration model and `RunnerRegistration` aggregate
Cites: dm §6.5, P10–P14, ADRs 0028, 0029, 0038. Model first: `formal/registration.qnt` (I19, I20,
I21, I22's registration half; deregister-before-stop, the busy race (409 on the Actions-service
path, 422 on REST: `doc/github-scaleset.md` §7), issue unknown; model-local
`noRemintAfterUnknown`, `deregisterBeforeStop`). The aggregate with its matrix, goldens and
determinism cases; the online confirmation after delivery (runner id seen online, else presumed
leaked).
Accept: model scenarios pass; mutant 21 (stop before deregister) fails the model and a unit test;
a runner name is never issued twice.

#### KER-10 Operations aggregates: `HostRun`, `RecoveryPlan`, `QuarantineCase`, effective mode
Cites: dm §6.8–§6.10, §8 (`OperatingMode`), ADRs 0025, 0026, 0031. `HostRun` (I17: epoch above the
stored maximum and the floor; safe mode entered and left only by a completed plan or a verified
restart), `RecoveryPlan` (I31: bindings refuse when stale; no `assert_outcome` action),
`QuarantineCase` (operator-only resolution; dispositions `delete_with_proof`,
`released_observed_absent`, `keep`; escalation never acts), the pure mode precedence function
with an exhaustive table.
Accept: I17 and I31 unit tests; a plan applied after the world moved is refused (F45); no case
resolves without an operator actor.

#### KER-10F Review fixes to the Operations aggregates
Cites: KER-10's review; ADR 0041; `formal/recovery.qnt`. H1: a plan binds each stream's revision
(`RecoveryPlanDrawn`, replacing KER-10's digest-bound `RecoveryPlanProposed`, which still decodes
and is refused when applied); an outcome carries the plan's own write (`RecoveryActionRecorded`)
and advances only that head, only from where it was bound; the model splits an action into
intent, the attempt command's own unit and the outcome, with a foreign writer and a negative
control. H2: the command's fenced run epoch must be the plan's. M1: a plan exit carries the plan's
epoch and a fresh verification, and the run restarts before recovery completes (the model's
`completePlan`). M2: `MintEpoch` carries the crash-loop finding. M4: `QuarantineCaseResolved` v2
records the instance id a delete relies on. LOW: `HostRun`'s fence is two-sided after the mint;
a quarantine stream refuses another attempt and an open case another reason.
Accept: the model's foreign-write scenarios pass and the absorbing outcome fails `i31PlanBound`;
unit tests for each item; `formal-verify --model recovery` at the bounded depths.

#### KER-10G Review fixes to KER-10F
Cites: KER-10F's review; ADR 0041. `BeginAction`/`CompletePlan` take `Observed`, whose heads are a
`HeadsRead` built only by `Bindings::read_now`; `Bindings::read_conditions` conditions the
attempt command; the CP-04 card's rule corrected (plan units read current revisions); a plan exit
only while `Recovering`; `ApprovePlan` refused under another run epoch; `crash_loop` named as
OPS-05's input.
Accept: unit tests for each; the read set equals the bound heads; `formal-verify --model
recovery` at the bounded depths.

#### KER-11 Image catalog: `Image`, `ImagePlacement`
Cites: dm §6.6–§6.7, P17, P18, ADRs 0030, 0039. `Image` (status monotonic, runner version and its
expiry), `ImagePlacement` (single pull per host and digest, materialise to a base named by digest
and runner version, eviction only from `Materialised` with zero references, pool base pointer with
the previous one kept for rollback).
Accept: matrices; eviction refused while referenced; a withdrawn image never becomes Verified.

### Phase 2 — journal (control plane)

#### ES-01 Journal port, in-memory journal, contract suite (landed)
Cites: plan §5.2, ver §5, ADR 0020, ADR 0021. Landed (f75829d) before the owner's store decisions:
the port, `MemoryJournal` and the first journal contract, with an outbox table in the port and
unbound refusal receipts. ES-01F realigns it.

#### ES-01F Align the landed journal port with the owner's decisions and its review
Cites: event-store §2, §3.1–§3.3, §5.3, §7.1; ADRs 0021, 0034. The ES-01 independent review and the
decisions E2, E3, E6:
- **E2:** remove the outbox table, its rows and transitions from the port, the unit and the suite;
  directives become `DirectiveIssued` events (delivery is ES-05).
- **E3 (review M6):** refusal receipts bound to `(stream, revision)` with the canonical intent
  digest; a repeat at the same revision gets the recorded refusal, after the stream moved it is
  decided afresh and its outcome replaces the refusal receipt; `CommandIdReused` for a reused id
  with another intent; JC-12..JC-14 and JC-28.
- **E6:** `supported_epochs`, `required_epoch`, `EpochAboveBuild`, `StreamEpochUnsupported` (JC-18);
  `reads` (JC-11), `lookup_event`, `streams` (key order, byte-wise / `COLLATE "C"`), `stored_kinds`.
- **M1:** `OutcomeUnknown` resolved by re-appending the identical unit (answered by its receipt or
  `DuplicateEventId` at the same place), with a factory hook that drops the reply (JC-25 on memory).
- **M2:** concurrent cases: two units with the same command id, the same inbox key and the same
  directive/event id across different streams; exactly one commits.
- **M3:** the suite documents that it checks interleavings, not freedom from a global lock; the
  PostgreSQL structural check (`pg_blocking_pids`) belongs to ES-09 (JC-22's blocking half).
- **M4:** the lock order of event-store §4.2 stated separately from the reporting order (JC-09
  pins the latter); isolation `READ COMMITTED` with row locks named for ES-09; `Busy` variant;
  `AppendError` `#[non_exhaustive]`.
- **M5:** inline projection rows applied inside the adapter's transaction, with a concurrent
  reader case (nothing visible before commit).
- **M7:** doc drift: plan §5.2 (done by DEC-01), ver §5 (done), ES-04's watermark (gone), the
  contracts' doc comments still saying "sequence".
- **LOW:** pin or explicitly leave unspecified the check order; NUL refused before any write
  (JC-19); a `usize::MAX` page limit clamped; `Unknown` → `Pending` retry semantics removed with the
  outbox; a `JournalFactory` crash hook for ES-02's crash mode.
Accept: the contract green on memory; each broken journal of event-store §7.6 that exists at this
stage fails exactly its named cases; no port method returns events of more than one stream; no
`outbox` left in `mayfly-control` or the testkit.

#### ES-02 SQLite journal adapter
Cites: event-store §4.1–§4.3, D7. Migrations (`schema_meta` with `compat_floor`; an automatic backup
before each migration), triggers refusing `UPDATE`/`DELETE` of journal rows and lowering heads or
epochs, durability verification at open, one writer thread plus the two test-only modes,
classification by extended code and constraint name, `StoreHooks` with `NoHooks`, refusal-receipt
storage (E3). Contract in modes fresh, reopened, two-connection, two-handle, crash-injected; JC-23,
JC-25..JC-28. Migration test from empty and from the previous schema fixture.
Accept: all JC cases green in five modes; `ARCH-store-sql` refuses an `INSERT OR IGNORE` fixture.
Folded in from the ES-01F review: JC-29b (racing re-decisions of a command whose refusal receipt
went stale; the receipt is replaced by compare-and-set) with a blind-upsert negative control,
JC-32 read in both stream orders, JC-25 lost replies of a refusal receipt and of an inbox-only
unit, and the receipt row named in ES-I16's lock order.

#### ES-03 `AggregateStore`
Cites: event-store §3.7, §6.2–§6.3; ADR 0034. The single write path with the step order of §3.7
(epoch check before the receipt answer; refusal receipts bound to the decided revision), derived
event ids (golden-tested), canonical intent digests per command type (a test fails on a command
without one), `with_revision_retry` (only `WrongExpectedRevision` and `ProjectionGenerationMoved`;
load and decide inside), `OutcomeUnknown` resolution by receipt, after-commit kicks,
`SUPPORTED_EPOCH`/`required_epoch`, the start-up kind check feeding safe mode; architecture rules
`ARCH-single-append`, `ARCH-no-global-order`, `ARCH-migration-frozen`, the epoch rule, the
golden-fixture `fixtures.lock` (fnd §3.3).
Accept: fixtures fail a second `append` caller and a global-order query; a retried command returns
its receipt; a refusal is replayed at the same revision and re-decided after it moved; a reused id
with another intent is refused; an unknown stored kind starts safe mode.
Landed with ADR 0041: refusal receipts bound to every stream the decision read (port, memory,
SQLite, JC-28's two-read half); `CommandBody`; `ARCH-single-append`. The epoch rule holds by
construction (epochs come from the registry) and in the golden harness. The global-order fixture,
`ARCH-journal-sql`, `ARCH-migration-frozen` and `fixtures.lock` moved to ES-03R.
`with_revision_retry` retries `Conflict` (`WrongExpectedRevision`) only: `ProjectionGenerationMoved`
does not exist before ES-12, which adds it as the second retried error (`SaveError` is
`#[non_exhaustive]`, and `definite` matches `AppendError` exhaustively so the new variant fails to
compile until its retry is decided).

#### ES-03R Remaining store rules
Cites: event-store §4.3, §6.1; fnd §3.3; ver §4.1; ADR 0041. `ARCH-no-global-order` (adapter SQL
ordering by `rowid`, a time column or a revision across streams; a store-port method returning
events of several streams), `ARCH-journal-sql`, `ARCH-migration-frozen` over `migrations.lock`, and
`fixtures.lock` (the hashes of every frozen event fixture, checked by the golden harness).
Accept: each rule's negative fixture fails it; changing a frozen fixture or a released migration
fails the gate.
Landed with ADR 0041's amendment: the four rules (`fixtures.lock` as `ARCH-fixtures-lock`, in the
architecture stage rather than the golden harness, so one check covers every family and crate),
`migrations.lock` and `fixtures.lock` at the repository root; the ES-03 review's fixes
(`ARCH-single-append` by path shape, narrowed exemptions, `execute` refusing reads).

#### ES-04 ⛳ Projections
Cites: event-store §3.6, §6.1, §6.4, I16. `ProjectionSpec`, stream-local inline projections computed
outside the transaction, `projection_row` documents with generations, rebuild by stream enumeration
with applied revisions (offline form here; online in ES-12), first projections `attempts`,
`slots`, `pools`, `pending_effects`, `recent_failures` (query-time views for cross-stream counts).
PC-01..PC-06 on memory and SQLite; `ARCH-projection-locality`.
Accept: PC green; "rebuild equals live" property; a cross-stream inline write is refused.
Note (ES-04, as built; event-store §3.6 "As built (ES-04)"): PC-01..PC-06 run on memory and SQLite
(fresh, reopened, two writers, crash-reopened); "rebuild equals live" is a property over seeded
pool histories (`tests/store/projections.rs`); a cross-stream write is `Malformed` in every journal
and `ARCH-projection-locality` in the code. Later tasks: CP-04 runs `projection::prepare` after the
stored-kinds check and before any writer, and RT-02 builds `AggregateStore::with_projections`
with `projection::control()` and a `BuildId` from the build; CP-01 makes `queries` the operator
use cases' read side; ES-08 must read the rows at the snapshot's revision (`stream_rows` with
`applied`) instead of folding from revision 0 once a load starts from a snapshot; ES-09 ports
migration 0004 and runs PC on PostgreSQL; ES-12 adds dual writes into the shadow generation and
the cutover fence to the unit.
Note (ES-04F, the ES-04 checkpoint review; event-store §3.6 "As fixed (ES-04F)"): a unit writes
its streams' whole row sets, `prepare` also rebuilds a projection with a stream behind its head,
the offline cutover is refused while a stream moved after its fold, and a rebuild claims its
generation (migration 0005, floor 2); PC-10..PC-12 and "rebuild equals live" over pool, host and
attempt histories run on memory and SQLite. ES-09 ports migration 0005 and the `behind` and
cutover checks; ES-12 keeps the claim for its shadow generation.

#### ES-06 Event-store model
Cites: event-store §7.3, D10, ver §6; ADR 0034 (E7: before delivery). `formal/event_store.qnt` and
its scenarios module with `step`, `implStepSerial`, the biased relation and `allInvariants`,
including refusal receipts bound to revisions (`refusalReplayedAtSameRevision`) and, for ES-12, the
rebuild-generation part (`shadowRebuildDualWrite`, `cutoverFence`); every invariant shown to catch a
scratch mutation; registered in `MODELS`; regression traces exported.
Accept: scenarios pass; the two negative worlds break their invariants; `formal-freshness` green.
Note (ES-06F, the ES-06 review): the model splits the store's epoch check from its receipt read
(`request`, `decide`) and found the journals answering a replay before the epoch: a rolled-back
build racing a newer one could answer for the newer build's outcome. Every journal now judges
the epochs of the streams a receipt's answer is for with the receipt (JC-33), and the store
re-reads them after the receipt; a refusal receipt carrying an inbox key is
`Malformed(RefusalWithInbox)` (JC-34, ADR 0041 rule 6); `EpochAboveBuild` names the lowest
stream. Component relations `storeOnlyStep` and `deliveryOnlyStep` take Apalache to where
ES-I14, ES-I15 and `directivesFromJournal` are reached (`doc/formal-models.md`, "event_store").

#### ES-05 Durable delivery
Cites: event-store §3.4, §5; ADR 0034 (E2). `DeliveryLedger` (memory, SQLite), marks written by the
append for registered consumers, `DeliveryWorker`, `catch_up`, kicks, `retry_after` with jittered
backoff, DC-01..DC-25 with `UnfencedLedger`, `ClearAllMarksLedger`, `HeadTrustingWorker`,
`SkippingLedger`; the `DirectiveIssued` event shape and the `directive_relay` consumer skeleton.
Accept: DC green on memory and SQLite (all modes); every broken ledger fails its cases.
ES-03 (ADR 0041 rule 6): a refusal consumes no inbox key, so a consumer or observation handler
that deduplicates by inbox treats `Outcome::Refused` as handled (acked) and never reads
"not in the inbox" as "not yet decided"; the refusal receipt answers a redelivery.
Note (ES-05, as built; event-store §3.4 "As built", §5.1): the model gained `retire` and the
scenario `retireThenRegister` first. Later tasks: CP-02 implements `DirectiveSink` (the relay
hands it the decoded `DirectiveIssued` with stream, revision, event id and fence) and keeps the
data plane's dedupe by `directive_id`; CP-04 and RT-02 call `DeliveryWorker::catch_up` per
consumer after the start-up checks, own the `Kicks` wiring (one per consumer, `AggregateStore`'s
notifier) and the coarse timer, and pass the SQLite ledger a store clock; ES-07 maps the model's
consumer actions onto the ledger (the claim's fence is its token); ES-09 ports migration 0002
(floor 2) and runs the DC suite in its modes.
Note (ES-05F, the review of ES-05; event-store §3.4 "As built (ES-05F)"): the worker cuts a
`handle` at `DeliveryConfig::handle_budget` (below the lease) on a `DeliveryTimer` and releases
with a backoff (DC-26), claims one stream at a time (DC-27), backs off and reports a read that ends
short of its mark; migration 0003 (floor 2) refuses lowering a dispatch mark, which ES-09 ports.
CP-04 picks the production store clock on SQLite: one that does not step forward (a forward step
ends leases early, which costs only duplicates), and RT-02's supervision passes the runtime clock
as the worker's `DeliveryTimer`.
Note (ES-05G, the review of ES-05F; event-store §3.4 "As built (ES-05G)"): a drain handles a
stream it gave up on (failed, cut, short read, lost) at most once and returns when discovery comes
back round to it (DC-28, `CursorlessLedger`); the short-read ERROR has its own book; the worker's
books evict the least recently touched entry; a worker without its `handle` deadline is DC-26's
negative control. The ES-09 PostgreSQL ledger keeps the round-robin cursor DC-28 requires.

#### ES-07 Event-store conformance driver
Cites: event-store §7.3, ver §6.2. Driver over memory (`step`) and SQLite (`implStepSerial`, two
connections), reading state through the ports only; the driver mutants of §7.3 each fail a named
committed trace at a named step; generated traces per push.
Accept: committed and 30 generated traces replay clean; each mutant caught where declared.

#### ES-08 Snapshots
Cites: event-store §3.5, ES-I8; ADR 0034 (E8). Port, memory and SQLite stores, worker, restore path
reachable only from `AggregateStore` (architecture rule), SC-01..SC-11, model actions and scenarios
added to ES-06's model with traces replayed by ES-07's driver, a property "snapshot plus tail
equals whole fold".
Accept: SC green; `BlindSnapshotUpsert` and `ResealingSnapshots` caught by contract and trace.

#### ES-09 ⛳ PostgreSQL adapter
Cites: event-store §4, §7.1, §7.3; ADR 0035 (E4). New crate `mayfly-adapter-postgres` (members,
`architecture.toml`, external allowlist: the PostgreSQL client crate its ADR picks, `tokio`,
`serde_json`, `tracing`, `thiserror`); migrations mirroring ES-02's numbers; `cargo xtask
schema-parity`; journal, delivery, snapshot and projection stores; owner lease (§4.5);
`cargo xtask` stage `contract-postgres` that starts a throwaway PostgreSQL 17 **Docker container**
from `mirror.gcr.io/library/postgres` pinned by digest in `tools/versions.toml` (labelled with
the run id, `127.0.0.1`, free port, removed afterwards); `doctor` checks Docker and the image; a
missing CLI or stopped daemon fails closed with a message; every contract in the PostgreSQL modes;
ES-07's driver on PostgreSQL with `pg_blocking_pids` checks; a `postgres` job in `ci.yml` under
`required` running the same stage. ADR for the client crate.
Accept: every JC/DC/SC/PC case green on PostgreSQL locally and in CI; committed traces replay with
blocker checks; parity green; `GlobalLockJournal` caught by JC-22's blocking half; the stage fails
(not skips) without Docker.
Note (ES-05F, marks against a concurrent registration): under `READ COMMITTED` (or snapshot
isolation) an append reads `delivery_consumer` at its first statement, so a `register` plus that
consumer's bootstrap page can commit between the append's read and its commit: the append marks
no row for the new consumer, and the bootstrap, which read the stream's head before the append
committed, marked it only up to the old head. The new event is then never marked: a skip. SQLite
cannot show it (one writer at a time). The PostgreSQL adapter must order the two: the append
takes a shared lock on the consumer set (`SELECT … FROM delivery_consumer FOR SHARE`, or a shared
advisory lock on a fixed key) before it writes its marks, and `register` takes the exclusive one
(`LOCK TABLE delivery_consumer IN SHARE ROW EXCLUSIVE MODE`, or the exclusive advisory lock), so a
registration commits wholly before or wholly after each unit; then the bootstrap sees every unit
that did not mark the consumer. ES-09 adds a contract case for it (the next free DC id: a
registration and its bootstrap racing appends on two connections, with the append held between
its consumer read and its commit by a test hook; every committed event is marked or covered by the
bootstrap) and the lock's place in §4.2's order (it is taken before any stream head, so it adds no
wait-for cycle with ES-I16).

#### ES-10 Store lab
Cites: event-store §7.5, ver §8, D11; ADR 0035 (E5). `tools/storelab-worker`, `ARCH-store-hooks`,
workloads SW1–SW10 on SQLite and PostgreSQL (Docker), nemesis of §7.5; histories exported to
**Knossos** (pinned JAR with SHA-256 in `tools/versions.toml`, on the pinned JVM) with one CAS
register per stream; identity and delivery as audit assertions; fixture histories with expected
verdicts (legal, illegal, ambiguous, harness-broken) through the real checker; the lab mutants of
§7.6 each turning its workload `FAIL` with a witness; `faultlab suite --only store-` in the weekly
lane. If Knossos does not scale, measure and propose Elle by an amendment of ADR 0035.
Accept: fixtures give their verdicts; core store workloads PASS; each mutant FAIL; a scenario
whose fault never hits is `INCONCLUSIVE`; a missing Knossos fails closed.

#### ES-11 Compatibility harness and upgrade matrix
Cites: event-store §6.1a, §6.5; ADR 0043. `cargo xtask compat` with a scenario corpus,
run as N → N+1 → N on SQLite locally and on both engines in CI, plus an **upgrade matrix** from
every released version to the current build (forward jumps of any size), from the first tagged
release on: CI opens the journal of **every released version** and upgrades it straight to the
current build, never only the previous one (ADR 0043).
Accept: a deliberately incompatible event change (a rename) fails it; a skipped-version upgrade in
the matrix passes; the matrix has one row per released version.

#### ES-12 Online projection rebuild
Cites: event-store §3.6, ES-I11, §6.4; ADR 0034 (E9). Shadow generations built stream by stream in
the background; dual writes of inline rows to the active and the shadow generation; the cutover
fence (one transaction flips the active generation; a unit computed for the old set is refused
`ProjectionGenerationMoved` and retried); resumable after a crash from applied revisions;
PC-07..PC-09 on memory, SQLite and PostgreSQL; the model's rebuild scenarios replayed by ES-07's
driver; `mayfly recover --rebuild-projections` triggers it.
Accept: PC-07..PC-09 green; admission is never blocked by a version mismatch; a driver mutant that
skips the dual write is caught.

### Phase 3 — the two planes and the walking skeleton

#### CP-01 Commands and queries
Cites: plan §5.4. `commands` and `queries` modules of `mayfly-control`; the CQRS semantic rule of
xtask selects them (commands name no query/projection type and return no read DTO; queries take no
write port). Operator use cases: drain, resume, pause, status, attempt timeline.
Accept: architecture fixtures for both directions; component tests with `MemoryJournal`.

#### CP-02 Attempt saga and directive relay
Cites: plan §5.4, §6, §3.2; event-store §5.2–§5.3; ADR 0034. Both are durable consumers on ES-05. The
saga turns lifecycle events into the next command (dm P6); the `directive_relay` delivers
`DirectiveIssued` events over the link stamped with the current epoch, holds create/destroy kinds
in safe mode (I29), and a deadline is the saga's `RecordDirectiveTimeout` command, which moves the
attempt to `Unknown` (never to success).
Accept: component tests: lost reply → `Unknown`, not retry-with-new-name; same directive id on
resend; a relay killed between send and advance resends the same `directive_id`.
Note (ES-05F): the relay's sends are at least once and **not ordered** across a takeover: a stale
relay worker whose lease ran out mid-send can put directive 5 on the link after the new holder sent
5 and 6 (the checkpoint stays fenced; the link is not). The `DirectiveSink` and the data plane
therefore dedupe by `directive_id` (derived from the `IssueNext` command's id,
`saga::directive_of`) and judge each directive on its own (generation,
host epoch, the attempt's state), never by per-stream order or "the one after the last seen"; a
component test sends 6 before a late 5 and shows 5 neither undoes 6 nor runs twice. `send` puts
its own deadline below the relay's `handle_budget` (DC-26).
As built: `mayfly_control::saga::AttemptSaga` (the pure table `attempt::next_step`),
`Attempt`'s new `RecordDirectiveSent` (event `DirectiveSent`, recorded by the sink once the link
took the frame: a deadline runs only from the send, so a held directive never times out) and
`RecordDirectiveTimeout`, `delivery::LinkSink` with the `DirectiveDetails` port, and
`sessions::HostSessions`; `doc/event-store.md` §5.2 "As built (CP-02)". The cross-plane
component tests are `crates/mayfly-composition/tests/directive_loop/` (one also on SQLite).

Note (CP-07): P2's `start_attempt` answers `Started::NotPlaced` when the pin's base is not
materialised on the host; whoever drives P2 waits for a placement kick and starts again then,
never releasing the lease or retrying at once.

#### CP-03 Reconciler
Cites: plan §9, §10; dm P5, P22, P23; ADRs 0024, 0028. Level-triggered pass (≤ 1/min plus events):
desired from the scale sets, capacity from the latest **reported** capacity per host, per-scale-set
carve-outs (I24), shortfall, `min_prepared`, hysteresis; reserve, retire, cleanup commands through
the capacity policy and the recovery table; a data-plane `insufficient_resources` refusal is a
definite no and re-read next pass.
Accept: 1,000-step seeded simulation in testkit never violates I02/I03/I04/I07/I08/I23/I24/I26/I27
and never allocates beyond a report.
Note (DEC-06F, ADR 0069): each pass hands `capacity::plan` every pool's policy through
`mayfly_composition::config::plan_policy` (a pool's `capacity_policy`; RT-02 wires it where
composition builds the plan's pool inputs). For a `demand` pool the pass derives `waiting` = the
pool's scale set's latest `totalAssignedJobs` beyond its live runners, plus its `min_prepared`
deficit, never capped by the pool's carve-out or the `maxCapacity` sent (a capped count feeds back:
carved nothing, told nothing; `formal/scheduling.qnt`'s `demandStarvesWhenDemandIsInvisibleTest`).
Accept adds: a `demand` pool with no waiting job keeps nothing ahead of a later pool, and one with
assigned jobs beyond its runners is carved for them (component test through the config).
Note (CP-03, as built): the pure pass is `mayfly_control_domain::reconcile::plan_pass`, the
driver `mayfly_control::reconciler` (reads `queries::fleet` over the new `hosts` and
`scale_sets` projections and the `attempts` row's `refused` and `vm_gone`, attempts v3).
Hysteresis: a pool's prepared surplus (prepared beyond its shortfall and `min_prepared`) is
retired `surplus` only once it has lasted `Settings::hysteresis` (5 min); reservations are never
delayed. An `insufficient_resources` refusal holds its host (nothing reserved, the scale set told
its runners only) until a newer report is applied. Cleanup: `ReleaseSlot` on `vm_deleted` /
`attempt_lost` evidence or `attempt_never_started` after `Settings::start_grace`, and
`CompleteHostDrain` / `CompletePoolDrain`. The recovery table is not consulted by the pass: its
inventory-driven rows stay CP-04's. Composition maps `capacity_policy` with
`config::reconciler_policies`; RT-02 wires the driver's `Kicks` and timer. `max_prepared_age`
retirement is not built here (it needs the prepared instant; CP-06 or OPS-06).
Note (CP-03F, review of CP-03): `ReleaseSlot{attempt_never_started}` stays planned by the pass
but is decided at the save: the reconciler saves it with a read of the attempt stream expecting
no stream, and P2's `StartAttempt` (the fleet kit's `World::start`) reads the host stream at the
revision showing the lease, so either order keeps a live VM's slot held
(`formal/scheduling.qnt`: `startAttempt`, `releaseUnstarted`, `noVmWithoutLease`). The
attempts projection is v4: `capacity_refused_at` (the revision of the latest
`insufficient_resources` refusal, kept after a retry) holds the host even when the attempt
retried before the pass, and `retirement_requested` keeps the pass from asking again or counting
the attempt. A draining host's preparing, prepared and listening VMs are left out of the pool's
need (no over-retirement below `min_prepared`). Reservation ids carry host, pool and terms
revision; releases are issued before reservations. The simulation adds the data plane's real
capacity (refusals only when a clone does not fit it), `target_absent` vanishes, pool drains,
image pin changes, a read lag, an independent I03, a held-host check and a no-VM-without-lease
check, and a negative control (`should_find_violations_when_a_plan_ignores_admission`). A longer
seeded run in a slower lane is not wired: the weekly `formal.yml` lane has no `xtask` stage that
runs the fleet simulation yet (a follow-up: a `--seeds/--steps` flag beside `conformance`).

#### CP-04 Observation inbox and startup recovery
Cites: plan §10, §3.2; dm P1, P3; event-store §5.4, §6.3; ADRs 0025, 0026, 0037. Observations enter
through units with `InboxKey { data_plane, observation_id }` (stale generations and epochs recorded
as ignored); the startup use case P1: migration floor and stored-kinds checks, epoch floor (I17),
`StartRun`/`MintEpoch`, handshake, inventory with shim locks and `exit.json`, `Unknown` resolved
only by observation, classification, `AdmissionOpened{recovery_done}`, then `catch_up`.
Accept: component tests for each recovery row; safe mode admits nothing and deletes nothing; F35,
F42, F43 at component level.
Note (DEC-04F, ADR 0052's correction): the unit that maps `CapacityReported` into the `Host`'s
`ReportedCapacity` zeroes what each dimension listed in the report's `unknown` can deliver (its
`total` share, `vm_slots` or `macos_guests`), because `capacity::admissible` reads totals,
foreign and base use, never `free`; until then the control plane may propose in an unknown
dimension and be refused `capacity_unknown` by the data plane. Accept adds: a report with an
unknown dimension admits nothing there (component test).
Note (DEC-05): the handshake's `Welcome` carries `cp_clock`, the `Clock` port's reading when it
is written (ADR 0020's amendment); the link's mapping of an answer reads `target_absent` as the
`Attempt`'s `RefusalReason::TargetAbsent` (ADR 0066). Accept adds: a welcome carries the clock.
Note (CP-02): the handshake records each session in `mayfly_control::sessions::HostSessions`
(`open` with the welcome's epoch and mode, `set_mode` on entering or leaving safe mode): the saga
issues no command and the relay's sink sends nothing for a host without one. The saga's deadline
timers are memory: after adopting the attempts, recovery calls `AttemptSaga::reconsider` for each
so a directive still in flight gets its timeout again. The sink holds a directive decided under
an older epoch (`attempt_not_adopted`) until the attempt is adopted; recovery decides what an
adopted attempt's pending directive becomes (inventory first, plan §10) before that redelivery
sends it. The inbox's mapping of an answer to a `Fact` replaces the test stand-in `Cp::record`
of `crates/mayfly-composition/tests/directive_loop/support.rs`.
Note (KER-10F, corrected by KER-10G; the repair use case of P20): a recovery plan's `heads` are
the revisions of the streams it read, the attempts it acts on included, and the use case keeps
the aggregate's input honest through the store, not as a value the caller echoes: (1)
`ProposePlan` is issued under the run's own epoch with `bindings.epoch` equal to it (and
`ApprovePlan` in the same run); (2) a **plan-stream** unit (`BeginAction`, `CompletePlan`) carries
`Observed { heads: bindings.read_now(..), .. }`, the bound streams read **now**, and appends with
`HeadsRead::read_conditions()`, those **current** revisions, as its ADR 0041 reads (built inside
its own `with_revision_retry` closure): a stream that moves after the read refuses the unit and
the retry reads it again, and the aggregate compares the current revisions with the bound ones and
records `RecoveryPlanRefused{journal_heads}` when they differ. Conditioning these units on the
bound revisions would refuse forever the unit that must record the refusal. (3) Only an action's
**attempt command** unit is conditioned on the **bound** revisions, `Bindings::read_conditions()`
(the attempt's own stream as its expected revision), so it never runs on a moved world (the
model's `planActWith`). (4) `RecordOutcome` carries the attempt unit's receipt (stream, revision
before and after), never a recomputed digest. Accept adds: a component test in which another
writer appends to a bound attempt stream between an action's intent and its attempt command: the
command is refused, the next `BeginAction` (conditioned on the current heads) is accepted and
records the plan refused, and no further action executes; and one in which a command under a new
run epoch refuses the plan
(`formal/recovery.qnt`: `foreignWriteBeforeTheActionRefusesTest`,
`foreignWriteBeforeTheOutcomeRefusesTest`). Leaving safe mode by a plan passes the plan's epoch
and a verification taken after `RecoveryPlanCompleted`; the daemon then restarts
(`restart_required` refuses `CompleteRecovery` in that run). `MintEpoch.crash_loop` is the
service manager's start count of P25.

Note (CP-04, as built): `mayfly_control::start` (P2: the lease and the host's revision read in
one fold, `AggregateStore::load_with_head`, the unit conditioned on that revision),
`mayfly_control::inbox` (P8: `InboxKey{dp-<host digest>, <epoch>/<observation id>}`, since a data
plane restarted under a new epoch may reuse ids; one unit per target with a command id derived
from host, epoch, observation and target; `Ok` is the licence to ack; a refusal is handled and
consumes no key; an inventory becomes `VmPresent`, `VmObservedStopped` or `VmAbsent` per attempt
the `Host` leases, keyed `<epoch>/<observation>/<attempt>/<n>`, the host's leases being the
bounded, host-scoped index of attempts that may have a VM; `RunnerExited` and
`VmObservedStopped` routed by exact VM name among them; a capacity report zeroes each unknown
dimension), `mayfly_control::recovery` (P1: verify through the `StructureCheck` port,
`projection::prepare` before any writer unless a kind is unknown, `StartRun`, hello,
`MintEpoch` with the floor and the `crash_loop` input OPS-05 computes, a verified restart,
`CloseAdmission{recovering}` on a registered host, `Welcome` with `cp_clock`,
`HostSessions::open`; then per inventory: wait on an incomplete one, propose only in safe mode,
else adopt, record the inventory through the inbox, `RequestRetirement`/`Quarantine` for the
rows that ask, `AttemptSaga::reconsider` per adopted attempt, and, unless a listed VM said
nothing (`Recovery::Waiting`), `CompleteRecovery` and `OpenAdmission{recovering}`;
`enter_safe_mode` calls `set_mode`, `session_ended` calls `close`) and
`mayfly_control::recovery::repair` (P20 application: plan units read now and conditioned on the
current heads, the attempt command on the bound ones through `SaveRequest::with_expected` for its
own stream, an open intent resumed by re-running the attempt command so its receipt answers, the
outcome from the attempt unit's receipt, `LeaveSafeMode{plan_completed}` with a verification taken
after the completion). Left for later tasks: `DeleteLeaked` and `Report` rows are reported (no
attempt command deletes for a terminal attempt). `SuspendCleanup`: the model adopts the attempt
with its effect held (`hold`), but the `Attempt` aggregate cannot hold a directive in flight after
an adoption (the relay would send the pending delete again; `Quarantine` is refused while a
directive is in flight), so the attempt is left unadopted: the sink holds its delete, its lease
keeps its slot, recovery completes and reopens admission with it in `Recovered::held`, and
`Startup::resume_held` classifies it again on every later inventory (RT-02 calls it) until GitHub
reports the runner offline or gone (an unreadable GitHub keeps it held, CP-04H). The divergence
is recorded in `doc/formal-models.md`;
CP-04C closes it model first. A pass that waits keeps its counts for the one that completes. A
plan's quarantine adopts first, after checking purely that the quarantine would be accepted on
the adopted state. A shared
`inventory_ref` derives an adoption's reference from the inventory observation or the plan's
inventory digest (no inventory event); a plan's `report`/`ignore` actions have no command
(`Applied::NoCommand`: the drawer leaves them out); the listing carries no shim or `exit.json`
facts (a running VM's shim is read as alive); a reconnect's new epoch, the per-consumer
`catch_up` and the wiring are RT-02's; F42 (restart within the session TTL) needs the listener
and moves to GH-04; the capacity report's `base` is recorded as zero (CP-04B).

#### CP-04B Our non-VM use in the capacity report
Cites: ADR 0024, ADR 0052, plan §3.2, `doc/event-store.md` §6 (compatibility). The protocol's
`CapacityReport` does not separate our own use outside any VM (image files, bases, pulls in
flight) from `used`, so CP-04's inbox records `ReportedCapacity::base` as zero and the control
plane may propose a VM the data plane's ledger then refuses `insufficient_resources` (the host is
held until a newer report, CP-03). Add an optional `base: Capacity` to `CapacityReport`
(additive; a reader that ignores it stays correct), written by the data plane's ledger
(`images` + `bases` + in-flight pulls) in a release that only writes it, read by the inbox in the
next (prepare, then activate). Interim, until then: the inbox derives a conservative `base` as
`used − foreign − Σ` the footprints of the host's leases, floored at zero per dimension, so the
control plane over-estimates our own use and under-admits rather than proposing what will be
refused.
Accept: a report with `base` records it; one without it records the interim estimate, never
more admissible than with it (property); a session older than the field gets none (`written_in`).

#### CP-04C Hold an attempt's cleanup after adoption
Cites: plan §10 ("Deleting | present | active"), `formal/recovery.qnt` (`apply`'s `hold`,
`activeRunnerSuspendsCleanupTest`), `doc/formal-models.md` (recovery, "Code diverges from the
model"), ADRs 0025, 0034. CP-04 leaves a held cleanup's attempt unadopted, because the `Attempt`
cannot hold a directive in flight after an adoption. Model first in `formal/lifecycle.qnt`: an
adoption reads a directive in flight as `Unknown` (as `recovery.qnt` already assumes), and a
`HoldCleanup` command (with its release) records the hold that the saga's next step and the
relay's sink respect; then the aggregate, its matrix, goldens and determinism cases, and start-up
adopting a held attempt with its cleanup held, as the model does.
Accept: the model's scenarios pass with a negative control (a hold the sink ignores sends the
delete); a held attempt is adopted and no delete is sent until the hold is released, which
happens only once GitHub reports the runner offline or gone (an unreadable GitHub keeps it); the
divergence note in `doc/formal-models.md` is removed.

#### CP-05 Registration manager and reaper
Cites: dm §6.5, P9–P14; ADRs 0029, 0038. Mint JIT through the scale-set client once per
registration, record issued/unknown, deregister-before-stop with the 422 race, the online
confirmation after delivery, the reaper (run start, long outage, daily, `doctor`; ≤ 1 listing/hour;
only journaled ids; unrecognised reported). After an unknown issue, record `RecordAbsent` only
after a second listing by name past a grace period (GitHub's listing may lag a lost mint; KER-09).
Accept: F06, F07, F32 component tests; only journaled ids deleted; unrecognised reported; no
name-pattern deletion (`NameGuessingRegistry` fails the contract).
Note (DEC-06F, ADR 0076): P11 serves the quarantine stop too. It reads an attempt's
`StopDeregistrationRequested` as a retirement key (`registration::retirement_step`, the model's
`requestQuarantineStop`) and answers it with `RequestDeregistration{Retirement}`, then
`ConfirmRetirement` or `WithdrawRetirement` on the attempt, which inside quarantine records
`StopDeregistrationConfirmed` or `StopDeregistrationRefusedBusy`. Without it a quarantined VM
whose runner may be registered is never stopped. A quarantine during a pending retirement
re-keys it: the newer event is the key, and a busy answer to the older request does not withdraw
the stop (`quarantineDuringPendingRetirementBusyTest`). Accept adds: a component test in which a
quarantined VM's idle runner is deregistered and the VM then stopped, and one in which a busy
answer keeps it running.

Note (GH-03): `RunnerRegistry` reads by id and lists by exact name only; the reaper's listing of
every runner of the scale set (to report unrecognised ones) needs a list call this port does not
have yet (`GET {svc}/_apis/distributedtask/pools/0/agents` without `agentName`, spec §2): add it
here with its contract case. `RunnerInfo::online` is the runner's `status`, apart from `busy`
(client-assumed shape); `FakeScaleSetApi::set_runner_online` scripts it.

Note (GH-00R): a removal answering 204 means the runner is gone, not that mayfly removed it
(GitHub's 2022 design record: 204 also for an absent runner), so the outcome is recorded as
gone; busy is told by `typeName` (400 or 409). Ghost runners (spec §6.3): a runner whose own job
acquire failed is deregistered by GitHub while its process keeps listening, so a registered VM
whose runner id reads 404 is retired by mayfly's observation and the attempt's deadline, never
by waiting for the guest to exit.
#### CP-06 Breaker, quarantine and Unknown-escalation policies
Cites: dm P15, P16, P19; ADR 0025. Breaker counting and trips per (pool, digest); quarantine opening
(ERROR, metric, status), escalation levels T_q1/T_q2 that never act on the VM, no timer and no cap
deleting anything (I28); `Unknown` re-observation at T_u1/T_u2 and the provider-limitation report.
There is no operator assertion.
Note (DEC-05, ADR 0066): a `vm_vanished_externally` case opens like any other (ERROR, metric,
status) and says it is a desync to report; nothing resolves it but an operator.
Note (DEC-07, ADR 0066 amended): no `vm_vanished_externally` case opens any more; P15 counts
`VmVanished` as a pool failure (`CountAttemptFailure` with the attempt's digest, under the stage
of the phase it vanished in; CP-06 fixes that mapping), so repeated vanishes open the breaker,
and the desync logs at ERROR (a defect to report). Accept adds: a component test where two vanishes in the window trip
the breaker.
Accept: F36, F46, F47 component tests on the fake clock; mutant 26 (delete on a timer) and 27
(resolve without observation) caught.
Note (KER-10F): `QuarantineCase` trusts its inputs at three points the policies must check. (1) A
case is addressed as `quarantine-<attempt_id>` from the command's own `attempt_id` (the aggregate
refuses another attempt once a case exists, not a wrong stream). (2) `released_observed_absent`
names an observation id; before resolving, the policy loads that stored observation and checks
that it is a complete inventory (never `InventoryIncomplete`, I25), taken after the case's
`opened_at`, that does not list this attempt's VM; otherwise it refuses. (3) A
`delete_with_proof` resolution records the case's instance id (`QuarantineCaseResolved` v2); the
`DeleteVm` issued for it must carry exactly that id as its proof, and a resolution of v1 (no id)
falls back to the attempt's recorded one. Accept adds: a component test of each refusal (an
incomplete inventory, one older than the case, one that lists the VM) and one in which the
`DeleteVm` proof equals the resolved instance id.

#### CP-07 Image placement manager
Cites: dm P17, P18; ADRs 0030, 0039. Pin → placement on every host serving the pool; the pool is
admissible on a host only once its base is materialised; prepared attempts on an old digest
retired; unreferenced digests evicted; `WithdrawImage` refused while pinned; runner-version expiry
trips the breaker.
Accept: component tests on fakes; withdraw refused while pinned; never a clone from an OCI
reference; a pool's `vm_resources` disk becomes its placed base's measured disk (the image's
manifest, ADR 0052's amendment of 2026-11-12) when `clone_disk_gib` is not configured. Until
CP-07, such a pool is planned with 0 disk and the data plane's gate (I33) refuses what does not
fit: the backstop, pinned by RT-06's ledger tests.
Note (KER-11 review): the placement stream is keyed by the host and the digest's first twelve hex
digits, so the manager passes the full digest it read (`PlacementState::digest`) as its proof:
`RequestPlacement` with another digest of the same prefix is refused `placement_differs`
(`tests/placement/rules.rs::should_refuse_another_digest_when_it_shares_the_twelve_digit_prefix`).
`RequestPlacement` while the placement is under way or `Materialised` records nothing, whatever its
runner version: a runner refresh is `RequestRefresh`, never a second request.
Size: LARGE (production code in four crates, a durable consumer, the inbox, a model change).
Note (CP-07, scope): `mayflyd` admits a pool on its host only once three tasks have landed: CP-07
(the pin placed, the base materialised, `Placements` filled), CP-08 (P4: the pool's terms applied
on the host; without them `Host` refuses every reservation) and OPS-02 (an image registered,
verified and pinned from the pool's manifest; nothing in `mayflyd` pins one before it).
Note (CP-07, as built): model first, `formal/scheduling.qnt` gains each pool's pin and the bases
materialised on the host (`placedOn`, `materialise`, `repin`, `evict`), the invariant
`leasesKeepTheirBase` and two negative controls (`startWithoutPlacementViolatesTest`,
`evictWhileReferencedViolatesTest`). An attempt takes the pool's pin when it starts (P2), not at
its reservation, and starts only while that pin's base is materialised on its host: the start is
saved with a read of the placement stream (`start::Started::NotPlaced`, which means "wait for a
placement kick", never a release or a hot retry;
`tests/store/start.rs::should_start_nothing_when_the_pin_is_not_materialised_on_the_host`,
`should_refuse_the_start_at_commit_when_the_placement_moves_after_the_read`; the model's
`repinBetweenReserveAndStartTest`). An eviction is saved with a read of the host stream and of
every pool stream at the revisions its pass read, and a lease whose attempt the pass did not see
counts as referencing every base on its host (fail closed), so the reference check holds at the
commit (`tests/store/placement.rs::should_refuse_the_eviction_when_a_lease_is_reserved_between_the_read_and_the_save`,
`should_keep_every_base_on_a_host_when_a_lease_there_has_no_attempt_yet`; the model's `evict`
says why one atomic step is faithful). In safe mode the manager records only timeouts
(`should_record_only_timeouts_when_the_manager_is_in_safe_mode`). The admission check is the reconciler's, not `Host`'s (decided
2026-11-12, `Host` unchanged): `reconcile::HostView::placed` from the new `placements` projection; a pool whose pin
is not materialised on a host is carved nothing, told nothing beyond its live runners and reserved
nothing there (`tests/reconcile.rs::should_reserve_nothing_on_a_host_when_the_pin_is_not_placed_there`).
The pure pass is `placement::policy::plan` (request per host, the due directive, timeouts, eviction
of an unpinned base no live lease references, P18's trip); the driver is
`mayfly_control::placement::PlacementManager`, the relay the new `placement_relay` consumer
(`delivery::PlacementRelay`, held in safe mode), and the inbox records the answers on the placement
stream through the manager's `PlacementIndex`; a complete inventory resolves an `Unknown`
materialisation or eviction by the base's absence only (a listed base proves nothing, ADR 0025),
and an `Unknown` pull only by its late answer. **Limitation (recorded at the review, 2026-11-12):**
nothing on the link proves that a pull or a materialisation *completed*: the inventory lists a
base's VM but not that it is `Ready`, and no observation carries the data plane's image listing.
A placement whose pull or materialisation became `Unknown` (its answer lost, for instance over a
restart) therefore stays `Unknown`, and its pool not admissible on that host, until a late answer
arrives; no new directive is issued while it is unknown (P7), and no operator asserts its outcome
(ADR 0025). The fix is DP-08: a protocol minor carrying the data plane's image listing (pulled
images verified against their digests, `Ready` bases of the owned-base registry), from which the
inbox resolves it. A failed placement is not requested again (P17's
failure path); the operator path is OPS-06's `mayfly image refresh <pool>`, which requests the
pool's placements again, a failed one included (planned there), or a new pin. P18: `Pool`'s new `TripBreaker{digest, reason, at}` (`PoolBreakerTripped` v2 with
an optional `reason`; v1 upcast to `None`); the manager trips under a command id derived from
the pool, the digest and the expiry's instant, so an operator's reset is final for that expiry
(`should_leave_the_breaker_reset_when_an_operator_resets_it_after_the_expiry`). `withdraw_image` is refused while a pool pins the
image: the pools are listed on every try, and the withdrawal saved with a read of every pool
stream (`should_refuse_a_withdrawal_when_a_pin_commits_between_the_check_and_the_save`). `mayflyd` runs the manager and the relay, and
replaces `Placements` with this host's materialised bases after every pass. The measured disk
(owner, 2026-11-12, F3): protocol 1.4 adds `ImageMaterialised`'s optional `disk_bytes` (the data
plane answers a materialisation with the base's disk from its store's listing, measured from the
manifest), the journal's `ImageMaterialised` v2 records it (v1 upcast to `None`), the
`placements` row carries it, and `reconcile::vm_on` plans a pool whose disk is not configured at
its placed base's disk on that host
(`tests/reconcile.rs::should_cap_reservations_by_the_hosts_disk_when_the_placed_base_is_measured`,
`tests/store/placement.rs::should_cap_reservations_by_the_hosts_disk_when_the_placed_base_reports_its_disk`);
an answer without it (a 1.3 data plane) reads as unknown, and the clone is planned with 0 disk, the
data plane's gate (I33) refusing what does not fit
(`tests/store/placement.rs::should_plan_no_disk_when_the_materialised_answer_carries_none`). The
model's disk accounting needs no change for F3: it already plans every VM at a disk of its own
(`VM_DISK`), which the Rust plan now matches where it planned 0 before; the data plane reports the
base's disk from its listing (`mayfly-data`'s
`tests/executor/effects.rs::should_answer_the_bases_measured_disk_when_a_materialisation_is_applied`,
`should_answer_no_disk_when_the_listing_fails_after_a_materialisation`). The link matrix
gains `welcome_e6_v1.5` as the unspoken minor.

#### DP-08 Image listing on the link: resolve `Unknown` placements after a restart
Cites: dm P7, P17, §6.7; plan §3.2; ADRs 0025, 0039, 0087. Near the front of the queue: a terminal
Ctrl-C during a pull (RT-05a) leaves its outcome `Unknown` routinely, and until this lands such a
placement stays `Unknown` and its pool not admissible on that host (CP-07's limitation). A protocol
minor carries the data plane's image listing (the pulled images, each verified against its
digest, and the `Ready` bases of the owned-base registry, ADR 0087) in an observation, sent with
or beside the inventory; an older session gets none. The inbox resolves an `Unknown` pull from it
(the image listed: `RecordPulled`; a complete listing without it: `RecordNotApplied`) and an
`Unknown` materialisation (the base listed `Ready`: `RecordMaterialised`, with its disk; absent:
`RecordNotApplied`), never issuing a new directive while the outcome is unknown (P7) and never
asserting one (ADR 0025). Model first: the image store model (`formal/image_store.qnt`) or the
scheduling model's placement, whichever the resolution's safety rests on, with a negative control
(a listing read as completion before `Ready`).
Accept: over fake-tart, `mayflyd` restarted mid-pull resolves the placement from the listing
without a new directive, and the pool becomes admissible once the base is `Ready`; a listed base
that is not `Ready` resolves nothing; a 1.4 session is written without the listing.
Note (CP-08): the `mayflyd` part of this Accept needs a registry `mayflyd` can pull from, which
VER-09 builds; until then nothing serves `mayflyd` an image.

#### CP-08 Terms policy P4
Cites: dm P4, §6.2, §6.3; ADR 0024. Issue `ApplyPoolTerms` (`pool::terms`) to every host serving
the pool on any `Pool` event that bumps `terms_revision` (configuration, pin, pause, resume, drain,
a breaker trip or reset), keyed `(pool, revision)`, retried with backoff; an older revision is
refused by `Host`, and no gap can open admission. Today only the fleet test kit applies terms
(`World::apply_terms`), so under `mayflyd` `Host` refuses every reservation.
Accept: component tests on fakes (each terms-bumping event reaches every host, a stale revision is
refused, a tripped breaker closes the terms there); `mayflyd` over fake-tart applies the pool's
terms to its host: closed while the pool has no pin, and, once the pool is pinned (through the store
in the test until OPS-02), `PoolTermsApplied` at the pool's current terms revision, open. *Amended
at intake (2026-11-12): the Accept first asked for the pool opened once its base is materialised
under `mayflyd`; no test can serve `mayflyd` an image to pull yet (its pull fetches the manifest
over HTTPS), so that part is VER-09's.*
Note (CP-08, as built): model first, `formal/scheduling.qnt`'s `applyTerms` already was P4 as one
atomic step (the pool's current terms, only at a newer revision); its documentation now says why
one step is faithful, and the scenario `termsFollowEveryBumpTest` drives pause, resume, repin, a
trip and a reset through it (each applied at the pool's revision, never the same revision twice).
P4 is `mayfly_control::terms::TermsPolicy`, a level-triggered pass like P5 and P17 rather than a
durable consumer of pool events (domain-model §7's P4 row says so): it reads each pool's stream
for its terms (`pool::terms`, the authoritative `terms_revision`) and the `hosts` projection for the
revision each registered host applied, and saves `ApplyPoolTerms` where a host lags, under a
command id derived from the pool, the revision and the host, on the host stream with a read of the
pool stream at the revision it read, under the store's revision retry (a conflict on either
stream reads the pool again and saves at its revision then). So a host registered after the pool's last event gets its
terms (`tests/store/terms.rs::should_apply_the_current_terms_when_a_host_registers_after_the_pool_changed`),
terms a newer pool event superseded are refused at the commit and never applied, the retry
applying the newer ones (`should_apply_nothing_superseded_when_the_pool_moves_between_the_read_and_the_save`),
a host stream that moves meanwhile costs a retry, not the pass
(`should_apply_the_terms_in_the_same_pass_when_the_host_moves_between_the_read_and_the_save`), an older revision is
refused by `Host` (`should_refuse_a_stale_revision_when_the_host_holds_a_newer_one`), each bump
(configure, pin, pause, resume, reconfigure, trip, reset, drain) reaches both hosts
(`should_apply_every_bump_on_every_host_when_the_pool_changes`), a trip closes the terms and a
reservation under them is refused `terms_closed`
(`should_close_the_terms_on_every_host_when_the_breaker_of_the_pin_trips`), a repeated pass applies
nothing (`should_apply_nothing_twice_when_a_pass_is_repeated`), two passes racing record the terms
once, the later answered by the receipt
(`should_record_the_terms_once_when_two_passes_apply_the_same_terms`), and in safe mode a pass applies
nothing, as the reconciler and the placement manager decide nothing new
(`should_apply_nothing_when_the_policy_is_in_safe_mode`). What a pass leaves
undecided (retries spent, a store fault, a pool unread) it reports (`TermsReport::complete`), and
the next pass, on a kick on a pool or host stream (at least a second after the last pass) or the
one-minute interval, reads again. In safe mode a pass applies nothing and is complete whatever it could not read (a pool stream
ending in a kind this build does not decode is what raises safe mode mid-run;
`should_complete_in_safe_mode_when_a_pool_stream_holds_an_unknown_kind`). In `mayflyd` the ready
line's promise now includes one complete terms pass after the consumers' first catch-up (retried
with their backoff until complete, so a store fault that persists in normal mode holds the ready
line back, as it does a consumer's catch-up), and
the policy's own loop, on its kicks (`Notifier::terms`), starts only after the ready line, so the
two never race on the host stream
(`tests/daemon.rs::should_apply_the_pools_terms_to_its_host_when_the_pool_is_pinned`).
"Opens and reserves after a real materialisation under `mayflyd`" is VER-09's.

#### DP-01 Data-plane executor and observer
Cites: plan §3.1, §8.1, §9; ADR 0024. `mayfly-data`: ports `VmProvider`, `ImageStore`,
`GuestTransport`, `HostProbe`; executor applying the KER-08 FSM with per-directive deadlines and
bounded concurrency; the resource ledger fed by inventory and probes, refusing what does not fit
and emitting `CapacityReported` on handshake, change and request; inventory observer emitting
`Inventory`/`InventoryIncomplete`; in-memory fake provider with scripted faults in the data-plane
test kit (ADR 0019).
Accept: component tests: duplicate directive executes once; ambiguous provider error → `Unknown`;
restart rebuilds ledger and inventory from observation; an over-allocating CP is refused (I33).
Note (KER-08 review): `Ledger::apply_inventory` treats a complete listing as the truth and drops
every VM absent from it, so the executor serialises inventory listing against VM effects (a
listing that overlapped an effect is discarded, never applied); `HostProbe::foreign` counts only
non-`mf-*` VMs, since the ledger counts malformed `mf-*` VMs itself from the inventory.
Note (DEC-04F): the re-observation before a `StopVm`/`DeleteVm` passes the provider's
`Inventory` (complete or not) to `DataPlane::reobserved`, which reads absence itself (ADR 0053's
notes); the executor never builds an "absent" answer of its own.

#### DP-02 In-process link and link contract suite
Cites: plan §3.3. `mayfly-adapter-link::inproc` with bounded channels and serde round-trip; the
**link contract suite** (handshake, version refusal, at-least-once, ordering not assumed,
backpressure, close semantics) in the shared test kit (ADR 0019) as factory-parameterised functions.
Accept: suite green on inproc.

#### DP-03 `mayfly-fake-tart`
Cites: ver §7, D11. A `tart`-compatible CLI (`clone`, `run`, `ip`, `list --format json`, `stop`,
`delete`, `exec`, `--version`) over a state directory, with a fault file (delay, hang, fail before
or after effect, crash) read per invocation. Independent of production crates.
Accept: its own tests; architecture rule shows no production edge.

#### DP-04 Process runner, per-VM shim and Tart adapter
Cites: plan §8.2, D12; ADRs 0026, 0037, 0039. Process runner (argv, env allowlist, deadline,
process-group kill, bounded output, redaction; `TART_NO_AUTO_PRUNE` on every call); the
`mayfly vm-shim` subcommand (`setsid`, `tart run` as its child, stdio to bounded files never a
pipe, own lock, atomic `exit.json`) and re-adoption inputs; stop only by `tart stop`; clone only
from a local base, then `tart set --random-mac` and MAC read-back; Tart commands with version
probe, JSON parsing against version-pinned fixtures; inventory `Incomplete` on any parse or exit
failure; `HypervisorLimitHit`. **VmProvider contract suite** run against the in-memory fake, the
fake-tart binary, and (gated by `MAYFLY_REAL_TART_HOME`, macOS only, run by HW-01X's `cargo xtask
hw01`) real Tart.
Note (DP-01 review): the executor never drops an effect's future (`mayfly-data::ports`): each
call carries the adapter's own deadline and its future completes, `Ambiguous(Deadline)` at worst,
only once what it started is stopped and reaped; a `start` returns only once its shim is settled,
since `tart run` is never signalled (ADR 0037) and no `Drop` can run `tart stop`. `clone_vm`
onto an existing name and `start` of a running VM fail `Definite`, as Tart does.
Accept: contract green on both fakes; hang → deadline → `Ambiguous` + re-inspect; a daemon killed
and restarted re-adopts a running VM through its shim (F48 at integration level); no signal is ever
sent to `tart run`.
SEC-02 (ADR 0058): every child is built with a cleared environment plus an explicit allowlist
(`ARCH-child-environment`), so no `env:` secret of the daemon reaches `tart` or the shim; the
`VmProvider` contract reuses `mayfly-testkit-shared::child_env` for an EX-04-style case.

#### DP-05 Guest transport and runner launcher
Cites: plan §8.3, D13; ADR 0038. `tart exec` transport only (no SSH): nonce verification
(`VerifyGuest`), typed bounded claims, the fixed launcher contract with the JIT config over
`tart exec -i` stdin after the lock-PID = shim-child and instance-id checks, launcher status file
for runner exit after a restart, diagnostics export bounded and redacted. fake-tart `exec` emulates
a guest whose runner "runs a job" per its fault file.
Accept: secret never appears in argv/env/log (the fake's recorded invocations); a foreign lock PID
or a changed MAC refuses delivery (F39); `UncheckedLockTransport` fails the contract.
SEC-02 (ADR 0058): `tart exec` children get a cleared environment plus an explicit allowlist
(`ARCH-child-environment`); the `GuestTransport` contract adds the `child_env` probe.
Decided (DP-05, ADR 0082, accepted by the owner 2026-11-11): the data plane takes the JIT config
by handle from an in-memory `LaunchStore`, bound to its VM, instance id and deadline, which the
link's data-plane end fills (still to be wired, RT-02); `LaunchRunner` carries the recorded
instance id from protocol 1.3; the launch exec holds a MAC handshake before the config; the
empty `mayfly-adapter-guest` crate is removed. The launcher's contract and Tart's run lock
(`vms/<vm>/config.json`, `F_GETLK`) are pending HW-01 on real Tart (`cargo xtask hw01`, HW-01X).

#### DP-06 Host resource probes
Cites: plan §9; ADR 0024. Disk free for the provider's home, available memory, boot id, clock gap
(wake), `#[cfg]` per OS, via an allowed crate (decide `rustix`/`sysinfo` in the commit; no
`unsafe`); readings feed the resource ledger, never the CP directly.
Accept: Linux test; macOS compiles in CI.

#### DP-07 Image cache directives and `ImageStore` port
Cites: dm §6.7, §9.3; ADR 0039. `ImageStore` (pull by digest, re-hash, materialise a base named by
digest and runner version through boot-once and runner install via `tart exec`, evict an
unreferenced base, list bases) with its contract suite on the fake and fake-tart; base deletion
refused by identity; single-flight pull per host.
Accept: contract green on fake and fake-tart; `TagPullStore` and `BaseEvictingStore` fail it.
Note (DP-04 review): `TartProvider::clone_vm` checks only that the base is a local Tart VM named
`mfb-…` (`tart get`), not that the service owns it (ADR 0039: "local, service-owned"); the record
of which bases this deployment materialised is DP-07's, and so is the check that `CloneVm` names
one of them. DP-06/DP-07 also decide how a base listed by `tart list` is counted, so its disk is
not counted twice (once as an image base, once as a stopped VM in the inventory).
Note (DP-06): the host probe's foreign use (`mayfly_data_domain::foreign_use`) skips every
`mfb-*` name, leaving bases to the image store's listing, so `ImageStore::list` must name every
`mfb-*` VM the provider lists (one it leaves out is counted nowhere), with its disk.
As built (DP-07, ADR 0087, revised after the owner's review): mayfly owns a private
`TART_HOME` checked at start-up with its ancestors (root and the daemon's user are trusted, nobody
else may write there); a durable registry of bases (`mayfly-adapter-tart::bases`: `base_id`, full
source digest, Tart and runner versions, recipe, state, file identity, final fingerprint, MAC as a
secondary check, clone references and intents) fails closed and is never rediscovered by name;
each base moves Reserved → Cloning → Customizing → Verifying → Ready → Retiring (+ Quarantined),
each state written before its step, with crash recovery by observation; `TartProvider::clone_vm`
clones only from Ready after revalidation (VP-17), and eviction protects mayfly's own clones only
(no Tart lineage), and never deletes a Quarantined base. Every image is verified from its raw
manifest fetched by digest over HTTPS with a size cap,
never Tart's cached copy (re-serialized by 2.28.0, as served by 2.41.1, unchecked by both): the layout (disk v2 only), config and NVRAM
against their descriptors, every disk chunk against its uncompressed digest, on the cache and
again on the fresh clone before its first boot; one retry, then refused. Signature policy is
recorded per source and `required` refused until SIG-01. `list` names only Ready bases that pass
the clone proof; every other `mfb-*` VM is counted by the inventory (DP-06F). Model:
`formal/image_store.qnt` (driver: VER-08). A drift is answered `effect_failed`;
`ImageDriftDetected` needs a protocol observation.
Note (DP-07F): the start-up check refused every symlink on the path, and macOS reaches `/var`,
`/tmp` and `/etc` through root's links into `/private`, so it refused every temporary directory
and every deployment under them (the macOS CI job, red since DP-07). `verify::check_path` now
walks the path from `/`: a link on the way is followed only when root or the daemon's user owns
it, in a directory that passes the ancestor rule, with no `.` or `..` in its target and at most
`verify::MAX_LINKS` followed, and its target is walked by the same rules; `TART_HOME` and the
registry's directory are still never links. The daemon keeps the configured path (every link and
directory on it is writable only by trusted users, so its resolution cannot change outside the
trust model; a later canonical path would be a second, unchecked resolution). The secrets walk
(`mayfly-adapter-secrets::path`) already followed only trusted links but let the kernel resolve
their targets; it now walks a target's components too. The tart tests reach their tempdirs
through a link (`support::LinkedDir`), so Linux CI catches a check that refuses one. Pinned by
`tests/tart/images.rs::should_follow_only_trusted_links_when_checking_a_private_directory` and
`tests/contract.rs::should_check_every_directory_a_trusted_link_leads_through_when_walking_to_a_secret`.
As built (HW-01X): the runner packages are pinned per version and platform
(`ImageConfig::runners`: version → `linux-arm64` / `osx-arm64` → SHA-256), because the runner
ships one package per platform; the image's guest OS (its cached `config.json`) selects the pin
before anything is created, and a platform with no pin is refused `Definite` (pinned by
`tests/tart/images.rs::should_pick_the_pin_of_the_image_guest_when_a_version_has_one_per_platform`).
An implementation detail of ADR 0087's pinned runner packages, not a new decision; no released
config held the per-version shape.

#### SIG-01 Image signature verification for pools that require it
Cites: ADR 0087 decision 8, dm §9.3, ADR 0033. DP-07 records each source's signature policy
(`digest_only`, `optional`, `required`) and refuses `required`. Verify a signature over the pinned
digest before the pull, against a trust anchor in the pool config (a public key by secret
reference, or a certificate identity and issuer): a sigstore client crate (licence per `deny.toml`)
or a `cosign` child under the process rules (argv, cleared environment, deadline); record the
result next to the source digest; `optional` checks when a signature exists. The default path
stays digest-only and works for unsigned images.
Accept: `required` with a valid, an invalid and a missing signature (fixtures, synthetic keys);
`optional` without a signature passes and records it; no key material in any log or record.

#### NET-01 Softnet privilege model
Cites: ADR 0032, plan §8.2, §13. Design and ADR choosing between Tart's softnet behind a narrow
`sudoers` rule and a privileged helper; the daemon stays non-root; `run` options for Softnet in the
Tart adapter; `mayfly service install` and `doctor` set up and check the chosen path; SEC-01's
reachability tests run with and without Softnet (hardware lane).
Accept: the ADR (owner decides); `doctor` fails on a missing or too-wide privilege; argv golden for
Softnet runs.

#### RT-01 Config and state-dir lock
Cites: plan §11, §13; ADRs 0027, 0028, 0033, 0040. TOML schema v1 with validation errors that name
the field: pools with `tier` (only `trusted`), `min_prepared`, `max_prepared_age`, breaker N/W;
`[github]` organisation scope (repository scope allowed for testing); secret values only as
reference URIs (parsed here, resolved by SEC-02); refused keys `allow_untrusted_prs` and a classic
`mode` with a message; `flock` on the state dir (second instance exits with a distinct code).
Accept: config golden examples (valid/invalid, including each refused key).

#### SEC-02 `SecretProvider` port and backends by URI
Cites: ADR 0033, ADR 0014, plan §7.1, §13. The port in `mayfly-control`; adapters in
`mayfly-adapter-secrets`: `keychain:` (macOS), `file:` (owner-only `0600` file in a `0700` dir; no
symlink, absolute path), `env:` (read once, never passed to a child), `exec:` (argv only, no shell,
deadline, bounded output); re-resolution on refresh; `doctor` resolves each reference without
printing it. The `SecretProvider` contract suite (ver §5) with `PermissiveFileProvider` and
`ShellExecProvider` failing it.
Accept: contract green on memory, file, env and exec; Keychain compiles on macOS and runs in
`cargo xtask hw01` (HW-01X); no secret in any `Debug` output.

#### RT-02 Composition and `mayflyd`
Cites: plan §3, §10, §4. Wire both planes with the inproc link; supervision tree with a root
cancellation token; startup sequence of plan §10; SIGTERM → drain-safe shutdown; `--role all`.
Accept: integration test starts `mayflyd` against fake-tart and a static demand, kills it, restarts it, and the inventory converges.
DEC-01: registers the store's consumers, takes the owner lease, runs event-store §6.3 then
`catch_up`, owns the kick wiring and timer; secrets through SEC-02; foreground mode with
Ctrl-C → P24 (ADR 0031).
SEC-02: build the secrets with `UriSecretProvider::from_process(references, deadline)` (it
captures `env:` names and withholds them from every child it runs); any child composition
spawns clears its environment (ADR 0058, `ARCH-child-environment`); `doctor` calls
`mayfly_control::secrets::check_references`.
CP-02: run `AttemptSaga` and `DirectiveRelay<LinkSink<..>>` as consumers (`catch_up` and kicks
each), a timer loop sleeping until `AttemptSaga::next_due` then calling `expire_due`, the
`DirectiveDetails` from the pool config and image placement (a nonce per directive id, the same
on every resend), the relay's `send` deadline through `delivery::send_deadline`, and one
`HostSessions` shared by the handshake, the saga and the sink. The per-kind deadlines are
`SagaConfig` (defaults; P6's per-guest-OS deadlines are config to add).
As built (RT-02, ADR 0091, Proposed): `mayfly_composition::daemon` runs `mayflyd --role all`
(`--role control|data` exits 64 until LNK-01; `--config` is required, OPS-01 passes it).
Places (ADR 0091 decision 5, owner 2026-11-12): `state_dir` and `tart_home` are optional; the
defaults are `mayfly_composition::paths` (macOS `~/Library/Application Support/mayfly` with
`TART_HOME` at `<state>/tart`; Linux `$XDG_STATE_HOME/mayfly` and `$XDG_DATA_HOME/mayfly/tart`),
a configured `state_dir` alone puts `TART_HOME` at `<state_dir>/tart`; the configuration
overrides every path; RT-02 writes no logs. Both places are held to ADR 0087's layout rule
before anything is written (`paths::prepare_private`): `verify::check_path` on each (every
ancestor owned by root or the daemon's user and not writable by others unless sticky, a link on
the way only when trusted, the place itself never a symlink, so a symlinked `state_dir` that
RT-01's lock followed is now refused), then each missing level created alone `0700` by `mkdir`
and `verify::check_private` on it, all before the state-dir lock (exit 73 on a refusal). The
shims, diagnostics and image directories are created the same way. The host id is read without
following a link: a dangling `host-id` link, or one a permission error hides, is refused, never
regenerated.
Start-up: the state-dir lock (the owner lease on SQLite); the host id kept in
`<state_dir>/host-id` (ADR 0091 decision 1); the journal `<state_dir>/journal.db` opened with its
`StructureCheck` (`PRAGMA quick_check`); the data plane from observation only: the epoch floor
`<TART_HOME>/mayfly-epoch-floor.json` (`FloorFile`; an unreadable one is refused, and a missing
one is refused while the host holds a recorded base or an `mf-*`/`mfb-*` VM, ADR 0026), one
shared `BaseRecords` at `<state_dir>/images/bases.json`, `TartImageStore::recover`,
`RegistryManifests` with the configured realm hosts, `OsProbe` over its own `TartProvider`,
shims under `<state_dir>/shims`, diagnostics under `<state_dir>/diagnostics`, `TART_HOME`
created `0700`, `tart` the first on `PATH`, `mayfly` beside `mayflyd`,
the daemon under umask `077`, children given only the allowlisted environment. Then per run:
`Startup::open` (ES-04's `prepare` with a `BuildId` of the version), the configured pools
applied (`ConfigurePool` only when one changed: the static demand), `ReportInventory` sent
outside the journal (ADR 0091 decision 4), every inventory to the run's one `Startup` until
`recover` completes, every observation through the `Inbox`, `resume_held` on each later
inventory while one is held; after recovery `catch_up` per consumer (saga and relay, each with
its own worker and `Kicks`), retried with a doubling backoff (0.5 s to 30 s) until both return
`Ok` or the run is cancelled, then the ready line on stdout, `mayflyd ready host=<id> epoch=<n>
mode=<normal|safe>` (its promise: this run's recovery finished and every consumer caught up
once); kicks drained, a 60 s coarse timer, the saga's `next_due` timer calling `expire_due`, the
reconciler with `reconciler_policies` ticking on its kicks and `next_due`, the SQLite ledger on
an `AnchoredStoreClock` (wall at start plus monotonic), the runtime clock as each worker's
`DeliveryTimer`, the relay's `send` bounded by `send_deadline`, one `HostSessions` per run shared
by the handshake, saga and sink, `DirectiveDetails` from the pools (`ConfigDetails`: run options,
a keyed nonce per directive id, `Placements`), a mid-run unknown kind entering safe mode. A link
that ends is a run that ends (ADR 0091 decision 3). SIGTERM and SIGINT both mean P24 until RT-05
gives Ctrl-C its terminal-mode stop: loops end between units of work, `BeginShutdown`/`EndRun`,
VMs left under their shims, the loops bounded at 25 s and the runtime's shutdown at 5 s more.
Config: `[images] repository, runners, realm_hosts` (required by `mayflyd`, ADR 0091 decision 2)
and `[host] vm_slots, macos_guests (at most 2), image_disk_gib, clone_disk_gib`, optional in the
schema with no default and required by `mayflyd` (ADR 0052: exit 78 naming each missing key);
*optional and measured since RT-06 (ADR 0052's amendment of 2026-11-12).*
Pinned by `crates/mayfly-composition/tests/daemon.rs`: start over fake-tart, SIGKILL, restart,
the unrecorded VM reported, nothing deleted, the floor at the last epoch, the pool configured
once, a second daemon 69, SIGTERM exit 0, the journal `0600`; a link lost after recovery gives a
second ready at a newer epoch and records run 1 unclean, one lost before recovery exits 1 (the
`SessionCut` seam); a missing floor beside an `mf-*` VM exits 1; a missing role, `[images]` or
sizing key is refused; with no place configured, the platform's places are created `0700` under
an injected `HOME`; a state directory under a `0777` ancestor exits 73 with nothing written. Run as non-root too: `setpriv --reuid=65534 --regid=65534 --clear-groups
env -i PATH=/usr/bin:/bin HOME=/tmp TMPDIR=/tmp <test binary>` for `tests/daemon.rs` (7 tests),
`mayfly-composition`'s unit tests (22, `identity` and `paths` among them) and
`mayfly-adapter-tart`'s `floor` tests (4), from a world-readable copy of the binaries (the session
scratchpad is root-only): all passed. `env -i` because the sandbox's `SSL_CERT_FILE` names a
root-only file, which the in-process tests' registry client cannot read as another user. Left to their tasks: GitHub's view in
recovery (none is passed: every runner reads `unreachable` and a held cleanup stays held; GH-04,
CP-05), the secrets provider built (`UriSecretProvider::from_process`) but read by no client yet
(GH-02), the `LaunchStore` shared with the guest transport and filled by no link end yet (CP-05,
ADR 0082), `Placements` filled by CP-07 (until then a clone is held `image_base_unknown`), P4's
pool terms on the host (CP-08, built; no pool is open before an image is pinned, OPS-02), `crash_loop` (OPS-05), the
operator socket (RT-03), logging's subscriber (RT-04), Ctrl-C's terminal mode (RT-05), and per-OS
stop graces (a fixed 30 s).

#### RT-03 Operator socket API and `mayfly` CLI
Cites: plan §11. Unix socket `0600`, JSON request/response, commands of plan §11 that exist so far
(`status`, `vm list`, `events --follow`, `pool drain|resume|pause`, `recover --dry-run|--apply`,
`config validate`, `doctor` basic).
Accept: `assert_cmd` black-box tests; `--json` outputs are golden-tested.
DEC-01 adds the CLI of plan §11: `host drain|resume`, `pool reset-breaker`, `quarantine
list|inspect|resolve`, `safe-mode enter`, `events --follow` per event-store §5.6; peer-uid check;
an actor on every mutating command. No `attempt assert` (ADR 0025).
Note (DEC-05, ADR 0066): `status` and `quarantine list|inspect` name `vm_vanished_externally` in
plain words (the VM vanished outside mayfly: a desync; send the diagnostics bundle).
Note (DEC-07, ADR 0066 amended): a vanish is no longer a quarantine: `status` and `vm
list|inspect` show the `attempts` view's `desync` (how it was found) on the lost attempt in the
same plain words.
Note (KER-10F): only the socket layer constructs `Actor::Operator { uid }`, from the peer
credentials of the `0600` socket (`SO_PEERCRED`/`getpeereid`); nothing a client sends names an
actor. `quarantine resolve --release` passes an observation id the CP-06 policy verifies (see its
card). Accept adds: a test that a request claiming an operator in its body is decided under the
peer's uid, and that a non-socket caller (a policy, startup) cannot obtain an operator actor.

Note (RT-02, ADR 0091 decision 5): the socket's default place, when the configuration names
none, is `$XDG_RUNTIME_DIR/mayfly.sock` on Linux and `<state dir>/mayfly.sock` on macOS
(`mayfly_composition::paths` holds the other defaults).

#### RT-04 Observability
Cites: plan §12. tracing JSON subscriber in composition, event naming rule, metrics endpoint
(socket or loopback), redaction test feeding secrets through every log path.
Accept: metric names and labels golden-tested; no id-valued label.
DEC-01: the metrics of dm §8 (loopback HTTP endpoint), the quarantine ERROR event, nothing sent
anywhere (ADR 0036).
Note (DEC-07, ADR 0066 amended): a counter of vanished VMs, `mayfly_vms_vanished_total{pool,
discovered_by}` (`inventory`, `stop_vm`, `start_vm`), fed by `VmVanished`; the control plane has
no metrics port yet, so DEC-07 adds none.

Note (RT-02, ADR 0091 decision 5): log files, if any, default to `~/Library/Logs/mayfly` on
macOS and `$XDG_STATE_HOME/mayfly/logs` on Linux (`mayfly_composition::paths`); RT-02 writes none.

#### RT-05 Terminal mode: a clean stop on Ctrl-C
Cites: plan §10 (P23 drain, P24 shutdown), §11; ADR 0031; ADR 0087 decision 1. The owner runs
`mayflyd` from a terminal as their own user (the primary deployment) and stops it with Ctrl-C.
P24 leaves VMs running under their shims for a service restart; in terminal mode Ctrl-C means
"stop working here": close admission and stop acquiring, retire Prepared and Listening attempts
(deregister first, P11), let Busy jobs finish up to a configured bound (a second Ctrl-C stops
waiting), stop and delete our VMs with their ownership proof, finish durable writes, release the
state-dir lock and exit; never touch a base or a foreign VM. The choice between P24 and this stop
is the run mode's, decided by the owner (an amendment to ADR 0031).
Accept: integration test over fake-tart and fake-github: Ctrl-C with an idle, a Prepared and a Busy
attempt ends with no runner registered, no `mf-*` VM left and the bases untouched; a second Ctrl-C
during the wait stops at once and leaves what remains recorded for the next start.

#### RT-06 Measured host sizing
Cites: ADR 0052 (amended 2026-11-12), ADR 0024, ADR 0087, plan §9, §11. The owner (2026-11-12):
usable out of the box with reasonable defaults, host sizing measured, not guessed. `[host]`'s
sizing keys become optional (no rename; a configured value overrides): `macos_guests` is 2,
Apple's limit, and a configured value is capped at 2; the image and clone disk ceilings are each
image's verified manifest's disk (the disk layers' `org.cirruslabs.tart.uncompressed-size`
summed), unknown until measured (`capacity_unknown`); `vm_slots` is derived from the host probe's
RAM and CPUs less a reserve (`reserve_memory_mib`, measured as max(25 % of RAM, 8 GiB) when
absent; the new `reserve_cpus`, default 2), divided by the largest memory and the most CPUs of
any pool (the safer divisors: the ledger does not track CPUs) and bounded by both; a probe that cannot read them leaves the value unknown. The start-up report
shows each value and its source until RT-03's status takes it.
Accept: unit tests of the measured slots on a scripted probe (64 GiB / 10 CPUs with 8 GiB,
2-CPU VMs → 4), the reserve configured, unreadable CPUs → `vm_slots` unknown, manifest-derived
disk ceilings and a pull measured before admission, configured values overriding,
`macos_guests` capped at 2, and the start-up report's sources; the `ImageStore` contract's
IC-15 on the in-memory store and the Tart adapter over fake-tart, with a guessing store failing it.
As built (RT-06): the probe (`mayfly_adapter_host::{vm_slots, reserve_memory}`) measures on
every reading, reading the CPUs only when slots are not configured; unreadable CPUs report
`vm_slots` unsized through the existing `foreign_vm_unsized` cause (owner, 2026-11-12: no new
protocol cause). `ImageStore::measure` fetches the manifest by digest and checks its hash before
any layer is pulled ("verified", owner 2026-11-12); the executor measures a `PullImage` whose
ceiling is neither configured nor known before admitting it, and the listing carries each
image's and base's disk from the manifest kept at its pull (`MeasuredDisks`, with each base's
image); a listed image whose kept manifest no longer reads is measured again on every observer
pass, and stays unknown (`images_not_read`, never dropped, which would undercount) until it
measures. A `PullImage` is fenced (session, version, epoch, duplicate, deadline, safe mode)
before its manifest is fetched. On Linux the probe's memory is bounded by the cgroup v2
`memory.max`, as its CPU count is by the quota. `mayflyd` writes
`mayflyd sizing <key>=<value> source=<configured|configured_capped_at_apple_limit|measured|manifest|apple_limit|default>`
lines to stderr at start-up, with ` reason=cpus_unreadable` (or `probe_unreadable`) on an
unknown value and one `unmeasured_image=<digest> reason=manifest_unreadable` line per image
that did not measure (`mayfly_composition::sizing`); stdout keeps only the ready line.
Until CP-07 places a pool's image, a pool without `clone_disk_gib` is planned with no disk and
the data plane's gate refuses what does not fit. Model: none, an input-side change: the
scheduling model takes slots and disk as given, and unknown capacity refuses through the
`capacity_unknown` path it already models. Also in this commit, the owner's accepted amendment of
ADR 0087 decision 1 (DP-07F's trusted symlinks), recorded in the ADR.

#### E2E-01 ⛳ Walking skeleton
Cites: plan §14 phase 3, ver §2. Black-box: `mayflyd` (static demand 1) + `mayfly-fake-tart` +
fake guest runner completes one "job", VM deleted, slot released; then a demand of 3 with 2 slots
never exceeds 2; then kill -9 mid-clone and restart converges without duplicates.
Accept: these tests in the e2e level (the `e2e-tests` stage, registered here), green in CI on Linux.

### Phase 4 — GitHub

#### GH-00R Settle the scale-set spec from public evidence
Cites: `doc/github-scaleset.md` (its sources, §1–§12), dm §9.1, `formal/scaleset.qnt`
(A1–A6), ADRs 0027, 0040, 0045, 0048, 0069. The owner decided on 2026-11-11 to settle the spec
from public evidence before any live session: mayfly reads `actions/scaleset`, ARC,
`actions/runner` and GitHub's documentation and uses what they show, as ADR 0048 says: read,
never port (no code, test, test vector or recorded payload is copied). The input is the GH-00R
research, a report of findings with their citations; the output is the spec, not the report.
- **Every open item answered or listed.** For each question of §12 and each item marked
  [unverified: GH-00] elsewhere in the spec, the spec records the answer, its evidence strength
  and a citation. Strengths: **server-observed** (a public source shows the service's own
  response: a recorded payload, a status or error body quoted from a real run), **client-assumed**
  (what a reference client sends, expects or tolerates: evidence of what the service accepts
  from it, not of what it does), **doc-stated** (GitHub's documentation says so). A citation names
  the repository, file and function or lines at a pinned commit (the spec's sources table pins
  each), or a documentation page with the date it was read. The header's marks follow: the three
  strengths replace [unverified: GH-00], and **[open: HW-02]** marks an item no public source
  settles.
- **Client-assumed stays an assumption.** A client-assumed answer remains a named assumption, a
  fake knob and a test name (ADR 0048), marked client-assumed wherever it is used: the spec,
  `formal/scaleset.qnt`'s A1–A6, GH-01's knob and the test that names it.
- **New behaviour joins the spec.** What the research finds that the spec does not yet state is
  added with its strength and citation. A finding that changes what the scale-set model allows or
  assumes (an A1–A6 refuted, a new message or error class) changes `formal/scaleset.qnt`, its
  scenarios and the conformance driver first (rule 7), then the spec and the cards that follow.
- **§12 becomes the HW-02 remainder:** the items without server-observed evidence that a live
  session can settle, each with what to capture, for GH-00. Item 6 (whether
  `X-ScaleSetMaxCapacity` caps `totalAssignedJobs`) stays on it, and `capacity_policy` keeps
  the default `exact` (ADR 0069) until item 6 is server-observed, whatever a client assumes or a
  page states.
- **The record follows.** ADR 0048 gets a dated amendment (owner, 2026-11-11): each fact carries
  its evidence strength, and "until GH-00's capture settles it" becomes "until it is
  server-observed"; plan §7.4, ver §7.3, dm §9.1 and `doc/formal-models.md` follow where they say
  the capture answers an item.
Accept: no §12 question and no item formerly marked [unverified: GH-00] is left without either a
citation with its strength or an explicit entry in the HW-02 remainder list; no upstream code,
test or payload in the repository (or each listed in `THIRD-PARTY-NOTICES`, ADR 0048); `cargo
xtask check --only docs` green, and the formal stages and conformance replay when the model
changed.
Note (GH-00R, done; review fixes 2026-11-11): the spec carries each fact's strength and citation (§1–§11), §12 is the
HW-02 remainder (eleven items), §13 the drafted App permission list. Design-changing findings went
model first into `formal/scaleset.qnt` (both flows, direct assignment capped by `maxCapacity`, the
trust gate of ADR 0090, a restarted numbering recovered, a dormant scale set re-created) and into
the cards GH-02H, KER-06G, GH-08, GH-09, GH-10 and GH-07 and the notes on GH-01, GH-03, GH-04, CP-05, SEC-01,
E2E-02 and VER-02. Ids were already `u64`; in code only doc comments changed (`demand` marked
experimental, the HW-02 item numbers).

#### GH-01 `mayfly-fake-github`
Cites: `doc/github-scaleset.md`, ver §7.3, ADRs 0012, 0028, 0040, 0048. An Axum fake built from
the spec as GH-00R settles it, never from upstream's tests, test server or fixtures (ADR 0048),
importing no production crate: the auth chain (installation token, registration token as an
auth step only, runner-registration with short-lived admin JWTs signed by a key generated at
test time), scale-set lookup and create, one live session per scale set (409 on a second),
refresh and delete, the long poll with a 202 hold and redelivery until delete, the 50-message
cap, statistics, acquire, JIT, runner get/list/remove, and a scripted job-queue control API for
e2e. Fault knobs: 401, 403, 409, 422, 429 with `Retry-After`, a rate-limited 403, a response
dropped after its effect, a stalled poll, session expiry. Every behaviour GH-00R leaves
client-assumed or open is a named knob or assumption until it is server-observed (GH-00, in
HW-02's session). A contract suite for the `ScaleSetApi` and
`RunnerRegistry` wire behaviour runs against the fake and, owner-gated, against a live
disposable organisation (HW-02).
Accept: its own tests; the contract suite green against the fake, and its own tests prove a
deliberately broken fake fails it (ver §5); the live run listed as pending (owner).
Note (GH-00R): the fake serves both flows of spec §6.1, selected per fake: github.com's direct
assignment (a `JobAssigned` with `runnerRequestId` 0, no offer, nothing assigned beyond the last
poll's `maxCapacity`, so 0 assigns nothing) and GHES's offers with acquire and `acquirablejobs`
(200 or 204); release and requeue of an assigned job no runner takes, three times, then failed;
409 `RunnerScaleSetSessionConflictException` on a second session; a refresh answering 400
`RunnerScaleSetSessionExpiredException`; a 200 with an empty body; runner removal answering 204
for an absent runner and 400 or 409 `JobStillRunningException` for a busy one (knob); 429
`ConfigureTooManyRunnersException` on JIT; dormant deletion of a scale set (404 after); a
restarted message numbering; and the REST workflow-run lookup and cancel routes of ADR 0090 (a
run with `head_repository` differing from `repository`, a lookup that fails, a cancel answering
202, 409 or lost), the fork-PR setting route and the runner-group read of the public-repository
report (spec §2, §6.2). Each client-assumed or open behaviour is a named knob.
Note (GH-01, done): `Knobs` names every client-assumed or open behaviour with its HW-02 item;
`Fault` covers the status overrides, `Retry-After`, the rate-limited 403, the 200 with an empty
body, a stall, a response dropped after its effect and a delay; the control API (`/_fake/`, and
the same as methods) queues, starts, completes, releases and cancels jobs, expires a session or
its token, deletes a dormant scale set, restarts a numbering and dumps the state without tokens.
The wire contract is `mayfly_fake_github::contract` (WC-01..WC-15, ver §7.3); its live run needs
a `WireWorld` over a TLS client and the App's credentials, which HW-02 adds (owner, pending).

#### GH-02 GitHub App auth and HTTP client
Cites: `doc/github-scaleset.md` §1, §2, §8, plan §7.1, ADRs 0014, 0033, 0045, 0048. The App JWT
(RS256, backdated issue, ≤ 10 min) from a `SecretProvider` key, the installation token, the
registration token and the runner-registration exchange to the admin token and service URL,
with lazy single-flight refresh 60 s before expiry; reqwest + rustls; the retry policy of spec
§8 as plain values (transport errors, 429, 5xx except 501; exponential backoff with jitter from
the randomness port; `Retry-After` honoured; rate-limit classification of 403 and 429), every
request with a deadline; request metrics with bounded labels; error text without query strings
or bodies.
Accept: tests against fake-github under `tokio::time::pause`; no token in Debug output or errors;
admin-token refresh exercised by a short-lived token.
ADR 0045: the client reads the installation's granted permissions so `mayfly doctor` can warn when
they exceed the published minimal list (the list is drafted by GH-00R from GitHub's documentation
and verified by GH-00/HW-02).

#### GH-03 Scale Set client
Cites: `doc/github-scaleset.md` §2-§7, plan §7.2, §7.4, ADRs 0040, 0048. The `ScaleSetApi` and
`RunnerRegistry` port implementations in `mayfly-adapter-github` on GH-02's client: scale set
lookup and create (never delete, ADR 0040), session create, refresh on 401 and delete, the long
poll with `lastMessageId` and `X-ScaleSetMaxCapacity`, message decoding in arrival order with
unknown types kept as protocol drift, ack, acquire returning the observed acquired set, JIT
generation, runner get/list/remove by id. Recorded, redacted fixtures
(`tests/fixtures/github-scaleset/`) are the contract: synthetic and labelled so until GH-00's
capture replaces them, replayed by a fixture replayer and run against fake-github.
Accept: session expiry, 409 on open, 429, ack after a lost response, partial acquire, JIT name
collision and busy-runner removal cases.
DEC-01: organisation scope (repository scope for tests); scale sets never deleted by the client
(ADR 0040); runner list/get/delete by id for CP-05.
Note (GH-02 review): `messageQueueUrl` receives the queue token, so it passes the same rule as the
Actions service URL and the configured bases (`credential_base_problem`: https, or http only to
loopback; no query, fragment or user info) before any call, refused as protocol drift otherwise.
Note (GH-00R): classify errors by `typeName` only, never by status (spec §2); a refresh answering
`RunnerScaleSetSessionExpiredException` is a lost session, created anew, never refreshed again;
a 200 whose body is empty or undecodable is a transient poll failure (no record, no ack); a
remove answering 204 is "gone", not proof that mayfly removed it; a create or read answering 404
for the journaled scale set reports it gone (KER-06G re-creates it); `JobAssigned` decodes with
`runnerRequestId` 0 and its classifying fields (repository, owner, event, labels) as optional,
missing meaning unclassifiable; on GHES, after a session opens with available or assigned jobs in
its statistics, list `acquirablejobs` and hand them to the domain as offers (spec §6.1). Fixture
cases add: session expired on refresh, 200 with empty body, 204 for an absent runner, a busy
runner at 400, direct assignment with request id 0. The listener's duplicate check follows
KER-06G: a receipt counts as a duplicate only for the cursor's id.
Note (GH-03, as built): the ports are `mayfly_control::github` (`ScaleSetApi`, `RunnerRegistry`,
their values and the failure classes, decided by `typeName` before status); no earlier card
defined them. The adapter's `ScaleSetClient` holds each session's queue URL and token (never
crossing the port; the session id does), refreshes on 401 once for racing callers, drops a session
whose refresh answers expired, passes every queue URL through `credential_base_problem`, sends
`api-version` in every service call's query and the query to the fake in process, and decodes
messages through its own ACL (`wire.rs`: arrival order, unknown types as `MessageItem::Unknown`,
bounded strings). A session's refreshes are serialised per scale set. A 404 reads as the scale set
gone only on a session create or a scale-set read; on a session delete it is done, on a refresh the
session is lost. A create answering an unusable session deletes it at once (best effort) and
returns drift. The contract is `mayfly_testkit::contracts::scaleset` (SS-01..SS-13,
RR-01..RR-04) over `FakeScaleSetApi`/`FakeRunnerRegistry`, the adapter over fake-github (both
flows) and the adapter over the synthetic recordings in
`crates/mayfly-adapter-github/tests/fixtures/github-scaleset/` (one per case, replayed strictly);
`NoCursorApi`, `RetryAfterIgnoringApi` and `NameGuessingRegistry` fail SS-04, SS-06 and RR-02.
Left to later cards: the GH-02H host suffix and HTTP/2 health, the run lookup and cancel (GH-10),
the listener (GH-04). The live run is HW-02's (owner, pending).

#### GH-04 Listener use case
Cites: plan §7.2, KER-06, `doc/github-scaleset.md` §3, §5, §6, §9 (ADR 0048). Poll → record
(inbox + statistics + desired + the acquire plan: `RecordMessage` runs `PlanAcquire` and
journals it before the call) → ack, and `AcquireJobs` for exactly the event's `acquire` list →
`RecordAcquireResult` (KER-06); empty poll still reconciles; never blocks on VM work; the session
id and the last acked message id are journaled for crash recovery.
Accept: component tests F08, F09, F10, F11, F25, F27 (ver §8.4) on fakes. (KER-06G) Each poll
sends `ScaleSetState::wire_max()` (pinned while a refusal sink is up), never
`CapacityApplied.wire_max`, a snapshot; a receipt counts as a `DuplicateMessage` only when
`ScaleSetState::is_duplicate(id)`, and a reused id with a receipt is recorded anew under its own
command id (a component test for each).
DEC-01: the record step is one unit with `InboxKey { github, message_id }`; ack after commit;
`DuplicateMessage` acked without a write; `PlanAcquire` before `AcquireJobs` (I23, I32); wire
behaviour of pause, drain and safe mode (ADR 0029); adds F41, F42, F44.
ES-03 (ADR 0041 rule 6): a refused command's unit carries no inbox key, so `inbox_contains` stays
false after a refusal. The listener acks a refused message once `save` answered
`Outcome::Refused` (the refusal receipt committed), never by `inbox_contains`; a redelivery after
a lost ack is answered by that receipt while its bound streams are unchanged and decided afresh
once they moved. A component test pins the ack of a refused message and its redelivery.
Note (GH-00R): on github.com there is no acquire: the record step classifies each `JobAssigned`
(KER-06G) and the reconciler starts a runner only through KER-06G's gate. A gap between
`totalAssignedJobs` and running jobs that stays wide (spec §3.4, a stale count GitHub keeps across
session re-creates) is a metric and an operator alarm, never a reason to delete the scale set.
Component tests add: an assigned untrusted job refused and acked, no runner started while it is
assigned; a restarted numbering recorded and the session kept; a 404 on create re-ensures.
Note (GH-03 review): `PolledMessage::statistics` is `None` when the envelope lacked any of the
seven counts; record such a message without statistics (an older count is never replaced by
none) and count it as protocol drift, as an unknown type is. A `ProtocolDrift` from
`create_session` journals no session: the client deletes the session GitHub made, best effort
(the error's detail says whether that went through). When that delete fails, the session may still
block the next create: the 409 handling of A3 applies as for any conflict.

#### GH-02H GitHub transport hardening
Cites: `doc/github-scaleset.md` §1, §4.1, §6.3, §8, ADRs 0045, 0048. Code on GH-02's client:
(1) when the API base is `https://api.github.com`, the Actions service URL and every queue URL
must have a host ending in `.actions.githubusercontent.com` (on top of `credential_base_problem`;
GHES keeps the https rule only); (2) HTTP/2 health: ping a silent connection (30 s, 10 s
timeout) or drop a connection whose attempt timed out, so a retry never reuses a dead one;
(3) 429 `ConfigureTooManyRunnersException` on JIT is a throttle of minutes: backed off within the
directive's deadline, never re-minted under a new name (I19).
Accept: tests over fake-github: a service URL on another host refused before any token is sent;
a stalled connection replaced on retry under `tokio::time::pause`; a JIT 429 backs off and
reports without a second name.

#### KER-06G `ScaleSet` aggregate follows GH-00R's model
Cites: `formal/scaleset.qnt` (GH-00R), `doc/github-scaleset.md` §3.1, §4.1, §4.3, §6, ADRs 0027,
0040, 0090. Rule 7: the model is already changed; this is the Rust side.
(1) **Assigned jobs** (ADR 0090, owner 2026-11-11 and 2026-11-12): `JobAssigned` facts carry
the classifying fields (repository, owner, `workflowRunId`, event, labels), keyed by `(jobId,
workflowRunId)`, never by `runnerRequestId` (0 on github.com); an assignment is `pending` until
its run lookup's observation (`RecordRunLookup{job, run facts | failed}`, from GH-10) classifies
it through a pure `demand::classify_assigned` (the pool's `allowed_repositories`, the scale set's
name, head repository against base, event); the stream keeps `pending`, `trusted` and `refused`
sets released by `JobStarted` or `JobCompleted`, a new assignment or a re-created scale set;
`AssignedJobRefused{reason}` is a journaled event and leads to a `StartSink` directive (once per
refusal, one sink at a time, only while nothing trusted or pending is known; the wire max pinned
at the assigned count while the sink is up, `demand::wire_max`) or, with `cancel_refused_runs`,
a `CancelRun` directive (once; its answer recorded); a pure `demand::runner_may_start` (a
classified-trusted job waiting, open and active; refusals hold nothing, GH-00S) gates the
reconciler's shortfall (CP-03) and minting. (2) **Restarted
numbering**: a session's first message strictly below the floor records `MessageIdsRestarted`,
applies as new, and restarts cursor and floor; later regressions still degrade. A message is a
`DuplicateMessage` only when its receipt exists and its id equals the cursor; any other id at or
below the cursor degrades (the receipt check alone would drop a reused id silently). (3) **Re-ensure**: a 404 for the
journaled scale set returns the stream to `Unconfigured`; `RecordScaleSetRecreated{id}` resets
cursor and floor and drops reservations, classifications and refusals on that observation.
New events are new types (schema 1), with golden fixtures and `DETERMINISM_CASES` accepted and
refused.
`demand::trust` takes the pool's `allowed_events` (default: `push`, `pull_request` from the base
repository, `workflow_dispatch`, `schedule`, `merge_group`, `release`; owner, 2026-11-12); every
other event is refused. A new assignment of a job replaces its earlier classification. Every
refusal is logged at WARN as `job_refused` (`repository`, `event`, `reason`, `run_id`).
Accept: rules and property tests naming the model's invariants `vmOnlyForAJobClassifiedTrusted`,
`assignedJobsClassified`, `cancelOnlyRefused`, `sinkOnlyForARefusedJob` and the renumbered branch
of `a1RefutationIsFlagged`; the mutants RUN_ASSIGNED and CANCEL_ANY fail them
(`noStepOfAnUntrustedJobRuns` and `sinkRunsNoStep` are the hook's, IMG-02 and IMG-03).
Note (KER-06G, as built): the trust policy enters the stream as `ApplyTrustPolicy`
(`TrustPolicyApplied`; none applied refuses every assignment) and the scale set's name is part of
it (an assignment's labels must name the set); the 404 is its own observation,
`RecordScaleSetVanished{id}` (`ScaleSetVanished`, back to `Unconfigured`; `EnsureScaleSet` is then
refused `scale_set_vanished`), before `RecordScaleSetRecreated{id}`; `StartSink` journals
`SinkStarted`, `RecordSinkEnded` lifts the pin, `RecordCancelAnswer` journals `CancelAnswered`, and
`ScaleSetState::refusal_action` names the next sink or cancel. The duplicate rule is
`ScaleSetState::is_duplicate` for the listener (GH-04: a receipt for any other id is recorded anew
under its own command id). The `job_refused` WARN log and the reconciler's use of
`runner_may_start` are wired by GH-08, where the use cases that record lookups live. Golden
fixtures were added, none rewritten; the decisions and matrix goldens were regenerated (a
session's first message below its floor now restarts, an earlier id later in a session degrades).
The scaleset conformance driver is VER-02's; the stream's state carries the model's `pending`,
`trusted`, `refused`, `cancelTried`, `sinkLive`, `sinkTried` and `renumbered` for it.
Review fixes (KER-06G): a `JobAssigned` without an event name is unclassifiable; pending and
trusted entries keep their `runnerRequestId`, and an acquire answer that names one drops it, so a
GHES job assigned while its acquire answer was lost counts once (the model's `acquired ∪
trusted`); a changed trust policy sends every trusted job back to `pending` for a new lookup (the
aggregate's own fail-closed rule: the model's `TRUST` is fixed). Gaps kept on the record: a
`JobAssigned` with no `jobId` or `workflowRunId` cannot be keyed, so it is never trusted and backs
no runner, but no refusal is journaled and no sink or cancel can target it (HW-02 item 8 shows
whether github.com ever omits them; the hook still fails it); the scale set's name is carried in
the trust policy rather than taken from `ScaleSetEnsured`, which does not hold it (adding it
there is an optional field on an existing event, left to GH-08 if wanted).

#### GH-08 Trust on github.com
Cites: ADR 0090 (owner, 2026-11-11 and 2026-11-12), `doc/github-scaleset.md` §2, §6.2, §13, ADR
0045. Config: a pool's `allowed_repositories` (required on github.com; full names; public allowed;
refused empty), `allowed_events` (default the list of ADR 0090) and `cancel_refused_runs`
(default false); an organisation's `check_fork_pr_setting` (default
false); an enterprise scope is refused with a message naming ADR 0040. Start-up and before every
session open: the scale set exists (or is created) and its session is reachable; nothing about
runner groups or the GitHub plan. With `check_fork_pr_setting`, the start-up read of the
organisation's fork-PR setting for private repositories refuses the organisation's pools (an
operator-visible state with the reason, `doctor` shows it) when
`run_workflows_from_fork_pull_requests` is true. `doctor` checks the App's permissions against
what the pools' options need (ADR 0045). Public repositories: it reads the scale set's runner
group (`GET /orgs/{org}/actions/runner-groups/{group_id}`, "Self-hosted runners") and reports, at
start-up and in `doctor`, each allowed repository that is public while the group's
`allows_public_repositories` is false (GitHub routes it nothing then); a report, not a refusal.
Wires KER-06G's classification and GH-10's lookup and cancel into the listener and the
reconciler, and hands the pool's lists to the launcher for the hook (IMG-02).
Accept: tests over fake-github: a public allowed repository's trusted job runs; a fork's run, an
unlisted repository and a failed lookup are refused end to end and no runner is started for
them; a trusted job's runner starts beside a refused one; with `cancel_refused_runs` the refused
run is cancelled; with
`check_fork_pr_setting` an enabled fork setting refuses to start and a disabled one starts; a
public allowed repository behind a group that does not admit public repositories is reported.
(KER-06G) Every `AssignedJobRefused` is logged at WARN as `job_refused` (`repository`, `event`,
`reason`, `run_id`, from the lookup's command), and the reconciler starts and mints a runner only
while `ScaleSetState::runner_may_start(idle)` holds (a test for each); the pool's settings reach
the stream as `ApplyTrustPolicy`.

#### GH-09 (Withdrawn) Operator release of a refusal
Withdrawn by GH-00S (owner, 2026-11-12, ADR 0090 amended): refused jobs hold nothing any more
(every runner's hook fails an untrusted job, and a refusal sink or a cancel disposes of a refused
one), so neither the release window nor an operator release is needed.

#### GH-10 Workflow-run lookup and cancel
Cites: ADR 0090 (owner, 2026-11-12), `doc/github-scaleset.md` §2, §6.2, §13. In
`mayfly-adapter-github`, with the installation token: `GET
/repos/{owner}/{repo}/actions/runs/{run_id}` ("Actions: Read") for each pending assignment,
decoded into run facts (repository, head repository, event, actor), every request with a
deadline, a failure (error, timeout, 404, a missing field) answered as `failed` so the domain
refuses; `POST .../actions/runs/{run_id}/cancel` ("Actions: Write") for a `CancelRun` directive,
its answer (202, 409, an error, or none) recorded as an observation, never retried past one
answered attempt. Strings are untrusted input, length-bounded.
Accept: tests over fake-github: a fork's run decoded as one; a lookup timeout answered `failed`;
a cancel's 202, 409 and lost answer each recorded; no token in an error.
Note (KER-06G review): the domain compares head and base repositories by full name; a rename
between assignment and lookup reads as a mismatch (refused, the safe side). Decoding and comparing
`head_repository.id` against `repository.id` as well would close that; decide it here.

#### GH-07 Several organisations
Cites: ADR 0040 (amended 2026-11-11), `doc/github-scaleset.md` §1. One daemon serves pools in
several organisations, one scale set per pool in its organisation, with the same GitHub App
installed in each: per-organisation installation ids, tokens, start-up checks (GH-08) and
rate budgets; nothing shared between organisations but the App key. Enterprise scope stays
unsupported unless the owner asks (a new ADR on the classic token).
Accept: an e2e test over two fake organisations, each with its own scale set and installation;
a failure in one organisation leaves the other's pools running.

#### E2E-02 ⛳ Scale-set end to end
Cites: plan §7.2, §9. `mayflyd` + fake-tart + fake-github: queue 1 job → 1 VM → job done → deleted;
queue 100 jobs with 2 slots → never more than 2 VMs, all jobs complete, no double provision;
cancel/reassign case.
Accept: e2e tests green in CI.
DEC-01: adds the decline of an untrusted offer and a cancel during deregistration (F33).
Note (GH-00R, GH-00S): on fake-github's direct flow, adds the refusal of a fork's run and of an
unlisted repository's job (taken by a refusal sink and failed, or, with `cancel_refused_runs`,
cancelled), a trusted job whose runner starts beside a refused one, and a public allowed
repository's job that runs (ADR 0090).

#### LNK-01 UDS transport and split roles
Cites: plan §3.3. Length-prefixed JSON over a Unix socket; `mayflyd --role control|data`; link
contract suite on UDS; e2e variant with two processes.
Accept: killing the data-plane process alone is recovered by the control plane.

### Phase 5–6 — evidence

#### VER-01 Conformance crate
Cites: ver §6. ITF decoding, seeded trace generation via `quint run --mbt`, per-step comparison of
canonical observations (initial state included), regression trace manifest, `QUINT_SEED` replay,
divergence report; lifecycle driver against the `Attempt` aggregate + saga with `MemoryJournal`.
Accept: a negative control (mutated transition) is caught at the first divergent step.
Note (VER-01F): the four divergences VER-01's replays found are resolved (the model's stop
resolution by listing, clone re-delivery, vanished flag and terminal retirement; the aggregate's
`ConfirmRetirement`/`WithdrawRetirement` in `Backoff` and `Finished`); each script replays clean
and `KNOWN_DIVERGENCES` is empty (`doc/formal-models.md`, lifecycle).

#### VER-02 Conformance drivers
Cites: ver §6. Drivers for scheduling (capacity policy + Host), recovery (startup use case with
SQLite journal and fake provider), scaleset (listener with fake ScaleSetApi), link (inproc and
UDS).
Accept: each driver has a negative control; CI runs bounded seeds per model.
DEC-01: adds the registration driver (KER-09) and the resource-ledger comparison in scheduling.

Note (GH-00R): the scaleset driver replays both inits (`initDirect`, `initAcquire`) and the
actions GH-00R added (`ghAssigns`, `startVm`, `dormantDeleted`, `recreateScaleSet`, the
renumbering branch of `record`) against KER-06G's aggregate.
#### VER-03 Fault lab skeleton
Cites: ver §8, D11. `tools/faultlab`: scenario schema, independent controller with monotonic clock,
fsync-before-dispatch history JSONL, subprocess workers driving `mayfly` CLI, artefact folder,
four verdicts with exit codes, guardrails (only harness-created state dirs; refuses real Tart unless
an ownership manifest and the explicit flag are present).
Accept: a scenario whose fault never lands is INCONCLUSIVE; malformed history is HARNESS_ERROR.

#### VER-04 ⛳ Fault lab nemesis and checker
Cites: ver §8. Nemesis (SIGKILL daemon at observed history points, fake-tart fault file, fake-github
faults, data-plane process kill with LNK-01), independent checker (temporal safety invariants at
every observation cut, bounded liveness after the healthy tail), the core scenarios F01–F16, F19, F20, F24, F31 (ver §8.4),
`cargo xtask faultlab suite|check|replay`, `doc/verification-status.md`.
Accept: core scenarios PASS on fakes in a weekly CI lane and on demand; each scenario has a mutant
that turns it FAIL.
DEC-01: core scenarios become F01–F16, F19, F20, F24, F31–F36, F39, F41–F43, F47, F48 (ver §8.4).

#### VER-05 Curated mutants
Cites: ver §9. The 18 negative controls of ver §9 as source patches applied by `cargo xtask mutants-curated`,
each expected to be killed by a named test; `cargo-mutants` nightly lane with a baseline document.
Accept: every curated mutant killed; survivors listed in `doc/mutation-baseline.md`.
DEC-01: the table has 27 rows (ver §9 adds 19–27).

#### VER-06 Fuzz
Cites: ver §10. cargo-fuzz targets (outside the workspace, pinned nightly): protocol decode, Tart
JSON parse, config parse, GitHub message decode, OwnedVmName parse. Nightly lane with a time budget.
Accept: targets build; corpus seeded from golden fixtures.

#### VER-07 Apalache verification lane
Cites: ver §6. `quint verify` with Apalache for each model's invariants at bounded depth in
`formal.yml` (weekly and by hand only, one job per model: owner, 2026-11-10, DEC-05); depths and
timings in `doc/formal-models.md`.
Accept: every invariant listed in ver §6.1 is checked; a seeded bad invariant fails.
DEC-01: adds `registration` and `event_store` to the lane.

#### VER-08 Conformance driver for the image store model
Cites: ver §6, ADR 0087, `doc/formal-models.md` (image_store, its conformance seam). Replay
`formal/image_store.qnt` traces against `TartImageStore` and `TartProvider::clone_vm` over
`mayfly-fake-tart` (crash points as a dropped store rebuilt over the same registry, the uncontrolled
writers as direct `tart` calls), comparing the registry entry's state and identity, Tart's listing
of the base and the listed clones after each step.
Accept: seeded traces replay clean; a negative control (an eviction that ignores intents) diverges
at its first step.

#### VER-09 Local OCI registry for daemon tests
Cites: ver §2; ADRs 0039, 0087; CP-07, CP-08. `mayflyd`'s pull fetches the image's manifest over
HTTPS from the configured repository (`RegistryManifests`, HTTPS only, the operating system's
trust roots), so no process-level test can place an image yet. A test-only HTTPS registry
(dependencies justified at this task) serving a manifest and its blobs by digest, its CA passed to
`mayflyd` through the test's environment, the same manifest and blobs seeded in fake-tart's
registry so its pull and the data plane's verification agree, and the runner pins of `[images]
runners` with the fake guest's install.
Accept: `mayflyd` over fake-tart, the pool pinned through the store (until OPS-02), materialises the
pin's base on its host and the reconciler reserves for the pool there. DP-08's `mayflyd` test and
E2E-01's walking skeleton need it too.

#### SEC-01 Threat model and secret audit
Cites: plan §13. `doc/security.md` (assets, actors, boundaries, mitigations with tests),
audit of every secret path, fixes as needed.
Accept: every mitigation names its test.
DEC-01: trust tier and fork-approval settings (ADR 0027), reachability with and without Softnet
(ADR 0032, NET-01), the secret providers (ADR 0033), the diagnostics bundle (ADR 0036), the
planned rule that only the GitHub adapter opens outbound connections, residual risks (a JIT config
is visible to the workflow it serves; hypervisor escape).
Note (GH-00R, GH-00S, ADR 0090): the threat model adds the github.com trust controls (the
configured repositories and events, the per-job workflow-run lookup and its refusals, the
in-guest job-started hook as the hard boundary, the refusal sink, the cancel option, the optional
fork-PR setting check) and public repositories being allowed. Residual risk: an untrusted job
that reaches a runner (assigned after the last message mayfly recorded, or picked for that
runner by GitHub) is failed by the hook before any step, but only after the runner has
downloaded and unpacked its actions inside the disposable VM; and a sink may fail a trusted job
assigned before its pinned poll.

### Phase 7 — operate

#### OPS-01 launchd service, foreground mode and doctor
Cites: plan §11, §13; ADR 0031. Foreground `mayflyd` documented for local use; LaunchAgent plist
(auto-login session, `KeepAlive` with exit-code handling so a lock refusal is not restarted
forever, `ThrottleInterval`, `ExitTimeOut`), `mayfly service install|uninstall`, `doctor` checks
(provider version, `TART_HOME` `0700` and its deployment marker, disk, memory, socket permissions,
secret references resolvable, sleep settings FAIL, clock). `service uninstall` removes what the service
install created and nothing else (philosophy §4.3); the purge of state, VMs and bases is OPS-08
(ADR 0044). `doctor` also warns when the GitHub App holds more permissions than ADR 0045's list
needs (once GH-02 exposes them).
Accept: plist golden; doctor JSON golden on Linux with fakes; (owner) install on the Mac.

Note (RT-02): a P24 stop takes up to 25 s for the loops and 5 s for the runtime
(`daemon::SHUTDOWN_DEADLINE`, `RUNTIME_SHUTDOWN`), so the plist's `ExitTimeOut` is at least 30 s;
the plist passes `--config`, which `mayflyd` requires.

#### IMG-02 In-guest job-started hook: the hard trust boundary
Cites: ADR 0090 (amended 2026-11-12, owner: accepted and required), ADRs 0030, 0082,
`doc/github-scaleset.md` §6.2. Bake a fixed job-started hook into every image beside the runner,
and have the launcher set `ACTIONS_RUNNER_HOOK_JOB_STARTED` to it and hand it the pool's
`allowed_repositories` and `allowed_events` as data (never interpolated into a command). The
hook reads `GITHUB_EVENT_NAME`, `GITHUB_REPOSITORY` and the payload at `GITHUB_EVENT_PATH`
(`pull_request.head.repo.full_name` against `repository.full_name`); a trusted job exits 0, any
other prints `::error title=Refused by mayfly::<reason>` and exits non-zero, failing closed
(missing or unreadable input refuses). It writes its verdict where the launcher reads it, so the
launcher reports a refusal as an observation and the control plane journals
`AssignedJobRefused{reason, by: hook}` and logs `job_refused`. The runner unpacks a job's
actions before the hook runs, so it never runs outside a disposable VM.
Accept: tests of the hook over synthetic payloads (each refused event, a fork, a missing or
unreadable payload: non-zero with its reason; each allowed event: zero); the launcher contract
test that the variable is set and the verdict reported; the image checks (OPS-02) refuse an image
without the hook. The timing, the failed job and what a fork PR's author sees are HW-02 checks
(spec §12 item 11, owner).

#### IMG-03 Refusal sink
Cites: ADR 0090 (amended 2026-11-12, owner: option A), `doc/github-scaleset.md` §5, §6.2, ADRs
0024, 0030, 0082. A small Linux image (official runner, IMG-02's hook set to refuse every job)
and a `StartSink` directive: the data plane clones and boots a disposable Linux VM (no macOS
slot), the control plane mints a JIT runner for it (I19), and the VM is deleted after its job or
at its deadline. KER-06G decides when (once per refusal, one at a time, only while nothing
trusted or pending is known) and pins the wire max at the assigned count while it is up. When no
sink can start (no capacity, no sink image), nothing waits: GitHub's release fails the job.
Accept: an e2e test over fake-github and fake-tart: a refused job is taken by the sink and
failed, the sink VM deleted, and the wire max pinned then restored; no sink while a trusted job
is assigned; no sink with `cancel_refused_runs`. Whether a Linux runner in a scale set whose jobs
ask for macOS labels receives them, the sink's latency and the pin with a backlog are HW-02
checks (spec §12 item 12, owner).

#### OPS-02 Image manifest
Cites: plan §8, OPS. Manifest schema (`schema`, provider, OS, arch, digest-pinned source, runner
version, capabilities, host requirements), `mayfly image verify`, refusal of tag-only references.
Accept: valid/invalid fixtures.
DEC-01: manifest fields runner version and its SHA, launcher hash, guest-agent requirement (images
without it refused, ADR 0038); `image withdraw` with CP-07.
Note (CP-07): the pin use case saves `PinImage` with a read of the image's `Image` stream and
refuses a withdrawn image. That closes the one window `withdraw_image` cannot: a pool stream
created and pinned after the withdrawal's last listing of the pools.

#### OPS-03 ⛳ Release workflow
Cites: fnd §7. Tag-triggered release: macOS arm64 (and Linux) archives, `SHA256SUMS`, build
provenance attestation, CycloneDX SBOM; signing/notarisation documented as (owner) follow-up.
Accept: dry-run on a branch produces artefacts; (owner) first tag.
ADR 0044: Homebrew is the distribution and update channel (a formula or tap, with a caveat that
points to `mayfly uninstall --purge`); no self-update or update check exists anywhere.

#### OPS-04 Runbooks
Cites: plan §10. `doc/runbook.md`: stale runner, disk full, GitHub outage, lost guest, orphan
review, keychain locked, safe mode, upgrade/rollback.
DEC-01 adds: quarantine cases (never auto-deleted; how to resolve), provider limitation reports,
image refresh, diagnostics bundle, the external dead-man alert on the heartbeat metric.

#### OPS-05 Watchdog, heartbeat metric, distinct exit codes
Cites: dm P25, ADR 0031. Heartbeats per supervised loop, stall > T_wd exits with the watchdog code,
crash-loop detection starts into safe mode, `mayfly_heartbeat_timestamp_seconds`, distinct codes
for lock refused, watchdog and invalid config.
Accept: F40 at integration level; the codes are distinct and documented.
Note (KER-10F/G): the crash-loop finding is the input `HostRun`'s `MintEpoch.crash_loop` takes
(K starts within W before `RecoveryCompleted`, from the watchdog's own start record outside the
journal, so it holds when the journal is what crashes). Accept adds: a component test in which
the K-th start within W mints with `crash_loop` and that run is refused a verified restart.

#### OPS-06 `mayfly image refresh <pool>`
Cites: ADRs 0030, 0039; dm P17, P18. The command (pull by digest and record it; base named by
digest and runner version; boot once and install the pinned SHA-checked runner via `tart exec`;
verify; move the pool's base pointer keeping the previous; GC unreferenced bases), runner-age
warnings in `status`, the optional daily runner-only refresh per pool, rollback to the previous
base.
Accept: e2e on fake-tart: refresh, use, roll back; a SHA mismatch refuses the runner.
Note (CP-07): the operator path for a `Failed` placement (P17's failure path: a refused pull or
first materialisation, or a drift) is this command: it requests the pool's placements again, a
failed one included (`RequestPlacement` after `Failed` starts again); the placement manager never
does it on its own.

#### OPS-07 `mayfly diagnostics bundle`
Cites: ADR 0036, philosophy §5.1. A local, redacted archive (version, config with references only,
`doctor --json`, `status --json`, recent logs, event kinds and counts, projection summaries,
provider and OS versions) and its listing; nothing sent anywhere.
Accept: a canary-secret test over every bundle file; golden listing.
Note (DEC-05, ADR 0066): the bundle carries every open quarantine case with its reason and
evidence, `vm_vanished_externally` included (golden listing).
Note (DEC-07, ADR 0066 amended): it also carries every attempt with a recorded desync (the
`attempts` view's `desync`, its `VmVanished` event and the stream's events before it), since a
vanish closes the attempt instead of opening a case (golden listing).

#### OPS-08 `mayfly restore` and `mayfly uninstall --purge`
Cites: ADR 0043, ADR 0044, plan §8.4, §10, §11. `mayfly restore <backup>` restores the automatic
pre-migration backup (taken before every migration; ES-09's restore path), refuses a backup of a
newer schema than the binary, and leaves the daemon in the recovery path of plan §10 so that VMs
created after the backup are quarantined (`ownership_doubt`), never deleted or asserted.
`mayfly uninstall --purge [--dry-run | --apply <plan-id>]` plans first, then removes exactly what
mayfly created: journal and state, its VMs and image bases (each only with the ownership proof:
journal record, exact `OwnedVmName` and generation; foreign or unprovable VMs and bases are
reported and left) and its runner registrations; Homebrew's caveat points to it. Both are
destructive operations on a real host: run only by the owner or on the owner's explicit request,
never by an agent (CLAUDE.md); agents test them on the fakes.
Accept: on the fake provider and fake-github, a purge removes only owned VMs, bases and
registrations and leaves a foreign VM and a base it did not create; `--dry-run` changes nothing;
a restore of an older backup quarantines later VMs and deletes none; (owner) on the real Mac.

#### TART-01 Tart 2.28.0 → 2.41.1
Cites: plan §8.2, ADR 0039, ADR 0087. Tart moved to github.com/openai/tart (licence FSL-1.1,
Apache-2.0 future; mayfly drives it by argv only). Bump the pin (`parse::SUPPORTED_VERSIONS`), re-read
every command and JSON shape the adapter parses (`list`, `get`, `clone`, `set`, `run`, `stop`,
`delete`, `exec`, `pull`; `tart set --mac` exists since 2.41.0, so the MAC is even weaker evidence),
record new fixtures under `tests/fixtures/tart-2.41.1/`, bring `mayfly-fake-tart` to parity (pull
layout, cache naming, chunk checks, new flags refused or accepted as Tart does), and re-check what
ADR 0087 assumed of 2.28.0 (the re-serialized cached manifest, unchecked blobs, the symlink of a
requested digest).
Accept: both versions' fixtures parse; the VmProvider, GuestTransport and ImageStore contracts green
over fake-tart in 2.41.1 mode; HW-01X's command gains the 2.41.1 confirmations.
Accept changed (TART-01): "both versions' fixtures parse" became "the 2.41.1 fixtures parse and
the 2.28.0 `get` shape is refused", since only 2.41.1 is supported (a version update needs no
ADR, owner 2026-11-11).
As built (TART-01): 2.41.1 only, since 2.28.0 cannot work: its `get` prints no `MACAddress` (no
instance id, ADR 0026), its `exec` aborts (`try! Term.GetSize()`) when stdout is not a terminal,
as under the process runner, and its `clone` silently replaces an existing VM (no
`--overwrite`). The licence changed with the release: 2.28.0 is under the Fair Source License
0.9, 2.41.1 under FSL-1.1-ALv2 (plan §1.3); mayfly only runs Tart as a separate program. So only
the 2.41.1 fixtures parse (derived from its source, `tests/fixtures/tart-2.41.1/README.md`) and the
2.28.0 `get` shape is pinned as refused (`parse::tests::should_refuse_tart_2_28_0_when_its_get_prints_no_mac`).
Units: `Disk` is whole decimal GB rounded down, `null` whenever Tart's read of the size fails
(for one, a running VM's ASIF disk); the ceiling is the larger of `(Disk + 1) × 10⁹` and `Disk`
GiB. Because an unknown disk closes disk admission host-wide (ADR 0052), `clone_vm` reads each
new clone while it is stopped and records its ceiling in the base registry (`clone_disks`, an
optional field, with the instance id it was read for); the inventory uses it only when Tart
prints `null` and the VM still has that instance id, else the disk stays unknown; an older build
ignores the field (pinned by `tests/tart/calls.rs::should_count_the_disk_recorded_at_clone_time_when_tart_prints_none`).
A macOS job clone gets `tart set --random-mac --random-serial` (a new machine identifier; the
flag exists in arm64 builds only), a base `--random-mac` only, since its config must stay the
image's but for the MAC (pinned by `tests/tart/calls.rs::should_clone_from_the_local_base_then_draw_a_random_mac_when_cloning`).
2.41.1 caches a pulled manifest as served (2.28.0 re-serialized it); it is still never compared
with the requested digest, so the image store still never reads it (pinned by
`tests/tart/images.rs::should_never_read_the_cached_manifest_when_verifying_an_image`). Tart
refuses disk v1 and a manifest out of order, and `verify::layout` now requires that order too;
`tart set --mac` (2.41.0) makes the MAC weaker evidence still; a linked digest is listed
`<repo>:<digest>` and `tart delete` of the link also collects its entry; 2.41.1 creates
`cache/content/` under `TART_HOME`. `TRACEPARENT`, `TRACESTATE`, `OTEL_*` and `SENTRY_DSN` never
reach Tart (foundation §3, child environment); the hypervisor limit is matched on Tart's own
wording. fake-tart emulates 2.41.1 only: served manifest bytes cached, the layout checked as Tart
does, a linked digest listed `<repo>:<digest>` and collected after its last link is deleted,
`set --mac` and `--random-serial`, short-only `exec -i`/`-t`, bare `RuntimeError` messages, an
ASIF disk printed `null` while it runs; its known gaps are listed in `fail.rs`.

#### DEP-01 Dependency and tool refresh
Cites: fnd §2, §7, §9; `deny.toml`; `tools/versions.toml`. `cargo update` within semver and the
gate; `cargo deny check`; each major bump one at a time, in its own commit with its reason; the
Rust toolchain, Quint, Apalache and Node pins; the GitHub Actions SHAs (with their version
comments).
Accept: the full gate green after each step; no new `deny.toml` exception without its audit.

#### HW-01X `cargo xtask hw01`: real-Tart checks from a terminal
Cites: ver §2, §5; fnd §9; ADRs 0015, 0026, 0037, 0038, 0039, 0058, 0082, 0087; the DEC-01,
SEC-02, DP-04 and DP-07 notes under HW-01. The owner decided on 2026-11-11 that the real-Tart
lane is one command the owner runs on their Mac from a checkout (Rust and Tart installed), with
no self-hosted runner. Agents build and test it against fake-tart; no agent runs it on real
Tart (CLAUDE.md). `cargo xtask hw01 [--macos] [--service-mode --service-user <user>] [--report
<file>]`:
- **A throwaway home.** It creates a `TART_HOME` in a fresh temp dir (`0700`, checked as ADR 0087
  checks mayfly's private home) and passes it explicitly to every `tart` call; it never reads or
  writes `~/.tart` or the caller's `TART_HOME`, and refuses to go on if the home a call would use
  is not the one it created.
- **Pinned images, explicit network.** It pulls small test images by digest only, each pinned
  with its repository, digest, size and reason in a committed `tools/hw01/images.toml` (a tag is
  refused): an arm64 Linux image with the Tart guest agent, always; a macOS image only with
  `--macos`, because macOS images are tens of gigabytes. The same file names every network need:
  anonymous pulls from the public registry hosting those images, and the runner release the
  launcher's `install-runner` downloads (host, version and SHA-256 pinned). Nothing else leaves
  the Mac.
- **Everything HW-01 lists today.** The real-Tart `VmProvider` contract (`cargo xtask test --only
  real-tart --real-tart-home <its home>`: VP-01..VP-10, VP-13..VP-15) and VP-17; the generic
  `SecretProvider` contract (SP-01..SP-08) on `KeychainProvider::system` against a disposable
  test keychain it creates, and ADR 0058's `security -w` facts; and every DEC-01, SEC-02, DP-04
  and DP-07 confirmation under HW-01: the pull by digest and its listing name, the cache layout
  including the link form, `tart clone` from a cached digest not reaching the registry and
  leaving the base's change times alone, and `tart exec` and `install-runner` in a booted base on
  Linux (and on macOS with `--macos`). TART-01's confirmations join when it lands. The launchd
  service-mode check runs only with `--service-mode`: the owner names an existing dedicated
  user; the command installs a uniquely named temporary LaunchDaemon (one `sudo` prompt), boots
  the Linux image under it without a GUI session and removes the daemon. It never creates or
  deletes a user.
- **The `ImageStore` contract's image.** IC-01..IC-14 need a registry holding an image with
  mayfly's launcher. A further step of the command builds one in the throwaway home: it clones
  the pinned Linux image, installs a test launcher (`tools/hw01/launcher`, a reviewed script that
  implements only ADR 0082's `install-runner` and `verify`, for this test; not the product
  launcher) through `tart exec`, and pushes it with `tart push --insecure` to a loopback OCI
  registry the command serves itself on `127.0.0.1` (port 0) for the run; the gated contract
  world beside `tests/tart/images.rs` (`tests/real_tart_images.rs`) then pulls it by digest. Its
  `ManifestSource` reads the loopback registry's directory, because `RegistryManifests` speaks
  HTTPS only; `RegistryManifests` is checked against the public registry instead
  (`public-manifest-fetch`, the pinned Linux image). IC-14 (a macOS base beside a Linux one) runs
  with `--macos`, which also pushes a macOS contract image, and both runner pins; without them it
  is `not_run`. If any step of that build fails, IC-01..IC-14 are reported `not_run` with
  the failing step, never passed; a check that needs `--macos` or `--service-mode` without the
  flag is `not_run` too.
- **It cleans up what it made, and only that.** Each VM, image, keychain, LaunchDaemon, process
  and file is written to the run's record (in the temp dir) before it is created. On every exit,
  success, failure or Ctrl-C, the command deletes exactly what the record names: a VM by its
  exact name after re-reading the MAC it recorded, a process by the PID it started, never by
  prefix, pattern or listing; then the temp dir. What it could not delete is listed in the report
  and on stderr.
- **A redacted, machine-readable report.** A versioned JSON file: per check `pass`, `fail` (with
  bounded evidence: exit codes, JSON shapes with values replaced by their types, the wording that
  matters) or `not_run` (with the reason), plus the macOS, Tart and toolchain versions and the
  image digests. Redaction happens before anything is written: no hostname, username, home path,
  serial number, hardware UUID, token or keychain content; MACs only synthetic (each real one
  replaced by a stable placeholder per VM); paths relative to the temp home. Evidence that fails
  redaction makes its check `fail` and is dropped. The owner sends the report back; agents turn it
  into fixtures (`crates/mayfly-adapter-tart/tests/fixtures/tart-<version>/`) and fixes.
Accept: the command refuses to start off macOS; its steps are tested on Linux against fake-tart:
a run creates and then deletes exactly its record's entries, leaving a VM and a home it did not
create untouched, also when a check fails midway and when the run is interrupted; the report's
schema test, and a redaction test that plants a hostname, username, serial number, real MAC and
token in check output and finds none in the report; an unbuildable contract image yields
`not_run` for IC-01..IC-14; `tools/hw01/images.toml` holds digests only. ADR 0015 gets a dated
amendment (owner, 2026-11-11): the real-Tart lane is `cargo xtask hw01` from a terminal and
`hardware.yml` is optional; the D14 row, fnd §7 and ver §2's hardware row follow. `cargo xtask
check` green.

#### HW-01 (owner) Real Tart checks: run `cargo xtask hw01` and send the report
Reworked by the owner's decision of 2026-11-11: no self-hosted runner. (owner) on the Mac, from a
checkout at the commit to check, runs `cargo xtask hw01` (with `--macos` for the macOS image
checks, `--service-mode --service-user <user>` for the launchd check) and sends the report back.
Agents turn the report into fixtures and fixes, each in its own task; a `fail` or `not_run` stays
listed here until a later report passes it. Before the first run, (owner) confirms the runner pin
of `tools/hw01/images.toml` (`[runners."2.338.2"]` with its `linux-arm64` and `osx-arm64` package
checksums, computed from github.com's release downloads and not yet cross-checked) against the
v2.338.2 release page. `hardware.yml` is optional: should the owner register
a self-hosted runner later, it runs the same command and uploads the report. What the command
confirms, by the task that asked for it:
DEC-01: also verifies `tart set --random-mac` and MAC read-back, `tart exec -i`, the run lock and
its PID, shim survival across a daemon kill and `tart stop` semantics.
SEC-02: also runs the generic `SecretProvider` contract (SP-01..SP-08) on the real Keychain
(`KeychainProvider::system`, a disposable item in a test keychain), and confirms what ADR 0058
assumes of `security -w`: its trailing line break, exit 44 for an absent item, 36/51 for a locked
keychain, and hex output for a non-printable password.
DP-04: the command runs `cargo xtask test --only real-tart --real-tart-home <DIR>` (feature
`real-tart` of `mayfly-adapter-tart`, `tests/real_tart.rs`; the home holds only a local base
`mfb-contract`), which runs VP-01..VP-10 and VP-13..VP-15 (VP-11, VP-12 and VP-16 need injected
faults or an environment log: fake-tart only). It also confirms what the adapter assumed and the
fixtures in `crates/mayfly-adapter-tart/tests/fixtures/tart-<version>/` pin: the `--version`
string, the `list`/`get` JSON shapes, `Memory` in MiB and `Disk` in whole decimal GB rounded down (TART-01), exit 2
with "does not exist", the hypervisor-limit wording of `tart run`, `tart set --memory` before a
run, and that `tart run` under the shim keeps its VM when the daemon is killed.
DP-07: `tests/real_tart.rs` also runs VP-17 (the world records `mfb-contract` as owned and
`Ready` with its fingerprint). The `ImageStore` contract (IC-01..IC-14) on real Tart needs a
registry holding a mayfly image with the launcher's `install-runner`: HW-01X builds that image and
the gated world beside `tests/tart/images.rs`, or reports the contract `not_run`. HW-01 confirms
what ADR 0087 assumes, on Tart 2.41.1 (TART-01's note): `tart pull <repo>@sha256:…` and the OCI listing's `Name` for
it (`<repo>@<digest>` for an entry, `<repo>:<digest>` for a link); the cache layout (`cache/OCIs/<repo>/<digest>/` with `config.json`, `nvram.bin`, `disk.img`, the
entry named by the served manifest's hash, and the link from a requested digest to it: absolute or
relative, and whether `tart list`/`tart delete` know the linked entry by its digest); that `tart clone <repo>@<digest>` does not reach the
registry once cached and leaves the base's files' change times alone; `tart exec` in a booted base
and the launcher's `install-runner`, on a macOS image and on an arm64 Linux image (for example a
Cirrus Ubuntu image with the Tart guest agent); and, for service mode, whether Tart runs VMs under a
launchd daemon of a dedicated user without a GUI session.
TART-01: everything above on Tart 2.41.1, capturing the fixtures of
`tests/fixtures/tart-2.41.1/` (derived from the source until then), and in particular: `tart
--version` prints `2.41.1`; `get --format json` prints `MACAddress`, `Memory` in MiB and `Disk`
in whole decimal GB rounded down, `null` when the read fails (for one, a running VM's ASIF
disk); `list --format json`
prints `Disk` the same way and lists local VMs, then OCI entries, each sorted by name; `tart
run` prints "The number of VMs exceeds the system limit (other running VMs: …)" and exits 1 at
the hypervisor's limit; `tart exec -i` works with stdout and stderr redirected to files and
passes the guest's exit code; `tart clone` onto an existing name exits 64 without
`--overwrite`; `tart stop` and `tart delete` exit 2 with "does not exist" for a missing VM, `stop`
exits 2 "is not running" for a stopped one, `delete` exits 1 "is running" for a running one; the
run lock is still a POSIX write lock on `vms/<vm>/config.json` held by the `tart run` process;
`tart set --random-serial` (arm64 builds only) gives a macOS clone a new `ecid` in its `config.json`, the guest
boots, keeps its MAC and runs its runner; `tart pull` writes the served manifest's bytes to
`manifest.json`, refuses disk v1 and a manifest out of order, links a requested digest with an
absolute symlink, and `tart delete` of that link also removes the entry it named; `tart clone`
and `tart get` change the VM directory's access time only, never its files' change times; with
`TART_NO_AUTO_PRUNE` set, neither `clone` nor `pull` deletes a cache entry; `TART_HOME` gains
`cache/content/` on a pull or a delete.

#### GH-00 (owner) Live remainder of the scale-set spec, folded into HW-02
Cites: `doc/github-scaleset.md` §12 (the HW-02 remainder GH-00R leaves), dm §9.1, ADRs 0045,
0048, 0069. Reworked by the owner's decision of 2026-11-11: no broad live capture. Only the items
GH-00R leaves without server-observed evidence are recorded, during HW-02's session on its
disposable App and organisation, not in a session of their own. Agents build a redacting capture
mode for HW-02's gated live contract suite (GH-01, over GH-03's client): it records only the
exchanges a remainder item names, writes synthetic names for the organisation, repository, scale
set, runners and hosts, and never writes a token, JIT config, key or credential-bearing URL; a
test over fake-github plants each and finds none in the recorded fixture. (owner) runs it in
HW-02's session.
Item 6, whether `X-ScaleSetMaxCapacity` caps `totalAssignedJobs`, keeps `capacity_policy`'s
default `exact` (ADR 0069) until it is server-observed here; neither a client's assumption nor
GitHub's documentation changes that default. (GH-00R: public evidence shows the cap at 0, so
`exact` stays and `demand` is experimental; the remainder is spec §12's items, the capacity
beyond 0 among them as item 3, and item 8 asks whether a `JobAssigned` carries enough to classify
a job, ADR 0090.)
Accept: each remainder item the session answers is committed as a redacted fixture under
`tests/fixtures/github-scaleset/`, and its spec entry is raised to server-observed, citing that
fixture (a refuted assumption changes the model first, rule 7); the minimal GitHub App permission
list, verified, is committed for ADR 0045; an item the session could not settle stays on the
remainder list with the reason. Pending owner item.

#### HW-02 (owner) Live GitHub scale set
Disposable repository and GitHub App with the permissions of ADR 0045's list (verified here, with the reason for each); gated live test of GH-03/GH-04;
(owner) creates the App and secrets. GH-00's remainder is recorded in the same session, with its
capture mode. GH-00S adds (spec §12 items 11 and 12): when the job-started hook runs relative to a
job container's setup, that a failing hook fails the job with no step run, and what a fork PR's
author sees (the annotation, the job page, the Checks tab) from a second account; that a Linux
runner in a scale set whose jobs ask for macOS labels receives its jobs; the refusal sink's
latency from assignment to failure; and that the pinned `maxCapacity` stops new assignments while
a sink is up, with a backlog behind it.

#### HW-03 (owner) Canary and cutover
New scale set and `runs-on` label beside Tartelet; compare queue time, failures, cleanup, disk;
(owner) moves workflows and retires Tartelet.
