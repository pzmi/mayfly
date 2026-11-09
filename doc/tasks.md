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
may override; until then tasks follow them as written. Only the owner flips a decision to
Accepted. The owner's answers to `doc/domain-model.md` §10 (OD1–OD20) and `doc/event-store.md`
§8.3 (E1–E9) are ADRs 0021 and 0024–0040.

| D | Decision | Status |
|---|---|---|
| [D1](adr/0002-control-plane-and-data-plane-in-a-multi-crate-workspace.md) | The multi-crate workspace of plan §4, with the control plane / data plane split of plan §3, is the code structure. The split is enforced by `architecture.toml` edges, not by convention. | Accepted |
| [D2](adr/0003-platforms-macos-arm64-product-linux-buildable.md) | Platforms: the product targets `aarch64-apple-darwin`; every crate also builds and tests on Linux x86_64/arm64 (CI and agent sandbox). Platform code uses `#[cfg]` items, never `cfg!` branches. Windows is out of scope. | Proposed |
| [D3](adr/0004-port-xtask-from-site-preview.md) | `tools/xtask` is ported from `pzmi/site-preview` (same owner, MIT OR Apache-2.0) and adapted, not rewritten: architecture checker with negative fixtures, check catalog, `doctor` with `tools/versions.toml`, `adr`, `land`, action-pin check. Site-preview-specific stages (bench, docs, release specifics) are dropped until needed. | Proposed |
| [D4](adr/0005-integrate-on-main-one-signed-commit-per-task.md) | The whole plan is integrated directly on `main`: one DCO-signed commit per task, subject `<ID>: <summary>`, landed by the orchestrator with `cargo xtask land` after a green gate on the merged tree; implementing agents never push. ⛳ checkpoints and CI on `main` are the quality net. | Accepted |
| [D5](adr/0006-pinned-stable-toolchain-edition-2024-dual-licence.md) | Toolchain pinned to stable 1.97.0, edition 2024, resolver 3; `Cargo.lock` committed; CI uses `--locked`. License `MIT OR Apache-2.0`; no crates.io publishing. | Proposed |
| [D6](adr/0007-event-sourcing-and-cqrs-in-the-control-plane-only.md) | Event sourcing and CQRS live in the **control plane only** (plan §5). The data plane keeps no durable state and re-observes the provider on start (plan §3.1). | Proposed |
| [D7](adr/0008-sqlite-through-rusqlite-as-the-first-journal-adapter.md) | Persistence is behind the storage-neutral `Journal` port (ADR 0021: the stream-scoped event store design, no global order). SQLite through `rusqlite` (bundled) is the first adapter, with WAL, `synchronous=FULL` and forward-only raw-SQL migrations; a PostgreSQL adapter follows. A single writer thread is an implementation detail of the SQLite adapter, never a premise of the port; `sqlx` is rejected (compile-time DB, two SQLite stacks). | Proposed (amended by 0021) |
| [D8](adr/0009-native-async-ports-used-through-generics.md) | Ports are native `async fn`/RPITIT traits used through generics; `async_trait` and `dyn` ports only where object safety is proven necessary (with a note in the ADR). | Proposed |
| [D9](adr/0010-serde-json-link-protocol-with-an-explicit-version.md) | The CP↔DP protocol is serde JSON with an explicit `protocol_version`; the in-process transport still encodes and decodes every message. Secrets cross only as `SecretHandle` (plan §3.2). `serde` derive in `mayfly-domain` is ADR 0017. | Proposed |
| [D10](adr/0011-model-first-with-quint-conformance.md) | **Model first from day one** (unlike site-preview's retrofit): the lifecycle, scheduling, recovery, scaleset and link Quint models precede the Rust code they govern; conformance replays ITF traces against production code (ver §6). | Proposed |
| [D11](adr/0012-independent-fault-lab-and-fakes.md) | The fault lab (`tools/faultlab`) imports **no** production crate: it drives the binaries, reads SQLite with its own SQL and fake-tart/fake-github state with its own parsers, and has its own checker (ver §8). `mayfly-fake-tart` and `mayfly-fake-github` likewise implement their wire behaviour independently. | Proposed |
| [D12](adr/0013-tart-through-its-cli-by-argv-guests-by-tart-exec-or-ssh.md) | Tart is driven only through its CLI by argv. Guests are reached by `tart exec` where the image supports it, else the system `ssh` binary with a pinned per-image host key; no Rust SSH library in v0.x. | Proposed |
| [D13](adr/0014-secrets-are-never-persisted-by-mayfly.md) | Secrets are never persisted by mayfly: `Secret<T>` with redacting `Debug`/`Display`, no `Serialize`; JIT configs live in memory and go to the guest launcher over stdin. A redaction test and the secret scan guard it. | Proposed |
| [D14](adr/0015-owner-gated-real-world-lanes.md) | Real-world lanes (real Tart on macOS, live GitHub scale set) are owner-gated: a manual `hardware.yml` workflow on the owner's self-hosted Mac and a disposable GitHub repository/App. Agents deliver everything up to them. | Proposed |
| [D15](adr/0016-agent-sandbox-and-fail-closed-tools.md) | Agent sandbox: Linux, no Docker, no macOS, no Tart. Quint (Node) and Apalache (JVM) are dev-only tools pinned in `tools/versions.toml`; a missing tool fails closed (never a skipped-but-green check). Actions are pinned by commit SHA. | Proposed |
| [ADR 0017](adr/0017-serde-derive-in-the-shared-kernel.md) | `mayfly-domain` depends on `serde` with `derive` and on no format crate; encoding stays in `mayfly-protocol`, the journal adapter and the golden-fixture harness. | Proposed |
| [ADR 0018](adr/0018-event-codec-and-aggregate-registry.md) | Events implement `DomainEvent`; an aggregate's events form one `event_set!` enum with a codec and a registry, so every kind has its schema version and golden fixture. | Proposed |
| [ADR 0019](adr/0019-plane-separation-over-dev-closures-and-policy-driven-planes.md) | Plane membership and the purity and independence roles are data in `architecture.toml` (`[planes]`, `[purity]`), pinned by a repository test, instead of dev-dependency closures. | Proposed |
| [ADR 0020](adr/0020-control-plane-time-and-the-journal-s-recorded-at.md) | The control plane's `Clock` port reads wall-clock milliseconds since the Unix epoch; every persisted instant is such a reading, and the journal records `recorded_at`. | Proposed |
| [ADR 0021](adr/0021-a-storage-neutral-stream-scoped-event-store.md) | The `Journal` port follows the stream-scoped event store design (`doc/event-store.md`): no global order, readers enumerate streams and read each after a revision. Amends ADR 0008; amended by 0034. | Accepted |
| [ADR 0022](adr/0022-strategic-ddd-with-a-data-oriented-implementation.md) | DDD is strategic (contexts, language, consistency boundaries in `doc/domain-model.md`); code is data-oriented: plain data, free pure functions and tables. | Accepted |
| [ADR 0023](adr/0023-fake-tool-binaries-may-wait-in-real-time.md) | Fake tool binaries may wait in real time (DP-03). | Proposed |
| [ADR 0024](adr/0024-the-data-plane-owns-host-resource-accounting.md) | The data plane accounts for and enforces host resources (memory, disk, macOS-guest slots incl. foreign VMs), refuses directives that do not fit and reports capacity; the control plane allocates only within reported capacity (OD4). | Accepted |
| [ADR 0025](adr/0025-quarantine-keeps-evidence-and-unknown-resolves-only-by-observation.md) | Quarantine whenever ownership or an outcome cannot be proven; never auto-delete a quarantined VM; capacity loss accepted; `Unknown` resolves only by re-observation, no operator assertion (OD9, OD10). | Accepted |
| [ADR 0026](adr/0026-provider-instance-identity-in-the-ownership-proof-and-an-external-epoch-floor.md) | Ownership proof includes the provider instance id (random MAC per clone, re-observed before delete); external epoch floor in `TART_HOME` (OD6, OD7). | Accepted |
| [ADR 0027](adr/0027-trusted-only-trust-tier-in-v0-x.md) | Trusted-only tier in v0.x; untrusted offers declined before acquire; `allow_untrusted_prs` refused (OD3). | Accepted |
| [ADR 0028](adr/0028-scale-sets-only-with-prepared-unregistered-vms.md) | Scale sets only, no classic mode; only prepared, unregistered VMs ahead of demand, no registered standby (OD18, OD8). | Accepted |
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
| [ADR 0040](adr/0040-one-scale-set-per-pool-at-organisation-scope.md) | One scale set per pool, organisation scope, never deleted automatically (OD14). | Accepted |

Open owner decisions (working assumptions, no ADR yet): **OD2** upgrade and rollback (any forward
jump supported through an upgrade-matrix test from every released version; one-step rollback;
deeper rollback restores the automatic pre-upgrade backup; an older build refuses writes on
streams of a newer epoch; dm §10.1); **P1/P3** packaging and uninstall (Homebrew, `mayfly
uninstall --purge`); **P4** the documented GitHub App permission list (philosophy §5.3); the
remaining sub-items of **OD20** (socket authorisation, audit).

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
| KER-01 | Shared kernel: ids, VM names, generations, epochs, fences, ownership proof | FND-02 | sonnet | |
| DDD-01 | Event storming and domain model: contexts, language, aggregates | KER-01 | opus | ⛳ |
| DEC-01 | Record the owner's decisions as ADRs and apply them to the documents | DDD-01 | opus | |
| KER-02 | CP↔DP protocol messages, versions, SecretHandle, link ports | KER-01, FND-06, DEC-01 | opus | |
| KER-03 | Event-sourcing kernel: aggregate, event registry, goldens, determinism | KER-01 | opus | |
| KER-04 | Lifecycle model and `Attempt` aggregate | KER-03, FND-06, DEC-01 | opus | |
| KER-05 | Scheduling model, `Host`/`Pool` aggregates, capacity policy | KER-03, FND-06, DEC-01 | opus | |
| KER-06 | Scale-set model, `ScaleSet` aggregate, demand, trust | KER-03, FND-06, DEC-01 | opus | |
| KER-09 | Registration model and `RunnerRegistration` aggregate | KER-03, FND-06, DEC-01 | opus | |
| KER-07 | Recovery model and decision table | KER-04, KER-05, KER-09, DEC-01 | opus | |
| KER-10 | Operations aggregates: `HostRun`, `RecoveryPlan`, `QuarantineCase`, effective mode | KER-03, KER-07 | opus | |
| KER-11 | Image catalog: `Image`, `ImagePlacement` | KER-03, KER-02 | sonnet | |
| KER-08 | Data-plane domain: executor, supervisor, resource ledger, fencing | KER-02, DEC-01 | opus | ⛳ |
| ES-01 | Storage-neutral journal port, memory journal, journal contract (landed) | KER-03 | opus | |
| ES-01F | Align the landed journal port with the owner's decisions and its review | ES-01, DEC-01 | opus | |
| ES-02 | SQLite journal adapter: schema, triggers, classification, five modes | ES-01F | opus | |
| ES-03 | `AggregateStore`: single write path, receipts, epochs, retry, reactors | ES-01F | opus | |
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
| CP-05 | Registration manager and reaper | KER-09, GH-03, CP-02 | opus | |
| CP-06 | Breaker, quarantine and Unknown-escalation policies | KER-10, CP-03 | sonnet | |
| CP-07 | Image placement manager | KER-11, DP-07 | sonnet | |
| DP-01 | Data-plane executor and observer, resource ledger, fake provider | KER-08 | opus | |
| DP-02 | In-process link transport and link contract suite | KER-02 | sonnet | |
| DP-03 | `mayfly-fake-tart` binary with fault knobs (landed) | FND-02 | sonnet | |
| DP-04 | Process runner, per-VM shim and Tart adapter, VmProvider contract suite | DP-01, DP-03 | opus | |
| DP-05 | Guest transport (`tart exec`) and runner launcher | DP-04 | opus | |
| DP-06 | Host resource probes | DP-01 | sonnet | |
| DP-07 | Image cache directives and `ImageStore` port | DP-04, KER-11 | opus | |
| NET-01 | Softnet privilege model: design, ADR, `doctor` checks | DP-04 | opus | |
| RT-01 | Config schema, validation, state-dir lock | KER-01 | sonnet | |
| SEC-02 | `SecretProvider` port and backends by URI | RT-01 | opus | |
| RT-02 | Composition: wiring, supervision tree, startup, `mayflyd` | CP-04, DP-02, DP-04, DP-06, ES-05, RT-01, SEC-02 | opus | |
| RT-03 | Operator socket API and `mayfly` CLI | RT-02 | sonnet | |
| RT-04 | Observability: tracing JSON, metrics, redaction tests | RT-02 | sonnet | |
| E2E-01 | Walking skeleton: static demand, fake-tart, one job end to end | RT-03, DP-05 | opus | ⛳ |
| GH-00 | (owner) Capture a live scale-set session in a disposable organisation | — | sonnet | |
| GH-01 | `mayfly-fake-github`: Scale Set and REST fake server | FND-02, GH-00 | opus | |
| GH-02 | GitHub App auth, HTTP client, rate limits | RT-01, SEC-02 | sonnet | |
| GH-03 | Scale Set client and recorded-fixture contract | GH-01, GH-02 | opus | |
| GH-04 | Listener use case: inbox, statistics demand, acquire policy, ack after commit | GH-03, KER-06, CP-04 | opus | |
| E2E-02 | Scale-set end to end, 100-job backlog on 2 slots | GH-04, CP-05, E2E-01 | sonnet | ⛳ |
| LNK-01 | UDS transport and split control/data roles | DP-02, RT-02 | sonnet | |
| VER-01 | Conformance crate: ITF replay, lifecycle driver | KER-04, FND-06 | opus | |
| VER-02 | Conformance drivers: scheduling, recovery, scaleset, link, registration | VER-01, CP-04, GH-04, LNK-01, KER-09 | opus | |
| VER-03 | Fault lab: controller, history, verdicts, guardrails | E2E-01 | opus | |
| VER-04 | Fault lab: nemesis, checker, core scenarios (ver §8.4) | VER-03, E2E-02, LNK-01 | opus | ⛳ |
| VER-05 | Curated mutants (negative controls) and mutation lane | VER-02, VER-04 | sonnet | |
| VER-06 | Fuzz targets and nightly fuzz lane | DP-04, GH-03, RT-01 | sonnet | |
| VER-07 | Apalache invariant verification lane | KER-07, KER-06, KER-09 | sonnet | |
| SEC-01 | Threat model and secret-handling audit | RT-04, DP-05 | opus | |
| OPS-01 | launchd service and foreground mode, install/uninstall, real `doctor` checks | RT-03 | sonnet | |
| OPS-02 | Image manifest, `mayfly image verify`, digest pinning | RT-01 | sonnet | |
| OPS-03 | Release workflow: macOS binaries, checksums, provenance, SBOM | FND-04, OPS-01 | sonnet | ⛳ |
| OPS-04 | Runbooks and operator docs | OPS-01, VER-04 | haiku | |
| OPS-05 | Watchdog, heartbeat metric, distinct exit codes | RT-02, RT-04 | sonnet | |
| OPS-06 | `mayfly image refresh <pool>` and runner-age warnings | CP-07, DP-05, OPS-02 | sonnet | |
| OPS-07 | `mayfly diagnostics bundle` | RT-03, RT-04 | sonnet | |
| HW-01 | (owner) Real Tart lane on the owner's Mac | DP-05, OPS-01 | sonnet | |
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
`std::process` allowed only in `mayfly-adapter-tart`, `mayfly-adapter-guest`, `mayfly-composition`
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
listed checks, if the owner wants one.

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

#### ARCH-02 Close the checker bypasses of the second FND-04 review
Cites: fnd §2–§3, ADR 0019. The adversarial re-review of ARCH-01 bypassed the checker with a renamed
`include!`, `include!` inside macros, clock and fs reads through aliases, `Self` and associated
types, cross-crate `pub type` aliases, exported macros and declarations of the other plane's
vocabulary. Landed: ARCH-include-macro, ARCH-pure-clippy-config (semantic `clippy.toml` layer per
pure crate), ARCH-macro-export, declaration checks in ARCH-plane-vocabulary, YAML-parsed CI
lockstep, the xtask checkout preflight, shared-crate dev edges (ADR 0019 amended).
Accept: each bypass has a negative fixture failing for its rule; `cargo xtask check` green.

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
(Landed: `doc/domain-model.md`; the owner reviewed it on 2026-11-09, applied by DEC-01.)

#### DEC-01 Record the owner's decisions as ADRs and apply them
Cites: dm §10–§11, event-store §8, philosophy, ADR 0001. ADRs 0024–0040 for OD1, OD3–OD20, E2–E9,
P2; ADR 0021 Accepted; ADR 0001 amended (record-not-rule, dated amendments, `Rejected`, ADR number
as id); plan, domain model, event store, verification, philosophy, foundation, CLAUDE.md and this
file follow; GH-05 removed. Open: OD2, P1/P3, P4.
Accept: `cargo xtask check --only docs` green; every decided OD/E names its ADR.

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
statistics, acquired ids) and pure `demand::desired(stats, warm, capacity)`.
Accept: tests for the mutants "count messages", "add TotalRunningJobs", "ack before record" fail
the model or the unit tests (ver §9 items 4–7).
DEC-01 (ADRs 0027–0029, 0040): `PlanAcquire` (I23: acquire only while assigned < deliverable),
trust classification (I32: trusted only; `pull_request_target`, `workflow_run`, `issue_comment`
and unclassifiable offers declined), per-scale-set carve-out input with a revision, `desired =
min(deliverable, assigned)` (no standby), initial statistics authoritative, unknown message types
(R7), the wire behaviour of pause, drain and safe mode; stream `scaleset-<pool>`. Mutants add
"acquire every offer" (19).

#### KER-07 Recovery model and decision table
Cites: plan §10, dm §11.3, ADRs 0025, 0026, 0037. Model first: `formal/recovery.qnt` (crash at every
await, epochs and the external floor, unknown outcomes, incomplete inventories; I04, I05, I09,
I10, I11, I12, I17, I25, I29, I31; model-local `noAssertion`). Pure
`operations::recovery::classify(journal_view, provider_view, github_view) -> Decision` covering
every row of plan §10's table (shim alive or gone, `exit.json`, instance-id mismatch → quarantine,
inventory incomplete → wait) plus safe mode. No row asserts an outcome or deletes a quarantined VM.
Accept: exhaustive table test; `mf-*` VM unknown to the journal is never `Delete`; an instance-id
mismatch is never `Delete`; an incomplete inventory releases nothing.

#### KER-08 ⛳ Data-plane domain
Cites: plan §3.1, §6 (DP FSMs), §8.4, §9; ADRs 0024, 0026, 0037. `mayfly-data-domain`: directive
executor FSM (`Received → Executing → Done | Refused | Unknown`), idempotency table semantics (same
`directive_id` → same outcome), epoch fencing and the floor, safe-mode refusals (I29),
ownership-proof check for `StopVm`/`DeleteVm` **after re-observing the instance id** and refusal of
any base name, VM supervisor FSM derived from observations (shim alive, `exit.json`,
`ProcessLost`), inventory `Complete | Incomplete` (I25), and the **resource ledger** (pure: totals,
used by ours and foreign VMs, in-flight reservations, fit check per directive, `CapacityReported`
content) with refusal `insufficient_resources` (I33).
Accept: exhaustive matrices; stale epoch refused; delete without matching proof or with a changed
instance id refused; a directive that does not fit is refused whatever the CP sent (mutant 24).

#### KER-09 Registration model and `RunnerRegistration` aggregate
Cites: dm §6.5, P10–P14, ADRs 0028, 0029, 0038. Model first: `formal/registration.qnt` (I19, I20,
I21, I22's registration half; deregister-before-stop, the 422 race, issue unknown; model-local
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

#### ES-04 ⛳ Projections
Cites: event-store §3.6, §6.1, §6.4, I16. `ProjectionSpec`, stream-local inline projections computed
outside the transaction, `projection_row` documents with generations, rebuild by stream enumeration
with applied revisions (offline form here; online in ES-12), first projections `attempts`,
`slots`, `pools`, `pending_effects`, `recent_failures` (query-time views for cross-stream counts).
PC-01..PC-06 on memory and SQLite; `ARCH-projection-locality`.
Accept: PC green; "rebuild equals live" property; a cross-stream inline write is refused.

#### ES-06 Event-store model
Cites: event-store §7.3, D10, ver §6; ADR 0034 (E7: before delivery). `formal/event_store.qnt` and
its scenarios module with `step`, `implStepSerial`, the biased relation and `allInvariants`,
including refusal receipts bound to revisions (`refusalReplayedAtSameRevision`) and, for ES-12, the
rebuild-generation part (`shadowRebuildDualWrite`, `cutoverFence`); every invariant shown to catch a
scratch mutation; registered in `MODELS`; regression traces exported.
Accept: scenarios pass; the two negative worlds break their invariants; `formal-freshness` green.

#### ES-05 Durable delivery
Cites: event-store §3.4, §5; ADR 0034 (E2). `DeliveryLedger` (memory, SQLite), marks written by the
append for registered consumers, `DeliveryWorker`, `catch_up`, kicks, `retry_after` with jittered
backoff, DC-01..DC-25 with `UnfencedLedger`, `ClearAllMarksLedger`, `HeadTrustingWorker`,
`SkippingLedger`; the `DirectiveIssued` event shape and the `directive_relay` consumer skeleton.
Accept: DC green on memory and SQLite (all modes); every broken ledger fails its cases.

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
Cites: event-store §6.5; dm OD2 (working assumption). `cargo xtask compat` with a scenario corpus,
run as N → N+1 → N on SQLite locally and on both engines in CI, plus an **upgrade matrix** from
every released version to the current build (forward jumps of any size), from the first tagged
release on. Revisit when the owner decides OD2.
Accept: a deliberately incompatible event change (a rename) fails it; a skipped-version upgrade in
the matrix passes.

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

#### CP-03 Reconciler
Cites: plan §9, §10; dm P5, P22, P23; ADRs 0024, 0028. Level-triggered pass (≤ 1/min plus events):
desired from the scale sets, capacity from the latest **reported** capacity per host, per-scale-set
carve-outs (I24), shortfall, `min_prepared`, hysteresis; reserve, retire, cleanup commands through
the capacity policy and the recovery table; a data-plane `insufficient_resources` refusal is a
definite no and re-read next pass.
Accept: 1,000-step seeded simulation in testkit never violates I02/I03/I04/I07/I08/I23/I24/I26/I27
and never allocates beyond a report.

#### CP-04 Observation inbox and startup recovery
Cites: plan §10, §3.2; dm P1, P3; event-store §5.4, §6.3; ADRs 0025, 0026, 0037. Observations enter
through units with `InboxKey { data_plane, observation_id }` (stale generations and epochs recorded
as ignored); the startup use case P1: migration floor and stored-kinds checks, epoch floor (I17),
`StartRun`/`MintEpoch`, handshake, inventory with shim locks and `exit.json`, `Unknown` resolved
only by observation, classification, `AdmissionOpened{recovery_done}`, then `catch_up`.
Accept: component tests for each recovery row; safe mode admits nothing and deletes nothing; F35,
F42, F43 at component level.

#### CP-05 Registration manager and reaper
Cites: dm §6.5, P9–P14; ADRs 0029, 0038. Mint JIT through the scale-set client once per
registration, record issued/unknown, deregister-before-stop with the 422 race, the online
confirmation after delivery, the reaper (run start, long outage, daily, `doctor`; ≤ 1 listing/hour;
only journaled ids; unrecognised reported).
Accept: F06, F07, F32 component tests; only journaled ids deleted; unrecognised reported; no
name-pattern deletion (`NameGuessingRegistry` fails the contract).

#### CP-06 Breaker, quarantine and Unknown-escalation policies
Cites: dm P15, P16, P19; ADR 0025. Breaker counting and trips per (pool, digest); quarantine opening
(ERROR, metric, status), escalation levels T_q1/T_q2 that never act on the VM, no timer and no cap
deleting anything (I28); `Unknown` re-observation at T_u1/T_u2 and the provider-limitation report.
There is no operator assertion.
Accept: F36, F46, F47 component tests on the fake clock; mutant 26 (delete on a timer) and 27
(resolve without observation) caught.

#### CP-07 Image placement manager
Cites: dm P17, P18; ADRs 0030, 0039. Pin → placement on every host serving the pool; the pool is
admissible on a host only once its base is materialised; prepared attempts on an old digest
retired; unreferenced digests evicted; `WithdrawImage` refused while pinned; runner-version expiry
trips the breaker.
Accept: component tests on fakes; withdraw refused while pinned; never a clone from an OCI
reference.

#### DP-01 Data-plane executor and observer
Cites: plan §3.1, §8.1, §9; ADR 0024. `mayfly-data`: ports `VmProvider`, `ImageStore`,
`GuestTransport`, `HostProbe`; executor applying the KER-08 FSM with per-directive deadlines and
bounded concurrency; the resource ledger fed by inventory and probes, refusing what does not fit
and emitting `CapacityReported` on handshake, change and request; inventory observer emitting
`Inventory`/`InventoryIncomplete`; in-memory fake provider with scripted faults in the data-plane
test kit (ADR 0019).
Accept: component tests: duplicate directive executes once; ambiguous provider error → `Unknown`;
restart rebuilds ledger and inventory from observation; an over-allocating CP is refused (I33).

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
fake-tart binary, and (gated by `MAYFLY_REAL_TART_HOME`, macOS only, used by HW-01) real Tart.
Accept: contract green on both fakes; hang → deadline → `Ambiguous` + re-inspect; a daemon killed
and restarted re-adopts a running VM through its shim (F48 at integration level); no signal is ever
sent to `tart run`.

#### DP-05 Guest transport and runner launcher
Cites: plan §8.3, D13; ADR 0038. `tart exec` transport only (no SSH): nonce verification
(`VerifyGuest`), typed bounded claims, the fixed launcher contract with the JIT config over
`tart exec -i` stdin after the lock-PID = shim-child and instance-id checks, launcher status file
for runner exit after a restart, diagnostics export bounded and redacted. fake-tart `exec` emulates
a guest whose runner "runs a job" per its fault file.
Accept: secret never appears in argv/env/log (the fake's recorded invocations); a foreign lock PID
or a changed MAC refuses delivery (F39); `UncheckedLockTransport` fails the contract.

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
Accept: contract green on memory, file, env and exec; Keychain compiles on macOS and runs in the
hardware lane; no secret in any `Debug` output.

#### RT-02 Composition and `mayflyd`
Cites: plan §3, §10, §4. Wire both planes with the inproc link; supervision tree with a root
cancellation token; startup sequence of plan §10; SIGTERM → drain-safe shutdown; `--role all`.
Accept: integration test starts `mayflyd` against fake-tart and a static demand, kills it, restarts it, and the inventory converges.
DEC-01: registers the store's consumers, takes the owner lease, runs event-store §6.3 then
`catch_up`, owns the kick wiring and timer; secrets through SEC-02; foreground mode with
Ctrl-C → P24 (ADR 0031).

#### RT-03 Operator socket API and `mayfly` CLI
Cites: plan §11. Unix socket `0600`, JSON request/response, commands of plan §11 that exist so far
(`status`, `vm list`, `events --follow`, `pool drain|resume|pause`, `recover --dry-run|--apply`,
`config validate`, `doctor` basic).
Accept: `assert_cmd` black-box tests; `--json` outputs are golden-tested.
DEC-01 adds the CLI of plan §11: `host drain|resume`, `pool reset-breaker`, `quarantine
list|inspect|resolve`, `safe-mode enter`, `events --follow` per event-store §5.6; peer-uid check;
an actor on every mutating command. No `attempt assert` (ADR 0025).

#### RT-04 Observability
Cites: plan §12. tracing JSON subscriber in composition, event naming rule, metrics endpoint
(socket or loopback), redaction test feeding secrets through every log path.
Accept: metric names and labels golden-tested; no id-valued label.
DEC-01: the metrics of dm §8 (loopback HTTP endpoint), the quarantine ERROR event, nothing sent
anywhere (ADR 0036).

#### E2E-01 ⛳ Walking skeleton
Cites: plan §14 phase 3, ver §2. Black-box: `mayflyd` (static demand 1) + `mayfly-fake-tart` +
fake guest runner completes one "job", VM deleted, slot released; then a demand of 3 with 2 slots
never exceeds 2; then kill -9 mid-clone and restart converges without duplicates.
Accept: these tests in the e2e level (the `e2e-tests` stage, registered here), green in CI on Linux.

#### GH-00 (owner) Capture a live scale-set session
Cites: dm §9.1, §10 [VERIFY] items, ADR 0040. In a disposable organisation: redacted traces of
session open, poll, acquire, JIT generation, job start and completion, cancel, 409, session expiry,
offer fields usable for trust classification, how long an unclaimed JIT registration lingers, and
whether `maxCapacity` is read as a total. Agents prepare the capture tool and redaction; (owner)
runs it.
Accept: the traces and answers committed (redacted); GH-01 cites them.

### Phase 4 — GitHub

#### GH-01 `mayfly-fake-github`
Cites: plan §7, ver §7, D11. Axum-based fake of the Scale Set endpoints (scale set CRUD, session,
long-poll messages with redelivery until delete, 50-message cap, statistics, acquire, JIT) and the
REST runner endpoints (registration token, list/delete runners), plus fault knobs (401, 403, 429
with Retry-After, dropped response after effect). Wire shapes from the `actions/scaleset` Go source
and GitHub docs, cited in comments; no production crate imported.
Accept: its own tests; a scripted job queue API for e2e.
DEC-01: depends on GH-00; built from its capture; no registration-token endpoint (ADR 0028);
fault knobs add 409 and 422.

#### GH-02 GitHub App auth and HTTP client
Cites: plan §7.1, D13. RS256 JWT, installation token cache with refresh-before-expiry, reqwest +
rustls, retry policy honouring Retry-After, request metrics.
Accept: tests against fake-github; token never in Debug output.

#### GH-03 Scale Set client
Cites: plan §7.2, §7.4. `ScaleSetApi` port implementation; recorded redacted fixtures as the
contract (`tests/fixtures/github-scaleset/`), run against fake-github and the fixture replayer.
Accept: session expiry, 429, ack-after-lost-response cases.
DEC-01: organisation scope (repository scope for tests); scale sets never deleted by the client
(ADR 0040); runner list/get/delete by id for CP-05.

#### GH-04 Listener use case
Cites: plan §7.2, KER-06. Poll → acquire → record (inbox + statistics + desired) → ack; empty poll
still reconciles; never blocks on VM work.
Accept: component tests F08, F09, F10, F11, F25, F27 (ver §8.4) on fakes.
DEC-01: the record step is one unit with `InboxKey { github, message_id }`; ack after commit;
`DuplicateMessage` acked without a write; `PlanAcquire` before `AcquireJobs` (I23, I32); wire
behaviour of pause, drain and safe mode (ADR 0029); adds F41, F42, F44.

#### E2E-02 ⛳ Scale-set end to end
Cites: plan §7.2, §9. `mayflyd` + fake-tart + fake-github: queue 1 job → 1 VM → job done → deleted;
queue 100 jobs with 2 slots → never more than 2 VMs, all jobs complete, no double provision;
cancel/reassign case.
Accept: e2e tests green in CI.
DEC-01: adds the decline of an untrusted offer and a cancel during deregistration (F33).

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

#### VER-02 Conformance drivers
Cites: ver §6. Drivers for scheduling (capacity policy + Host), recovery (startup use case with
SQLite journal and fake provider), scaleset (listener with fake ScaleSetApi), link (inproc and
UDS).
Accept: each driver has a negative control; CI runs bounded seeds per model.
DEC-01: adds the registration driver (KER-09) and the resource-ledger comparison in scheduling.

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
`formal.yml` (scheduled + on model changes); depths and timings in `doc/formal-models.md`.
Accept: every invariant listed in ver §6.1 is checked; a seeded bad invariant fails.
DEC-01: adds `registration` and `event_store` to the lane.

#### SEC-01 Threat model and secret audit
Cites: plan §13. `doc/security.md` (assets, actors, boundaries, mitigations with tests),
audit of every secret path, fixes as needed.
Accept: every mitigation names its test.
DEC-01: trust tier and fork-approval settings (ADR 0027), reachability with and without Softnet
(ADR 0032, NET-01), the secret providers (ADR 0033), the diagnostics bundle (ADR 0036), the
planned rule that only the GitHub adapter opens outbound connections, residual risks (a JIT config
is visible to the workflow it serves; hypervisor escape).

### Phase 7 — operate

#### OPS-01 launchd service, foreground mode and doctor
Cites: plan §11, §13; ADR 0031. Foreground `mayflyd` documented for local use; LaunchAgent plist
(auto-login session, `KeepAlive` with exit-code handling so a lock refusal is not restarted
forever, `ThrottleInterval`, `ExitTimeOut`), `mayfly service install|uninstall`, `doctor` checks
(provider version, `TART_HOME` `0700` and its deployment marker, disk, memory, socket permissions,
secret references resolvable, sleep settings FAIL, clock). Uninstall removes what mayfly created
and nothing else (philosophy §4.3; the packaging channel and `--purge` are OWNER P1/P3, pending).
Accept: plist golden; doctor JSON golden on Linux with fakes; (owner) install on the Mac.

#### OPS-02 Image manifest
Cites: plan §8, OPS. Manifest schema (`schema`, provider, OS, arch, digest-pinned source, runner
version, capabilities, host requirements), `mayfly image verify`, refusal of tag-only references.
Accept: valid/invalid fixtures.
DEC-01: manifest fields runner version and its SHA, launcher hash, guest-agent requirement (images
without it refused, ADR 0038); `image withdraw` with CP-07.

#### OPS-03 ⛳ Release workflow
Cites: fnd §7. Tag-triggered release: macOS arm64 (and Linux) archives, `SHA256SUMS`, build
provenance attestation, CycloneDX SBOM; signing/notarisation documented as (owner) follow-up.
Accept: dry-run on a branch produces artefacts; (owner) first tag.

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

#### OPS-06 `mayfly image refresh <pool>`
Cites: ADRs 0030, 0039; dm P17, P18. The command (pull by digest and record it; base named by
digest and runner version; boot once and install the pinned SHA-checked runner via `tart exec`;
verify; move the pool's base pointer keeping the previous; GC unreferenced bases), runner-age
warnings in `status`, the optional daily runner-only refresh per pool, rollback to the previous
base.
Accept: e2e on fake-tart: refresh, use, roll back; a SHA mismatch refuses the runner.

#### OPS-07 `mayfly diagnostics bundle`
Cites: ADR 0036, philosophy §5.1. A local, redacted archive (version, config with references only,
`doctor --json`, `status --json`, recent logs, event kinds and counts, projection summaries,
provider and OS versions) and its listing; nothing sent anywhere.
Accept: a canary-secret test over every bundle file; golden listing.

#### HW-01 (owner) Real Tart lane
`hardware.yml` (manual dispatch, self-hosted label on the owner's Mac): VmProvider contract on real
Tart in a dedicated `TART_HOME`, one-job run with a tiny Linux image. Agents write the workflow and
docs; (owner) registers the self-hosted runner and runs it.
DEC-01: also verifies `tart set --random-mac` and MAC read-back, `tart exec -i`, the run lock and
its PID, shim survival across a daemon kill and `tart stop` semantics.

#### HW-02 (owner) Live GitHub scale set
Disposable repository and GitHub App with documented permissions; gated live test of GH-03/GH-04;
(owner) creates the App and secrets.

#### HW-03 (owner) Canary and cutover
New scale set and `runs-on` label beside Tartelet; compare queue time, failures, cleanup, disk;
(owner) moves workflows and retires Tartelet.
