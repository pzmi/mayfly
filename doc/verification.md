# mayfly: verification strategy

**Status:** plan, 2026-11-09. Owner: Patryk Żmigrodzki.

This document owns *how mayfly is shown to be right*: the test levels, determinism, golden
fixtures, contract suites, the Quint models and their conformance replay, the fakes, the fault
lab, the curated mutants, the lanes that run each of them, and what counts as evidence.
[`plan.md`](plan.md) owns behaviour and the invariants' meaning, [`foundation.md`](foundation.md)
owns the rules that enforce structure, and [`tasks.md`](tasks.md) owns order. A citation "ver §6"
points here.

---

## 1. Principles

1. **Independent oracles.** Something that checks production must not share production's
   assumptions. The Quint models are written before the code they govern (D10); the fault lab,
   `mayfly-fake-tart` and `mayfly-fake-github` share no code with production (D11, fnd §2.6);
   golden fixtures are files, not round trips. An encoder and decoder that agree by construction
   prove nothing about the wire.
2. **Fail closed.** A missing tool, an empty selection, a trace that does not decode, a fault
   that never landed, or a harness that crashed is never a pass. Every check has a negative
   control showing it can fail.
3. **Four verdicts.** Every evidence-producing run ends in exactly one of `PASS`, `FAIL`,
   `INCONCLUSIVE` (the run did not reach what it set out to test) or `HARNESS_ERROR` (the
   instrument broke). Only `PASS` is success. Ordinary tests collapse to pass/fail because
   nextest and the gate already fail closed; the fault lab, conformance generation and Apalache
   keep all four.
4. **Lowest level that can fail.** A test lives at the lowest level that can fail for the reason
   it exists (§2). A rule about `decide` is a unit test, not an e2e test; a SQLite constraint is
   an integration test, not a component test over the memory journal. A test moves down only
   when the lower level asserts the same thing, and a removed test names its replacement.
5. **Races are found, never retried.** nextest runs with zero retries. A failure that passes on
   a rerun is a race: repeat it, read its trace, name the race, then add a test that forces the
   interleaving every time (barriers, scripted fakes, fake clock), never one that relies on load.
6. **Every invariant names its pins.** Each of I01–I33 lists the model, tests, fault scenarios
   and mutants that would fail if it broke (§11.2). A claim without a pin is a hope.

---

## 2. Test levels

| Level | Location | May use | Budget | Gate | A test lives here when |
|:--|:--|:--|:--|:--|:--|
| **Unit** | `#[cfg(test)] mod tests` beside the code; exhaustive matrices in `crates/<c>/tests/` | the crate under test, testkit values | whole suite < 60 s | `unit-tests` | it decides a pure rule: `decide`, `evolve`, an FSM `step`, capacity policy, demand, recovery table, name parsing |
| **Golden** | `crates/<c>/tests/fixtures/…` + harness in testkit (§4) | files, the codec | < 10 s | `unit-tests` | a byte-level shape is a promise: events, protocol, CLI JSON, config, Tart JSON, metrics |
| **Property** | `proptest!` in `src/` or `crates/<c>/tests/properties.rs` | generated inputs, persisted regressions | 1024 cases per property in `check` | `unit-tests` / `integration-tests` | a law holds for all inputs: round trips, "admitted ≤ every limit", replay determinism |
| **Contract** | suites in `mayfly-testkit::contracts`, run from each implementation's crate (§5) | the port, a factory | < 2 min total | `integration-tests` | two or more implementations of one port must agree |
| **Component** | `crates/mayfly-control/tests/`, `crates/mayfly-data/tests/` | real application code, testkit fakes, `MemoryJournal`, fake clock, paused Tokio time | < 2 min | `integration-tests` | a use case, saga, reconciler or listener behaves across several steps with no OS resource |
| **Integration** | `crates/mayfly-adapter-*/tests/`, `crates/mayfly-composition/tests/` | real SQLite files, real processes (fake-tart, fake-github), tempdirs, port 0, `poll` | < 5 min | `integration-tests` | the reason is an OS or library fact: SQL, fsync, process groups, signals, sockets, JSON from a binary |
| **E2E** | `crates/mayfly-cli/tests/e2e_*.rs` | built `mayflyd` and `mayfly`, fake-tart, fake-github, `assert_cmd` | < 5 min | `e2e-tests` | the promise is a process boundary: CLI, exit codes, startup, restart, the walking skeleton |
| **Conformance** | `formal/` + `crates/mayfly-conformance` (§6) | Quint (pinned), ITF traces, real code behind drivers | < 10 min per push | `formal-*`, `conformance-*` | a model step must equal what production does |
| **Formal verify** | `formal/` via `cargo xtask formal-verify` (§6.3) | Apalache (pinned) | ≤ 60 min | nightly / on model change | an invariant must hold for every behaviour to a bound |
| **Fault lab** | `tools/faultlab` (§8) | the binaries, fake-tart, fake-github, signals | ≤ 5 min per scenario | weekly, release | a claim is about what clients observed under crashes and lost replies |
| **Architecture** | `tools/xtask` + fixtures (fnd §3) | `cargo metadata`, `syn` | < 60 s | `architecture` | the rule is structural: edges, purity, naming, single append path |
| **Mutation / fuzz** | `cargo xtask mutants-curated`, `mutation`, `fuzz` (§9, §10) | source patches, `cargo-mutants`, `cargo-fuzz` on the pinned nightly | per lane | nightly, weekly | the question is whether the tests can fail, or a parser survives hostile input |
| **Hardware** | `hardware.yml`, gated test binaries (§5, §10) | real Tart in a dedicated `TART_HOME`, a disposable GitHub App and repository | owner's time | owner-gated (D14) | only real Virtualization.framework or the live scale-set service can answer |

Rules across levels:

1. **Naming and shape.** `should_<outcome>_when_<condition>` with `// given`, `// when`,
   `// then` in order (fnd §3.3).
2. **No mocks.** Real objects first, testkit fakes second; no mocking framework, no patching of
   production functions. A fake is never looser than production (§5).
3. **Isolation.** Tempdirs per test, port 0, a fresh SQLite file per test, a fresh fake-tart state
   directory per test. No test reads another's state or a fixed path.
4. **nextest.** Profiles `default`, `ci` (no fail-fast, JUnit) and `serial`; `retries = 0` in
   every profile; slow-timeout 30 s, terminated after four periods. Tests that start the daemon or
   fake binaries share a test group bounded to the lane's parallelism.
5. **Nothing skips.** A test that cannot run its environment fails. The real-Tart and live-GitHub
   tests live in their own test binaries selected only by the hardware lane; run without their
   environment they fail, never pass empty.

---

## 3. Determinism

1. **`DETERMINISM_CASES`.** Each aggregate module of `mayfly-control-domain` exports a list of
   cases `{ name, history: Vec<Event>, command, expect: Accepted | Refused }`. The testkit
   harness folds `history` twice, calls `decide` once from each state and requires the two
   results to agree, refusals included, and the declared outcome. Agreement is checked by
   `PartialEq`, by the `Debug` form of states and results, by each event's encoded bytes and by
   the refusal's `Rejection::code()`, so a hand-written `PartialEq` cannot hide a difference.
   Every module contributes at least one accepted and one refused case, at least one case with a
   non-empty history, and unique names; a refusal's code must be in `Rejection::CODES`, and every
   code listed there must be returned by some refused case. The set of modules with cases must
   equal the set of `Aggregate` impls: a registered aggregate without cases does not compile
   (`AggregateVisitor::visit` requires `DeterminismCases`), and an `impl Aggregate` left out of
   `ControlAggregates` is refused by the architecture rule `ARCH-aggregate-registered`
   (fnd §3.3), not by the harness, which only sees registered aggregates. Each
   aggregate's codes and case outcomes are pinned in a decisions golden (§4), which catches an
   outcome that changes between builds or processes. The data-plane FSMs carry the same harness
   over `step`.
2. **Fake clock.** Time reaches application code only through the `Clock` port (`now`,
   `sleep_until`, `timeout`). `FakeClock` advances only when the test says so; component tests
   that exercise Tokio timers use `tokio::time::pause` and `advance`. Deadlines, backoff,
   Retry-After, session expiry and lease ages are all tested on fake time.
3. **Seeded randomness.** Jitter and id generation come through the `Rng`/`Ids` ports.
   `SeededRng` takes an explicit seed; every randomised test, simulation (CP-03's 1,000-step run)
   and property prints its seed on failure, and setting the same seed reproduces the run.
   `SequentialIds` gives readable deterministic ids.
4. **No sleeps.** No test sleeps (`ARCH-test-sleep`). Synchronisation is by channels, barriers,
   scripted fakes and fake time.
5. **The poll helper.** `mayfly_testkit_shared::poll::wait_until(deadline, reason, condition)` is the
   only sanctioned wait, for real-OS conditions in integration and e2e tests (a process exited, a
   socket appeared, fake-tart state changed). It is bounded, carries a reason shown on timeout,
   and is never a correctness oracle: what is asserted is the state reached, not the time taken.
   It lives in the shared test kit `mayfly-testkit-shared`, which depends on no workspace crate,
   so the control plane's and the data plane's tests both use it (ADR 0019); `try_wait_until`
   returns the timeout for a caller that must clean up first.
6. **Property seeds.** Failing proptest seeds are persisted under `crates/<c>/proptest-regressions/`
   and committed; case counts come from the gate stage (1024), never from code.
7. **Race tests** are bounded by iteration count, never by time, and include a negative control
   showing the naive implementation really races.

---

## 4. Golden fixtures

A golden fixture is a committed file that production must produce or accept byte for byte (after
declared normalisation). Each family has one harness in testkit, which fails on a missing fixture
and on an orphan fixture.

| Family | Location | Harness rule | Task |
|:--|:--|:--|:--|
| Events | `crates/mayfly-control-domain/tests/fixtures/events/<type>/v<n>.json`, plus any `v<n>.<slug>.json` (`<slug>` is `[a-z0-9][a-z0-9-]*`) | every registered `(EVENT_TYPE, SCHEMA_VERSION)` has its primary sample `v<n>.json`; every sample decodes, re-encodes byte-identically and folds through `evolve` from `initial()` without a panic; together the samples show every field and enum variant of the payload's traced shape (an optional field or variant no sample shows fails, and so does a part the tracer cannot see, such as an enum coded as a string or integer: ADR 0018); an unregistered file or a name outside the layout fails | KER-03 |
| Event upcasts | `…/events/<type>/v<n>.json` and `v<n>.<slug>.json` of a legacy version, each with its `.expected.json` (`v<n>.expected.json`, `v<n>.<slug>.expected.json`) | each old sample decodes through its upcaster into the current version of its own type and into exactly the expected current encoding; the samples cover the old shape as above; the chain has no gaps, one current version, no legacy version at or above it | KER-03, first bump |
| Decisions | `crates/mayfly-control-domain/tests/fixtures/decisions/<aggregate>.txt` | the aggregate's sorted `Rejection::CODES` and each determinism case's outcome (events as kind and compact payload, or the refusal code) equal the file; a file for no registered aggregate fails; regenerated deliberately from the failure's text | KER-03 |
| Protocol | `crates/mayfly-protocol/tests/fixtures/<message>/v<major>.json` | one per message kind and major version; round trip byte-identical; an unknown major is refused | KER-02 |
| CLI JSON | `crates/mayfly-cli/tests/fixtures/json/<command>.json` | `--json` output equals the fixture after normalising the fields listed in one place (instants, ids) | RT-03 |
| Config | `crates/mayfly-composition/tests/fixtures/config/{valid,invalid}/*.toml` | valid files load; each invalid file fails with the error naming the field recorded beside it | RT-01 |
| Tart JSON | `crates/mayfly-adapter-tart/tests/fixtures/tart/<version>/*.json` | real output captured per supported Tart version parses to the expected inventory | DP-04 |
| GitHub recordings | `crates/mayfly-adapter-github/tests/fixtures/github-scaleset/` | redacted request/response pairs replayed against the client; the redaction check refuses any token-shaped string | GH-03 |
| Metrics, doctor, plist | `crates/mayfly-composition/tests/fixtures/` | metric names and label sets; `doctor --json`; launchd plist | RT-04, OPS-01 |

Rules:

1. **Existing event and protocol fixtures are frozen** (every sample and expectation alike).
   Planned (ES-03R): their hashes listed in `fixtures.lock`, so changing one fails; until then
   the rule is kept by review and by the byte-identical round trip. A shape change adds a new fixture (new version or new
   type) beside the old; the old one stays for ever, because the journal is backward-transitive
   (fnd §6).
2. Other families may be regenerated, but only deliberately: the commit says which fixture
   changed and why. Never to get green.
3. Fixtures are synthetic or redacted: no real token, hostname, repository or personal path.
4. Golden files seed the fuzz corpora (VER-06).

---

## 5. Contract suites

A port with more than one implementation has **one suite**, written once in
`mayfly-testkit::contracts` as functions over a factory, and run from each implementation's
crate. Each suite has a `check_*` form returning a report of failed cases, used by its own tests
to show a **deliberately broken implementation fails** named cases. A fake that passes less than
production is a bug in the fake.

| Port | Implementations run | Broken implementations that must fail | Core cases | Task |
|:--|:--|:--|:--|:--|
| `Journal` | the modes of event-store §7.1: memory; SQLite fresh, reopened, two-connection, two-handle, crash-injected; PostgreSQL in Docker fresh, two-pool, crash-injected (ADR 0035) | event-store §7.6: `LooseRevisionJournal`, `PartialCommitJournal`, `ForgetfulReceiptsJournal`, `IntentBlindReceipts`, `ForgetfulInbox`, `CachedHeadJournal`, `GlobalLockJournal`, `LateIdCheckJournal`, `TornUnitJournal` | JC-01..JC-32 (event-store §7.1; JC-26, JC-27 adapters only): expected-revision conflict without mutation; duplicate event id; all-or-nothing unit; receipt replay; refusal receipts bound to their revision (E3); intent reuse refused; inbox dedupe; racing writers on one stream (exactly one wins); independent streams; read sets; epochs; per-stream reads, the event-id registry and the kind manifest (no global order); nothing partial visible after a crash or to a concurrent reader; units sharing a command id, inbox key or event id on different streams (exactly one commits); `OutcomeUnknown` only at commit, resolved by re-appending the identical unit | ES-01, ES-01F, ES-02, ES-09 |
| Projections | memory, SQLite, PostgreSQL | `ClockReadingProjection`, `OrderDependentProjection` | PC-01..PC-09: rebuild by stream enumeration equals live (I16); version bump (either direction) rebuilds; inline rows commit with their events; stream locality; online rebuild with dual writes and the cutover fence (E9) | ES-04, ES-12 |
| `DeliveryLedger` | memory, SQLite (all modes), PostgreSQL | `UnfencedLedger`, `ClearAllMarksLedger`, `HeadTrustingWorker`, `SkippingLedger` | DC-01..DC-25: marks per registered consumer, revision-ordered delivery, fenced leases, no skip, redelivery after a crash | ES-05, ES-09 |
| `SnapshotStore` | memory, SQLite, PostgreSQL | `BlindSnapshotUpsert`, `ResealingSnapshots` | SC-01..SC-11: snapshot plus tail equals whole fold; stale, corrupt or foreign snapshots ignored | ES-08, ES-09 |
| Link (`DirectiveSink`/`ObservationSink` pairs) | `inproc`; `uds` | `SilentDropLink` (loses a message and reports success), `NoVersionCheckLink` | handshake; unsupported major refused; at-least-once; no ordering assumed; bounded backpressure; close semantics; every message survives a serde round trip | DP-02, LNK-01 |
| `VmProvider` | `FakeVmProvider` (data-plane kit); Tart adapter over `mayfly-fake-tart`; Tart adapter over real Tart (hardware lane) | `PrefixDeleteProvider` (deletes by prefix), `AmbiguousAsDefiniteProvider` (timeout reported as definite), `MacBlindProvider` (deletes without re-reading the instance id), `EmptyInventoryProvider` (reads a failed listing as empty) | clone/start/stop/delete idempotent by name; clone sets a random MAC and reports it; delete of a base image, a foreign name or a mismatched instance id refused; hang → deadline → `Ambiguous`, then re-inspect; inventory reports exactly what exists or `Incomplete`; start goes through the shim and a daemon restart re-adopts it; stop only by `tart stop` | DP-01, DP-04, HW-01 |
| `ImageStore` | fake; Tart adapter over fake-tart; real Tart (hardware lane) | `TagPullStore` (pulls by tag), `BaseEvictingStore` (evicts a referenced base) | pull by digest only, re-hash; materialise a base named by digest and runner version; evict only unreferenced bases; base set listed | DP-07 |
| `GuestTransport` | fake; `tart exec` over fake-tart; `tart exec` on real Tart (hardware lane) | `ArgvSecretTransport` (passes the JIT config in argv), `UncheckedLockTransport` (skips the lock-PID check) | nonce verification; launcher receives the secret on stdin only (`tart exec -i`), after the lock-PID = shim-child and instance-id checks; bounded, redacted diagnostics; no SSH (ADR 0038) | DP-05 |
| `ScaleSetApi` | `FakeScaleSetApi` (testkit); GitHub adapter against `mayfly-fake-github`; the recorded-fixture replayer; live (hardware lane) | `NoCursorApi` (omits `lastMessageId`), `RetryAfterIgnoringApi` | session open/refresh/expiry; redelivery until delete; 50-message cap; idempotent acquire; 401/403/429 + Retry-After; ack after a lost response | GH-03, HW-02 |
| `RunnerRegistry` | fake; GitHub adapter against fake-github; live | `NameGuessingRegistry` (deletes by name pattern) | list by runner name, get and delete by recorded id only; 204/404/422 classified; no classic registration token (ADR 0028) | CP-05, GH-03 |
| `SecretProvider` | memory; `file:`; `env:`; `exec:` (over a test script); `keychain:` (macOS, hardware lane) | `PermissiveFileProvider` (accepts `0644` or another owner), `ShellExecProvider` (runs through a shell) | resolves a reference URI to `Secret<T>`; refuses wrong permissions, symlinks, relative paths, unset variables, non-zero exits and timeouts; never returns a secret through `Debug`; re-resolves on refresh (ADR 0033) | SEC-02 |
| `HostProbe` | fake; Linux; macOS (CI `macos` job) | `CachedProbe` (returns a stale value) | disk and memory reported for the provider's home; low-water mark visible; feeds the data plane's resource ledger (ADR 0024) | DP-06 |

---

## 6. Formal models and conformance

Models live in `formal/`, one file per model, each with one `allInvariants` value combining every
invariant, a `step` relation for the model's full contract, an `implStep` relation restricted to
what the composition can actually do, and a biased relation for trace generation. A `scenarios`
module per model holds named runs reviewed by the owner. The `MODELS` registry in xtask lists
every model and its relations; every stage reads invariants from `allInvariants`, so a new
invariant cannot be left out. Models come first (D10); never remove an invariant to get green.

### 6.1 Models and invariants

Invariant ids (plan §1, §6, §8–§10):

| Id | Invariant |
|:--|:--|
| I01 | An attempt owns exactly one `(host, slot, generation)` and a unique VM name. |
| I02 | No two live attempts own the same slot generation. |
| I03 | Starts never exceed capacity; reservations count every attempt in provisioning, running and deleting. |
| I04 | An unrecognised VM is never deleted. |
| I05 | A base image is never deletable. |
| I06 | A completed attempt never serves a new job. |
| I07 | Never advertise more capacity than can be delivered. |
| I08 | Nothing is launched after drain. |
| I09 | A stale epoch cannot mutate a newer attempt. |
| I10 | An ambiguous effect stays `Unknown` until observed. |
| I11 | Every managed VM is eventually adopted, drained, quarantined or deleted (liveness, bounded). |
| I12 | A durable record precedes the GitHub ack. |
| I13 | No unbounded duplicate clones. |
| I14 | One VM serves at most one job and one image generation. |
| I15 | The guest cannot reach host state or keys. |
| I16 | Journal replay gives identical projections. |
| I17 | Run epochs strictly increase across restarts, DP reconnects and journal restores; the external floor is written before an epoch is used; a journal below the floor puts the host in safe mode (ADR 0026). |
| I18 | An aggregate refuses a command whose `run_epoch` is lower than the highest it has accepted. |
| I19 | A registration is deregistered only by a journaled `runner_id`; a runner name is issued at most once, ever. |
| I20 | Every issued registration reaches a terminal state or is on the reaper's list (bounded liveness). |
| I21 | A registered-idle runner is deregistered before its VM is stopped; a busy refusal (422) cancels that retirement. |
| I22 | A JIT config is minted only after an activation grant and a guest verification of this attempt in the current epoch, delivered at most once; a delivery `Unknown` or orphaned handle means presumed leaked and the attempt never serves a job. |
| I23 | Acquisition stops when assigned ≥ deliverable. |
| I24 | Σ `reported_max` over the scale sets a host serves ≤ that host's deliverable (refines I07). |
| I25 | An incomplete inventory is never read as the absence of any VM. |
| I26 | No activation grant while host or pool terms are closed (refines I08). |
| I27 | Stage failures per (pool, digest) are bounded per window; at N the breaker trips until an operator resets it. |
| I28 | A quarantined VM is never deleted automatically; its lease keeps counting until an operator resolves the case; a pool whose capacity is consumed admits nothing (ADR 0025). |
| I29 | In safe mode no clone, start, stop, delete, pull or evict executes, and no secret is minted or taken. |
| I30 | Guest-originated data never decides ownership or a job's result and never reaches a VM name, path, argv or shell. |
| I31 | A recovery plan applies only against the journal heads, inventory digest and epoch it was drawn from; each action is intent-then-outcome. |
| I32 | A pool serves one trust tier; an offer is acquired only by a pool whose tier admits it (v0.x: trusted only). |
| I33 | The data plane never executes a create, start, pull or materialise directive that does not fit its own resource ledger (ADR 0024). |

Definitions and the aggregate that protects each: `doc/domain-model.md` §6.0.1. I10 is strict:
`Unknown` leaves only on an observation, never on a timeout or an operator assertion (ADR 0025).
I14 reads "image revision" for "image generation".

| Model | File | Governs | Invariants | Model-local invariants | Task |
|:--|:--|:--|:--|:--|:--|
| lifecycle | `formal/lifecycle.qnt` | `Attempt` aggregate and saga (plan §6, dm §6.4) | I06, I10, I13, I14, I22 (attempt half), I30 | `busyNeverWarm`, `quarantineTerminal`, `quarantineNeverAutoDeleted`, `retryKeepsNameAndGeneration` | KER-04 |
| scheduling | `formal/scheduling.qnt` | `Host`/`Pool` aggregates, capacity policy, the data plane's resource ledger (plan §9) | I01, I02, I03, I07, I08, I13, I24, I26, I27, I28, I33 | `generationsMonotonic`, `macosGuestsAtMostTwo`, `externalConsumptionShrinksAdmissible`, `noGrantWhileClosed`, `quarantineHoldsCapacity`, `allocationWithinReport` | KER-05 |
| recovery | `formal/recovery.qnt` | startup use case and decision table (plan §10), crash at every await | I04, I05, I09, I10, I11, I12, I17, I25, I29, I31 | `safeModeNoDestructiveAction`, `deleteNeedsProof`, `noAssertion` | KER-07 |
| scaleset | `formal/scaleset.qnt` | `ScaleSet` aggregate, listener, demand (plan §7.2) | I07, I12, I13, I23, I32 | `noDoubleReservationPerRequest`, `desiredFromLatestStatistics` | KER-06 |
| link | `formal/link.qnt` | CP↔DP protocol: at-least-once, duplicates, reordering, fencing, held kinds in safe mode (plan §3.2) | I09, I18, I29 | `executesAtMostOncePerEpoch`, `refusedMeansNothingHappened` | KER-02 |
| registration | `formal/registration.qnt` | `RunnerRegistration` aggregate, reaper, deregister-before-stop, 422 race, issue unknown | I19, I20, I21, I22 (registration half) | `noRemintAfterUnknown`, `deregisterBeforeStop` | KER-09 |
| event_store | `formal/event_store.qnt` | the journal, receipts, inbox, delivery, snapshots, online rebuild (event-store §7.3) | I16 via ES-I11; ES-I1..ES-I8, ES-I12, ES-I14..ES-I16 | the ES invariants | ES-06 |

I15 is not model-checkable: it is pinned by security tests (SEC-01: no host path, socket or key
reachable from the guest; secrets only over stdin) and the redaction tests. I16 is pinned by the
projection contract and a property over random histories (ES-04). I11 is liveness: the models
state it as **bounded** liveness (after the fault phase ends, every managed VM reaches a terminal
classification within *k* steps, encoded as a safety property over a step counter), and the fault
lab checks it after the healthy tail (§8.5).

### 6.2 Trace replay

1. **Traces.** ITF traces come from two sources: committed **regression traces**
   (`formal/traces/regression/*.itf.json`, one per named scenario, listed in `manifest.toml` with
   model, main module, relation, `max_steps` and seed) and **generated traces** from `quint run
   --mbt --step <relation> --invariant allInvariants` (30 traces of 25 steps per model per push;
   more nightly), recorded in a generated manifest and not committed.
2. **Decoding.** `mayfly-conformance::itf` decodes ITF with its own narrow decoder (big integers,
   sets, maps, tuples, variants, options) and reads `mbt::actionTaken` and `mbt::nondetPicks`.
   A trace without them, or with a length other than `max_steps + 1`, is a trace error, not a pass.
3. **Drivers.** One driver per model maps each action and its picks onto a stimulus of the real
   code and exposes `observe()`: a canonical observation built **from what production exposes**,
   never recomputed from the model. Lifecycle drives the `Attempt` aggregate and saga over
   `MemoryJournal`; scheduling the capacity policy and `Host`; recovery the startup use case over
   a real SQLite journal and the fake provider; scaleset the listener over the fake
   `ScaleSetApi`; link the `inproc` and `uds` transports. A driver may read model variables only
   where they are environment choices outside the implementation, declared in `MODEL_READS`;
   reading an undeclared path fails.
4. **Comparison.** At every step, **initial state included**, the projected expected
   observation is compared with the driver's; replay stops at the first divergence and reports
   model, seed, trace id, step, action, picks, expected, actual and a line diff.
5. **Seeds.** One seed per run, always printed; `QUINT_SEED=<seed>` reproduces it. Failed CI runs
   upload the generated traces.
6. **Freshness.** `formal-freshness` regenerates every regression trace from its manifest entry
   and byte-compares it with the committed file; a stale trace fails.
7. **Negative controls.** Each driver has controls that swap one component for a deliberately
   faulty one behind a driver seam (faults live in test code only) and assert the replay fails at
   the **named step, action and field**.
8. **Per-field sensitivity.** For every observed field, a test edits one model variable at one
   state of a passing trace and requires a mismatch exactly there. This catches a constant
   `observe` or a projection that drops a field.
9. **Promoting a counterexample.** Reproduce with the seed; decide which side is wrong (model
   first if the model is wrong); reduce to a named run in the scenarios module; add its manifest
   entry and export the trace; add it to the driver test's named set. A fault-lab `FAIL` follows
   the same path when the model can express it.

### 6.3 Apalache verification lane

`cargo xtask formal-verify` runs `quint verify` with the pinned Apalache (SHA-256 checked) for
every model and relation of `MODELS`, checking every conjunct of `allInvariants` to a bounded
depth per model (start at 4–8 steps, recorded with timings in `doc/formal-models.md`), plus a
long multi-seed simulation (three seeds derived from one printed base). A run counts as a
counterexample only when Quint reports a violation; any other failure (JVM, timeout, tool error)
is `HARNESS_ERROR`, never a pass. A seeded false invariant per model is a negative control that
must be reported as violated. The lane runs nightly and on any change under `formal/` (VER-07).

---

## 7. Fakes

### 7.1 Testkit fakes

| Fake | Port | Notes |
|:--|:--|:--|
| `FakeClock` | `Clock` | starts at a given instant; moves only on `advance` |
| `SeededRng`, `SequentialIds` | `Rng`, `Ids` | explicit seed; readable ids |
| `MemoryJournal` | `Journal` | full transactional semantics; passes the journal contract |
| `FakeVmProvider` | `VmProvider` | scripted faults per call: delay, hang, fail before or after effect, ambiguous |
| `FakeGuestTransport`, `FakeHostProbe` | `GuestTransport`, `HostProbe` | scripted readiness and resources |
| `FakeScaleSetApi`, `FakeRunnerRegistry` | `ScaleSetApi`, `RunnerRegistry` | scripted jobs, statistics, redelivery, 409, 422, 429 |
| `MemorySecretProvider` | `SecretProvider` | holds `Secret<T>` only |
| `inproc` link pair | link ports | the production transport, used directly |

Every fake runs its port's contract suite (§5). A fake grows a capability only together with the
contract case that pins it.

### 7.2 `mayfly-fake-tart`

A `tart`-compatible binary (`--version`, `clone`, `set --random-mac`, `get`, `run` (holding a
lock on the VM's `config.json` while it runs), `ip`, `list --format json`, `stop`, `delete`,
`exec [-i]`, `pull`) over a state directory named by `TART_HOME`. Each invocation reads an optional
**fault file** in that directory: rules matched by command and VM name with an occurrence
selector, each one of `delay_ms`, `hang`, `fail_before_effect`, `fail_after_effect`,
`crash_after_effect` (exit by signal after the state change), or `exit_code`. It records each
invocation (argv, environment variable names, with values only for `TART_HOME` and the fake's own
`MAYFLY_FAKE_*` controls, and the stdin length and digest, never stdin itself) so secret-handling
tests can prove the JIT config never appeared in argv, in the variable names the child received,
or in any state file. An environment value is never logged otherwise: a CI or operator
environment can hold tokens. `exec` emulates a guest runner that "runs a job" for a duration the fault file
chooses. Its output formats follow the real Tart versions pinned in the Tart JSON fixtures.

### 7.3 `mayfly-fake-github`

An HTTP server implementing the scale-set endpoints (scale set CRUD, session create, refresh and
delete, long-poll messages with redelivery until delete, the 50-message cap, statistics, acquire,
JIT config) and the REST runner endpoints (list, get and delete runners; no registration token,
ADR 0028), built from the GH-00 capture. Fault
knobs per endpoint: status override (401, 403, 429 with `Retry-After`, 5xx), dropped response
after the effect, session expiry, latency. A control API under a reserved prefix scripts the job
queue (queue, cancel, reassign) and dumps its state as JSON for the fault lab's observer. Wire
shapes are taken from public documentation and the open-source scale-set client, cited in
comments.

### 7.4 Independence (D11)

The fake binaries and the fault lab import no production crate (`ARCH-independent-oracle`), and no
production crate imports them. Their own test suites pin their behaviour; when a fake and
production disagree, the disagreement is investigated against the real system's documented or
recorded behaviour, and the side that is wrong is fixed. Neither side is edited merely to match
the other.

---

## 8. Fault lab

### 8.1 Claims and non-claims

**Claims.** Under the faults of §8.4, at every observed cut of a run, the safety invariants I01–I10,
I12–I14 and the observable ones of I17–I33 (I17, I18, I19, I21–I25, I27–I29, I32, I33) hold over what the controller observed (fake-tart state, fake-github state, `mayfly status
--json`, the journal read with the lab's own SQL), and after the faults heal, I11 holds within the
scenario's bound: the system converges to a classified inventory without unbounded replacements.

**Non-claims.** The lab does not test real Virtualization.framework, real Tart timing, the live
GitHub service, power loss below SQLite's `fsync` (process kill only, unless a scenario declares a
storage fault), multi-host operation, or guest isolation (I15). It shows the absence of
counterexamples in the runs it made, not a proof.

### 8.2 Harness layout

```text
tools/faultlab/
  src/main.rs       run | suite | check | replay | clean
  src/scenario.rs   scenario schema (validated on load), the scenario table
  src/controller.rs single-threaded loop: schedule, dispatch, nemesis timeline, quiescence
  src/history.rs    write-ahead JSONL recorder, one sequence, fsync per record
  src/workers.rs    subprocess workers driving the `mayfly` CLI and fake-github's control API
  src/nemesis.rs    SIGKILL/SIGSTOP of mayflyd or the data-plane process, fake-tart fault
                    files, fake-github knobs, clock jumps, disk low-water, journal corruption
  src/observe.rs    reads fake-tart state, fake-github dumps, CLI JSON, the journal (own SQL)
  src/checker.rs    temporal safety at every cut; bounded liveness after the healthy tail
  src/verdict.rs    the four verdicts, exit codes, report
  src/guardrails.rs ownership marker, target validation, real-Tart refusal
  scenarios/        one TOML file per scenario
  mutants/          scenario-to-mutant table for negative controls (§9)
tools/faultlab/tests/   self-tests: fixture histories with expected verdicts, a no-fault smoke
```

The controller is outside every fault domain; it never runs in the process it kills.

### 8.3 History schema

JSON Lines, one record per line, `"v": 1`. `seq` is the controller's single counter; `t` is the
controller's monotonic nanoseconds. No correctness rule reads a wall clock or a daemon's clock.

| `kind` | Meaning | Key fields |
|:--|:--|:--|
| `invoke` | the controller is about to dispatch an operation | `op`, `process`, `f` (`queue_job`, `cancel_job`, `reassign_job`, `cli_status`, `cli_drain`, `cli_recover`, …), `input` |
| `ok` | the operation completed with a result | `op`, `output` |
| `fail` | the operation definitely did not take effect | `op`, `error_class` |
| `info` | the outcome is unknown (timeout, worker killed, lost reply) | `op`, `reason` |
| `nemesis` | a fault started or healed | `action`, `target`, `phase` (`start`/`heal`), `hit` |
| `observe` | a cut of external state | `vms`, `jobs`, `runners`, `attempts`, `slots`, `epoch`, digests |
| `restart` | a process under test was (re)started or found dead | `process`, `pid`, `exit` |

Rules:

1. **fsync before dispatch.** Every `invoke`, `nemesis start` and `restart` record is written and
   fsynced before the action it describes is taken, so a controller crash can only truncate the
   history, never lie about it.
2. Every `op` has exactly one `invoke` and at most one completion. A missing completion is `info`
   only if the controller recorded the worker's death; otherwise the run is `HARNESS_ERROR`.
3. The first record is a manifest: commit, scenario, seed, binary digests, fake versions, journal
   schema version.
4. Records carry ids, generations, codes and digests only; never a secret.
5. An `info` retires its worker; a retry is a new `invoke` from a fresh worker.

### 8.4 Scenarios

**Core** scenarios (marked ●) gate VER-04 and every release. "Also" names the lower level that
pins the same reason, so the lab is never the only evidence.

| Id | Scenario | Nemesis | Expected | Invariants | Core | Also |
|:--|:--|:--|:--|:--|:--|:--|
| F01 | Kill after durable `SlotReserved`, before clone | SIGKILL at the observed record | restart resumes `CloneVm` with the same name and generation; one clone | I03, I13 | ● | CP-04 component |
| F02 | Kill after clone succeeds, before confirmation | fake-tart `crash_after_effect` + SIGKILL | inventory shows the owned clone; adopted, not cloned again | I10, I13 | ● | recovery table |
| F03 | Timeout mid-clone | fake-tart `hang` | deadline → `Unknown`; resolved by inventory; no new name | I10, I13 | ● | CP-02 component |
| F04 | Kill while VM starting | SIGKILL during `StartVm` | adopted or stopped and deleted with proof; slot never double-booked | I02, I03 | ● | recovery table |
| F05 | Guest never reachable | fake-tart `exec` hangs | probe deadline → backoff → budget → quarantine or delete; slot held until delete observed | I03, I11 | ● | lifecycle conformance |
| F06 | JIT accepted, response dropped | fake-github drop-after-effect on JIT | attempt retired; new attempt with new generation; stale runner removed by recorded id | I06, I14 | ● | GH-03 contract |
| F07 | JIT registration issued, reply lost | fake-github drop-after-effect on JIT generation | `RegistrationIssueUnknown`; the reaper finds it by name and deregisters it by id; the attempt retired, never relaunched; no second runner in use | I10, I19, I20 | ● | CP-05 component |
| F08 | Disconnect during poll | fake-github drops the long-poll | session resumes; no message lost; desired from latest statistics | I12 | ● | GH-04 component |
| F09 | `DeleteMessage` response dropped after ack | drop-after-effect on delete | ack retried idempotently; redelivery deduped; no double reservation | I12, I13 | ● | GH-04 component |
| F10 | Duplicate and out-of-order lifecycle messages | fake-github redelivers and reorders | inbox dedupes; stale ignored; no double provision | I13, I14 | ● | GH-04 component |
| F11 | 100 queued jobs, 2 slots, > 50 messages | none (load) | never more than 2 VMs; all jobs complete; demand never from message count | I03, I07 | ● | E2E-02 |
| F12 | Listening runner, then cancel or reassign | control API cancel/reassign | runner deregistered then retired, or serves the reassigned job; never two jobs | I06, I14, I21 | ● | E2E-02 |
| F13 | SIGKILL daemon while a job runs | SIGKILL during `Busy` | restart adopts the busy VM; **never deletes it**; job completes; then cleanup | I04, I11 | ● | recovery table |
| F14 | Guest dies during job | fake-tart kills the guest | exit observed; attempt finished; VM deleted; slot released; never reused | I06, I14 | ● | lifecycle conformance |
| F15 | Delete finishes, reply missed | `crash_after_effect` on delete | `Unknown` until inventory shows absence; slot released once | I10 | ● | lifecycle conformance |
| F16 | Delete fails because VM still running | fake-tart `fail_before_effect` on delete | definite refusal → stop, then delete; never by prefix | I04, I10 | ● | VmProvider contract |
| F17 | Disk low-water mark during provisioning | fake host probe below reserve | admission stops; `reported_max` drops; in-flight attempts finish or back off | I03, I07 | | KER-05 property |
| F18 | Resume after sleep; session expired | clock jump + fake-github expiry | re-authenticate, reopen session; no stale epoch acts | I09, I12 | | GH-04 component |
| F19 | Two daemons start concurrently | two `mayflyd` on one state dir | one wins the lock; the other exits non-zero with no journal write | I02 | ● | RT-01 integration |
| F20 | Stale effect from epoch N after N+1 | delay a directive across a DP reconnect | data plane refuses epoch N; its late observation ignored | I09 | ● | KER-08 unit, link conformance |
| F21 | Corrupt projection only | overwrite projection rows | detected at start; rebuilt from the journal; identical to live | I16 | | ES-04 contract |
| F22 | Corrupt or truncated journal | truncate or alter the journal file | safe mode: no new VM, no destructive action; `recover --dry-run` plan | I04, I08 | | CP-04 component |
| F23 | Duplicate guest-exit during drain | duplicate exit observation | one `Finished`; drain completes; nothing launched | I08, I14 | | DP-01 component |
| F24 | Unknown `mf-*` VM present | pre-seed fake-tart with a foreign `mf-*` VM | reported for review; **never deleted** | I04 | ● | KER-07 table |
| F25 | 429 with `Retry-After` | fake-github 429 | no request before the delay; capacity not raised; recovers after | I07 | | GH-04 component |
| F26 | Linux and macOS pools on low memory | fake host probe low memory | admission respects reserve and at most two macOS guests | I03 | | KER-05 property |
| F27 | Timeout just as `AcquireJobs` succeeded | drop-after-effect on acquire | idempotent re-acquire; job served once | I13 | | GH-04 component |
| F28 | Invalid guest bootstrap secret | launcher rejects the JIT config | definite failure; attempt retired; secret absent from all records | I14 (I15 by SEC-01) | | DP-05 integration |
| F29 | Stop while exporting logs | SIGTERM during diagnostics export | shutdown within its deadline; attempt left recoverable; deletion completes after restart | I11 | | RT-02 integration |
| F30 | Provider recovers after all jobs complete | fake-tart faults cleared at the end | converges to `min_prepared` and nothing more | I07, I11 | | CP-03 simulation |
| F31 | Kill the data-plane process alone (UDS split) | SIGKILL `mayflyd --role data` | CP marks in-flight directives `Unknown`, mints epoch N+1 on reconnect, reconciles by inventory | I09, I10 | ● | LNK-01 e2e |
| F32 | Crash after JIT issued, before handle issue | SIGKILL after `RegistrationIssued` | `JitHandleOrphaned` at restart; registration deregistered by id; new attempt, new name | I19, I20, I22 | ● | CP-05 component |
| F33 | Cancel while runner registered-idle; job lands during deregistration | fake-github 422 on delete | retirement withdrawn; job served; no VM stopped before the answer | I06, I21 | ● | KER-09 model |
| F34 | `tart list` unparseable / Tart upgraded mid-run | fake-tart garbage output, version change | `InventoryIncomplete`; no slot released; admission closed until re-probe | I25 | ● | KER-08 unit |
| F35 | Journal restored from an older backup | replace state dir with an older copy | epoch below floor → safe mode; no destructive action | I17, I29 | ● | CP-04 component |
| F36 | Broken image: every guest fails to verify | fake-tart exec hangs for one digest | ≤ N attempts per window; breaker trips; pool terms closed; other pools unaffected | I27 | ● | CP-06 component |
| F37 | Foreign macOS guest running | pre-seed a running foreign macOS VM | the capacity report shows one macOS slot; a third start never executed (refused by the data plane if the CP over-allocates) | I03, I33 | | KER-05 property |
| F38 | Host sleep/wake with many deadlines | clock jump during provisioning | no mass quarantine; re-observe; prepared VMs older than the gap retired; session reopened | I10, I09 | | CP-03 component |
| F39 | Wrong guest answers (IP reuse / sibling clone / lock PID not the shim's child) | fake-tart routes exec to another VM or reports a foreign lock PID | challenge or lock check fails; no JIT minted or delivered; quarantine `guest_identity_refused` | I22, I30 | ● | DP-05 contract |
| F40 | Daemon wedged (store writer stalls) | fault knob stalls the writer | watchdog exit with its code; restart recovers; nothing duplicated | I11, I13 | | OPS-05 integration |
| F41 | 100 offers on 2 slots in one batch | none (load) | acquired ≤ deliverable at every point; others declined `no_capacity` | I23, I07 | ● | KER-06 unit |
| F42 | Restart within the session TTL | SIGKILL, fake-github keeps the old session | 409 → backoff; reopen; desired from initial statistics; no job lost or double-served | I12, I13 | ● | GH-04 component |
| F43 | Zombie writer of an older run | replay a delayed command with run epoch e−1 | every aggregate refuses it; nothing changes | I18 | ● | KER-03 determinism |
| F44 | Unknown message type | fake-github injects a new type | recorded and acked; degraded past threshold; never safe mode | I12 | | GH-04 component |
| F45 | Recovery plan applied after the world moved | inventory changes between dry run and apply | `RecoveryPlanRefused{stale}`; no action executed | I31 | | KER-10 unit |
| F46 | Hostile workflow wedges its VM repeatedly | fake-tart stop/delete hang per attempt | quarantined VMs kept, never deleted; the pool's capacity consumed, it admits nothing; page; other pools intact | I28 | | CP-06 component |
| F47 | Provider cannot answer for a long time | fake-tart `list` hangs or fails after a lost delete reply | effect stays `Unknown`, reported as a provider limitation; slot held; nothing asserted or released | I10, I25 | ● | CP-06 component |
| F48 | Daemon killed and upgraded while VMs run | SIGKILL `mayflyd`, restart a newer binary | shims keep VMs running; re-adopted from shim locks and `exit.json`; a Busy job completes | I04, I11 | ● | DP-04 integration |

Core gate: F01–F16, F19, F20, F24, F31–F36, F39, F41–F43, F47, F48. Every scenario declares the faults it requires; a
required fault that hit no operation makes the run `INCONCLUSIVE`.

### 8.5 Audit and verdicts

After the fault phase the controller heals every fault, stops issuing operations, waits for the
healthy tail (a declared bound of fake time and observations) and takes a final quiescent
observation. The checker then:

1. evaluates every safety invariant at **every `observe` cut** and between consecutive cuts (for
   example, a VM present at one cut and absent at the next must have a recorded delete with
   proof);
2. checks bounded liveness (I11, convergence to the warm baseline) over the healthy tail;
3. reconciles each `info` against later observations, annotating, never rewriting, the history;
4. checks coverage: required faults hit, observation floors met.

| Verdict | When | Exit |
|:--|:--|:--|
| `PASS` | every invariant held, liveness reached, coverage met | 0 |
| `FAIL` | an invariant broke; the first violating cut and its witness are saved | 1 |
| `INCONCLUSIVE` | a required fault never hit, too few observations, liveness bound not reached for reasons the run cannot attribute | 2 |
| `HARNESS_ERROR` | malformed or truncated history, a completion without an invoke, a fake or controller crash, an unhealable fault | 3 |

Artefacts in `target/faultlab/<run-id>/`: `manifest.json`, `scenario.toml`, `history.jsonl`,
`observations/`, `checks.json`, `report.md`, `counterexample.json` (on `FAIL`), daemon and fake
logs, a copy of the journal and fake-tart state. `report.md` ends with the replay command. A
negative-control run (§9) succeeds only on `FAIL` with a witness on the invariant its mutant
targets.

### 8.6 Guardrails for real Tart

The lab runs on fakes by default and in every automated lane. Against real Tart it refuses to
start unless all hold: an explicit `--real-tart` flag; a dedicated `TART_HOME` created by the
harness and carrying an ownership marker with the run id; an ownership manifest listing exactly
the VM names the run may touch; no VM in that home it did not create. Every destructive action
re-checks the marker and the manifest first. Base images are copied into the run's home, never
referenced from the owner's. Only the owner runs it (D14).

---

## 9. Mutants

**Curated mutants** are negative controls: source patches in `tools/xtask/mutants/NN-<name>.patch`,
each declaring the test, conformance trace or scenario expected to kill it. `cargo xtask
mutants-curated` applies each in a scratch tree, runs only its named killer, and fails if the
killer passes, if the patch no longer applies (a stale mutant is a failure, never a skip), or if
an unnamed test is needed to kill it.

| # | Mutant | Where | Killed by (lowest level first) |
|:--|:--|:--|:--|
| 1 | Remove the slot capacity check | capacity policy | unit property "admitted ≤ every limit" (KER-05); scheduling conformance (I03); F11 |
| 2 | Reuse a generation | `Host` aggregate | unit "generations strictly increase" (KER-05); scheduling conformance (I01, I02) |
| 3 | Treat a timeout as definite failure | executor / directive relay | component "lost reply → `Unknown`" (CP-02); lifecycle conformance (I10); F03 |
| 4 | Ack before journal commit | listener | component ack-after-record (GH-04); scaleset conformance (I12); F09 |
| 5 | Count `JobAssigned` messages for demand | demand | unit (KER-06); F11 |
| 6 | Add `TotalRunningJobs` to demand | demand | unit (KER-06) |
| 7 | Ignore redelivery dedupe | inbox | journal contract "inbox dedupe" (ES-01); component F10 (GH-04) |
| 8 | Prefix-only cleanup | recovery / ownership | unit recovery table "unknown `mf-*` never deleted" (KER-07); VmProvider contract; F24 |
| 9 | Re-send a directive without an inventory check | startup / relay | component recovery rows (CP-04); recovery conformance (I13); F02 |
| 10 | Lose receipts on reopen | SQLite journal | journal contract, reopened mode (ES-02) |
| 11 | Count a Prepared VM as a registered runner | demand | unit (KER-06); scaleset conformance (I07) |
| 12 | Delete a busy VM during drain | reconciler | component drain test (CP-03); scheduling conformance (I08, I14); F13 |
| 13 | Accept an old epoch in the data plane | data-plane domain | unit fencing matrix (KER-08); link conformance (I09); F20 |
| 14 | `Unknown` → `Deleted` without evidence | `Attempt` aggregate | exhaustive matrix (KER-04); lifecycle conformance (I10); F15 |
| 15 | Count errors as healthy in readiness | composition readiness | integration readiness test (RT-04); `doctor --json` golden |
| 16 | Secret in a log field | any log path | redaction test feeding secrets through every path (RT-04, SEC-01) |
| 17 | Data plane executes a directive twice on a duplicate `directive_id` | data-plane domain / executor | unit idempotency table (KER-08); component duplicate directive (DP-01); link conformance |
| 18 | Projection rebuild differs from live | projections | projection contract "rebuild equals live" (ES-04, I16); F21 |
| 19 | Acquire every offer | `ScaleSet` acquire policy | unit (KER-06); scaleset conformance (I23); F41 |
| 20 | Mint a JIT before a fresh verification | activation manager | lifecycle matrix (KER-04, I22); F39 |
| 21 | Stop before deregister | retirement policy | registration model and unit (KER-09, I21); F33 |
| 22 | Release a slot on an incomplete inventory | inbox / recovery | unit (KER-08, KER-07, I25); F34 |
| 23 | Accept an older run epoch | any aggregate | determinism cases (KER-03, I18); F43 |
| 24 | Execute a directive that does not fit the resource ledger | data-plane executor | unit (KER-08, I33); scheduling conformance; F37 |
| 25 | Delete without re-reading the instance id | data-plane executor | `VmProvider` contract (`MacBlindProvider`); unit (KER-08); F39 |
| 26 | Delete a quarantined VM on a timer or a cap | quarantine policy | component (CP-06, I28); F46 |
| 27 | Resolve `Unknown` without an observation | `Attempt` aggregate / P19 | matrix (KER-04, I10); F47 |

Each core fault-lab scenario additionally has a mutant (from this table or a lab-only one in
`tools/faultlab/mutants/`) that turns it `FAIL` (VER-04).

**`cargo-mutants`** runs nightly in-diff and weekly in full, sharded, over the pure crates and
the application crates, with `nextest` and the conformance tests excluded where they cannot reach
the mutated code. `doc/mutation-baseline.md` records the score
`caught / (caught + missed + timeout)` and triages every survivor (equivalent, or needs a test and
why). The target is 90% on the pure crates after triage; a new survivor in a changed function is
triaged before the next ⛳ checkpoint.

---

## 10. Lanes and budgets

| Lane | Trigger | Runs | Budget |
|:--|:--|:--|:--|
| **quick** | while working | fmt-check, architecture, clippy, unit tests | < 3 min |
| **check / per push** | every commit to `main`, every pull request, `land` | static, architecture, unit, golden, property (1024 cases), contract (memory, SQLite in its five modes, PostgreSQL in Docker (`contract-postgres`, ADR 0035); link inproc/uds; VmProvider fake and fake-tart; ScaleSetApi fake-github and recordings), component, integration, e2e, conformance (committed traces + 30 generated per model, negative controls, field sensitivity), coverage, changed lines, security; `macos` native job; `cross` | < 25 min wall clock in CI |
| **nightly** | schedule | fresh-seed conformance (more traces, longer), Apalache `formal-verify`, `mutants-curated`, `cargo-mutants` in-diff, fuzz per target (time-boxed) | < 3 h |
| **weekly** | schedule | fault lab core then extended suite, `cargo-mutants` full (sharded), security over full history | < 6 h |
| **release** | tag | full `check`, fault lab core, `mutants-curated`, `formal-verify`, `release-check` per target, SBOM | must be green on the tagged commit |
| **hardware** (owner) | manual dispatch on the owner's Mac | VmProvider and GuestTransport contracts on real Tart, one-job smoke with a tiny Linux image, live scale-set contract and listener on a disposable repository | owner's time |

**Coverage floors** (`coverage-floor.toml`, measured by `cargo-llvm-cov` over every in-process
suite):

| Scope | Floor at start |
|:--|:--|
| overall (`crates/`) | 85% lines |
| pure crates (`mayfly-domain`, `mayfly-protocol`, `mayfly-control-domain`, `mayfly-data-domain`) | 95% lines each |
| changed lines in `crates/**/src` | 90% |

Floors **ratchet only up**: a floor rises to the measured value rounded down when it exceeds the
floor by a whole point; lowering one needs the owner. `xtask`, `faultlab` and the fake binaries
are reported, not floored. Coverage measures execution; mutants (§9) measure whether tests can
fail.

---

## 11. Evidence and reporting

### 11.1 What every run leaves behind

1. **Gate runs** print a stage table (stage, result, duration) and the tool versions from
   `doctor`; CI keeps JUnit, LCOV and, on failure, generated traces.
2. **Seeds** for proptest failures, generated traces, simulations and fault-lab runs are printed
   and saved with their artefacts; every failure is reproducible from its seed or saved history.
3. **Fault-lab runs** keep their artefact folder when the verdict is not the expected one;
   `faultlab replay` reruns the scenario and seed and succeeds only if the verdict reproduces.
4. An agent's report (fnd §11.4) names each gate run, its exit status, versions and seeds, and
   what was not run.

### 11.2 Invariant pins

| Inv. | Model | Unit / component / contract | Fault lab | Mutant |
|:--|:--|:--|:--|:--|
| I01 | scheduling | `OwnedVmName` round trip and rejection table (KER-01) | F01, F04 | 2 |
| I02 | scheduling | `Host` generation tests (KER-05) | F04, F19 | 2 |
| I03 | scheduling | capacity property (KER-05); CP-03 simulation | F01, F11, F17, F26 | 1 |
| I04 | recovery | recovery table (KER-07); VmProvider contract | F13, F16, F24 | 8 |
| I05 | recovery | base-image names never parse as owned (KER-01); delete refusal (KER-08) | F24 | 8 |
| I06 | lifecycle | exhaustive matrix (KER-04) | F06, F12, F14 | 14 |
| I07 | scaleset, scheduling | demand unit tests (KER-06) | F11, F25, F30 | 5, 6, 11 |
| I08 | scheduling | drain admits nothing (KER-05) | F22, F23 | 12 |
| I09 | link, recovery | fencing matrix (KER-08); link contract | F18, F20, F31 | 13 |
| I10 | lifecycle, recovery | lost reply → `Unknown` (CP-02); matrix (KER-04) | F02, F03, F15, F31 | 3, 14 |
| I11 | recovery (bounded) | startup component tests (CP-04) | F05, F13, F29, F30 | 9 |
| I12 | scaleset, recovery | ack after record (GH-04) | F08, F09, F18 | 4 |
| I13 | lifecycle, scheduling, scaleset | same name and generation on retry (CP-02) | F01–F03, F09, F27 | 7, 9 |
| I14 | lifecycle | matrix (KER-04); duplicate directive (DP-01) | F12, F14, F23 | 12, 17 |
| I15 | — | SEC-01 security tests; redaction (RT-04); fake-tart invocation records (DP-05) | F28 (secret absence) | 16 |
| I16 | event_store | projection contract and replay property (ES-04); online rebuild PC-07..PC-09 (ES-12) | F21 | 18 |
| I17 | recovery | floor check (CP-04); `HostRun` (KER-10) | F35 | — |
| I18 | link | run-epoch determinism cases (KER-03) | F43 | 23 |
| I19 | registration | `RunnerRegistration` unit (KER-09); `RunnerRegistry` contract | F07, F32 | — |
| I20 | registration | reaper component (CP-05) | F07, F32 | — |
| I21 | registration | 422 race (KER-09) | F12, F33 | 21 |
| I22 | lifecycle, registration | activation matrix (KER-04); `GuestTransport` contract (DP-05) | F32, F39 | 20 |
| I23 | scaleset | `PlanAcquire` unit (KER-06) | F41 | 19 |
| I24 | scheduling | carve-out property (KER-05) | F11, F41 | — |
| I25 | recovery | inventory `Incomplete` (KER-08, KER-07) | F34, F47 | 22 |
| I26 | scheduling | grant refusal table (KER-05) | F23 | 12 |
| I27 | scheduling | breaker unit (KER-05, CP-06) | F36 | — |
| I28 | scheduling | quarantine policy component (CP-06) | F46 | 26 |
| I29 | link, recovery | held kinds (KER-02, KER-08) | F22, F35 | — |
| I30 | lifecycle | guest-claim parsing (DP-05, KER-04) | F39 | — |
| I31 | recovery | plan bindings (KER-10) | F45 | — |
| I32 | scaleset | trust classification (KER-06) | — | 19 |
| I33 | scheduling | resource ledger refusals (KER-08, DP-01) | F37 | 24 |

### 11.3 Verification status

`doc/verification-status.md` (introduced with VER-04) holds the current numbers: last
`formal-verify` depths and timings, the last fault-lab suite with verdict per scenario, the
mutation baseline score, and coverage per crate. Numbers are replaced, not appended; history lives
in git. A claim in any document about what mayfly guarantees names the row above that pins it.
