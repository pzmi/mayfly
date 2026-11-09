# Formal models

The Quint models of mayfly, the invariants they state, the depth to which Apalache checks them
and how to run everything. The design is [verification §6](verification.md) (models,
invariants, trace replay, the Apalache lane); the decisions are D10 (model first) and D15 (pinned
tools, fail closed). Models come first: a change to lifecycle, scheduling, recovery, the
scale-set session or the CP↔DP link changes its model before the Rust code, and an invariant is
never removed or weakened to get green.

## Toolchain

| Tool | Pin | Where | Checked by |
|:--|:--|:--|:--|
| Node | 22.x (the major) | the runner or the laptop | `doctor`, every formal stage |
| Quint | 0.33.1 | `formal/package-lock.json`, installed by `npm ci --prefix formal` | `doctor`, every formal stage |
| Java | 21 or newer | a JDK on `PATH` (CI: Temurin 21) | `doctor --all`, `formal-verify` |
| Apalache | 0.62.1, SHA-256 `38202a8c4ebeb3ab73e0fe5a7e4bb948442d46ad5de8a7fb45c9e5cac2beb2a6` | `~/.quint/apalache-dist-0.62.1` (or `$QUINT_HOME`), unpacked by `formal-verify` | `doctor --all`, `formal-verify` |

The pins live in `tools/versions.toml` (Quint's version is owned by the lockfile and noted
there). Everything is development-only: the shipped binaries never need Node or a JVM.

- **Quint** always runs as `npx --prefix formal --no -- quint ...`: `--no` refuses to fetch a
  package from the registry, and every `quint run` and `quint test` passes `--backend
  typescript`, because the default Rust evaluator is downloaded from GitHub at first use and no
  lockfile pins it.
- **Apalache** would be downloaded by Quint without a check. `formal-verify` therefore fetches
  the release tarball itself (HTTPS only), refuses it unless its SHA-256 equals the pinned one,
  and unpacks it where Quint looks for it before Quint runs; Quint never downloads anything. The
  pinned digest was computed from the real download of the release, and the same digest is
  recorded by site-preview for the same release. A digest mismatch deletes the download and
  fails, naming both digests (`apalache::tests::should_refuse_the_release_when_its_digest_differs`).
  A new version needs a new digest, computed from the real download, in the same commit.
- **Fail closed.** A missing Node, a Quint that is absent from `formal/node_modules` or at
  another version, a JVM older than 21 or an Apalache that cannot be fetched fails the stage
  with the command that fixes it (`formal::tests::should_fail_closed_when_quint_is_missing_or_wrong`,
  `formal_verify::tests::should_explain_the_fix_when_java_is_missing_or_too_old`). Nothing is
  skipped, and a tool failure is never reported as a verification result: only Quint's own
  `[violation]` is a counterexample (`formal_verify::tests::should_tell_a_counterexample_from_a_tooling_failure_when_a_run_fails`).

## Commands

```bash
npm ci --prefix formal                 # install the locked Quint (once, and after a lockfile change)
cargo xtask formal-typecheck           # quint typecheck of every formal/*.qnt
cargo xtask formal-test                # quint test of every registered model, at least one test each
cargo xtask check --only formal-typecheck,formal-test   # what the `formal` job of ci.yml runs
cargo xtask formal-verify              # Apalache bounded check + long simulation (not in `check`)
QUINT_SEED=<seed> cargo xtask formal-verify   # repeat a simulation; the base seed is always printed
cargo xtask doctor [--all]             # pinned tools; --all adds Java and Apalache
```

`formal-typecheck` and `formal-test` are `check` stages (job `formal` in `ci.yml`).
`formal-verify` is the weekly lane of `.github/workflows/formal.yml`, which also runs on every
change under `formal/`, in `tools/versions.toml` and in the xtask files that drive it. Its logs,
Apalache's run directories (a counterexample as ITF and TLA+) and the ITF trace of a failing
simulation are kept in `target/formal-verify/` and uploaded when a run fails.

## How a model is registered

A model is one file `formal/<name>.qnt` whose main module has the same name. It must have:

- `val allInvariants = and { ... }`, the one list of its invariants: every stage reads the
  invariants from it, so a new invariant cannot be left out of simulation or verification;
- a `step` relation (and, from KER-02 on, the `implStep` and biased relations of verification §6)
  for `quint run` and `quint verify --step`;
- at least one `run <name>Test` (a named scenario `quint test` executes): a model with no test
  fails `formal-test` (`formal::tests::should_fail_a_model_when_quint_test_runs_no_test`).

It is then registered in two places, both in `tools/xtask/src`:

1. an entry of `MODELS` in `formal.rs` (file and step relations): without it `formal-typecheck`
   and `formal-test` fail for the unregistered file
   (`formal::tests::should_name_unregistered_and_missing_files_when_the_registry_differs`);
2. an entry of `BOUNDED` in `formal_verify.rs` for every step relation of the entry: the test
   `should_bound_every_step_relation_when_models_are_registered` fails otherwise.

Then add its row to the table below (invariants, depth, measured duration) in the same commit.
Apalache explores every enabled action at every step, so the cost grows with the number of
actions in the relation and the size of the state: choose the depth that finishes in a few
minutes on a hosted runner (start at 4 to 8 steps) and record the timing. A seeded false
invariant is a negative control that must be reported as violated (VER-07 automates it); until
then, a model's task records the one it tried.

## Models

| Model | File | Governs | Invariants (`allInvariants`) | Step relations | Bounded depth | Duration | Task |
|:--|:--|:--|:--|:--|:--|:--|:--|
| smoke | `formal/smoke.qnt` | nothing in mayfly: a bounded counter that exercises every stage of the toolchain | `withinLimit` | `step` | 6 steps | about 5 s (Apalache; 4-CPU sandbox, measured at FND-06) | FND-06 |
| lifecycle | `formal/lifecycle.qnt` | the `Attempt` aggregate and its saga, the data plane's execution of its directives and the provider's inventory (plan §6, dm §6.4) | `i06CompletedNeverServesAgain`, `busyNeverWarm`, `i10UnknownLeavesOnlyOnObservation`, `i13NoDuplicateClones`, `i14OneJob`, `i22JitMintedOnceAfterGrantAndVerification`, `i30GuestNeverDecidesOwnership`, `quarantineTerminal`, `quarantineNeverAutoDeleted`, `retryKeepsNameAndGeneration` | `step`, `serveStep`, `activationStep` | 8 steps each (for `activationStep`, 8 steps after a one-step preparation: deeper into activation than 8 of `step`) | about 60 s for `step`, 32 s for `serveStep`, 87 s for `activationStep` (Apalache, run two at a time; 4-CPU sandbox, measured at KER-04F) | KER-04, KER-04F |
| link | `formal/link.qnt` | the CP↔DP protocol: handshake and version negotiation, run-epoch minting and the floor, at-least-once delivery with loss, duplicates and reordering, fencing, per-epoch idempotency, safe-mode held kinds, the inbox's epoch fence (plan §3.2, ADR 0042) | `i09NoStaleEpochExecutes`, `executesAtMostOncePerEpoch`, `refusedMeansNothingHappened`, `i17EpochsIncrease`, `i18InboxFence`, `i29SafeModeHoldsKinds`, `versionNegotiated` | `step`, `implStep` | 10 steps for `step`, 8 for `implStep` | about 85 s for `step`, 20 s for `implStep` (Apalache through `formal-verify`, run two at a time; 4-CPU sandbox, measured at KER-02) | KER-02 |

The planned models are listed in verification §6.1: `scheduling` (KER-05), `scaleset` (KER-06), `recovery` (KER-07) and `registration` (KER-09).

### Simulation budget

`formal-verify-simulate` runs `quint run` of every model and step relation over three seeds
derived from one printed base seed (`QUINT_SEED` repeats it), each 5000 traces of 50 steps,
checking every conjunct of `allInvariants`. It reaches beyond the depth Apalache covers but only
samples: Apalache's bounded check is the proof for its depth, the simulation is breadth.

### smoke

One counter that `increment`s up to `LIMIT = 3` and `reset`s to zero.

| Id | Invariant | Pinned by |
|:--|:--|:--|
| `withinLimit` | `0 <= count <= LIMIT` in every reachable state | `formal-verify-bounded` (Apalache, 6 steps), `formal-verify-simulate` |

Tests (`quint test`, stage `formal-test`): `climbsToLimitTest`, `resetReturnsToZeroTest`,
`resetNeedsNonZeroTest`. Negative control tried: tightening the invariant to `count < LIMIT`
makes `quint verify` report `[violation]` with a counterexample that reaches `count = 3`.

### lifecycle

One attempt stream (`a`), the provider's truth about its VM (`w`, with whether the pending
directive has taken effect) and history for the invariants (`g`, read by no guard). Each action
is one stimulus of the Rust aggregate `mayfly_control_domain::attempt`: `issueNext` is the
saga's `IssueNext`; `replySucceeded`, `replyRefused`, `replyProofRefused`, `replyGuestRejected`,
`replyLost` (`OutcomeUnknown`, the effect applied or not), `replyLaunchUnknown`,
`inventoryPresent`, `inventoryAbsent`, `vmObservedStopped` and `runnerExited` are
`RecordObservation` facts; `beginActivation`, `recordJitHandle`, `bindJob`,
`requestRetirement`, `confirmRetirement`, `withdrawRetirement`, `quarantineCommand`, `adopt`,
`resolveQuarantine` and `close` are commands; `vmReplacedOrRemoved` changes the provider behind
the journal's back. A reply is merged with the execution of its effect, so a lost reply is the
effect happening or not, then `Unknown`. `step` offers every stimulus at every state;
`serveStep` is the biased relation of verification §6 (no policy retirement before the job ends,
no interference with the provider), because a uniform random walk of `step` almost never strings
the fifteen stimuli of the happy path together: `serveStep` reaches Busy, Finished and Closed in
simulation, `step` does not. `activationStep` (KER-04F) starts with `prepareAtOnce`, which turns
the preparation (and, nondeterministically, the grant and the fresh verification) into one
step, then offers the stimuli around activation: the JIT handle, the launch, the job, restarts,
lost replies, launch outcomes, refused proofs, a replaced VM, quarantine and retirement. It
exists because the KER-04 review found a counterexample nine steps from `init` (a restart with
the launch in flight, the late launch reply, a job bound to a runner whose JIT was presumed
leaked) that `step` at depth 8 could not reach and random simulation did not sample; at depth 8
of `activationStep` it is six steps deep.

**What the invariants are evidence of.** Most invariants hold in the model because its guards
enforce them (`recordJitHandle` needs the grant and the fresh verification, so
`illegalMint` stays false): checking them proves that the guards and the transitions of the
model, taken together, cannot reach a violating state, including through interleavings nobody
wrote down, which is how the review's counterexample was a real defect of the model. They are
evidence about the Rust code only through the matrix golden and the Rust tests named below,
and fully once VER-01's conformance replay drives the aggregate with the model's traces; until
then a guard the Rust code lacks is caught only if a Rust test exercises it.

| Id | Invariant | Pinned by |
|:--|:--|:--|
| `i06CompletedNeverServesAgain` | I06: no job is bound and no activation begins after the attempt finished or was retired | `formal-verify`; `tests/attempt/lifecycle.rs::should_never_return_to_prepared_when_a_runner_has_served` |
| `busyNeverWarm` | no path from Listening, Busy or Finished back to Prepared | `formal-verify`; the same Rust test |
| `i10UnknownLeavesOnlyOnObservation` | I10: when the pending outcome stops being `Unknown`, the last stimulus was an observation | `formal-verify`; `tests/attempt/lifecycle.rs::should_leave_unknown_only_on_an_observation_when_every_command_is_tried` (mutant 27) |
| `i13NoDuplicateClones` | I13: no directive issued while one is pending, every clone targets the one (name, generation), clones bounded by the retry budget | `formal-verify`; `tests/attempt/lifecycle.rs::should_keep_name_and_generation_when_a_stage_is_retried` |
| `i14OneJob` | I14: at most one job is ever bound | `formal-verify`; `tests/attempt/lifecycle.rs::should_bind_one_job_when_a_second_is_offered` |
| `i22JitMintedOnceAfterGrantAndVerification` | I22 (attempt half): a JIT handle only with the grant and a verification of the current epoch, at most one launch, no job once the JIT is presumed leaked (a launch of unknown outcome, or a handle a restart orphaned, even with its launch in flight) | `formal-verify`; `tests/attempt/lifecycle.rs::should_refuse_a_jit_handle_when_activation_lacks_a_grant_or_a_fresh_verification`, `should_never_launch_again_when_a_launch_outcome_is_unknown`, `should_record_a_late_launch_and_never_bind_a_job_when_a_restart_orphaned_its_handle` |
| `i30GuestNeverDecidesOwnership` | I30: the recorded instance id is one the provider reported, never a guest's claim | `formal-verify`; `tests/attempt/lifecycle.rs::should_carry_the_recorded_instance_id_when_a_delete_is_issued` |
| `quarantineTerminal` | automation never takes an attempt out of quarantine (only an operator's resolution or an observed absence) | `formal-verify`; `tests/attempt/lifecycle.rs::should_never_delete_when_quarantined_until_an_operator_resolves` |
| `quarantineNeverAutoDeleted` | I28: no `DeleteVm` while quarantined and unresolved | the same |
| `retryKeepsNameAndGeneration` | every `CloneVm`, first or retried, targets the name and generation fixed at start | `formal-verify`; `tests/attempt/lifecycle.rs::should_keep_name_and_generation_when_a_stage_is_retried` |

Tests (`quint test`, stage `formal-test`, all deterministic): `happyPathTest`,
`unknownCloneResolvedByInventoryTest`, `unknownCloneNotAppliedBacksOffTest`,
`stageFailureBacksOffThenRetiresTest`, `guestRejectedIsQuarantinedTest`,
`activationNeedsGrantAndFreshVerificationTest`, `launchUnknownNeverServesTest`,
`busyServesOneJobTest`, `retirementWaitsForDeregistrationTest`,
`replacedVmQuarantinesOnDeleteTest`, and from KER-04F `adoptDuringLaunchNeverServesTest` (the
review's counterexample, kept as a regression), `deleteRefusedRepeatedlyQuarantinesTest`,
`quarantinedStopsAreBoundedTest`, `prepareAtOnceTest`. Stops and deletes count against the
retry budget like the stage directives: a cleanup the data plane keeps refusing ends in
quarantine (`delete_refused_repeatedly`), and inside quarantine the stops, a refused proof
included, end. The Rust code asserts the model's invariants on every accepted transition two
stimuli deep from every matrix state
(`tests/attempt/properties.rs::should_keep_the_lifecycle_invariants_when_any_two_stimuli_are_accepted`).
Negative controls tried: removing the `jitLeaked` guard of `bindJob` (the KER-04 model) makes
`quint verify --step activationStep` report `[violation]` of
`i22JitMintedOnceAfterGrantAndVerification` at depth 6 in about a minute; dropping the fresh-verification
guard of `recordJitHandle` makes simulation of `serveStep` report `[violation]` of
`i22JitMintedOnceAfterGrantAndVerification`; letting `requestRetirement` run while a directive
is pending (mutant 27) makes simulation of `step` report `[violation]` of
`i10UnknownLeavesOnlyOnObservation`.

### link

One control plane (`cp`), one data plane (`dp`), a network (`net`: two sets of frames, so no
order) and history for the invariants (`g`, read by no guard). Two directives stand for the
kinds: id 1 is a `CloneVm` (held in safe mode), id 2 a `ReportInventory` (never held). Each
action is one stimulus of the protocol: `dpHello` (start or reconnect, offering major 1 or 2),
`dpRestart` (memory lost, floor kept), `dpWelcome` (enter a session strictly above the floor),
`dpDeliver` (fence, repeat, hold or execute one frame, possibly leaving a duplicate behind;
an execution ends applied, refused for resources, or Unknown with the effect applied or not),
`cpWelcome` (mint `max(minted, floor) + 1`, or refuse an unsupported major), `cpIssue`
(`DirectiveIssued`), `relaySend` (re-send under the current session until the answer is
definite), `cpReceive` (the inbox, fenced by epoch), `loseToDp`, `loseToCp` and
`foreignMajor` (a frame of another major). `step` is every behaviour plan §3.2 allows;
`implStep` is what the composition can do (its control plane never writes another major). The
Rust published language is `mayfly_protocol`; its pure rules `negotiate`, `admit_frame`,
`mint_epoch`, `enters_session`, `check_epoch` and `DirectiveTag::held_in_safe_mode` are this
model's guards. Safety only: liveness (a session eventually opens) is not claimed.

| Id | Invariant | Pinned by |
|:--|:--|:--|
| `i09NoStaleEpochExecutes` | I09: no frame of an epoch older than the floor executes | `formal-verify`; `crates/mayfly-protocol/src/handshake.rs::should_fence_frames_by_the_session_epoch_when_a_directive_arrives`; later the fencing matrix (KER-08) and the link contract (DP-02) |
| `executesAtMostOncePerEpoch` | each directive id executes at most once per epoch, under re-sends and duplicates | `formal-verify`; later KER-08 and the link contract |
| `refusedMeansNothingHappened` | a definite refusal names an (epoch, directive) that never executes; an applied answer one that did (ADR 0042) | `formal-verify`; `staleFrameIsNeverRefusedTest`; the golden `observation/v1.unknown-stale-epoch.json` |
| `i17EpochsIncrease` | sessions are entered at strictly increasing epochs, the floor equals the open session's epoch and never exceeds what was minted | `formal-verify`; `handshake.rs::should_mint_above_the_journal_and_the_floor_when_a_session_opens`, `should_enter_only_above_the_floor_when_a_welcome_arrives` |
| `i18InboxFence` | the inbox applies no answer older than the newest it applied | `formal-verify`; `olderAnswerIgnoredTest`; the `Attempt` epoch fence (KER-04) |
| `i29SafeModeHoldsKinds` | in a safe session no held kind executes | `formal-verify`; `directive.rs::should_hold_exactly_the_effect_kinds_when_in_safe_mode` |
| `versionNegotiated` | a session exists only in a major both peers speak; no frame of another major executes | `formal-verify`; `version.rs` tests; `tests/golden.rs::should_refuse_every_fixture_when_its_major_is_unknown` |

Tests (`quint test`, stage `formal-test`, all deterministic): `handshakeAndExecuteTest`,
`unsupportedMajorRefusedTest`, `duplicateExecutesOnceTest`, `staleFrameIsNeverRefusedTest`,
`oldWelcomeAfterRestartIgnoredTest`, `epochMintedAboveFloorTest`, `safeModeHoldsCreateTest`,
`foreignMajorDiscardedTest`, `frameBeforeWelcomeDiscardedTest`, `olderAnswerIgnoredTest`.
Negative controls tried: answering a stale frame `refused` (the rule of plan §3.2 before
ADR 0042) violates `refusedMeansNothingHappened` on the reconnect path of
`staleFrameIsNeverRefusedTest` (a directed run; uniform simulation of 20 000 traces does not
reach the 12-step path); entering a welcome at the floor (`>=` for `>`) makes Apalache report
`[violation]` of `i17EpochsIncrease` at 5 steps; dropping the safe-mode hold makes simulation
and Apalache report `[violation]` of `i29SafeModeHoldsKinds`.

## Promoting a counterexample

A counterexample from `formal-verify` or a fault-lab verdict is promoted as verification §6.2
step 9 says: reproduce it with the seed, decide which side is wrong (the model first, if the
model is), reduce it to a named run in the model's tests, and keep it as a regression.
