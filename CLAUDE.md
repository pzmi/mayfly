# CLAUDE.md

## Project

mayfly is a headless Rust service running **single-use GitHub Actions runners in disposable
VMs** (one job per VM, cloned from a pinned image, destroyed after). Demand comes from GitHub
Runner Scale Sets; VMs come from a `VmProvider` port whose first adapter drives Tart by argv.
Binaries: `mayflyd` (daemon) and `mayfly` (operator CLI over a `0600` Unix socket). The
**control plane** (event-sourced SQLite journal, CQRS, outbox/inbox, reconciler, scale-set
listener) decides; the **data plane** (directive executor, observers) acts and stores nothing
durable; `architecture.toml` enforces the split. Public, open source (`MIT OR Apache-2.0`).

## Docs

| File | What it holds |
|:--|:--|
| `doc/plan.md` | Product and architecture: goals and guardrails (§1), CP/DP split and protocol (§3), crates (§4), event sourcing and CQRS (§5), attempt lifecycle (§6), GitHub (§7), data plane and ownership (§8), capacity (§9), recovery (§10), operator interface (§11), observability (§12), security (§13) |
| `doc/foundation.md` | Engineering rules: dependency law (§2), enforcement (§3), logging (§5), event-sourcing rules (§6), CI (§7), commands (§9), commits (§10), agent contract (§11) |
| `doc/verification.md` | Test levels (§2), contract suites (§5), formal models and conformance (§6), fault lab (§8), curated mutants (§9) |
| `doc/tasks.md` | Decisions D1–D15, rules for every task, task order and dependencies, model per task, ⛳ checkpoints, the live gate |
| `doc/adr/` | Decisions, Nygard style (`NNNN-kebab-title.md`: Status, Context, Decision, Consequences); index in `doc/adr/README.md` |

Citations read "plan §5.2", "fnd §3", "ver §6"; read the sections a task cites, not whole
documents. Decisions in `doc/tasks.md` and Accepted ADRs override earlier plan text.

## Workflow

- `/orchestrate [ids|lane]`: run ready tasks through background agents in worktrees, land each
  with `cargo xtask land`, watch CI, stop at ⛳ checkpoints. The orchestrator never implements.
- `/work <ID>`: one task through intake, plan, implement, test, polish, review, qa, ship,
  babysit. State in `.work/<ID>/` (gitignored). The skill lives in `.claude/skills/work/`; an
  agent that cannot load it by name reads `SKILL.md` and its station files and follows them.
- `/review [range]`: read-only review; default range since the newest `checkpoint-*` tag.
- `CLAUDE.md` and `.claude/` belong to the orchestrator: a task agent reports a needed change.

## The gate

Before FND-02 lands, the gate is the fallback:

```bash
cargo fmt --all --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked
```

From FND-02 on:

```bash
cargo xtask quick          # fmt check, architecture, clippy, fast tests: while working
cargo xtask check          # the full blocking gate (fnd §9): before every commit
cargo xtask check --only <stages>   # named stages through the same code (what each CI job runs)
cargo xtask architecture   # crate/plane policy and its negative fixtures
cargo xtask list-checks    # every check, machine-readable
cargo xtask doctor         # every pinned tool: run it first when local and CI disagree
cargo xtask adr new "<title>" [--number N]   # and `cargo xtask adr index --fix`
cargo xtask land <sha> --push origin main    # orchestrator only (D4)
```

Later tasks add stages (coverage FND-05, formal FND-06, mutants VER-05). `quick` is not `check`;
never report one as the other. A repeatable job becomes an `xtask` subcommand. A missing tool
fails, never a skipped-but-green check. Run the gate in the foreground; commit only on green.

## Enforced rules

- **Dependency law** (fnd §2, §3; plan §4): cli → composition → adapters → application
  (`mayfly-control`, `mayfly-data`) → pure crates. Adapters never reference sibling adapters or
  composition. Production crates never depend on `mayfly-testkit`, `mayfly-conformance` or the
  fakes; `tools/faultlab` and the fakes import no production crate (D11). A new crate gets its `[planes]` entry,
  `members` entry, `architecture.toml` entry and external allowlist in the same commit. Fix an
  architecture failure in the structure, never by loosening `architecture.toml`.
- **Control plane / data plane** (plan §3.4, D1): no `mayfly-control*` crate or CP adapter
  (sqlite, github, secrets) reaches a `mayfly-data*` crate or DP adapter (tart, guest, host), or
  the reverse; both meet only in `mayfly-domain` and `mayfly-protocol`, and only
  `mayfly-composition` sees both. The data plane names no journal type; the control plane names
  no process API. They talk only through protocol messages (`Directive`, `Observation`).
- **Pure crates** (`mayfly-domain`, `mayfly-protocol`, `mayfly-control-domain`,
  `mayfly-data-domain`): no `std::{fs,net,process,env}`, clock, randomness, Tokio, HTTP or SQL.
  Application crates take time and ids only through ports. `std::process` only in
  `mayfly-adapter-tart`, `mayfly-adapter-guest`, `mayfly-composition` and test tools.
  Pure crates also use no std `HashMap`/`HashSet` (use `BTreeMap`/`BTreeSet`), no atomics,
  locks or interior-mutable statics, no `.elapsed()` (`[purity]` in `architecture.toml`).
  Test kits are per plane (ADR 0019): `mayfly-testkit` is the control plane's; data-plane crates
  never dev-depend on it or on `mayfly-conformance`. Spawns only in the modules fnd §3 lists.
- **Language**: `unsafe_code` forbidden; no `todo!`, `unimplemented!`, `dbg!`, `#[ignore]`; no
  `#[allow]`/`#[expect]` without an `architecture.toml` exception citing an ADR; zero clippy
  warnings; platform code by `#[cfg]` items or blocks, never a `cfg!(target_os…)` branch.
- **Tests** (ver §2): `should_<outcome>_when_<condition>` with `// given`, `// when`, `// then`;
  at the lowest level that can fail for the reason it exists. No sleeps (fake clock,
  `tokio::time::pause`, channels; a real-OS wait only through the shared test kit's `poll` helper (FND-05), bounded,
  with a reason). Real objects or testkit fakes, no mocking frameworks; tempdirs, port 0. A port
  with a fake and a real adapter runs one contract suite against both (ver §5), and the suite's
  own tests prove a deliberately broken implementation fails it. Never delete a failing test,
  weaken a modelled invariant or rewrite a golden fixture to get green.
- **Model first** (ver §6, D10): a change to lifecycle, scheduling, recovery, the scale-set
  session or the CP↔DP link updates the Quint model in `formal/`, its scenarios and the
  conformance driver before the Rust code. Never remove an invariant to get green.
- **Data-oriented DDD** (ADR 0022): DDD is strategic (bounded contexts, ubiquitous language,
  consistency boundaries in `doc/domain-model.md`; code names follow the language). Code is
  data-oriented: plain data with public fields and enums, behaviour in free pure functions and
  tables; no getters/setters or privacy ceremony beyond validated boundary value types. Follow the
  ADRs: a task cites the ADRs it implements, and a deviation needs a new or superseding ADR.
- **Event sourcing** (plan §5, fnd §6): `AggregateStore::save` is the only caller of
  `Journal::append`; one transaction holds the events, synchronous projections, outbox rows,
  command receipt and the inbox entry that caused it; reactors run after commit and are never
  replayed. `decide`/`evolve` are pure and deterministic, refusals included (`DETERMINISM_CASES`
  with accepted and refused cases for every aggregate). Events are immutable, past tense,
  `EVENT_TYPE` + `SCHEMA_VERSION`, one golden JSON fixture per type and version; evolve by
  optional fields, new types or upcasters, never a rename. Commands never read projections or
  return read DTOs; queries never take a write port. A projection's change of meaning is a new
  spec version. Migrations are forward-only raw SQL, never edited once released.
- **Effects** (plan §6, §3.2): state reflects an observed fact, never an attempted command.
  `Unknown` leaves only on an observation; a timeout never becomes success or `Deleted`. A retry
  reuses the same VM name and generation. Directives carry the host epoch; a stale one is refused.
  The scale-set listener acks a message only after its inbox record commits.
- **Ownership** (plan §8.4, G4): a VM is deleted only with positive proof: journal record **and**
  exact `OwnedVmName` **and** generation, matched against the fence. Never a base image, never a
  foreign VM, never by name prefix; an unknown `mf-*` VM is reported, never deleted.
- **Tart and processes** (plan §8.2, D12): argv only, never `sh -c`; every call has a deadline,
  its own process group, bounded captured output and redaction.
- **Secrets** (plan §13, D13): `Secret<T>` with redacting `Debug`/`Display` and no `Serialize`;
  never in the journal, outbox (a `SecretHandle` only), logs, metrics, argv or a child's env; a
  JIT config reaches the guest launcher over stdin only.
- **Logging** (fnd §5): `tracing`, fixed `snake_case` event names, data in fields; (almost)
  nothing at INFO on the healthy path; metric labels bounded, never ids.
- **Dependencies**: `cargo add`, reason in the commit message; `Cargo.lock` committed, builds
  `--locked`; licences per `deny.toml`; GitHub Actions pinned by commit SHA.
- **Decisions**: a change that adds or reverses an architectural decision adds an ADR
  (`cargo xtask adr new "<title>"`, with `--number N` when the orchestrator reserved one) or
  supersedes one, in the same commit. Only the owner flips Proposed to Accepted.

## Commits

- Direct on `main` (D4): one DCO-signed commit per task, landed by the orchestrator with
  `cargo xtask land`; implementing agents commit on their lane and never push or tag.
- Author and `Signed-off-by`: `Patryk Żmigrodzki <pzmi@users.noreply.github.com>` (pass
  `--author`). The committer stays the environment's identity: the environment signs every
  commit with its key and GitHub verifies the signature against the committer.
- Subject `<ID>: summary`, imperative, under 72 chars. The body says why, names each new
  dependency with its reason, and carries an `Impact:` line.
- The trailer block is last, written by hand, in this order: `Signed-off-by`, `Co-Authored-By`,
  `Claude-Session`; the last two exactly as the session's attribution instructions give them;
  add none of your own. `git commit -s` appends `Signed-off-by` *after* an existing trailer
  block, so do not use `-s` with other trailers.
- Stage only your own files (never `git add -A` or `git add .`). Never `--no-verify`, never
  force-push, never push tags; `checkpoint-*` tags are local. No model ids in repo content
  (code, docs, commit bodies); the session's attribution trailers are the only exception.

## Sandbox

- Linux, 4 CPUs and 15 GB RAM: **at most two concurrent lanes**, each with
  `CARGO_BUILD_JOBS=2` and its own `CARGO_TARGET_DIR` under the session scratchpad, deleted when
  the lane is done. `/dev/shm` is RAM: use it only when free memory clearly allows.
- No Docker, no macOS, no Tart. Provider tests use the in-memory fake and `mayfly-fake-tart`;
  GitHub tests use `mayfly-fake-github`. Never run a real `tart` command. Real-Tart and live
  GitHub lanes (HW-01..03, `hardware.yml`, `MAYFLY_REAL_TART_HOME`) are owner-gated (D14): deliver
  everything up to them and report them as pending.
- Node (Quint) and the JVM (Apalache) are dev-time only, for `formal/`; binaries never need them.
- `gh` may be unavailable: use the GitHub MCP tools (`actions_list`, `actions_get`, `get_job_logs`).
- Hooks (each a no-op until its prerequisite exists): SessionStart installs the pinned toolchain
  and fetches crates (remote sessions); every edited `*.rs` is rustfmt'ed; Stop runs the fast gate
  when Rust, manifests, the policy or a model changed.
- Temporary files go in the session scratchpad, never `/tmp` or the repo.
- Kill only PIDs you started, never by name pattern (`pkill -f`, `killall`). Never end a turn
  with the gate, a daemon or other work still running in the background.

## Public repository

Never commit secrets, tokens, JIT configs, private keys, credentials, internal hostnames,
private-repo content or personal paths. **Never name a private project** of the owner's anywhere
(code, docs, ADRs, fixtures, commit messages): no name, no path, and no hint that a design came
from elsewhere; every design is described as mayfly's own, on its own merits. Fixtures (GitHub recordings, Tart JSON, configs) are
redacted or synthetic.
