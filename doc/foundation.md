# mayfly: engineering foundation

**Status:** plan, 2026-11-09. Owner: Patryk Żmigrodzki.

This document owns the engineering rules: how the workspace is cut, which edges are legal, how
the rules are enforced, the language and lint policy, logging and error conventions, the
event-sourcing discipline, CI and supply chain, tool pins, the command surface, commits, and what
an implementing agent may do. [`plan.md`](plan.md) owns *what* is built,
[`verification.md`](verification.md) owns *how it is shown to be right*, and
[`tasks.md`](tasks.md) owns order. A citation "fnd §3" points here.

A rule in this document is either **enforced** (a check fails; the check is named) or
**reviewed** (a `/review` checklist item). Every enforced rule names the task that introduces its
check. Until that task lands, the rule binds by review.

---

## 1. Purpose and scope

1. **Purpose.** Make the properties of plan §1.1 (G1–G8) structural: wrong code should fail to
   compile, fail the architecture checker, or fail a named test, before a reviewer has to notice.
2. **Scope.** Every crate under `crates/` and `tools/`, the Quint models under `formal/`, the
   workflows under `.github/`, and the repository's process files. The fuzz workspace (`fuzz/`)
   is covered by §2.5 and §7.
3. **Out of scope.** Product behaviour (plan), the test strategy and fault model (verification),
   and the owner's operational procedures (`doc/runbook.md`, OPS-04).
4. **Precedence.** Accepted ADRs override this document; this document overrides earlier plan
   text on engineering matters. A conflict found while working is reported, not resolved by
   choosing the convenient side.
5. **Heritage.** The gate, the architecture checker, the ADR and land tooling are ported from
   site-preview (D3), adapted to mayfly's crate names and its two planes. The event-sourcing
   discipline and the fault-lab method are mayfly's own, written for a single-host SQLite journal.

---

## 2. Workspace and dependency law

### 2.1 Crate categories

`architecture.toml` sorts every workspace member into exactly one category. A crate missing
from the policy, or listed but absent, fails the checker.

| Category | Crates | May be reached from production |
|:--|:--|:--|
| **pure** (production) | `mayfly-domain`, `mayfly-protocol`, `mayfly-control-domain`, `mayfly-data-domain` | yes |
| **application** (production) | `mayfly-control`, `mayfly-data` | yes |
| **adapter** (production) | CP: `mayfly-adapter-sqlite`, `mayfly-adapter-github`, `mayfly-adapter-secrets`; DP: `mayfly-adapter-tart`, `mayfly-adapter-guest`, `mayfly-adapter-host`; plane-neutral: `mayfly-adapter-link` | yes |
| **root** (production) | `mayfly-composition`, `mayfly-cli` | yes |
| **test_only** | `mayfly-testkit`, `mayfly-conformance`, `mayfly-fake-tart`, `mayfly-fake-github` | never |
| **tool_only** | `xtask`, `faultlab` | never |
| **fuzz_only** | `mayfly-fuzz` (own workspace under `fuzz/`) | never |

Plane membership is a second, orthogonal attribute:

| Plane | Crates |
|:--|:--|
| **CP** (control plane) | `mayfly-control-domain`, `mayfly-control`, `mayfly-adapter-sqlite`, `mayfly-adapter-github`, `mayfly-adapter-secrets` |
| **DP** (data plane) | `mayfly-data-domain`, `mayfly-data`, `mayfly-adapter-tart`, `mayfly-adapter-guest`, `mayfly-adapter-host` |
| **shared** | `mayfly-domain`, `mayfly-protocol`, `mayfly-adapter-link` |
| **both** (sees CP and DP) | `mayfly-composition`, `mayfly-cli` (through composition), `mayfly-testkit`, `mayfly-conformance`, `mayfly-fuzz` |

### 2.2 The edge table (normal dependencies)

The arrows point from dependent to dependency and are the *only* legal normal edges between
workspace crates. They are plan §4's diagram written as a table.

| Crate | May depend on |
|:--|:--|
| `mayfly-domain` | — |
| `mayfly-protocol` | `mayfly-domain` |
| `mayfly-control-domain` | `mayfly-domain` |
| `mayfly-data-domain` | `mayfly-domain`, `mayfly-protocol` |
| `mayfly-control` | `mayfly-domain`, `mayfly-protocol`, `mayfly-control-domain` |
| `mayfly-data` | `mayfly-domain`, `mayfly-protocol`, `mayfly-data-domain` |
| `mayfly-adapter-sqlite` | `mayfly-domain`, `mayfly-control-domain`, `mayfly-control` |
| `mayfly-adapter-github` | `mayfly-domain`, `mayfly-control` |
| `mayfly-adapter-secrets` | `mayfly-domain`, `mayfly-control` |
| `mayfly-adapter-tart` | `mayfly-domain`, `mayfly-data` |
| `mayfly-adapter-guest` | `mayfly-domain`, `mayfly-data` |
| `mayfly-adapter-host` | `mayfly-domain`, `mayfly-data` |
| `mayfly-adapter-link` | `mayfly-domain`, `mayfly-protocol` |
| `mayfly-composition` | every production crate except `mayfly-cli` |
| `mayfly-cli` | `mayfly-composition` |
| `mayfly-testkit` | pure and application crates (no adapter) |
| `mayfly-conformance` | `mayfly-testkit`, pure and application crates, `mayfly-adapter-sqlite`, `mayfly-adapter-link`, `mayfly-composition` |
| `mayfly-fake-tart`, `mayfly-fake-github`, `xtask`, `faultlab` | — (no workspace crate) |
| `mayfly-fuzz` | `mayfly-domain`, `mayfly-protocol`, `mayfly-adapter-tart`, `mayfly-adapter-github`, `mayfly-composition` |

Rules over the table:

1. **Acyclic, explicit.** The table is acyclic; workspace `members` is an explicit list, never a
   glob. A new crate adds its `members` entry and its policy entry in the same commit.
2. **CP/DP separation.** No CP crate reaches a DP crate, and no DP crate reaches a CP crate, over
   any normal or build edge, directly or transitively. Only the *both* row of §2.1 sees both
   planes. The adapters of one plane never name a sibling adapter, composition or the CLI.
3. **Dev edges.** A dev-dependency may target a test_only crate or a crate already in the
   dependent's normal list. A dev edge never breaks the plane rule: a DP crate's tests do not
   use the SQLite adapter.
4. **Production closure.** No production crate reaches a test_only, tool_only or fuzz_only crate
   by any kind of edge in its normal or build closure.
5. **Build dependencies** are declared in `[dependencies.build]`; none exist in v0.x.
6. **Targets.** Target-specific edges are evaluated for `aarch64-apple-darwin`,
   `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu` (D2).

### 2.3 Pure-crate restrictions

`mayfly-domain`, `mayfly-protocol`, `mayfly-control-domain` and `mayfly-data-domain` name none
of the following, in any `cfg` branch, through any alias or re-export:

- `std::{fs, net, process, env, thread, os}`, `std::io::{stdin, stdout, stderr}`;
- `SystemTime::now`, `Instant::now`, `elapsed`, `RandomState`, `print!`/`eprint!` and their `ln`
  forms, `env!`, `option_env!`;
- `tokio`, `futures` executors, `reqwest`, `hyper`, `axum`, `rusqlite`, `rustix`, `rand`,
  `getrandom`, `uuid` generators, `tracing`, `log`, `metrics`;
- any application, adapter, root or non-production crate.

Time, ids and randomness arrive as values on commands and directives (plan §5.1). The pure
crates may hold `async` trait *declarations* (link ports in `mayfly-protocol`) but never an
executor. `serde` derive is permitted in `mayfly-protocol` and `mayfly-control-domain` (wire and
journal shapes); whether `mayfly-domain` may derive it is decided by the D9 ADR in KER-01.

**Application crates** (`mayfly-control`, `mayfly-data`) may use `tokio` with the `sync`
feature, `tokio-util` (cancellation, task tracking), `tracing` and `thiserror`. They reach time
only through the `Clock` port and processes, files, sockets and SQL only through ports. They name
no adapter type.

### 2.4 External allowlist

`[external.normal]` and `[external.dev]` list, per crate, every third-party crate it may name
directly and the features it may enable. A crate absent from its list fails
`ARCH-unlisted-external`; an extra feature fails `ARCH-external-feature`; a non-crates.io source
fails `ARCH-external-source` unless an ADR-backed exception exists. The table below is the
**ceiling** for v0.x; a crate enters the policy only in the commit of the task that first uses
it, added with `cargo add` and named with its reason in the commit message.

| Crate | Ceiling (normal) |
|:--|:--|
| `mayfly-domain` | `thiserror`; `serde` (derive) only if the D9 ADR allows |
| `mayfly-protocol` | `serde` (derive), `thiserror` |
| `mayfly-control-domain` | `serde` (derive), `thiserror` |
| `mayfly-data-domain` | `thiserror` |
| `mayfly-control`, `mayfly-data` | `tokio` (`sync`), `tokio-util`, `tracing`, `thiserror`, `serde_json` (control only: outbox payloads) |
| `mayfly-adapter-sqlite` | `rusqlite` (`bundled`), `serde_json`, `tokio` (`sync`, `rt`), `tracing`, `thiserror` |
| `mayfly-adapter-github` | `reqwest` (`rustls-tls`, `json`), a JWT crate (RS256), `serde`, `serde_json`, `tokio`, `url`, `tracing`, `thiserror` |
| `mayfly-adapter-secrets` | `rustix` (`fs`); macOS-only Keychain binding chosen by its task with an ADR |
| `mayfly-adapter-tart`, `mayfly-adapter-guest` | `tokio` (`process`, `io-util`, `time`), `rustix` (`process`), `serde`, `serde_json`, `tracing`, `thiserror` |
| `mayfly-adapter-host` | `rustix` or `sysinfo` (DP-06 decides), `thiserror` |
| `mayfly-adapter-link` | `tokio` (`net`, `io-util`, `sync`), `serde_json`, `tracing`, `thiserror` |
| `mayfly-composition` | `tokio` (`rt-multi-thread`, `signal`, `net`, `time`, `process`), `tokio-util`, `tracing`, `tracing-subscriber` (`json`, `env-filter`), `toml`, `serde`, `thiserror` |
| `mayfly-cli` | `clap` (`derive`), `serde_json`, `anyhow` |
| `mayfly-fake-tart` | `serde`, `serde_json`, `anyhow` |
| `mayfly-fake-github` | `axum`, `tokio`, `serde`, `serde_json`, `anyhow` |
| `xtask` | `cargo_metadata`, `clap`, `syn`, `proc-macro2`, `serde`, `toml`, `tempfile`, a YAML parser, `anyhow` |
| `faultlab` | `serde`, `serde_json`, `rusqlite`, `rustix` (`process`), `clap`, `toml`, `tempfile`, `anyhow` |

Dev ceilings: `proptest` (pure and application crates), `tempfile`, `tokio` (`rt`, `macros`,
`test-util`), `assert_cmd` (`mayfly-cli`, `mayfly-composition`).

### 2.5 Test-only, tool-only and fuzz-only crates

1. `mayfly-testkit` holds fakes, contract suites, the golden and determinism harnesses and the
   poll helper (verification §3, §5, §7). It depends on no adapter, so an adapter's own tests can
   run the suites against it without a cycle.
2. `mayfly-conformance` holds ITF decoding, replay and per-model drivers (verification §6).
3. `fuzz/` is its own workspace with its own `Cargo.lock` and `deny.toml`; its crate is
   `fuzz_only` in the policy and builds on the pinned nightly only.
4. `xtask` is the single source of truth for every repeatable command (§9).

### 2.6 Independent oracles

`faultlab`, `mayfly-fake-tart` and `mayfly-fake-github` exist to disagree with production when
production is wrong, so they share **no code** with it (D11):

1. They depend on no workspace crate, normal or dev, and on no crate that does.
2. `faultlab` reads the SQLite journal with its own SQL, reads fake-tart state and fake-github
   dumps with its own parsers, and drives the product only through the `mayflyd` and `mayfly`
   binaries.
3. `mayfly-fake-tart` implements the `tart` CLI surface from Tart's documented behaviour and
   pinned real output, never from `mayfly-adapter-tart`'s types; `mayfly-fake-github` implements
   the wire protocol from public documentation and the open-source scale-set client, never from
   `mayfly-adapter-github`'s types.
4. A shared type would make a production bug and its oracle agree by construction; duplication
   here is the point.

---

## 3. Enforcement

Three layers: the compiler (crate boundaries, visibility, `trybuild` compile-fail tests for
critical APIs such as `OwnedVmName` and `Secret<T>`), the **policy** (`architecture.toml` over
the resolved Cargo graph) and **semantic rules** (`syn` over every source file a target reaches).
A tool that is absent, a selector that matches nothing, or a file that fails to parse is a
failure, never a pass.

### 3.1 `architecture.toml` schema

```toml
schema = 1

[workspace]
production = [ ... ]          # §2.1 pure, application, adapter, root
test_only  = [ ... ]
tool_only  = ["xtask", "faultlab"]
fuzz_only  = ["mayfly-fuzz"]

[planes]
control = [ ... ]             # §2.1 CP
data    = [ ... ]             # §2.1 DP
shared  = [ ... ]
both    = [ ... ]

[purity]
pure = ["mayfly-domain", "mayfly-protocol", "mayfly-control-domain", "mayfly-data-domain"]
process_authority = ["mayfly-adapter-tart", "mayfly-adapter-guest", "mayfly-composition"]
independent = ["faultlab", "mayfly-fake-tart", "mayfly-fake-github"]

[dependencies.normal]         # §2.2, one key per crate
[dependencies.build]

[external.normal]             # §2.4, crate = { dep = ["feature", ...] }
[external.dev]
[external.build]

[exceptions]
# [exceptions.sources] "<source>" = { adr = "NNNN" }
# [[exceptions.lints]] path = "...", lint = "...", adr = "NNNN"
```

Parsed strictly: an unknown key, a crate in two categories, or another `schema` value fails
`ARCH-policy-schema`. An exception citing an ADR file that does not exist, or an exception that
no longer matches anything, fails.

### 3.2 The checker (`cargo xtask architecture`)

Ported from site-preview (D3) with its module layout: policy parse, source inventory vs
`members` vs policy, the Cargo graph from `cargo metadata --locked` with edges by kind and full
dependency paths in each diagnostic, the external allowlist, and the semantic visitor (name
resolution through `use`, globs, re-exports and manifest renames; macro token streams scanned
conservatively; every `cfg` branch). Diagnostics are `path:line: ARCH-<id>: message`. Rules are
declared in one `rules!` table (id, owning task, description, fixtures) that also feeds
`list-checks`.

### 3.3 Rules

Ported rules keep their site-preview ids; mayfly's new rules are marked **new**.

| Rule | What fails | Introduced by |
|:--|:--|:--|
| `ARCH-policy-schema`, `ARCH-metadata`, `ARCH-missing-member`, `ARCH-not-a-member`, `ARCH-unknown-crate`, `ARCH-missing-crate`, `ARCH-duplicate-category` | policy or inventory drift | FND-02 |
| `ARCH-forbidden-edge`, `ARCH-forbidden-dev-edge`, `ARCH-undeclared-build-dep`, `ARCH-production-closure`, `ARCH-policy-cycle` | an edge outside §2.2 | FND-02 |
| `ARCH-unlisted-external`, `ARCH-external-feature`, `ARCH-external-source` | §2.4 | FND-02 |
| `ARCH-lint-config` | a member without `[lints] workspace = true`, a workspace lint set to allow, lint caps in rustflags | FND-02 |
| **`ARCH-plane-separation`** (new) | a CP crate reaching a DP crate or the reverse, by any path | FND-02 |
| **`ARCH-plane-vocabulary`** (new) | a DP crate naming `Journal`, `AggregateStore`, a projection or `rusqlite`; a CP crate naming `std::process`, `tokio::process` or a VM provider type | FND-02 |
| **`ARCH-process-authority`** (new) | `std::process`, `tokio::process` or `Command` in a production crate outside `[purity].process_authority`; test and tool crates are exempt | FND-02 |
| **`ARCH-independent-oracle`** (new) | `faultlab`, `mayfly-fake-tart` or `mayfly-fake-github` reaching any workspace crate | FND-02 |
| `ARCH-domain-purity` | §2.3 in a pure crate | FND-02 |
| `ARCH-layer-isolation` | an application crate naming an adapter, composition or a transport crate | FND-02 |
| `ARCH-unsafe-code`, `ARCH-unfinished-code`, `ARCH-platform-branch` | `unsafe`; `todo!`/`unimplemented!`/`dbg!`; `cfg!(target_os…)` runtime branches | FND-02 |
| **`ARCH-anyhow-in-library`** (new) | `anyhow` named in a library target | FND-02 |
| **`ARCH-unbounded-channel`** (new) | `unbounded_channel` or an unbounded queue in a production crate | FND-02 |
| **`ARCH-detached-spawn`** (new) | `tokio::spawn`/`spawn_blocking` outside the supervision module of composition, the SQLite writer, and the link and process adapters' listed modules (tasks are spawned on a tracker handed down) | FND-02 |
| **`ARCH-secret-serialize`** (new) | a type that is or contains `Secret<_>` or `SecretHandle` payload deriving or implementing `Serialize`, or implementing `Display`/`Debug` other than the redacting impl | FND-02 (exercised from KER-02, RT-01) |
| **`ARCH-single-append`** (new) | a call of `Journal::append` or `Journal::commit` anywhere but `AggregateStore::save`, the `Journal` impls and the testkit contract suite | ES-03 |
| **`ARCH-journal-sql`** (new) | SQL naming `event_journal`, `event_stream`, `command_receipt`, `outbox` or `inbox` outside `mayfly-adapter-sqlite`'s store and migration modules; any `UPDATE`/`DELETE` of `event_journal` anywhere | ES-02 |
| **`ARCH-migration-frozen`** (new) | a released migration whose content hash differs from `migrations.lock` | ES-02 |
| **`ARCH-exhaustive-transition`** (new) | a wildcard (`_`) arm in a `decide`, `evolve` or FSM `step` match over state or stimulus | KER-04 |
| **`ARCH-command-isolation`**, **`ARCH-query-purity`** (CQRS) | a command naming a query, projection or read DTO, or returning one; a query naming a write port, the `AggregateStore` or an aggregate | CP-01 |
| **`ARCH-log-event-name`** (new) | a `tracing` event whose message is not a literal `snake_case` name | RT-04 |
| `ARCH-observability-isolation` | a `tracing` value reaching a branch condition or return in application code | RT-04 |
| `ARCH-lint-suppression` | `#[allow]`, `#[expect]` or `coverage(off)` without a matching `[[exceptions.lints]]` row citing an ADR; a stale row | FND-02 |
| `ARCH-ignored-test` | `#[ignore]` | FND-02 |
| `ARCH-test-name`, `ARCH-test-structure` | a test not named `should_<outcome>_when_<condition>`; missing `// given`, `// when`, `// then` in order | FND-02 |
| `ARCH-test-sleep` | a sleep in any test outside `mayfly-testkit::poll` | FND-05 |
| **`ARCH-rule-fixture`** (new) | a rule without a negative fixture, or a fixture without a rule | FND-02 |

### 3.4 Fixtures

1. Every rule has at least one **negative fixture** under `tools/xtask/tests/architecture-invalid/
   <name>/`: a miniature workspace (manifest, committed lockfile, policy, sources, third-party
   crates stubbed as local path sources so it resolves offline) and an `expected.txt` with
   `rule:`, `at:` and optional `contains:`/`only:` lines.
2. Positive fixtures under `architecture-valid/` keep the rules from over-reaching (for example,
   an adapter's integration test spawning a process is legal).
3. FND-02 ships at least: `control-depends-on-data`, `data-names-journal`, `domain-std-fs`,
   `faultlab-imports-control-domain`, `test-without-should`; each later rule ships its own
   fixture in the commit that adds it.
4. `check` runs the fixtures; `ARCH-rule-fixture` makes the catalog complete.

### 3.5 Hygiene

- Workspace lints only (§4.2); no crate-level overrides.
- No `#[allow]`/`#[expect]` without an ADR-backed exception row; no `#[ignore]`.
- Tests named `should_<outcome>_when_<condition>` with `// given`, `// when`, `// then`.
- No mocking frameworks; real objects over testkit fakes; tempdirs and port 0.
- A rule is never loosened to get green. A change to `architecture.toml` that widens an edge,
  allowlist or exception needs the task to say so and an ADR.

---

## 4. Language and lint policy

### 4.1 Baseline

Stable toolchain pinned exactly in `rust-toolchain.toml` (D5: 1.97.0, edition 2024, resolver 3,
`rustfmt`, `clippy`, `llvm-tools-preview`). `Cargo.lock` committed; every cargo command in the
gate runs `--locked`. `publish = false` everywhere. A dated nightly is pinned in
`tools/versions.toml` for `cargo-fuzz` only and never builds a shipped artefact.

### 4.2 Workspace lints

```toml
[workspace.lints.rust]
unsafe_code = "forbid"
unused_must_use = "deny"
missing_docs = "warn"
unreachable_pub = "warn"

[workspace.lints.clippy]
all = { level = "deny", priority = -1 }
dbg_macro = "deny"
todo = "deny"
unimplemented = "deny"
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
indexing_slicing = "deny"
await_holding_lock = "deny"
large_futures = "deny"
```

`clippy.toml` allows `unwrap`/`expect`/`panic` inside `#[cfg(test)]` and test targets only.
Zero warnings: `-D warnings` in the gate. Each crate's `lib.rs` opens with a `//!` stating its
single responsibility and its plane.

### 4.3 Errors

1. **Libraries** use typed errors with `thiserror`; one error enum per port or use case, each
   variant meaningful to a caller. `anyhow` is allowed only in binary targets (`mayfly-cli`,
   the fakes, `xtask`, `faultlab`) (`ARCH-anyhow-in-library`).
2. **Definite vs ambiguous** is part of the type wherever an effect crosses a process or network
   boundary (`VmError::Definite | Ambiguous`, `DirectiveRefused` vs `DirectiveOutcomeUnknown`,
   plan §3.2, §8.1). A timeout, a lost process or a dropped connection is ambiguous, never a
   definite failure and never a success.
3. **Domain refusals** are values (`Rejection`), deterministic and never retried.
   `WrongExpectedVersion` is the only error a retry loop retries (ES-03); a busy journal is
   reported, not retried blindly.
4. **No panics on production paths.** No `unwrap`, `expect`, `panic!`, unchecked indexing or
   `unreachable!` that input can reach. An invariant violation discovered at run time puts the
   control plane in safe mode (plan §10) rather than aborting with VMs in flight.
5. Errors crossing a log or the operator API are classified (`error_class`) and redacted; raw
   upstream bodies are bounded and scrubbed.

### 4.4 Async rules

1. **Ports are native `async fn`/RPITIT traits** used through generics (D8). `async_trait` and
   `dyn` ports appear only where object safety is proven necessary, recorded in an ADR.
   Returned futures are `Send`.
2. **Cancellation safety.** Every `async fn` on a port documents whether it is cancel-safe. A
   future used in `select!` must be cancel-safe; an effect that is not (a `tart clone` in flight)
   runs in a tracked task that outlives the selecting caller and reports its outcome as an
   observation. Dropping a future never leaves a half-recorded intent: intent is durable before
   the effect starts (plan §6 rule 1).
3. **No blocking in async.** No `std::fs`, `std::thread::sleep`, blocking process waits or SQLite
   calls on a runtime worker. SQLite lives on one dedicated writer thread behind a channel (D7);
   other blocking work goes through `spawn_blocking` in the adapter that owns it.
4. **Structured concurrency.** Composition owns one root `CancellationToken` and a task tracker;
   every long-lived task is a child with its own child token, joined on shutdown in a declared
   order (listener → reconciler → relay → executor → link → journal). No detached task
   (`ARCH-detached-spawn`). Shutdown has a deadline; what remains after it is reported, not
   abandoned silently.
5. **Bounded everything.** Channels are bounded and the capacity is a named constant with a
   reason; a full channel applies backpressure or is a counted, logged refusal, never an
   unbounded buffer (`ARCH-unbounded-channel`). Concurrency per operation kind is limited by
   semaphores (clones, starts, deletes, GitHub calls). Captured process output, upstream bodies
   and retry budgets are bounded.
6. **Deadlines.** Every directive and every external call carries a deadline derived from the
   `Clock` port; timeouts never become success.
7. **Locks.** No lock is held across `.await` (`await_holding_lock`); prefer message passing to
   an owning task over shared mutable state.

---

## 5. Logging, metrics and errors

1. **Events, not sentences.** Every `tracing` event's message is a fixed `snake_case`,
   noun-first name (`directive_refused`, `github_poll_failed`); everything that varies is a field
   (`ARCH-log-event-name`). Spans carry `attempt_id`, `pool`, `generation`, `directive_id`,
   `session_id`, bound once at the start of the unit of work.
2. **Levels.** A healthy daemon logs (almost) nothing.
   - `DEBUG`: diagnosis; off by default.
   - `INFO`: an important change only: start, stop, migration applied, config loaded or changed,
     safe mode entered or left, projection rebuilt. **Zero INFO on the happy path** of a job.
   - `WARN`: a condition the system handles that deserves a look in volume (retry, 429,
     ambiguous outcome resolved, stale observation ignored).
   - `ERROR`: the operator must act (quarantine, safe mode, cleanup refused, invariant breach).
3. **Redaction.** Secrets are `Secret<T>` with redacting `Debug`/`Display` and no `Serialize`
   (D13, `ARCH-secret-serialize`). The JSON subscriber also scrubs known secret shapes from every
   string field as a second line. No token, JIT config, private key or registration token
   appears in logs, metrics, status, events, argv or child environments (RT-04, SEC-01 tests).
4. **Metrics.** Names and labels are fixed by plan §12 and golden-tested (RT-04). Label values
   come only from code (enums, pool names from config, status classes), never from ids, error
   messages or upstream data. A new label needs a bounded value set stated beside it.
5. **Counters after commit.** Business counters (`mayfly_directives_total`, attempt state
   transitions) are bumped by a telemetry port called after `AggregateStore::save` commits, never
   inside `evolve` or a projection, so replay and projection rebuild never re-count.
6. **Tests do not assert log lines.** Logs are instrumentation; the checker pins their shape and
   the redaction tests pin their absence of secrets.

---

## 6. Event-sourcing rules

Event sourcing and CQRS live in the control plane only (D6). The data plane holds no durable
state.

1. **Events are immutable facts.** Past-tense names, `#[non_exhaustive]`, each declaring
   `EVENT_TYPE` and `SCHEMA_VERSION`, registered in one registry, with a golden JSON fixture per
   type and version (verification §4). An event carries ids, generations, digests, codes and
   instants; never a secret, never a JIT config, never free text from upstream.
2. **Purity.** `decide(&State, Command) -> Result<Vec<Event>, Rejection>` and
   `evolve(State, &Event) -> State` read no clock, mint no id, draw no randomness, log nothing.
   `decide` is deterministic, refusals included; every aggregate module contributes accepted and
   refused `DETERMINISM_CASES` (verification §3). Matches over state and stimulus are exhaustive
   (`ARCH-exhaustive-transition`).
3. **One write path.** `AggregateStore::save` is the only caller of the journal's write methods
   (`ARCH-single-append`): one SQLite transaction holds the expected-revision check, the events,
   synchronous projections, outbox rows, the command receipt and the inbox entry; reactors run
   after commit and are never replayed (plan §5.2).
4. **Idempotency.** Commands carry a `command_id`; a repeat returns the receipt's result.
   Observations and GitHub messages are deduplicated by the inbox. Directives are idempotent by
   `directive_id` and target name.
5. **Compatibility contract.** Ask of every change: *is a build that ignores this still
   correct?*
   - **Journal: backward-transitive for ever.** Every build replays every event any earlier build
     wrote. Old versions are read through pure upcasters, applied on load, never on write; a
     version bump without its upcaster and legacy fixture fails the golden harness.
   - **Adjacent releases N ↔ N+1** are compatible both ways: N+1 reads N's journal; N run over a
     journal N+1 wrote either understands every event or enters safe mode. A change N must not
     ignore ships in two releases (prepare: read it; activate: write it).
   - **Events** take optional fields or new types only; never a rename, a removal or a retyped
     field. A change an older build must not skip is a **new event type**, not a version bump.
   - **Protocol** messages follow the same additive rule; a peer refuses an unsupported major
     version (plan §3.2).
   - **Config** keys are never renamed; `schema_version` gates breaking shapes.
6. **Migrations** are forward-only raw SQL, numbered, applied in one transaction each, and
   **never edited once released** (`ARCH-migration-frozen`). No downgrade exists; rollback is
   N+1 → N under rule 5.
7. **Projections.** Each is a versioned spec; a compatible change bumps `spec_version` and the
   next start rebuilds it; a **change of meaning is a new spec** beside the old one. Rebuild from
   sequence 0 equals the live projection (contract, I16). Projections are disposable; the
   journal is not.
8. **Unknown event → safe mode.** An unregistered type or a version newer than this build
   understands means the daemon cannot prove it understands ownership: it starts in safe mode
   (no new VMs, no destructive action) and says why (plan §10). Decoding never guesses.
9. **Nothing rewrites history.** No `UPDATE` or `DELETE` of journal rows (`ARCH-journal-sql`);
   SQLite triggers refuse them as a second line.

---

## 7. CI and supply chain

### 7.1 Workflows

| Workflow | Trigger | Content | Task |
|:--|:--|:--|:--|
| `ci.yml` | push to `main`, pull request, dispatch | the gate, split into jobs (7.2) | FND-04 |
| `formal.yml` | nightly, on changes under `formal/`, dispatch | fresh-seed conformance, Apalache verification | FND-06, VER-07 |
| `faultlab.yml` | weekly, dispatch | fault-lab core scenarios, then extended | VER-04 |
| `mutation.yml` | nightly (curated + in-diff), weekly (full, sharded), dispatch | `mutants-curated`, `cargo-mutants` | VER-05 |
| `fuzz.yml` | nightly, dispatch | each fuzz target for a time budget; crash inputs uploaded | VER-06 |
| `security.yml` | weekly, dispatch | advisories, licences, secret scan over full history, workflow lint | FND-04 |
| `codeql.yml` | push to `main`, pull request, weekly | CodeQL for Rust and Actions | FND-04 |
| `scorecard.yml` | push to `main`, weekly | OpenSSF Scorecard, SARIF upload | FND-04 |
| `release.yml` | tag `v*`, dispatch | build, SBOM, checksums, provenance, draft release | OPS-03 |
| `hardware.yml` | manual dispatch only, self-hosted Mac | real Tart contract and one-job smoke; live GitHub lane | HW-01, HW-02 |
| `.github/dependabot.yml` | weekly | cargo, github-actions, npm (`formal/`), grouped minor+patch | FND-04 |

### 7.2 The gate in CI

1. **Each job is `cargo xtask check --only <stages>`.** CI never re-implements a stage in YAML;
   local and CI run the same code. Initial jobs: `architecture`; `static` (`doctor`, then
   fmt-check, clippy, docs checks); `tests` (unit, integration, doctests); `e2e`; `conformance`;
   `coverage` (coverage, changed-lines); `security` (deny, secrets, workflows, action-pins);
   `macos` (native build and tests on arm64, `test --only platform-tests`); `cross` (`cargo check
   --target aarch64-apple-darwin` from Linux).
2. **One aggregate job, `required`**, runs `if: always()`, needs every other job of `ci.yml`, and
   fails unless every result is `success` (skipped and cancelled fail).
3. **Self-tests** in xtask parse `ci.yml` and fail when an `in_check` stage is in zero jobs or
   in two, when `required` does not need every job, or when a gate step has `continue-on-error`
   or a condition. `doc/required-checks.md` lists the jobs; a test keeps it in step.
4. Scheduled and owner-gated workflows are outside `required`; their failures open an issue and
   are fixed, not muted.
5. A red job is fixed, never re-run to green. One re-run is allowed only when the runner died
   before a test body ran, and the reason is written down.

### 7.3 Workflow hygiene

- `permissions: contents: read` at the top; a job adds only the scope one step needs, with the
  reason beside it.
- Every `uses:` is pinned to a 40-hex commit SHA with the version in a comment, or is a local
  action; container references are pinned by digest (`action-pins` stage).
- `actions/checkout` with `persist-credentials: false`; no `pull_request_target`; inputs reach
  scripts through `env`, never interpolated into `run:`.
- Concurrency per ref; cancel-in-progress only for pull requests.
- Caches are keyed by toolchain, lockfile and `tools/versions.toml`, and saved only from `main`.

### 7.4 Supply chain

- `cargo-deny`: advisories (yanked denied), licences (permissive allowlist; each exception per
  crate with an ADR), bans (no wildcards), sources (crates.io only).
- `gitleaks` over the full history; `.gitleaks.toml` allowlists only synthetic fixtures.
- CodeQL, Scorecard, Dependabot as in 7.1.
- Releases (OPS-03): `SHA256SUMS`, build-provenance attestation, CycloneDX SBOM, third-party
  notices; signing and notarisation are an owner follow-up.
- **Owner-gated hardware lane** (D14): `hardware.yml` runs only on manual dispatch on the
  owner's self-hosted Mac with a dedicated `TART_HOME`, never on pull requests and never from a
  fork. Agents write it; the owner registers the runner and dispatches it.

---

## 8. Tooling pins

1. `tools/versions.toml` pins every tool a stage needs, each with an install `method`
   (`cargo`, `rustup`, `manual`, `lockfile`), a version and a `probe` whose output must contain
   that version. Initial entries: `cargo-nextest`, `cargo-deny`, `cargo-llvm-cov`,
   `cargo-mutants`, `cargo-fuzz` and its nightly, `cargo-cyclonedx`, `gitleaks`, `actionlint`
   with `shellcheck`, Node (major), Quint (owned by `formal/package-lock.json`), Apalache (version
   and SHA-256), Java (major, for Apalache).
2. `cargo xtask doctor [--json]` reports each tool as found, missing or wrong version, with the
   install hint; it exits non-zero if any tool a requested lane needs is not right.
   `cargo xtask install-tools [tool…]` installs what can be installed.
3. **Fail closed.** Every stage declares the tools it needs; a missing or wrong tool makes that
   stage fail with the hint. A stage is never reported as skipped-but-green (D15). When local and
   CI disagree, run `doctor` first.
4. Quint always runs from `formal/node_modules` through the TypeScript backend and never
   downloads an evaluator at run time; Apalache is fetched by xtask and verified against its
   SHA-256 before use.
5. A version bump is its own commit, through Dependabot or by hand, with the gate green.

---

## 9. Commands

Every repeatable job is an xtask subcommand; an ad-hoc command that becomes repeatable becomes a
subcommand. `quick` is not `check`; never report one as the other. A command whose task has not
landed exits 3 with "not available yet: introduced by <task>".

| Command | What it does | Task |
|:--|:--|:--|
| `cargo xtask quick` | fmt-check, architecture, clippy, unit tests: the inner loop | FND-02 |
| `cargo xtask check` | the full blocking gate; runs every `in_check` stage, prints a stage table, never downgrades a failure | FND-02 |
| `cargo xtask check --only <a,b>` | named stages in gate order through the same code; unknown name exits 2 listing valid ones | FND-02 |
| `cargo xtask architecture` | policy, semantic rules, negative and positive fixtures, catalog completeness | FND-02 |
| `cargo xtask list-checks` | the catalog as JSON: stage, level, command, owning task, `in_check`, plus one row per ARCH rule | FND-02 |
| `cargo xtask doctor [--json]` | every pinned tool: found, missing, wrong | FND-02 |
| `cargo xtask install-tools [tool…]` | install pinned tools | FND-02 |
| `cargo xtask adr new "<title>" [--number N]` | allocate an ADR from the template and add its index row | FND-02 |
| `cargo xtask adr index [--fix]` | check (or rewrite) the ADR index against the files | FND-02 |
| `cargo xtask land <ref>… (--push R B \| --no-push)` | cherry-pick onto a clean, current `main`, run `check`, fast-forward and push only on green | FND-02 |
| `cargo xtask test [--only <stage>]` | test stages alone (unit, integration, doctests, e2e, platform-tests) | FND-02, FND-05 |
| `cargo xtask coverage [--changed-since <rev>]` | llvm-cov over in-process suites vs `coverage-floor.toml`; changed-line gate | FND-05 |
| `cargo xtask formal-typecheck` / `formal-test` | typecheck every `.qnt`; run Quint scenario tests | FND-06 |
| `cargo xtask conformance [--typecheck]` | all formal and replay stages: typecheck, scenarios, simulate, freshness, generate, replay, negative controls | VER-01 |
| `cargo xtask formal-verify` | Apalache bounded verification of every model's `allInvariants` plus long multi-seed simulation | VER-07 |
| `cargo xtask mutants-curated [--only <n>]` | apply each curated mutant in a scratch tree; require its named killer to fail | VER-05 |
| `cargo xtask mutation [--in-diff <rev>] [--shard k/n]` | `cargo-mutants` over the configured scope | VER-05 |
| `cargo xtask fuzz <target> [--time S] [--reproduce F]` | bounded fuzz run on the pinned nightly, or reproduce a crash | VER-06 |
| `cargo xtask faultlab run <scenario> [--seed N]` | build binaries, run one fault-lab scenario, exit by verdict | VER-03 (scenarios VER-04) |
| `cargo xtask faultlab suite [--only <prefix>]` | run a scenario set, never stopping at a failure; summary file | VER-04 |
| `cargo xtask faultlab check <run-dir>` | re-run the checker over a saved history | VER-03 |
| `cargo xtask faultlab replay <run-dir>` | re-run the saved scenario and seed on fresh state; succeed only if the verdict reproduces | VER-03 |
| `cargo xtask release-check [--target T]` / `sbom` | package, extract and smoke the archive; produce the SBOM | OPS-03 |

Stages of `check` in order (each a catalog row): `fmt-check`, `architecture`, `clippy`, `docs`,
`unit-tests`, `integration-tests`, `doctests`, `e2e-tests`, `formal-typecheck`,
`formal-scenarios`, `formal-simulate`, `formal-freshness`, `formal-generate`,
`conformance-replay`, `conformance-controls`, `coverage`, `changed-lines`, `deny`, `deny-fuzz`,
`secrets`, `workflows`, `action-pins`. A stage enters `check` in the commit of the task that
introduces it, together with its `ci.yml` job entry.

---

## 10. Commits, ADRs and integration

### 10.1 Commits

1. **One commit per task**, DCO-signed, subject `<ID>: <summary>`: imperative, under 72
   characters. The body says why, names new dependencies with their reason, and carries an
   `Impact:` line (none, dependency graph, public API, journal schema, protocol, config, CLI).
2. **Author** is the human driving the work: `Patryk Żmigrodzki <pzmi@users.noreply.github.com>`
   (a public no-reply address; this repository is public). The **committer** is the
   environment's own identity, which signs the commit.
3. **Trailer block last, in this order:** `Signed-off-by`, `Co-Authored-By`, `Claude-Session`.
   Because `git commit -s` appends after an existing trailer block, the block is written by hand.
   No other trailers.
4. Stage only the task's files (never `git add -A`); never `--no-verify`; never force-push; never
   push tags. `checkpoint-*` tags are local.
5. No secrets, internal hostnames, private-repository content, personal paths or model ids in
   repository content (the attribution trailers are the only exception).

### 10.2 Integration (D4)

Integration is directly on `main`: the orchestrator lands each task's commit with `cargo xtask
land`, which starts from a clean `main` equal to the remote after a fetch, cherry-picks on a
detached head, runs the full `check` on the merged tree, and fast-forwards and pushes only on
green. Conflicts are auto-resolved only in `Cargo.lock` (take integration's side, then `cargo
update -w`) and in the ADR index (`adr index --fix`); anything else aborts and names the files.
One commit lands per green CI run on `main`. A red `main` is fixed immediately, by a new commit.

### 10.3 ADRs

Nygard format in `doc/adr/NNNN-kebab-title.md`: `# NNNN. Title`, `Date:`, `## Status`
(`Proposed`, `Accepted`, `Superseded by NNNN`), `## Context`, `## Decision`, `## Consequences`.
The index `doc/adr/README.md` is checked against the files by the `docs` stage. ADR 0001 records
the practice; D*n* in tasks.md is ADR *n*+1. A new or reversed architectural decision adds or
supersedes an ADR in the same commit; a decision is never edited away. Only the owner moves a
decision to Accepted.

---

## 11. Agent contract

### 11.1 Definition of Ready

A task is ready when its card names the cited sections, its dependencies have landed on `main`,
its acceptance criteria are verifiable by a command or test, and its model impact (if any) is
stated.

### 11.2 What an implementing agent does

1. Reads tasks.md's Decisions and rules, then only the sections the task cites, then the files it
   changes.
2. Works in **its own git worktree** on a branch from current `origin/main`, with **its own
   `CARGO_TARGET_DIR`** (deleted when done); temporary files go in the session scratchpad.
3. Updates policy and ADRs first when a new dependency or capability is needed.
4. **Model first** (D10): a change to lifecycle, scheduling, recovery, the scale-set session or
   the link changes the Quint model, its invariants, named scenarios and the conformance driver
   before the Rust code.
5. Writes failing tests first, then the smallest code that passes, at the lowest test level that
   can fail for the reason (verification §1).
6. Iterates with `cargo xtask quick`; runs the live gate (`cargo xtask check`, or the fallback
   before FND-02) **in the foreground** on the exact tree it commits, and commits only on green.
7. Commits once, as §10.1 says.

### 11.3 What an implementing agent never does

Push, land, tag, merge or change remote settings; edit `CLAUDE.md` or `.claude/`; flip a
decision to Accepted; delete or weaken a failing test, a modelled invariant, an architecture
rule or a golden fixture to get green; add `#[ignore]`, an unbacked `#[allow]` or a sleep; run a
gate in the background and end its turn; kill processes by name pattern (only PIDs it started);
run real Tart or live GitHub (owner-gated lanes); report `quick` as `check`.

### 11.4 Report format

The final report states, in this order:

1. Task id, commit SHA and subject.
2. Files changed (paths).
3. Gates run, each with its exit status: the exact command, tool versions from `doctor`, and the
   seeds used (`QUINT_SEED`, proptest, fault lab).
4. Acceptance criteria, each mapped to the test or command that proves it; `(owner)` criteria
   listed as pending.
5. Deviations from the card, with reasons; ADRs added; decisions needing the owner.
6. Anything not run, and why. No unverified claim of quality.

### 11.5 Definition of Done

Code, tests and models agree; the architecture rules and their fixtures pass; the gate is green
on the merged tree; affected documents and ADRs are updated; the report above is complete. An
exception needs the owner's explicit approval and a follow-up task, with no false green signal.
