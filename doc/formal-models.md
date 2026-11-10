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
| scheduling | `formal/scheduling.qnt` | one host's `Host` aggregate, the `Pool` terms it applies, the capacity policy and the data plane's resource ledger (plan §9, dm §6.2, §6.3, ADR 0024) | `i01OneSlotPerAttempt`, `i02NoSharedSlotGeneration`, `i03StartsWithinCapacity`, `i07NeverAdvertiseMore`, `i08NothingAfterDrain`, `i13NoDuplicateClones`, `i24CarveOutsFitTheHost`, `i26NoAdmissionWhileClosed`, `noGrantWhileClosed`, `i27BreakerBounded`, `i28QuarantineNeverAutoDeleted`, `i33DataPlaneNeverOvercommits`, `generationsMonotonic`, `macosGuestsAtMostTwo`, `externalConsumptionShrinksAdmissible`, `quarantineHoldsCapacity`, `allocationWithinReport`, `carveOutAdmissible` | `step`, `admitStep` | 8 steps each | about 7 min for `step`, 3.5 min for `admitStep` (Apalache, run two at a time; 4-CPU sandbox, measured at KER-05; depth 6 took 100 s and 56 s); simulation about 3 min per seed and relation | KER-05 |
| recovery | `formal/recovery.qnt` | one host's startup use case across restarts (lock, open with backup, verify, `StartRun`, `MintEpoch`, handshake with the floor, inventory, classification, `CompleteRecovery`), the recovery decision table, the saga's and the data plane's effects after it, the GitHub listener's record-before-ack, safe mode and the recovery plan; crashes at every await, restores, incomplete inventories, stale frames and a provider that changes behind the journal (plan §10, dm §11.3, ADRs 0025, 0026, 0037) | `i04UnrecognisedNeverDeleted`, `i05BaseNeverDeletable`, `i09NoStaleEpochExecutes`, `i10UnknownLeavesOnlyOnObservation`, `i11RecoveryCompletes`, `i12RecordBeforeAck`, `i17EpochsIncrease`, `i25IncompleteNeverAbsence`, `i29SafeModeNoEffects`, `i31PlanBound`, `safeModeNoDestructiveAction`, `deleteNeedsProof`, `noAssertion`, `quarantineNeverDeleted`, `busyNeverRetiredWhileRunning` | `step`, `restartStep`, `repairStep` | 12 steps each | about 6 min for `step`, 3 min for `restartStep`, 2.5 min for `repairStep` (Apalache, run two at a time; 4-CPU sandbox, measured at KER-07; depth 8 took about 40 s); simulation about 1.5 min per seed and relation | KER-07 |

The planned models are listed in verification §6.1: `scaleset` (KER-06) and `registration` (KER-09).

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

### scheduling

One host and two pools (`linux` with 1-unit VMs, `macos` with 2-unit VMs and G3's two guest
slots), three attempt ids and three slots. The data plane (`dp`, `vms`, `sent`) is the truth:
foreign consumption, our VMs and the capacity reports it has sent, which the host applies late
and out of order (`recordReport`: a newer `id` wins, an older one is ignored). The host
(`host`, `leases`, `slotGen`, `terms`) is the journal: `reserve` takes the lowest free slot
under its next generation if the pool's applied terms are open, the gate is open, the pool is
under `max_running` (quarantined leases counted) and the VM fits the last report on every
dimension it uses; `grant`, `quarantine`, `stopQuarantined` (a stopped quarantined lease
shrinks to disk and slot), `release` (only on the VM's observed absence), the gate, the drain
and `applyTerms` (policy P4, only at a newer revision) are its other commands. The pools
(`pools`) pause, resume, count failures and trip (I27), and reset by an operator. `dpCreate`
starts a lease's VM only if it fits the data plane's own ledger (I33); `foreignChanges` and
`vmVanishes` are the world. `planOf` is `capacity::plan`: free capacity after every live
lease, carved greedily in pool-name order up to each pool's headroom, and `reportedMaxOf`
(only live runners while closed). `step` offers every stimulus; `admitStep` is the biased
relation of verification §6: reports, terms, reservations, creates, quarantine and releases
under changing foreign consumption, without the gate, drain and pool modes that close
admission, so a walk fills the host and keeps it full.

What the invariants are evidence of is as for `lifecycle`: the guards and transitions of the
model, together, cannot reach a violating state; the Rust code is held to the same invariants
by the tests named below until VER-01's conformance replay drives `Host` and `capacity::plan`
with the model's traces. `carveOutAdmissible` ties the two halves: whatever the policy carves
out, the host admits; it found that the first draft's admission refused a Linux VM while a
newer report had shrunk the macOS slots below what the host held, though the policy carved
one (model and code now check only the dimensions a VM uses).

| Id | Invariant | Pinned by |
|:--|:--|:--|
| `i01OneSlotPerAttempt` | I01: no slot holds two live leases | `formal-verify`; `tests/host/properties.rs::should_strictly_increase_generations_per_slot_when_commands_are_random` |
| `i02NoSharedSlotGeneration` | I02: no (slot, generation) is reserved twice | the same (mutant 2) |
| `generationsMonotonic` | a slot's generation only grows; a lease's is its slot's last | the same |
| `i03StartsWithinCapacity` | I03: every reservation fit the last applied report, every lease counted; every start fit the data plane's ledger | `formal-verify`; `tests/capacity.rs::should_admit_within_every_reported_limit_when_inputs_are_random` (mutant 1), `tests/host/properties.rs::should_allocate_within_the_last_report_when_commands_are_random` |
| `allocationWithinReport` | the same, and an older report never replaces a newer one | `tests/host/rules.rs::should_never_allocate_beyond_the_newer_report_when_a_stale_one_arrives` |
| `i33DataPlaneNeverOvercommits` | I33: the data plane starts nothing that does not fit its ledger, whatever the host allocated | `formal-verify`; the data plane's own tests arrive with KER-08 (mutant 24) |
| `macosGuestsAtMostTwo` | G3: at most two running macOS guests, foreign included; no reservation counts a third | `tests/host/rules.rs::should_admit_two_macos_guests_at_most_when_foreign_guests_run`, `tests/capacity.rs::should_never_admit_a_third_macos_guest_when_inputs_are_random` |
| `i07NeverAdvertiseMore` | I07: a pool's `reported_max` is within what it can deliver | `tests/capacity.rs::should_fit_every_carve_out_together_when_inputs_are_random` |
| `i24CarveOutsFitTheHost` | I24: the carve-outs fit the host's free capacity together | the same |
| `externalConsumptionShrinksAdmissible` | more foreign consumption never frees capacity or carves more | `tests/capacity.rs::should_never_admit_more_when_foreign_consumption_grows` |
| `i08NothingAfterDrain` | I08: no reservation and no grant after drain start; the drain completes with no lease | `tests/host/rules.rs::should_admit_nothing_when_the_host_drains`, `tests/capacity.rs::should_admit_nothing_and_report_live_runners_when_admission_is_closed` |
| `i26NoAdmissionWhileClosed`, `noGrantWhileClosed` | I26: no reservation and no grant with the gate or the pool's terms closed | `tests/host/rules.rs::should_refuse_a_grant_when_any_term_or_cause_is_closed` |
| `i27BreakerBounded` | I27: failures bounded by the threshold, a tripped breaker published closed, nothing reserved under terms that show it | `tests/pool/rules.rs::should_trip_at_the_threshold_and_count_nothing_more_when_failures_repeat`, `should_close_the_terms_when_the_breaker_of_the_pin_trips` |
| `i28QuarantineNeverAutoDeleted`, `quarantineHoldsCapacity` | I28: a quarantined VM is never deleted by automation, its lease released only on the VM's absence, counted against capacity and `max_running` | `tests/host/rules.rs::should_hold_capacity_when_a_lease_is_quarantined`, `tests/capacity.rs::should_hold_quarantined_capacity_when_inputs_are_random` |
| `i13NoDuplicateClones` | I13: at most one VM is created per lease | `formal-verify` (the attempt's retries are `lifecycle`'s) |
| `carveOutAdmissible` | whatever the policy carves out, the host admits | `tests/host/properties.rs::should_admit_every_planned_reservation_when_the_host_decides_them` |

Tests (`quint test`, stage `formal-test`, all deterministic so each runs once):
`fillsThenRefusesTest`, `generationsIncreaseTest`, `macosAtMostTwoTest`,
`staleReportIgnoredTest`, `drainAdmitsNothingTest`, `quarantineHoldsCapacityTest`,
`quarantineConsumesPoolTest`, `breakerTripsTest`, `pauseReportsLiveOnlyTest`,
`dataPlaneRefusesWhatDoesNotFitTest`. Negative controls tried: removing the capacity guard of
`reserve` (mutant 1) makes simulation of `admitStep` report `[violation]` of
`allocationWithinReport`; reserving every slot at generation 1 (mutant 2) makes it report
`i02NoSharedSlotGeneration`; dropping the gate guard of `grant` makes simulation of `step`
report `noGrantWhileClosed`. Simulation runs at about 38 traces of 50 steps per second
(TypeScript evaluator, 4-CPU sandbox).

### recovery

One host after an earlier run of epoch 1 ended, cleanly or not: two attempt records (each in
any of Cloning, Prepared, Busy, Deleting, Deleted or Quarantined, with its pending directive's
outcome unknown), the provider's VMs (each running or stopped, under a live shim or not, with
the recorded instance id or another), an image base, an `mf-*` VM no record names, the external
epoch floor (raised by a later run whose journal was rolled back, or not), whether every stored
event kind is known, and the earlier run's frames still on the link. Durable state (`j`, the
journal with the safe-mode marker and the recorded GitHub messages; `backup`; `floor`; `world`;
`net`; `plan`) survives `crash`, which can happen between any two actions, so at every await of
the startup sequence; the run (`daemon`) and the data plane's session (`dp`) do not. Every reply
of the data plane is lost, so an effect is learned only from an inventory, the hardest case for
I10.

The startup sequence is `start` (lock, open, backup), `verify` (floor, kinds, structure, marker:
safe mode is recorded first), `mint` (`StartRun`, `MintEpoch` above the journal and the floor),
`welcome` (the data plane raises the floor, then enters the session), `inventory` (complete or
not; an incomplete one lists nothing), `classifyRecord` (the decision table, `classify`, one
record at a time, with GitHub's view of its runner as an environment choice), `classifyStray`
and `complete` (`CompleteRecovery`; admission opens). After it `issueNext` (the saga's next
directive: a retried clone by name, or a delete carrying the recorded instance id), `dpExecute`
(a frame runs only in its own session, never in a safe one, a delete only on the proof's
instance id and never on a base; a frame may stay on the link as a duplicate), `observe` (a
complete inventory resolves an `Unknown` outcome, the only way out) and `github` (deliver,
record, ack). In safe mode `proposePlan` draws a plan bound to the journal heads, the
inventory and the epoch, an operator approves it, each action's intent precedes its outcome,
the plan is refused when the world moved (`refusePlan`), and `completePlan` leaves safe mode for
a verified restart. `restore` puts the pre-migration backup back while the daemon is down (ADR
0043) and `worldChange` makes a VM vanish, be replaced under its name, stop or lose its shim.

`classify` is the abstraction of `mayfly_control_domain::operations::recovery::classify` for
the inputs the model generates; the rows that depend on a runner registration (Activating,
Listening) belong to the registration model (KER-09) and are pinned in Rust by the table
golden. `step` offers every action; `restartStep` leaves out GitHub and the plan, so a bounded
check spends its depth on the pipeline, its crashes and its effects; `repairStep` leaves out the
saga and the data plane (which safe mode holds anyway), because a uniform walk of `step` almost
never strings proposal, approval, two intents, two outcomes and completion together without a
fault in between.

| Id | Invariant | Pinned by |
|:--|:--|:--|
| `i04UnrecognisedNeverDeleted` | I04: no directive names a VM no record names, and none deletes one | `formal-verify`; `crates/mayfly-control-domain/tests/recovery.rs::should_never_delete_when_an_mf_vm_is_unknown_to_the_journal` |
| `i05BaseNeverDeletable` | I05: no directive targets the image base, which always exists | `formal-verify`; `tests/recovery.rs::should_never_touch_when_the_vm_is_a_base_or_foreign` |
| `i09NoStaleEpochExecutes` | I09: no frame takes effect outside its own session or below the epoch its attempt accepted | `formal-verify` (the link's own fencing is `link`'s) |
| `i10UnknownLeavesOnlyOnObservation` | I10: an `Unknown` outcome is cleared only by a classification or an observation of a complete inventory | `formal-verify`; `tests/recovery.rs::should_assert_no_outcome_when_any_row_is_decided` |
| `noAssertion` | every resolution of an `Unknown` outcome agrees with the inventory: a present VM is never read as a clone not applied or as deleted, an absent one never as a clone applied | the same |
| `i11RecoveryCompletes` | I11 (bounded): a start the journal trusts never stalls before admission, and one with no fault completes within the pipeline's nine steps with every record classified | `formal-verify` |
| `i12RecordBeforeAck` | I12: a GitHub message is acked only after its durable record, across crashes | `formal-verify` (the listener's own test is KER-06's) |
| `i17EpochsIncrease` | I17: sessions are entered at strictly increasing epochs, the floor is raised before an epoch is used, a journal below the floor is never trusted | `formal-verify`; `tests/recovery.rs::should_enter_safe_mode_when_the_journal_is_below_the_floor_or_does_not_verify` |
| `i25IncompleteNeverAbsence` | I25: no decision reads absence from an incomplete inventory, and admission never opens on one | `formal-verify`; `tests/recovery.rs::should_release_nothing_when_the_inventory_is_incomplete` |
| `i29SafeModeNoEffects` | I29: in a safe session no clone or delete executes | `formal-verify` |
| `safeModeNoDestructiveAction` | in safe mode no directive is issued and no decision or plan action retires, retries or deletes | `formal-verify`; `tests/recovery.rs::should_propose_nothing_destructive_when_the_journal_is_untrusted` |
| `i31PlanBound` | I31: a plan acts only against the heads, inventory and epoch it was drawn from, intent before outcome | `formal-verify` (the `RecoveryPlan` aggregate is KER-10's) |
| `deleteNeedsProof` | a delete carries the recorded instance id and executes only on the VM that has it | `formal-verify`; `tests/recovery.rs::should_delete_only_a_leaked_vm_with_proof_when_any_row_is_decided`, `should_never_delete_when_the_instance_id_differs` |
| `quarantineNeverDeleted` | I28 at recovery: no decision, plan action or directive retires or deletes a quarantined VM | `formal-verify`; `tests/recovery.rs::should_keep_when_the_attempt_is_quarantined` |
| `busyNeverRetiredWhileRunning` | F13 at recovery: a Busy attempt whose VM runs is never retired, whatever GitHub says | `formal-verify`; `tests/recovery.rs::should_never_retire_when_a_busy_vm_runs` |

Tests (`quint test`, stage `formal-test`, all deterministic): one per row of plan §10 the model
expresses (`busyAdoptedTest`, `unreachableIsNotAbsentTest`, `stoppedBusyRetiresWithProofTest`,
`unknownCloneAbsentRetriesTest`, `unknownCloneAppliedAdoptsTest`, `deletingAbsentConfirmedTest`,
`activeRunnerSuspendsCleanupTest`, `incompleteInventoryWaitsTest`, `incompleteThenCompleteTest`,
`instanceMismatchQuarantinesTest`, `quarantinedKeptTest`, `leakedVmDeletedWithProofTest`,
`strayReportedNeverDeletedTest`, `preparedAdoptedTest`,
`preparedWithAnUnknownRunnerQuarantinedTest`), and the crashes and faults around them
(`staleFrameNeverExecutesTest`, `replacedVmNeverDeletedTest`, `crashBeforeFloorRaisedTest`,
`rolledBackJournalIsSafeModeTest`, `restoredBackupIsSafeModeTest`, `unknownKindIsSafeModeTest`,
`planRefusedWhenTheWorldMovesTest`, `recordBeforeAckTest`, `safeSessionExecutesNothingTest`).
Negative controls tried: reading an incomplete inventory as the table's input (dropping
`classify`'s first row) makes simulation of `restartStep` report `[violation]` of
`i25IncompleteNeverAbsence`; retiring a Busy attempt whenever GitHub shows its runner absent
makes it report `busyNeverRetiredWhileRunning`. Simulation runs at about 60 traces of 40 steps
per second (TypeScript evaluator, 4-CPU sandbox).

## Promoting a counterexample

A counterexample from `formal-verify` or a fault-lab verdict is promoted as verification §6.2
step 9 says: reproduce it with the seed, decide which side is wrong (the model first, if the
model is), reduce it to a named run in the model's tests, and keep it as a regression.
