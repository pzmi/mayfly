---
name: review
description: Read-only review of a git range (default since the newest checkpoint-* tag) against mayfly's enforced architecture and control-plane/data-plane split, the cited plan/foundation/verification sections and ADRs, event sourcing (single write path, outbox/inbox atomicity, goldens, determinism), effects and Unknown outcomes, fencing and epochs, VM ownership and delete safety, secret handling, Tart argv and process groups, the scale-set listener, model-first Quint conformance, contract suites, fault-lab independence, test quality and supply chain. Produces findings by severity with file:line and a verdict. Use at review checkpoints (⛳ tasks in doc/tasks.md) or when asked to /review.
---

# /review

Review a range of commits (or an uncommitted diff against `HEAD`) and report. **This skill never
edits a file, never commits, never tags, never pushes and never merges.** Fixes are the
implementer's job, after the report. A subagent that cannot load it by name reads this file and
follows it.

## Input

`/review [range]` where `range` is any git range (`a..b`, a single commit, `HEAD~N`), or `HEAD`
plus "uncommitted" for the working tree.

Default range: `<newest checkpoint tag>..HEAD`, found with
`git tag --list 'checkpoint-*' --sort=-creatordate | head -1`. If there is no such tag, use the
root commit to `HEAD` and say so. If the range is empty, report a clean verdict for an empty
range and stop.

## Procedure

1. `git log --oneline <range>` and `git diff --stat <range>`. Note the task ids in the subjects
   (`<ID>: summary`).
2. For each task id, read its section in `doc/tasks.md`: the sections of `doc/plan.md`,
   `doc/foundation.md` (fnd) and `doc/verification.md` (ver) it cites, and its `Accept:` clauses.
   Read those sections, and the ADRs in `doc/adr/` that touch the changed crates. Decisions in
   `doc/tasks.md` and later ADRs override earlier plan text.
3. Read every changed file in full (not just the hunks), plus the tests beside it.
4. Run the live gate (`cargo xtask check`, or the fallback in `CLAUDE.md` before FND-02) in the
   foreground and record pass/fail and the test count. If it rewrites tracked files, report
   which as a finding; do not revert or keep them yourself.
5. Walk the checklist below. Only report what you verified in the code; cite `file:line`.

## Checklist

**Enforced architecture** (`architecture.toml`, `cargo xtask architecture`; fnd §2, §3; plan §4):
cli → composition → adapters → application → pure crates; adapters never reference sibling
adapters or composition; production crates never depend on `mayfly-testkit`,
`mayfly-conformance` or the fakes; a new crate has its `members` entry, `architecture.toml`
entry and external allowlist in the same commit, and each new rule has a negative fixture. A
change that loosens `architecture.toml`, `[workspace.lints]`, `deny.toml` or a test without an
ADR is HIGH.

**Control plane / data plane** (plan §3, D1, D6): no `mayfly-control*` crate or CP adapter
reaches a `mayfly-data*` crate or DP adapter, or the reverse; they meet only in `mayfly-domain`
and `mayfly-protocol`, and only composition sees both. The data plane holds no durable state,
names no journal type and rebuilds from inventory on start; the control plane names no process
API. Planes exchange only `Directive`/`Observation`; even `inproc` encodes and decodes every
message (D9). A direct call across the planes, or a shared mutable structure, is HIGH.

**Purity and determinism** (plan §5.1): pure crates use no `std::{fs,net,process,env}`, clock,
randomness, Tokio, HTTP or SQL; time and ids arrive in the command or through `Clock`/`Ids`
ports; `decide`/`evolve` have no logging; `DETERMINISM_CASES` cover every aggregate with
accepted **and** refused commands; State × Stimulus matrices list every pair with no wildcard
arm (plan §6 rule 6).

**Data-oriented DDD** (ADR 0022): flag getters/setters, private fields or object ceremony on domain state, commands or events that add no safety; flag names that drift from `doc/domain-model.md` §5; flag a deviation from any Accepted or Proposed ADR without a new or superseding ADR.

**Event sourcing** (plan §5, fnd §6): `AggregateStore::save` is the only caller of
`Journal::append`, pinned by an architecture rule; one transaction holds expected-revision
checks, events, synchronous projections, outbox rows, the command receipt and the inbox entry
(a partial commit visible to any reader is HIGH); reactors are notified only after commit,
idempotent and never replayed; `with_version_retry` retries only `WrongExpectedVersion`. Events
are past tense, `EVENT_TYPE` + `SCHEMA_VERSION`, registered, with a golden fixture per version;
a renamed field, an edited golden or an edited released migration is HIGH; an unknown event type
puts the daemon in safe mode. Commands never read projections or return read DTOs; queries take
no write port. A projection's meaning change is a new spec version, and rebuild equals live.

**Effects and `Unknown` outcomes** (plan §6, G5): a state reflects an observed fact, never an
attempted command (intent persisted, directive sent, transition on observation). `Unknown`
leaves only on an observation; a timeout that becomes success, `Deleted` or a slot release is
HIGH. A retry reuses the same VM name and generation (I13); a new name on a lost reply is HIGH.
`VmError` keeps `Definite` and `Ambiguous` apart, and an ambiguous result is re-inspected.
`Busy` never returns to `Warm`; `Finished` never becomes `Activating` (I06).

**Fencing and epochs** (plan §3.2, I09): every directive carries the host epoch; the data plane
refuses an older epoch before any effect; directives are idempotent by `directive_id` (a repeat
returns the earlier outcome); observations are deduplicated by `observation_id`, and a stale
generation is ignored and recorded, never applied.

**Ownership and delete safety** (plan §8.4, §10, G4): `DeleteVm` requires an `ownership_proof`
(journal record, exact `OwnedVmName`, generation) matched against the fence; the control plane
never emits one without a record. No delete by name prefix, of a base image or of a foreign VM;
an unknown `mf-*` VM is reported, never deleted (I04); `OwnedVmName` parsing is strict and a
foreign name never parses as owned. Safe mode admits nothing and deletes nothing. Any path to a
delete without proof is HIGH.

**Capacity** (plan §9, G3): every attempt in `Reserved..Deleting` counts against capacity
(I03); admitted never exceeds slots, the macOS-guest limit (two), memory or disk reserve;
`reported_max` never exceeds what can be delivered (I07); drain admits nothing.

**Scale-set listener** (plan §7.2): inbox record, statistics and desired count commit in one
transaction **before** `DeleteMessage` (ack after commit); a redelivered message is a no-op;
`AcquireJobs` is idempotent per request id; demand is
`min(C, TotalAssignedJobs + warm_registered)`, never counting messages or adding
`TotalRunningJobs`; the poller never waits on VM work; 401/403/429 and Retry-After honoured.

**Outbox and relay** (plan §5.4): the outbox stores a `SecretHandle`, never a JIT config;
resends reuse the `directive_id`; backoff jitter comes through a port; a timeout records
`Unknown`.

**Secrets** (plan §13, D13): `Secret<T>` with redacting `Debug`/`Display` and no `Serialize`;
no secret in the journal, outbox, logs, metrics, status, diagnostics, argv, a child's env, a
panic message or a test fixture; the JIT config reaches the launcher over stdin only, and a
lost secret retires the attempt. A redaction test covers each new log or output path. Any leak
is HIGH.

**Tart and processes** (plan §8.2, §8.3, D12): argv only, never `sh -c`, never interpolation;
every call has a deadline, its own process group (killed as a group), bounded captured output,
redaction and an env allowlist; `tart list --format json` parsed against version-pinned
fixtures. SSH uses a pinned host key, never `StrictHostKeyChecking no`. `std::process` outside
`mayfly-adapter-tart`, `mayfly-adapter-guest`, composition and test tools is HIGH.

**Concurrency and lifecycle** (plan §10, RT-02): every spawned task owned by the supervision
tree, cancellable and joined or bounded on shutdown; no blocking I/O on the runtime outside
`spawn_blocking` (the SQLite writer thread is the one owner of the connection); no lock held
across `.await`; bounded channels; startup follows plan §10's order; SIGTERM drains safely; the
state dir is `flock`ed, `0700`, the socket and DB `0600`.

**Formal conformance** (ver §6, D10): a lifecycle, scheduling, recovery, scale-set or link
change updates the Quint model, named scenarios and the conformance driver in the same change
(model first) and registers new models in `MODELS`; the driver drives real code and compares
after every step; a negative control exists and still fails; no invariant removed or weakened
(HIGH).

**Contract suites** (ver §5): a port with a fake and a real adapter (journal, link, VmProvider,
ScaleSetApi) runs one shared suite against both, in every mode the task names (fresh, reopened,
crash-injected); the suite's own tests prove a deliberately broken implementation fails it. A
fake that passes by sharing production code is MEDIUM.

**Fault lab and fakes** (ver §8, D11): `tools/faultlab`, `mayfly-fake-tart` and
`mayfly-fake-github` import no production crate (also checked by the architecture rules); the
faultlab reads SQLite with its own SQL and has its own checker; a scenario whose fault never
lands is INCONCLUSIVE, never PASS; the faultlab refuses real Tart without an ownership manifest
and the explicit flag.

**Logging and metrics** (fnd §5, plan §12): `tracing`, fixed `snake_case` event names, data in
fields, no INFO on the healthy path, no secret or id-valued metric label.

**Language rules**: `unsafe_code` forbidden; no `todo!`, `unimplemented!`, `dbg!`; no
`#[ignore]`; no `#[allow]`/`#[expect]` or coverage exclusion without an `architecture.toml`
exception citing an ADR; `#[cfg]` not `cfg!`; no `unwrap`/`expect` on input-derived values in
production paths.

**Plan fidelity and ADRs**: behaviour matches the cited sections and every `Accept:` clause; a
deviation is recorded in `doc/plan.md`/`doc/tasks.md` in the same commit or is a finding. A
decision without an ADR (crate boundary, dependency of consequence, protocol, security or
recovery policy, rule exception) is MEDIUM; code contradicting an Accepted ADR is MEDIUM, HIGH
if it loosens a security or architecture rule. A Proposed decision flipped to Accepted by an
agent is MEDIUM.

**Tests** (ver §2): each `Accept:` clause has a test that would fail without the change, at the
lowest level that can fail for its reason; names `should_<outcome>_when_<condition>` with
`// given / when / then`; no sleeps (fake clock, `tokio::time::pause`; real-OS waits only via
`mayfly-testkit::poll`); tempdirs and port 0; real objects or testkit fakes, no mocking
frameworks; property regressions committed with their seed; no test depends on real Tart, macOS
or the network unless gated for an owner lane. Missing tests for a new branch is MEDIUM.

**Supply chain** (fnd §7): a new dependency added with `cargo add`, justified in the commit
message, in the crate's external allowlist, licence accepted by `deny.toml`, no git or foreign
registry source without an ADR; `Cargo.lock` consistent (`--locked` passes); Actions pinned by
full commit SHA with minimal `permissions`; no `pull_request_target` with checkout of untrusted
code; self-hosted runners only in owner-dispatched workflows.

**Public repository and housekeeping**: no tokens, keys, internal hostnames, personal paths,
model ids or private-repo content in any file; fixtures redacted or synthetic; a doc comment's
behavioural claim names the test that pins it, and that test exists; stale docs are findings; no
unrelated files; commit subject `<ID>: summary`, an `Impact:` line, the trailer block last
(`Signed-off-by`, `Co-Authored-By`, `Claude-Session`).

## Output

Findings only, most severe first. Severity: **HIGH** (wrong behaviour, security, broken
invariant, rule loosened or suppressed), **MEDIUM** (missing test, missing ADR, doc drift,
likely bug), **LOW** (style, naming, nit).

```
## Review <range> (<n> commits, tasks <ids>)
gate: <command> pass|fail (<test count>)

### HIGH
- crates/x/src/file.rs:42: what is wrong; why it matters; suggested fix direction
### MEDIUM
- ...
### LOW
- ...

Verdict: CLEAN | CHANGES REQUESTED (<n> HIGH, <m> MEDIUM)
```

Omit empty severities. A clean range gets an explicit `Verdict: CLEAN` with one line on what was
checked. Any HIGH means CHANGES REQUESTED.

## After the review

A checkpoint task ends with a local annotated tag `checkpoint-<ID>` (e.g. `checkpoint-KER-08`).
The orchestrator creates it, only once the review and the adversarial second review have passed
(clean, or every HIGH and MEDIUM fixed), and never pushes it. This skill does not tag.
