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

Each decision becomes an ADR in FND-03; D*n* is ADR *n*+1 (ADR 0001 is "Record architecture
decisions"). **Proposed** decisions are planner defaults the owner may override; until then tasks
follow them as written. Only the owner flips a decision to Accepted.

| D | Decision | Status |
|---|---|---|
| D1 | The multi-crate workspace of plan §4, with the control plane / data plane split of plan §3, is the code structure. The split is enforced by `architecture.toml` edges, not by convention. | Accepted |
| D2 | Platforms: the product targets `aarch64-apple-darwin`; every crate also builds and tests on Linux x86_64/arm64 (CI and agent sandbox). Platform code uses `#[cfg]` items, never `cfg!` branches. Windows is out of scope. | Proposed |
| D3 | `tools/xtask` is ported from `pzmi/site-preview` (same owner, MIT OR Apache-2.0) and adapted, not rewritten: architecture checker with negative fixtures, check catalog, `doctor` with `tools/versions.toml`, `adr`, `land`, action-pin check. Site-preview-specific stages (bench, docs, release specifics) are dropped until needed. | Proposed |
| D4 | The whole plan is integrated directly on `main`: one DCO-signed commit per task, subject `<ID>: <summary>`, landed by the orchestrator with `cargo xtask land` after a green gate on the merged tree; implementing agents never push. ⛳ checkpoints and CI on `main` are the quality net. | Accepted |
| D5 | Toolchain pinned to stable 1.97.0, edition 2024, resolver 3; `Cargo.lock` committed; CI uses `--locked`. License `MIT OR Apache-2.0`; no crates.io publishing. | Proposed |
| D6 | Event sourcing and CQRS live in the **control plane only** (plan §5). The data plane keeps no durable state and re-observes the provider on start (plan §3.1). | Proposed |
| D7 | Persistence is SQLite through `rusqlite` (bundled), owned by one writer thread behind the async `Journal` port; `sqlx` is rejected (compile-time DB, two SQLite stacks). WAL, `synchronous=FULL`, forward-only raw-SQL migrations. | Proposed |
| D8 | Ports are native `async fn`/RPITIT traits used through generics; `async_trait` and `dyn` ports only where object safety is proven necessary (with a note in the ADR). | Proposed |
| D9 | The CP↔DP protocol is serde JSON with an explicit `protocol_version`; the in-process transport still encodes and decodes every message. Secrets cross only as `SecretHandle` (plan §3.2). | Proposed |
| D10 | **Model first from day one** (unlike site-preview's retrofit): the lifecycle, scheduling, recovery, scaleset and link Quint models precede the Rust code they govern; conformance replays ITF traces against production code (ver §6). | Proposed |
| D11 | The fault lab (`tools/faultlab`) imports **no** production crate: it drives the binaries, reads SQLite with its own SQL and fake-tart/fake-github state with its own parsers, and has its own checker (ver §8). `mayfly-fake-tart` and `mayfly-fake-github` likewise implement their wire behaviour independently. | Proposed |
| D12 | Tart is driven only through its CLI by argv. Guests are reached by `tart exec` where the image supports it, else the system `ssh` binary with a pinned per-image host key; no Rust SSH library in v0.x. | Proposed |
| D13 | Secrets are never persisted by mayfly: `Secret<T>` with redacting `Debug`/`Display`, no `Serialize`; JIT configs live in memory and go to the guest launcher over stdin. A redaction test and the secret scan guard it. | Proposed |
| D14 | Real-world lanes (real Tart on macOS, live GitHub scale set) are owner-gated: a manual `hardware.yml` workflow on the owner's self-hosted Mac and a disposable GitHub repository/App. Agents deliver everything up to them. | Proposed |
| D15 | Agent sandbox: Linux, no Docker, no macOS, no Tart. Quint (Node) and Apalache (JVM) are dev-only tools pinned in `tools/versions.toml`; a missing tool fails closed (never a skipped-but-green check). Actions are pinned by commit SHA. | Proposed |

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
8. **Decisions:** a new or reversed architectural decision adds an ADR (`cargo xtask adr new`, with
   `--number N` when reserved) in the same commit. Agents never flip Proposed to Accepted.
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
tags `checkpoint-<ID>` locally and reports to the owner. Checkpoints: **FND-04** (gate + CI),
**KER-08** (kernel and models), **ES-04** (journal), **E2E-01** (walking skeleton), **E2E-02**
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
| KER-01 | Shared kernel: ids, VM names, generations, epochs, fences | FND-02 | sonnet | |
| KER-02 | CP↔DP protocol messages, versions, SecretHandle, link ports | KER-01, FND-06 | opus | |
| KER-03 | Event-sourcing kernel: aggregate, event registry, goldens, determinism | KER-01 | opus | |
| KER-04 | Lifecycle model and `Attempt` aggregate | KER-03, FND-06 | opus | |
| KER-05 | Scheduling model, `Host`/`Pool` aggregates, capacity policy | KER-03, FND-06 | opus | |
| KER-06 | Scale-set model, `ScaleSet` aggregate, demand calculation | KER-03, FND-06 | opus | |
| KER-07 | Recovery model and decision table | KER-04, KER-05 | opus | |
| KER-08 | Data-plane domain: directive executor and VM supervisor FSMs | KER-02 | opus | ⛳ |
| ES-01 | Journal port, in-memory journal, journal contract suite | KER-03 | opus | |
| ES-02 | SQLite journal: migrations, receipts, outbox, inbox, crash matrix | ES-01 | opus | |
| ES-03 | AggregateStore single write path, command bus, reactors | ES-01 | opus | |
| ES-04 | Projections: specs, versions, rebuild, first read models | ES-02, ES-03 | sonnet | ⛳ |
| CP-01 | Commands and queries (CQRS rule live), operator use cases | ES-04, KER-05 | sonnet | |
| CP-02 | AttemptSaga and OutboxRelay | ES-04, KER-04, KER-02 | opus | |
| CP-03 | Reconciler: desired vs observed, admission, retirement | CP-02, KER-07 | opus | |
| CP-04 | Observation inbox and startup recovery / safe mode | CP-03 | opus | |
| DP-01 | Data-plane executor and observer, provider ports, fake provider | KER-08 | opus | |
| DP-02 | In-process link transport and link contract suite | KER-02 | sonnet | |
| DP-03 | `mayfly-fake-tart` binary with fault knobs | FND-02 | sonnet | |
| DP-04 | Process runner and Tart adapter, VmProvider contract suite | DP-01, DP-03 | opus | |
| DP-05 | Guest transport (`tart exec`, pinned SSH) and runner launcher | DP-04 | opus | |
| DP-06 | Host resource probes | DP-01 | sonnet | |
| RT-01 | Config schema, validation, file secret store, state-dir lock | KER-01 | sonnet | |
| RT-02 | Composition: wiring, supervision tree, startup, `mayflyd` | CP-04, DP-02, DP-04, DP-06, ES-02, RT-01 | opus | |
| RT-03 | Operator socket API and `mayfly` CLI | RT-02 | sonnet | |
| RT-04 | Observability: tracing JSON, metrics, redaction tests | RT-02 | sonnet | |
| E2E-01 | Walking skeleton: static demand, fake-tart, one job end to end | RT-03, DP-05 | opus | ⛳ |
| GH-01 | `mayfly-fake-github`: Scale Set and REST fake server | FND-02 | opus | |
| GH-02 | GitHub App auth, HTTP client, rate limits | RT-01 | sonnet | |
| GH-03 | Scale Set client and recorded-fixture contract | GH-01, GH-02 | opus | |
| GH-04 | Listener use case: inbox, statistics demand, ack after commit | GH-03, KER-06, CP-04 | opus | |
| GH-05 | Classic ephemeral registration adapter | GH-02 | sonnet | |
| E2E-02 | Scale-set end to end, 100-job backlog on 2 slots | GH-04, E2E-01 | sonnet | ⛳ |
| LNK-01 | UDS transport and split control/data roles | DP-02, RT-02 | sonnet | |
| VER-01 | Conformance crate: ITF replay, lifecycle driver | KER-04, FND-06 | opus | |
| VER-02 | Conformance drivers: scheduling, recovery, scaleset, link | VER-01, CP-04, GH-04, LNK-01 | opus | |
| VER-03 | Fault lab: controller, history, verdicts, guardrails | E2E-01 | opus | |
| VER-04 | Fault lab: nemesis, checker, core scenarios (ver §8.4) | VER-03, E2E-02, LNK-01 | opus | ⛳ |
| VER-05 | Curated mutants (negative controls) and mutation lane | VER-02, VER-04 | sonnet | |
| VER-06 | Fuzz targets and nightly fuzz lane | DP-04, GH-03, RT-01 | sonnet | |
| VER-07 | Apalache invariant verification lane | KER-07, KER-06 | sonnet | |
| SEC-01 | Threat model and secret-handling audit | RT-04, DP-05 | opus | |
| OPS-01 | launchd service, install/uninstall, real `doctor` checks | RT-03 | sonnet | |
| OPS-02 | Image manifest, `mayfly image verify`, digest pinning | RT-01 | sonnet | |
| OPS-03 | Release workflow: macOS binaries, checksums, provenance, SBOM | FND-04, OPS-01 | sonnet | ⛳ |
| OPS-04 | Runbooks and operator docs | OPS-01, VER-04 | haiku | |
| HW-01 | (owner) Real Tart lane on the owner's Mac | DP-05, OPS-01 | sonnet | |
| HW-02 | (owner) Live GitHub scale set on a disposable repository | GH-04, HW-01 | sonnet | |
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
Cites: ver §2, §10. `.config/nextest.toml` (default, ci, serial profiles), `mayfly-testkit::poll`
(the only place a test may wait on a real-OS condition, bounded, with a reason), coverage stage
with `cargo-llvm-cov` and `coverage-floor.toml` (start 85% overall, 90% changed lines, ratcheted).
Accept: `check` runs nextest; a sleeping test fails the hygiene rule; coverage stage in CI.

#### FND-06 Formal toolchain
Cites: ver §6, D10, D15. `formal/package.json` + lockfile pinning Quint; Apalache pinned with
SHA-256 in `tools/versions.toml`; xtask stages `formal-typecheck` (all `.qnt`), `formal-test`
(Quint tests), the `MODELS` registry (empty), `formal.yml` workflow. A trivial `formal/smoke.qnt`.
Accept: stages fail closed when Quint is missing; CI runs them.

### Phase 1 — kernel (pure crates, models first)

#### KER-01 Shared kernel
Cites: plan §4, §8.4. `mayfly-domain`: `PoolName` and `HostId` restricted to `[a-z0-9]{1,16}` so the name separators are unambiguous, `AttemptId`, `HostId`, `PoolName`, `DirectiveId`,
`ObservationId`, `CommandId`, `Generation`, `Epoch`, `Fence`, `OwnedVmName` (format and strict
parse of `mf-<pool>-<host>-s<slot>-g<gen>-<short>`; rejects anything else), `ImageDigest`,
`Resources`, `Millis`/`Deadline` value types (no clock), error types. No serde in the domain unless
the ADR for D9 allows `serde` derive here (decide and record).
Accept: proptest round-trip format/parse; rejection table for malformed names; foreign names never
parse as owned.

#### KER-02 CP↔DP protocol
Cites: plan §3.2, D9, D13. Model first: `formal/link.qnt` (at-least-once, duplicates,
reordering, epoch fencing; invariant: no directive from an older epoch executes; every directive
id executes at most once per epoch). `mayfly-protocol`: `Hello`/`Welcome`, `Directive`,
`Observation`, `DirectiveOutcome` (`Refused` definite vs `Unknown`), `SecretHandle`,
`ProtocolVersion` negotiation (pure fn), link port traits (`DirectiveSink`, `ObservationSink`, and
their receiving halves) as transport-agnostic async traits.
Accept: golden JSON fixture per message kind and version; unknown-major refusal test; model typechecks and its tests pass.

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

#### KER-04 Lifecycle model and `Attempt` aggregate
Cites: plan §6, ver §6, D10. Model first: `formal/lifecycle.qnt` with invariants I06, I10, I13, I14
(names in ver §6.1), scenario tests. Adds the `ARCH-exhaustive-transition` rule (fnd §3.3). Then the `Attempt` aggregate: states and stimuli of plan §6,
exhaustive State × Stimulus matrix (no wildcard arms; an architecture or unit test proves every
pair is listed), events with goldens, determinism cases.
Accept: matrix test covers every pair; model tests pass; register the model in `MODELS`.

#### KER-05 Scheduling model, `Host`/`Pool`, capacity policy
Cites: plan §9, §5.1. Model first: `formal/scheduling.qnt` (I01, I02, I03, I07, I08, I13). `Host`
aggregate (epoch, slots, monotonic generations, reservations, drain, safe mode), `Pool` aggregate,
pure `capacity::plan(inputs) -> CapacityReport + decisions`.
Accept: proptest "admitted ≤ every limit" over random inputs; two macOS guests max; drain admits
nothing; generations strictly increase per slot.

#### KER-06 Scale-set model, `ScaleSet` aggregate, demand
Cites: plan §7.2. Model first: `formal/scaleset.qnt` (redelivery, ack loss, >50 messages,
reassignment; invariants: no double reservation per request id, desired derives from latest
statistics only, ack only after durable record; I12, shared with recovery). `ScaleSet` aggregate (session FSM, cursor,
statistics, acquired ids) and pure `demand::desired(stats, warm, capacity)`.
Accept: tests for the mutants "count messages", "add TotalRunningJobs", "ack before record" fail
the model or the unit tests (ver §9 items 4–7).

#### KER-07 Recovery model and decision table
Cites: plan §10. Model first: `formal/recovery.qnt` (crash at every await, epochs, unknown
outcomes; I04, I05, I09, I10, I11, I12). Pure `recovery::classify(journal_view, provider_view,
github_view) -> Decision` covering every row of plan §10's table plus safe mode.
Accept: exhaustive table test; `mf-*` VM unknown to the journal is never `Delete`.

#### KER-08 ⛳ Data-plane domain
Cites: plan §3.1, §6 (DP FSMs), §8.4. `mayfly-data-domain`: directive executor FSM
(`Received → Executing → Done | Refused | Unknown`), idempotency table semantics (same
`directive_id` → same outcome), epoch fencing, ownership-proof check for `DeleteVm`, VM supervisor
FSM derived from observations.
Accept: exhaustive matrices; stale epoch refused; delete without matching proof refused.

### Phase 2 — journal (control plane)

#### ES-01 Journal port, in-memory journal, contract suite
Cites: plan §5.2, ver §5. `Journal` port in `mayfly-control` (append with expected revisions,
read stream, read all from sequence, receipts, outbox, inbox — one atomic `commit(UnitOfWork)`).
`MemoryJournal` in testkit. The **journal contract suite** in testkit as functions taking a
factory: expected-revision conflict without mutation, duplicate event id rule, all-or-nothing unit
of work, receipt replay returns the original result, inbox dedupe, outbox status transitions,
read-all ordering.
Accept: suite green on `MemoryJournal`; a deliberately broken journal in the suite's own tests fails it.

#### ES-02 SQLite journal
Cites: plan §5.2, D7. `mayfly-adapter-sqlite`: schema and migrations (`schema_meta`), WAL,
`synchronous=FULL`, one writer thread, the `Journal` impl. Contract suite on a fresh file, on the
same file **reopened**, and with an injected crash (connection dropped inside the transaction) at
each statement of a unit of work: nothing partial is ever visible.
Accept: contract green in the three modes; migration test from an empty DB and from the previous
schema fixture.

#### ES-03 AggregateStore, command bus, reactors
Cites: plan §5.2, §5.4. `AggregateStore::save` as the only caller of `Journal::append`
(architecture rule added to xtask), load = fold of stream (snapshots deferred), `with_version_retry`
for `WrongExpectedVersion` only, `ARCH-migration-frozen` (released migrations hash-locked),
the golden-fixture `fixtures.lock` (fnd §3.3), command bus with receipts (idempotent by `command_id`), reactor
notification after commit only.
Accept: architecture fixture fails a second `append` caller; a retried command returns its receipt.

#### ES-04 ⛳ Projections
Cites: plan §5.5. Projection specs (name, version, apply, tables), synchronous projection update
inside the unit of work, watermark catch-up on start, full rebuild, first projections:
`attempts`, `slots`, `pools`, `pending_directives`, `recent_failures`.
Accept: contract "rebuild equals live" on memory and SQLite; spec version bump rebuilds.

### Phase 3 — the two planes and the walking skeleton

#### CP-01 Commands and queries
Cites: plan §5.4. `commands` and `queries` modules of `mayfly-control`; the CQRS semantic rule of
xtask selects them (commands name no query/projection type and return no read DTO; queries take no
write port). Operator use cases: drain, resume, pause, status, attempt timeline.
Accept: architecture fixtures for both directions; component tests with `MemoryJournal`.

#### CP-02 AttemptSaga and OutboxRelay
Cites: plan §5.4, §6, §3.2. Saga reacts to lifecycle events with the next command/directive; relay
sends pending outbox rows over the link port with deadline, exponential backoff with full jitter
(randomness through an `Ids`/`Rng` port), and records `Unknown` on timeout.
Accept: component tests: lost reply → `Unknown`, not retry-with-new-name; same directive id on resend.

#### CP-03 Reconciler
Cites: plan §9, §10. Periodic and event-triggered; compares desired capacity (demand source port:
static or scale set) with projections and the last inventory; issues reserve/retire/cleanup
commands through capacity policy and the recovery table.
Accept: 1,000-step seeded simulation in testkit never violates I02/I03/I04/I07/I08.

#### CP-04 Observation inbox and startup recovery
Cites: plan §10, §3.2. Observations → `RecordObservation` commands, deduped by observation id,
stale generations ignored; startup use case (epoch, inventory, resolve `Unknown`, classify, safe
mode on an unknown event type or failed verification).
Accept: component tests for each recovery row; safe mode admits nothing and deletes nothing.

#### DP-01 Data-plane executor and observer
Cites: plan §3.1, §8.1. `mayfly-data`: ports `VmProvider`, `GuestTransport`, `HostProbe`; executor
applying the KER-08 FSM with per-directive deadlines and bounded concurrency (semaphores per
operation kind); inventory observer emitting `Inventory` on start and periodically; in-memory fake
provider in testkit with scripted faults.
Accept: component tests: duplicate directive executes once; ambiguous provider error → `Unknown`;
restart rebuilds from inventory.

#### DP-02 In-process link and link contract suite
Cites: plan §3.3. `mayfly-adapter-link::inproc` with bounded channels and serde round-trip; the
**link contract suite** (handshake, version refusal, at-least-once, ordering not assumed,
backpressure, close semantics) in testkit as factory-parameterised functions.
Accept: suite green on inproc.

#### DP-03 `mayfly-fake-tart`
Cites: ver §7, D11. A `tart`-compatible CLI (`clone`, `run`, `ip`, `list --format json`, `stop`,
`delete`, `exec`, `--version`) over a state directory, with a fault file (delay, hang, fail before
or after effect, crash) read per invocation. Independent of production crates.
Accept: its own tests; architecture rule shows no production edge.

#### DP-04 Process runner and Tart adapter
Cites: plan §8.2, D12. Process runner (argv, env allowlist, deadline, process-group kill,
bounded output, redaction), Tart commands with version probe, JSON parsing against version-pinned
fixtures. **VmProvider contract suite** run against the in-memory fake, the fake-tart binary, and
(gated by `MAYFLY_REAL_TART_HOME`, macOS only, used by HW-01) real Tart.
Accept: contract green on both fakes; hang → deadline → `Ambiguous` + re-inspect.

#### DP-05 Guest transport and runner launcher
Cites: plan §8.3, D12, D13. `tart exec` transport and SSH transport (system `ssh`, pinned host key,
no `StrictHostKeyChecking no`), readiness probe, the fixed launcher script contract (JIT over
stdin), diagnostics export bounded and redacted. fake-tart `exec` emulates a guest whose runner
"runs a job" per its fault file.
Accept: secret never appears in argv/env/log (test inspects the fake's recorded invocations).

#### DP-06 Host resource probes
Cites: plan §9. Disk free for the provider's home and available memory, `#[cfg]` per OS, via an
allowed crate (decide `rustix`/`sysinfo` in the commit; no `unsafe`).
Accept: Linux test; macOS compiles in CI.

#### RT-01 Config, secret store, state-dir lock
Cites: plan §11, §13. TOML schema v1 with validation errors that name the field; file secret
store requiring `0600`; `flock` on the state dir (second instance exits with a clear error).
Accept: config golden examples (valid/invalid); permission refusal test.

#### RT-02 Composition and `mayflyd`
Cites: plan §3, §10, §4. Wire both planes with the inproc link; supervision tree with a root
cancellation token; startup sequence of plan §10; SIGTERM → drain-safe shutdown; `--role all`.
Accept: integration test starts `mayflyd` against fake-tart and a static demand, kills it, restarts it, and the inventory converges.

#### RT-03 Operator socket API and `mayfly` CLI
Cites: plan §11. Unix socket `0600`, JSON request/response, commands of plan §11 that exist so far
(`status`, `vm list`, `events --follow`, `pool drain|resume|pause`, `recover --dry-run|--apply`,
`config validate`, `doctor` basic).
Accept: `assert_cmd` black-box tests; `--json` outputs are golden-tested.

#### RT-04 Observability
Cites: plan §12. tracing JSON subscriber in composition, event naming rule, metrics endpoint
(socket or loopback), redaction test feeding secrets through every log path.
Accept: metric names and labels golden-tested; no id-valued label.

#### E2E-01 ⛳ Walking skeleton
Cites: plan §14 phase 3, ver §2. Black-box: `mayflyd` (static demand 1) + `mayfly-fake-tart` +
fake guest runner completes one "job", VM deleted, slot released; then a demand of 3 with 2 slots
never exceeds 2; then kill -9 mid-clone and restart converges without duplicates.
Accept: these tests in the e2e level (the `e2e-tests` stage, registered here), green in CI on Linux.

### Phase 4 — GitHub

#### GH-01 `mayfly-fake-github`
Cites: plan §7, ver §7, D11. Axum-based fake of the Scale Set endpoints (scale set CRUD, session,
long-poll messages with redelivery until delete, 50-message cap, statistics, acquire, JIT) and the
REST runner endpoints (registration token, list/delete runners), plus fault knobs (401, 403, 429
with Retry-After, dropped response after effect). Wire shapes from the `actions/scaleset` Go source
and GitHub docs, cited in comments; no production crate imported.
Accept: its own tests; a scripted job queue API for e2e.

#### GH-02 GitHub App auth and HTTP client
Cites: plan §7.1, D13. RS256 JWT, installation token cache with refresh-before-expiry, reqwest +
rustls, retry policy honouring Retry-After, request metrics.
Accept: tests against fake-github; token never in Debug output.

#### GH-03 Scale Set client
Cites: plan §7.2, §7.4. `ScaleSetApi` port implementation; recorded redacted fixtures as the
contract (`tests/fixtures/github-scaleset/`), run against fake-github and the fixture replayer.
Accept: session expiry, 429, ack-after-lost-response cases.

#### GH-04 Listener use case
Cites: plan §7.2, KER-06. Poll → acquire → record (inbox + statistics + desired) → ack; empty poll
still reconciles; never blocks on VM work.
Accept: component tests F08, F09, F10, F11, F25, F27 (ver §8.4) on fakes.

#### GH-05 Classic registration adapter
Cites: plan §7.3. `RunnerRegistry` classic impl: registration token after guest readiness,
`config.sh --ephemeral` contract, runner lookup and delete by recorded id only.
Accept: tests against fake-github.

#### E2E-02 ⛳ Scale-set end to end
Cites: plan §7.2, §9. `mayflyd` + fake-tart + fake-github: queue 1 job → 1 VM → job done → deleted;
queue 100 jobs with 2 slots → never more than 2 VMs, all jobs complete, no double provision;
cancel/reassign case.
Accept: e2e tests green in CI.

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

#### VER-05 Curated mutants
Cites: ver §9. The 18 negative controls of ver §9 as source patches applied by `cargo xtask mutants-curated`,
each expected to be killed by a named test; `cargo-mutants` nightly lane with a baseline document.
Accept: every curated mutant killed; survivors listed in `doc/mutation-baseline.md`.

#### VER-06 Fuzz
Cites: ver §10. cargo-fuzz targets (outside the workspace, pinned nightly): protocol decode, Tart
JSON parse, config parse, GitHub message decode, OwnedVmName parse. Nightly lane with a time budget.
Accept: targets build; corpus seeded from golden fixtures.

#### VER-07 Apalache verification lane
Cites: ver §6. `quint verify` with Apalache for each model's invariants at bounded depth in
`formal.yml` (scheduled + on model changes); depths and timings in `doc/formal-models.md`.
Accept: every invariant listed in ver §6.1 is checked; a seeded bad invariant fails.

#### SEC-01 Threat model and secret audit
Cites: plan §13. `doc/security.md` (assets, actors, boundaries, mitigations with tests),
audit of every secret path, fixes as needed.
Accept: every mitigation names its test.

### Phase 7 — operate

#### OPS-01 launchd service and doctor
Cites: plan §11, §13. LaunchAgent/LaunchDaemon plist templates, `mayfly service install|uninstall`,
`doctor` checks (provider version, `TART_HOME`, disk, memory, socket permissions, keychain
availability on macOS 15+, clock).
Accept: plist golden; doctor JSON golden on Linux with fakes; (owner) install on the Mac.

#### OPS-02 Image manifest
Cites: plan §8, OPS. Manifest schema (`schema`, provider, OS, arch, digest-pinned source, runner
version, capabilities, host requirements), `mayfly image verify`, refusal of tag-only references.
Accept: valid/invalid fixtures.

#### OPS-03 ⛳ Release workflow
Cites: fnd §7. Tag-triggered release: macOS arm64 (and Linux) archives, `SHA256SUMS`, build
provenance attestation, CycloneDX SBOM; signing/notarisation documented as (owner) follow-up.
Accept: dry-run on a branch produces artefacts; (owner) first tag.

#### OPS-04 Runbooks
Cites: plan §10. `doc/runbook.md`: stale runner, disk full, GitHub outage, lost guest, orphan
review, keychain locked, safe mode, upgrade/rollback.

#### HW-01 (owner) Real Tart lane
`hardware.yml` (manual dispatch, self-hosted label on the owner's Mac): VmProvider contract on real
Tart in a dedicated `TART_HOME`, one-job run with a tiny Linux image. Agents write the workflow and
docs; (owner) registers the self-hosted runner and runs it.

#### HW-02 (owner) Live GitHub scale set
Disposable repository and GitHub App with documented permissions; gated live test of GH-03/GH-04;
(owner) creates the App and secrets.

#### HW-03 (owner) Canary and cutover
New scale set and `runs-on` label beside Tartelet; compare queue time, failures, cleanup, disk;
(owner) moves workflows and retires Tartelet.
