# babysit

Dispatch: inline

## Inputs

ship.{commitSha, pushed}, intake.branch

## Steps

1. **Find the run** for the pushed commit: MCP `actions_list` (workflow runs, filter by branch, match `head_sha`), then `actions_get` until its `status` is `completed`; with `gh`, `gh run watch`. Poll patiently with the Monitor tool or spaced checks; never a busy loop, and never tell the owner to "check back" if you can wait.
2. **Green**: every required job succeeded (`doc/required-checks.md` once FND-04 has landed). Done.
3. **Red**: read the failing job with MCP `get_job_logs` (`failed_only`, `tail_lines` 60 to 120; a large log lands in a file: grep it). Name the failing line. Reproduce with the matching `cargo xtask check --only <stages>`, fix, run the gate in the foreground, commit on green with the rules of `ship.md` (a fix commit repeats the task id: `<ID>: fix <what>`), land it, wait again. A macOS-only failure (the `macos-latest` job, `aarch64-apple-darwin`) that cannot be reproduced on Linux: fix from the log, say so. An owner-gated lane (`hardware.yml`, live GitHub) is not yours to run: report it. At most 3 fix attempts, then stop and report the failing job, step and your diagnosis.
4. Never skip, `#[ignore]`, or re-run a flaky test to green; a flake is a race or a seed to find and pin. The one exception is a job that died before any test body ran (runner lost, checkout or toolchain download failed) or a failure shown not to be this commit's (red on the previous commit too): re-run it at most once, after recording the job, what failed and why it is not this change. A second failure is real.
5. Write `babysit.json`.

## Output

```
{ finalStatus: green|blocked, checks[{name, conclusion}], fixesPushed[{sha, summary}], ciAttempts, issues[] }
```

## Sensors

CI green, or blocked after 3 fixes with the diagnosis recorded.

## Gates

To end: always (terminal).
