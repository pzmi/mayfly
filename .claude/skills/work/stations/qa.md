# qa

Dispatch: inline

## Inputs

implement.{affectedCrates, filesChanged}, intake.{criteria, taskId, plane}

## Steps

1. **Is there a surface?** `mayflyd` or `mayfly` flags, output, `--json`, exit codes; the operator socket; config validation; `mayfly-fake-tart` or `mayfly-fake-github` behaviour; the daemon's end-to-end loop: yes. Pure crates, the journal alone, tooling, CI, models or docs: record `{skipped: "no runnable surface: <reason>"}` and pass.
2. **Build** what the surface needs: `cargo build --locked --bin mayflyd --bin mayfly` (plus `-p mayfly-fake-tart` / `-p mayfly-fake-github` when they are involved), with the lane's `CARGO_TARGET_DIR`. Everything runs against a **fresh state dir, `TART_HOME`-equivalent and config under the session scratchpad**; never the repo, never a real `tart`, never a real GitHub endpoint. Put the fake `tart` first on `PATH` only for the commands you start.
3. **Exercise the surface.**
   - Start `mayflyd` (when it exists) in the background with output to the scratchpad, note its PID, and wait for readiness with a bounded loop on its socket or its first status (no blind sleep).
   - Golden path: the commands the task touched (`mayfly status --json`, `vm list`, `pool drain|resume`, `config validate`, `recover --dry-run`, `events --follow` …) return what the task promises.
   - At least one refusal per changed area: an invalid config names the field and exits nonzero; a second `mayflyd` on the same state dir exits with a clear error (`flock`); the socket and DB are `0600` and the state dir `0700`; a VM the journal does not own is reported, never deleted; a stale-epoch or proofless delete is refused; safe mode starts nothing.
   - Secrets: grep the scratchpad logs, the `--json` output and the fake's recorded invocations for the test secret you configured; it must be absent.
   - Stop the daemon with SIGTERM (`kill -TERM <pid>`, the PID you started; never by pattern), confirm a clean exit and that no fake VM was left behind that the journal says was deleted.
4. **Regressions**: a neighbouring command that used to work. Any regression goes back to implement as a finding.
5. `git status` shows no change from QA itself (no state written into the repo); remove the scratchpad state.
6. Write `qa.json`.

## Output

```
{ skipped: string?, exercised[], refusalsTested[], secretScan: "clean"|"leak"|null, regressions[], issues[] }
```

## Sensors

Skipped with a reason, or `exercised` non-empty with at least one refusal tested, every process you started stopped, `secretScan` not `leak`, and `regressions` empty.

## Gates

To ship: no regression.
