# 0023. Fake tool binaries may wait in real time

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09)

## Context

`ARCH-test-sleep` (fnd §3, §5) forbids real-time waits in test code, outside the bounded poll
helper of `mayfly-testkit-shared`, and treats every file of a test-only crate as test code. The
fake tool binaries (`mayfly-fake-tart`, later `mayfly-fake-github`) are test-only crates, but
real time is part of what they fake: a fault's `delay_ms`, an `exec` job that runs for a while,
`tart ip --wait`, the grace period of `tart stop`. That waiting is the product behaviour of the
binary, not a test synchronising on something. The fakes may not use the poll helper either:
D11 and `ARCH-independent-oracle` forbid any edge from an independent oracle to a workspace
crate. The rule has therefore no compliant way to express a delay, and the way around it (a timer
the rule cannot see, such as a `recv_timeout` method call) would hide a real-time wait from the
very rule that exists to find them.

## Decision

A test-only crate's production code may wait in real time in a file that an
`[[exceptions.lints]]` entry with `lint = "test_sleep"` approves, citing an ADR (this one). Each
fake keeps its waiting in one small file (`crates/mayfly-fake-tart/src/wait.rs`), uses a plain
`std::thread::sleep` there, and names this ADR in a comment. The exception is honoured only in
production code of a crate listed under `test_only`: a `tests/` file, a test module or any other
crate still fails `ARCH-test-sleep`, and an entry whose file does not sleep fails
`ARCH-lint-suppression` ("matches no suppression"). Tests of the fakes still never sleep: they
wait on the commands under test (`ip --wait`, a blocking read of a hang's stderr line).

## Consequences

Easier: a fake can model latency honestly and the rule stays an absolute for tests. Harder: each
new fake binary that needs time adds one exception entry, visible in review. Enforced by
`xtask` (`rules_hygiene`, fixture `test-sleep-exception-in-test-file`, unit tests
`should_report_sleep_when_exception_is_missing_or_not_for_that_file` and
`should_report_unused_exception_when_the_excepted_file_does_not_sleep`). Implemented first by
`mayfly-fake-tart`. Accepted by the owner 2026-11-09.
