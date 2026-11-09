# Required checks

The CI result that must be green on `main` is the `required` job of
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml). It is an aggregate: it needs every
other job of the workflow and fails when any of them failed, was cancelled or was skipped, so a
job cannot drop out of the gate by not running ([foundation §7.2](foundation.md)). Each job runs
`cargo xtask check --only <stages>` (or, for `macos`, `cargo xtask test --only platform-tests`),
so the same stages run on a laptop with `cargo xtask check`.

## Branch protection

Integration is direct on `main` (D4), so "required" here means "must be green, and red is fixed
immediately on `main`", not "GitHub refuses the merge". No ruleset is created by any task.
**(owner, pending)** If the owner wants a branch ruleset on `main`, it requires the single check
`Required` below and nothing else: the job's name is the check name GitHub shows, and
requiring the aggregate means a job added to `ci.yml` later is required without editing the
ruleset.

## The checks

The check name shown by GitHub is the job's `name`. The workflow runs on pushes to `main`, on
pull requests and by hand. A test in `tools/xtask/src/check.rs`
(`should_keep_required_checks_doc_in_step_when_ci_jobs_change`) fails when this table and
`ci.yml` disagree.

| Job id | Display name | Runs (`cargo xtask ...`) |
|:--|:--|:--|
| `architecture` | Architecture and policy | `check --only architecture` |
| `static` | Static quality | `check --only fmt-check,clippy,docs` (docs: the ADR index matches the ADR files), after `doctor` has asserted every pinned tool a `check` stage needs (Node 22 and the locked Quint included) |
| `cross` | Cross check (aarch64-apple-darwin) | `check --only cross-check` (Clippy over every target for the macOS target, warnings denied) |
| `tests` | Tests | `check --only unit-tests,integration-tests,doctests` (nextest under the `ci` profile; doctests on `cargo test --doc`) |
| `formal` | Formal models (Quint) | `check --only formal-typecheck,formal-test` (typecheck every `formal/*.qnt`, run each registered model's Quint tests; Node 22 and `npm ci --prefix formal` first) |
| `macos` | macOS (arm64) | `cargo build --workspace --locked`, then `test --only platform-tests` on `macos-latest`, with `RUSTFLAGS=-D warnings` |
| `coverage` | Coverage | `check --only coverage,changed-lines` (the instrumented nextest run against `coverage-floor.toml`, then the changed lines against the merge base; full history) |
| `security` | Security | `check --only deny,secrets,workflows,action-pins` |
| `required` | Required | the aggregate; needs all of the above |

Every stage of `check` is in exactly one job; a test fails when a stage is in none or in two,
and when any step of a job other than `required` (the macOS build and `doctor` included) has
`continue-on-error` or an `if:`. Runs on `main` are never cancelled or replaced by a newer push:
each commit there has its own concurrency group, so every commit on `main` is verified.
`platform-tests` is a stage of `test`, not of `check`: it is the unit and integration tests of
every crate except `xtask` (whose fixtures need the Linux-only scanners), so on Linux the stages
it repeats already run in `tests`.

## Other workflows

These are not in `required`. `security.yml` repeats the security stages weekly and by hand,
because a new advisory can fail an unchanged tree. `codeql.yml` (Rust and Actions) also runs on
pushes and pull requests, as its own workflow. `scorecard.yml` (OpenSSF Scorecard) runs on pushes
to `main`, weekly and by hand. A red run of any of them is reported to the owner and fixed as a
task, but does not make `Required` red. `formal.yml` (the Apalache lane, `cargo xtask
formal-verify`) runs on changes under `formal/`, weekly and by hand. Later tasks add
`faultlab.yml`, `mutation.yml`, `fuzz.yml`, `release.yml` and the owner-gated `hardware.yml`
(foundation §7.1).

## When a check goes red

Read the failed job's log, name the failing line, and fix it on `main`. Locally,
`cargo xtask check --only <stage>` reruns one stage through the same code, and `cargo xtask
doctor` comes first when local and CI disagree. In CI a red job is fixed, never re-run to
green.

- **Re-run at most once**, and only when the job died before any test body ran (the runner was
  lost, a checkout or toolchain download failed) or to prove the failure is not the commit's
  (it is red on the previous commit too). Before the re-run, the report names the job, what
  failed and why it is not this change. A second failure is real: fix it.
- **A failure that passes on a rerun is a race to find**, not a flake: repeat it, read its
  trace, name the race, and add a test that forces the interleaving every time (verification
  §1.5).
- **Before cancelling a run, read every job's state.** A run that looks stuck may have
  recovered; cancelling it throws away its green jobs and, on `main`, its signal.
