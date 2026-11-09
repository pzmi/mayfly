# implement

Dispatch: subagent (general-purpose). Parallel sub-subagents only for independent, crate-scoped pieces.

## Inputs

plan.{planPath, checklist, adrs}, intake.{criteria, cites, taskId, plane, invariants, modelImpact, gate}, review.findingsToFix (if looping)

## Prompt

Implement task {taskId}. Checklist: {plan.planPath}. Criteria: {criteria}. Gate: {gate}. {findingsToFix ? "Fix these review findings only: " + findingsToFix : ""}

1. Read `CLAUDE.md`; its rules bind you. Read the cited `doc/plan.md`, `doc/foundation.md` and `doc/verification.md` sections, the ADRs for the crates you touch, and the files the checklist names.
2. Work the checklist in order, ticking `- [x]` in `.work/<id>/plan.md` as each lands. Write the test with the code, not after; a failing test first where practical.
3. Model first: if the task touches lifecycle, scheduling, recovery, the scale-set session or the link, change the Quint model, its scenarios and the conformance driver before the Rust code (ver §6). Never remove or weaken a modelled invariant to get green.
4. Rules that fail the gate or the review if broken:
   - **Architecture law** (fnd §2, §3): pure crates have no `std::{fs,net,process,env}`, clock, randomness, Tokio, HTTP or SQL; adapters do not reference sibling adapters or composition; production crates never depend on testkit, conformance or the fakes; a new crate gets its workspace member, `architecture.toml` entry and external allowlist in the same commit. Fix an architecture failure by fixing the structure, never by loosening `architecture.toml` unless the task says so.
   - **Planes**: no control crate reaches a data crate or DP adapter, or the reverse; they exchange only `Directive`/`Observation`; the data plane keeps no durable state and names no journal type.
   - **Event sourcing** (plan §5): only `AggregateStore::save` calls `Journal::append`; events, projections, delivery (`DirectiveIssued`), receipt and inbox commit in one transaction; reactors after commit; `decide`/`evolve` pure; new events registered with a golden fixture; never edit an existing golden or a released migration.
   - **Effects**: transition on observations, not on sent directives; ambiguous outcomes become `Unknown` and leave it only on an observation; retries reuse the VM name and generation; stale epochs and generations are refused or ignored, never applied.
   - **Ownership**: no delete without proof (journal record, exact `OwnedVmName`, generation); never by prefix; an unknown `mf-*` VM is reported.
   - **Processes and secrets**: argv only, never `sh -c`; deadline, own process group, bounded output; `Secret<T>` for every secret; nothing secret in the journal, delivery (`DirectiveIssued`), logs, argv, child env or fixtures; JIT over stdin only.
   - `unsafe_code` is forbidden. No `todo!`, `unimplemented!`, `dbg!`. No `#[ignore]`. No `#[allow]`/`#[expect]` without an `architecture.toml` exception citing an ADR. `#[cfg]`, never `cfg!`.
   - Tests are named `should_<outcome>_when_<condition>` with `// given`, `// when`, `// then`. No sleeps: fake clock, `tokio::time::pause`, channels; a real-OS wait only via `mayfly-testkit-shared::poll` (bounded, with a reason). Real objects or testkit fakes; no mocking frameworks; tempdirs, port 0. Never a real `tart`, the network or macOS outside an owner-gated test.
   - Logging: `tracing`, fixed `snake_case` event names, data in fields; nothing at INFO on the healthy path.
5. A doc comment's behavioural claim names the test that pins it. Comments only for non-obvious constraints and why.
6. A new dependency: `cargo add` (in the right crate, or `[workspace.dependencies]`), its `architecture.toml` external allowlist entry, and list it in `deviations` so ship names it in the commit message. An architectural decision: `cargo xtask adr new "<title>"` (with the reserved `--number`) or supersede the ADR from the checklist, then `cargo xtask adr index --fix`.
7. `cargo fmt --all`, then the gate in the foreground with the lane's `CARGO_TARGET_DIR` and `CARGO_BUILD_JOBS=2` until green (`cargo xtask quick` while iterating once it exists). Nothing outside the task's files is staged or reverted.

## Output

```
{ filesChanged[], affectedCrates[], newDependencies[], adrsWritten[], summary, deviations[], issues[] }
```

`affectedCrates`: `git diff --name-only HEAD; git ls-files --others --exclude-standard` mapped to `crates/<name>`, `tools/<name>` or `formal/`.

## Sensors

Diff non-empty; `cargo fmt --all --check` exit 0; every checklist item ticked or listed in `deviations` with a reason.

## Gates

To test: always.
