# test

Dispatch: subagent (general-purpose)

## Inputs

implement.{filesChanged, affectedCrates}, intake.{criteria, invariants, modelImpact, contractSuites, gate}

## Prompt

Verify test coverage and run the gate for the working-tree diff. Read `CLAUDE.md` first.

1. Map every `Accept:` clause to at least one test that would fail if the behaviour broke. A clause with none: write the test.
2. Per changed source file, check the level (ver §2); a test lives at the lowest level that can fail for the reason it exists:
   - **unit**: pure logic beside the module (`#[cfg(test)] mod tests`), no I/O; State × Stimulus matrices, `DETERMINISM_CASES`, golden-fixture round trips
   - **property** (`proptest`): invariants over generated inputs (`OwnedVmName` round trip, admitted ≤ every limit, generations strictly increasing, demand formula); a found counterexample is committed as a regression seed
   - **component**: real application code over testkit fakes (`MemoryJournal`, fake provider, fake clock), e.g. lost reply → `Unknown`, duplicate directive executes once
   - **contract** (ver §5): a port with a fake and a real adapter (journal, link, VmProvider, ScaleSetApi) runs one shared suite against both, in every mode the task names (fresh, reopened, crash-injected), and the suite's own tests prove a deliberately broken implementation fails it
   - **architecture**: `cargo xtask architecture` and its negative fixtures; a new rule has a negative case
   - **integration**: real SQLite in a tempdir, sockets on port 0, `mayfly-fake-tart`/`mayfly-fake-github` as processes
   - **e2e** (`assert_cmd`): the built `mayflyd` and `mayfly` against the fakes; stdout, `--json` goldens, stderr and exit codes
   - **compile-fail** (`trybuild`): an API misuse that must not compile (a secret that serialises, an unowned name passed to delete)
   - **conformance** (ver §6): Quint traces replayed against the real code, plus the negative control
   - **secret redaction**: every new log, error, status or diagnostics path fed a known secret, asserted absent
3. Conventions: `should_<outcome>_when_<condition>`, `// given / when / then`, real objects over fakes, no mocking frameworks, no `#[ignore]`, no sleeps (fake clock, `tokio::time::pause`; real-OS waits only via `mayfly-testkit::poll`), port 0, `tempfile` dirs. Never assert on log lines alone. Never a real `tart`, the network or macOS outside an owner-gated test.
4. Run the gate (`cargo xtask check`, or the fallback `doc/tasks.md` names) in the foreground with the lane's `CARGO_TARGET_DIR`, never in the background. Iterate to green. A missing tool is installed at its pinned version or reported; a skipped check is never green. Kill any daemon or fake you started, by its PID.
5. Failures outside your diff (someone else's WIP): report them, do not fix or hide them.

## Output

```
{ testsAdded[], criteriaCoverage[{criterion, tests[]}], gate, checkPassed: bool, testCount: int, gaps[], issues[] }
```

## Sensors

Gate exit 0; every criterion has a test; `gaps` empty.

## Gates

To polish: always.
