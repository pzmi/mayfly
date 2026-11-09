---
name: orchestrate
description: >
  This skill should be used when the owner asks to "run the next tasks", "orchestrate",
  "work through the task list", "do KER-01 to KER-04", "run lane X", "spawn agents for ...", or
  hands over a batch of tasks or a CI failure to drive to green. Runs several `/work`
  tasks through background subagents in git worktrees, lands each onto `main` with
  `cargo xtask land` (gate on the merged tree, then push), watches CI through the
  GitHub MCP tools, and stops at ⛳ checkpoints for the owner. One task alone: use `/work`
  directly instead.
---

# Orchestrate: many tasks, one branch (`main`)

The orchestrator never implements a task itself. It picks tasks, starts one background subagent
per task that follows the `/work` skill, lands the result onto `main`, and keeps the owner
informed. The owner may be driving from a phone: short messages, plain words, one question at a
time, never a token in chat.

## Input

`$ARGUMENTS`: task ids (`KER-04 KER-05`), a phase or prefix from `doc/tasks.md` (`ES`, `phase 3`),
a GitHub Actions run or job URL (a failure to fix), or nothing (the next ready tasks in
`doc/tasks.md` order).

## Startup

1. Read `CLAUDE.md`, the top of `doc/tasks.md` (Decisions, Rules for every task, the live gate:
   the fallback before FND-02, then `cargo xtask check`) and the overview rows of the tasks in
   scope with their dependencies, model and ⛳ mark.
2. A task is **ready** when every dependency has a commit and the task has none yet. Subjects
   are `<ID>: summary`, so: `git log --oneline main | grep " <dep>:"`. Tasks whose title starts
   `(owner)` (HW-01..03) are never dispatched: an agent may write their workflow and docs only
   when the task says so; the run belongs to the owner.
3. `git status` on `main` must be clean; someone else's edits are left alone and reported.
   `git fetch origin` and confirm the local branch is not behind.
4. Toolchain: the SessionStart hook installs the pinned toolchain and fetches crates. If it
   reported a missing tool the gate needs (cargo-nextest, cargo-deny, cargo-llvm-cov, quint,
   Apalache), install it once now at the version `tools/versions.toml` pins (`cargo xtask
   doctor` says what is missing or wrong; `npm ci --prefix formal` for Quint), not per agent.
5. Resources: the sandbox has 4 CPUs and 15 GB RAM. **At most two lanes run at once**, each
   with `CARGO_BUILD_JOBS=2`. `df -h /` and `free -g` before every dispatch round; start fewer
   lanes, or delete finished lanes' target dirs, rather than fill the disk or memory.

## Lanes and worktrees

Tasks whose file sets are disjoint run in parallel, one worktree each, under the session
scratchpad (never `/tmp`, never inside the repo):
`git worktree add <scratchpad>/wt-<lane> -b <lane> main`.
Reuse a lane's worktree for its next task after `git -C <scratchpad>/wt-<lane> reset --hard
main`; `git reset --hard` is only ever run with `-C` naming a scratchpad worktree.
Each lane builds with its own `CARGO_TARGET_DIR` under the scratchpad
(`<scratchpad>/<lane>-target`), named in the agent's prompt; `/dev/shm` only when `free` shows
the memory clearly spare. Delete a lane's target dir once its commit has landed and the lane
has no next task.

Good pairings keep the planes apart: a control-plane task (KER-04..07, ES-*, CP-*) beside a
data-plane task (KER-08, DP-*) or a fake (DP-03, GH-01), rather than two tasks in one crate.

Shared files that serialise tasks (run them one after another, or expect a rebase):

| File | Why | On conflict |
|:--|:--|:--|
| `Cargo.toml` (workspace `members`, `[workspace.dependencies]`) | every new crate or shared dep | merge both lists by hand, keep order |
| `Cargo.lock` | every dependency change | never hand-merge: take the integration side (`git checkout --ours Cargo.lock` in a cherry-pick), then `cargo update -w` to re-resolve; `cargo generate-lockfile` only as a last resort, since it bumps everything |
| `architecture.toml` (crate lists, edges, external allowlists, exceptions) | every new crate, edge, dependency or rule exception; the CP/DP split lives here | merge both entries; `cargo xtask architecture` must pass; never loosen to resolve |
| `tools/xtask/`, `.cargo/config.toml`, `tools/versions.toml` | the gate itself and its pinned tools | serialise: never two lanes on the gate |
| `MODELS` registry in `tools/xtask/` and `.github/workflows/formal.yml` (model matrix) | every new Quint model (KER-02, KER-04..07) | one lane owns them per dispatch round; the others list their rows in the report for the owner lane or a follow-up |
| `mayfly-testkit` contract suites (journal, link, VmProvider) and its fakes | ES-01, DP-01, DP-02, DP-04 and every adapter that runs a suite | one lane changes a suite at a time; an adapter lane consumes it, never edits it |
| event registry and `crates/mayfly-control-domain/tests/fixtures/events/` | every new event (KER-04..06) | merge registry entries; never rewrite an existing golden |
| `doc/tasks.md`, `doc/plan.md` | status notes, minimal corrections | keep both edits |
| `doc/adr/README.md` | every new ADR | reserve the numbers when dispatching (pass `--number N`); `cargo xtask adr index --fix` regenerates the index; `land` resolves it |

## Model per task

The `Model` column of `doc/tasks.md` decides; when a row has none:

| Work | Model |
|:--|:--|
| Design, CP/DP boundary, protocol, fencing, recovery, ownership/delete safety, secrets, concurrency, Quint models, the architecture checker, anything a ⛳ guards | opus |
| A well-specified implementation with clear `Accept:` | sonnet |
| Mechanical: ADR files, doc pointers, a pinned version bump, a lint fix | haiku |
| Final multi-aspect review, an incident with unclear cause | the strongest model available |

## The task agent's prompt

Start each with the Agent tool, `run_in_background: true`, the model from the table, and this
prompt, filled in. The agent **reads** the work skill's files rather than calling
`Skill('work')`: the skill registry may not list a repo's skills mid-session, and a file read
always works.

```
Task <ID> of mayfly (<one-line title>). Work ONLY in the worktree <path> (branch <lane>);
first `git -C <path> reset --hard main`, then cd there for every command.
Read CLAUDE.md, then read .claude/skills/work/SKILL.md, processes.md and work-order.md and
follow them for task <ID>: read each .claude/skills/work/stations/<station>.md on entering that
station, and run every station in order (do not call Skill('work'); read the files).
Context the task row does not carry: <facts from this session: what CI or the owner showed,
decisions already made, files another lane is changing right now, shared registries or
contract suites this lane owns or must not touch>.
ADR numbers reserved for you: <NNNN, ... | none>; create them with
`cargo xtask adr new "<title>" --number NNNN`.
Gate: <`cargo xtask check` | the fallback in CLAUDE.md>, as doc/tasks.md says.
Run it in the foreground with CARGO_TARGET_DIR=<lane target dir> and CARGO_BUILD_JOBS=2;
commit only on green; never end your turn with anything still running; kill only PIDs you
started, never by pattern. Never run a real `tart` command; use the fakes.
Constraints: one commit on <lane> with
--author "Patryk Żmigrodzki <pzmi@users.noreply.github.com>" (the committer stays the
environment's identity), subject `<ID>: <summary>`, a body with why and an `Impact:` line,
ending with exactly this trailer block, written by hand (no `-s`):
Signed-off-by: Patryk Żmigrodzki <pzmi@users.noreply.github.com>
<the Co-Authored-By and Claude-Session lines from the session's attribution instructions>
Stage only your own files. Do not edit CLAUDE.md or .claude/ (report a needed change). Do not
push, do not touch main, no `gh`, no PR, no tags. No model ids, secrets or personal paths in
repo content. Report: commit sha, files changed, the last line of the gate's output with the
test count, deviations from the task row and why, `(owner)` criteria left pending, and anything
a later task must know.
```

A follow-up to a finished agent goes through `SendMessage` to the same agent, which keeps its
context; a new Agent call starts cold. Never start a second agent on the same task.

## Land and gate

1. Read the report. A claim that matters (a gate result, a test count, a sha) is spot-checked
   with `git -C <wt> log -1 --format=%B` / `show --stat` (subject, `Impact:` line, trailer block
   last and ordered, author identity).
2. Land **one commit at a time**, and the next only after the previous commit's CI run is
   green: `cargo xtask land <sha> --push origin main` (`--no-push` lands on the local `main`
   only). It refuses a dirty or stale `main`, cherry-picks, resolves only `Cargo.lock` and the
   ADR index, aborts on any other conflict, runs `check` on the merged tree and pushes only when
   it is green. Integration is direct on `main` (D4); there is no PR phase. Never land on red CI.
   Before FND-02 lands `land` does not exist: cherry-pick onto a clean `main`, run the fallback
   gate on the merged tree, and push with `git push origin main` only on green.
3. An aborted land: a conflict in code the agent wrote goes back to that agent via
   `SendMessage` (rebase its lane, re-run the gate); a shared file follows the table above.
   Red `check` on the merged tree but green in the lane is an interaction bug: hand it to the
   later task's agent, never "fix" it here.
4. A rejected push: `git fetch`, then `land` again. Never force. Then delete the lane's target
   dir if the lane has no next task.
5. When several commits are ready, land a change to how CI runs (a workflow, an xtask stage, a
   toolchain pin) first, so every later run has it.
6. Never edit a worktree while its gate or `land` runs in it: a change made under a running
   gate makes its result meaningless; stop the run and start again.

## After the push

- **CI**, after every push and before the next land (`gh` may be unavailable, so use MCP):
  `actions_list` (workflow runs for the branch, match the head sha), `actions_get` until
  `completed`, then `get_job_logs` with `failed_only` and `tail_lines` 60 to 120 (a large log
  lands in a file: grep it). Red is work now: name the failing line, hand the fix to a work
  agent (or the owning lane), land it. Never skip, `#[ignore]`, or retry a test to green; a
  macOS-only failure is fixed from the log and said so. Extract a long CI log through a
  subagent and act on its verbatim summary of the failing lines, not on the raw dump.
- **Before cancelling a run, read every job's state.** A run that looks stuck may have
  recovered; cancelling it throws away its green jobs and, on `main`, its signal.
- **A red job is re-run at most once**, and only when it died before any test body ran (runner
  lost, checkout or toolchain download failed) or to prove the failure is not this commit's
  (red on the previous commit too). Name the job, what failed and why it is not this change
  before re-running; a second failure is real.
- **A failure the owner reports**: read the failed job's log first, name the line that failed,
  then fix the code, or tell the owner the one setting to change and where.

## Checkpoints and the owner

- ⛳ tasks: **FND-04, DDD-01, KER-08, ES-04, E2E-01, E2E-02, VER-04, OPS-03**. A ⛳ task stops its lane
  after its commit is landed and green: run `/review` over `<newest checkpoint-* tag>..HEAD` in a
  subagent (it reads `.claude/skills/review/SKILL.md`), then a **second, adversarial subagent**
  that tries to refute each HIGH and MEDIUM finding and to find what the first missed, aimed at
  this checkpoint's risks (language and aggregate boundaries against the data-oriented rule at
  DDD-01, fencing and epochs at KER-08, single write path and atomicity at
  ES-04, ownership and `Unknown` outcomes at E2E-01, ack-after-commit and demand at E2E-02,
  oracle independence at VER-04, provenance and secrets at OPS-03). Hand the surviving findings
  to a work agent as a fix task, land it, then create the local annotated tag
  `checkpoint-<ID>` (never pushed), report to the owner and wait for a go.
- At each ⛳, batch the open owner decisions (Proposed decisions D2–D15 to accept or override,
  `(owner)` acceptance criteria in `doc/tasks.md`, owner-only settings such as a branch
  ruleset, the self-hosted Mac, the GitHub App) into that report, rather than saving them for
  the end of the plan.
- Ask only what is the owner's to decide (spend, an account, a destructive or outward step, a
  real product choice). Recommend one option. Everything else: decide, do, report.
- A credential is never pasted into chat. It goes into the environment settings or a GitHub
  secret; say which variable name will be read.
- Report in plain words: what now works, what the owner needs to do (numbered, with the exact
  screen and button), and what is still running. Never only a sha.

## Recovery

| Situation | Do |
|:--|:--|
| CI unavailable | Run `cargo xtask check --only <stages>` locally for each CI job's stages, report it as local, and land nothing more until CI confirms. |
| Usage limit or session end | Agents' findings and state live in files (`.work/<ID>/`, the scratchpad). Resume each agent by `SendMessage`, never by a new spawn. |
| Disk or memory pressure | Delete finished lanes' target dirs; drop to one lane; never `cargo clean` a lane that is building. |
| A stuck process (a daemon, fake-tart, a test) | Stop it by the PID you started; never by name pattern. |
| A task needs real Tart or live GitHub | Deliver everything up to it; report the `(owner)` step as pending (D14). |

## Never

Implement a task in the orchestrator's own context; merge a pull request; force-push; push
tags; delete a remote branch; land on red CI or more than one commit per CI run; run more than
two lanes; kill a process by name pattern; run a real `tart` command; end a turn with the gate
running in the background; `git reset --hard` outside a scratchpad worktree; hand-merge
`Cargo.lock`; loosen `architecture.toml`, a lint, a model invariant or a test to get green;
start a second agent on the same task; or predict an agent's result before its notification
arrives.
