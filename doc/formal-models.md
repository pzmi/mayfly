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
cargo xtask check --only formal-typecheck,formal-test,formal-freshness,conformance-replay   # what the `formal` job of ci.yml runs
cargo xtask formal-verify              # Apalache bounded check + long simulation (not in `check`)
cargo xtask formal-verify --model lifecycle   # one model only: what each job of formal.yml runs
QUINT_SEED=<seed> cargo xtask formal-verify   # repeat a simulation; the base seed is always printed
cargo xtask conformance [--model lifecycle] [--seed S] [--traces K] [--steps N]   # generate and replay (VER-01)
cargo xtask doctor [--all]             # pinned tools; --all adds Java and Apalache
```

`formal-typecheck`, `formal-test`, `formal-freshness` and `conformance-replay` are `check` stages
(job `formal` in `ci.yml`); the last two are VER-01's (verification §6.2): freshness regenerates
every committed regression trace and compares it byte for byte, and the replay generates 10 traces
of 25 steps per step relation of every driven model under a printed seed and replays them with the
regression traces. `formal.yml`'s weekly `conformance` job runs `cargo xtask conformance --traces
1000 --steps 50`. `formal-verify` is the weekly lane of `.github/workflows/formal.yml`, which runs
only on its schedule and by hand (owner, 2026-11-10: "We want to run it on a weekly ci only"),
never on a push or a pull request: a change to a model is verified by dispatching the workflow, or
by running its model locally. The workflow runs one job per model of `MODELS`, in parallel, each
`cargo xtask formal-verify --model <name>` with a timeout of about three times the model's measured
time below (20 min for `smoke`; 45 for `lifecycle`, `link`, `registration`, `scaleset`; 60 for
`scheduling`; 90 for `recovery`; 300 for `event_store`, whose `storeOnlyStep` alone took about 78
min); an xtask test keeps the matrix equal to `MODELS` and the triggers to the schedule and
dispatch. Its logs, Apalache's run directories (a counterexample as ITF and TLA+) and the ITF trace
of a failing simulation are kept in `target/formal-verify/` and uploaded when a run fails.

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
| lifecycle | `formal/lifecycle.qnt` | the `Attempt` aggregate and its saga, the data plane's execution of its directives and the provider's inventory (plan §6, dm §6.4) | `i06CompletedNeverServesAgain`, `busyNeverWarm`, `i10UnknownLeavesOnlyOnObservation`, `i13NoDuplicateClones`, `i14OneJob`, `i21DeregisterBeforeStop`, `i22JitMintedOnceAfterGrantAndVerification`, `i30GuestNeverDecidesOwnership`, `quarantineTerminal`, `quarantineNeverAutoDeleted`, `mismatchStaysQuarantined`, `vanishedClosedForGood`, `noAbsenceFromIncomplete`, `retryKeepsNameAndGeneration` | `step`, `serveStep`, `activationStep` | 8 steps each (for `activationStep`, 8 steps after a one-step preparation: deeper into activation than 8 of `step`) | about 60 s for `step`, 32 s for `serveStep`, 87 s for `activationStep` (Apalache, run two at a time; 4-CPU sandbox, measured at KER-04F); at DEC-04 67 s, 48 s and 97 s; at DEC-05, with `replyTargetAbsent`, 90 s, 53 s and 98 s (`formal-verify --model lifecycle`); at VER-01F, before CP-02's `relaySend`, 143 s, 86 s and 104 s (with another lane building alongside); at KER-04G, with `i21DeregisterBeforeStop`, 119 s, 56 s and 155 s; at DEC-06, with the quarantine stop's deregistration, 187 s, 101 s and 195 s (beside a simulation run); at DEC-07, with `listingIncomplete` and the three vanish invariants, 192 s, 73 s and 252 s (with quint simulations and a test build running alongside), and on top of DEC-06 246 s, 128 s and 254 s (beside conformance sweeps); at DEC-06F, with `runnerExited` in quarantine whatever the JIT, measured on DEC-06's model before DEC-07 was rebased in, 139 s, 67 s and 185 s | KER-04, KER-04F, DEC-04, DEC-05, VER-01F, KER-04G, DEC-06, DEC-07, DEC-06F |
| link | `formal/link.qnt` | the CP↔DP protocol: handshake and version negotiation, run-epoch minting and the floor, at-least-once delivery with loss, duplicates and reordering, fencing, per-epoch idempotency, safe-mode held kinds, the inbox's epoch fence (plan §3.2, ADR 0042) | `i09NoStaleEpochExecutes`, `executesAtMostOncePerEpoch`, `refusedMeansNothingHappened`, `i17EpochsIncrease`, `i18InboxFence`, `i29SafeModeHoldsKinds`, `versionNegotiated` | `step`, `implStep` | 10 steps for `step`, 8 for `implStep` | about 85 s for `step`, 20 s for `implStep` (Apalache through `formal-verify`, run two at a time; 4-CPU sandbox, measured at KER-02) | KER-02 |
| scheduling | `formal/scheduling.qnt` | one host's `Host` aggregate, the `Pool` terms it applies, the capacity policy, the data plane's resource ledger, and each pool's pin with the bases materialised on the host, which the reconciler's pass requires before it admits a pool there (plan §9, dm §6.2, §6.3, P17, ADR 0024; CP-07) | `i01OneSlotPerAttempt`, `i02NoSharedSlotGeneration`, `i03StartsWithinCapacity`, `i07NeverAdvertiseMore`, `i08NothingAfterDrain`, `i13NoDuplicateClones`, `i24CarveOutsFitTheHost`, `i26NoAdmissionWhileClosed`, `noGrantWhileClosed`, `grantsWithinDemand`, `i27BreakerBounded`, `i28QuarantineNeverAutoDeleted`, `i33DataPlaneNeverOvercommits`, `noVmWithoutLease`, `noStartWhileCapacityUnknown`, `generationsMonotonic`, `macosGuestsAtMostTwo`, `externalConsumptionShrinksAdmissible`, `quarantineHoldsCapacity`, `allocationWithinReport`, `carveOutAdmissible`, `leasesKeepTheirBase` | `step`, `admitStep` | 8 steps each | about 7 min for `step`, 3.5 min for `admitStep` (Apalache, run two at a time; 4-CPU sandbox, measured at KER-05; depth 6 took 100 s and 56 s); at DEC-04, with `vmFactsChange`, about 7.7 min and 4 min (two at a time beside another lane's runs); KER-05D's carve ran Apalache out of memory before it checked anything; at DEC-06, the state machine in `scheduling_machine` with the plan carved from counts and both capacity policies chosen by `init`, 489 s and 293 s (`formal-verify --model scheduling`); simulation about 3 min per seed and relation; at CP-07 (placement: `materialise`, `repin`, `evict`), not re-verified by Apalache in the lane (the weekly `formal.yml` run measures it), `quint test` of the 27 scenarios took about 5 min; at CP-09 (`host.jobs`, `grantsWithinDemand`), not re-verified by Apalache in the lane, `quint test` of the 34 scenarios took about 3.6 min | KER-05, DEC-04, DEC-06, CP-07, CP-08, CP-09 |
| recovery | `formal/recovery.qnt` | one host's startup use case across restarts (lock, open with backup, verify, `StartRun`, `MintEpoch`, handshake with the floor, inventory, classification, `CompleteRecovery`), the recovery decision table, the saga's and the data plane's effects after it, the GitHub listener's record-before-ack, safe mode and the recovery plan; crashes at every await, restores, incomplete inventories, stale frames and a provider that changes behind the journal (plan §10, dm §11.3, ADRs 0025, 0026, 0037) | `i04UnrecognisedNeverDeleted`, `i05BaseNeverDeletable`, `i09NoStaleEpochExecutes`, `i10UnknownLeavesOnlyOnObservation`, `i11RecoveryCompletes`, `i12RecordBeforeAck`, `i17EpochsIncrease`, `i25IncompleteNeverAbsence`, `i29SafeModeNoEffects`, `i31PlanBound`, `safeModeNoDestructiveAction`, `deleteNeedsProof`, `noAssertion`, `quarantineNeverDeleted`, `busyNeverRetiredWhileRunning` | `step`, `restartStep`, `repairStep` | 12 steps each | about 8 min for `step`, 3.5 min for `restartStep`, 3.5 min for `repairStep` (Apalache, run two at a time; 4-CPU sandbox, measured at KER-07F with eleven phases and seven GitHub views; KER-07 measured 6, 3 and 2.5 min; depth 8 took about 40 s); simulation about 1.5 min per seed and relation | KER-07 |

| registration | `formal/registration.qnt` | two attempts' `RunnerRegistration` streams, policies P9–P14 around them (mint, delivery and its online confirmation, issue unknown and the reaper with its second listing past the grace, deregister before stop, the busy race, presumed leaks) and GitHub's runner list, lagging behind a lost mint, with a foreign runner of our name shape (dm §6.5, ADRs 0029, 0038, `doc/github-scaleset.md` §6–§7) | `i19OnlyJournaledIdsDeregistered`, `i19NameIssuedOnce`, `noRemintAfterUnknown`, `i20SettledOrOnReaperList`, `deregisterBeforeStop`, `i21BusyCancelsRetirement`, `i22IssuedAndDeliveredOnce`, `issueUnknownLeavesOnlyOnObservation`, `absentOnlyWhenNoRunner` | `step`, `raceStep` | 8 steps for `step`, 12 for `raceStep` | about 5 min for `step`, 1.5 min for `raceStep` (Apalache through `formal-verify`, run two at a time; 4-CPU sandbox, measured at KER-09); simulation about 80 s and 20 s per seed; at DEC-06F, with `requestQuarantineStop` (re-keying a pending retirement), 244 s for `step` and 145 s for `raceStep`, simulation 80 s and 17 s per seed (`formal-verify --model registration`, beside a `quint test` run); at CP-05, with the listing lag, not measured yet (Apalache is the formal lane's) | KER-09, DEC-06F, CP-05 |
| scaleset | `formal/scaleset.qnt` | one pool's `ScaleSet` stream, the listener that polls, records, acquires and acks for it, and GitHub's side: jobs, the message queue with its batch cap and redelivery until delete, sessions with expiry, a lost create and 409s escalated, message ids that restart (A1 refuted, recovered), both assignment flows (github.com's direct assignment capped by the advertised `maxCapacity`, GHES's offers and acquire), runners with the in-guest hook, the refusal sink and the trust gate of ADR 0090, a dormant scale set deleted and re-created (plan §7.2, dm §6.1, ADRs 0024, 0027, 0028, 0029, 0090, `doc/github-scaleset.md` §3–§6) | `i12RecordBeforeAck`, `noDoubleReservationPerRequest`, `i23AcquireBelowDeliverable`, `i32TrustedOnly`, `desiredFromLatestStatistics`, `i07WireWithinDeliverable`, `heldAcquiresNothing`, `messageAppliesOnce`, `acquireUnknownLeavesOnlyOnObservation`, `neverStealsASession`, `redeliveryAppliesNothing`, `a1RefutationIsFlagged`, `persistentConflictsAreFlagged`, `vmOnlyForAJobClassifiedTrusted`, `noStepOfAnUntrustedJobRuns`, `sinkRunsNoStep`, `sinkOnlyForARefusedJob`, `assignedJobsClassified`, `cancelOnlyRefused` | `step`, `messageStep` | 6 steps each | about 2.7 min for `step`, 1.6 min for `messageStep` (Apalache, run two at a time beside another lane's run; 4-CPU sandbox, measured at KER-06F); depth 8 found nothing through state 7 of either relation but took over an hour, one state-8 invariant alone more than 15 min; simulation about 1 min per seed and relation; at GH-00R, with both flows, runners, the trust gate and dormant re-creation, 512 s for `step` and 274 s for `messageStep` (two at a time, beside another lane's build), and after the review's fixes (bounded releases, the release window, the operator release, the duplicate rule) 565 s and 284 s; with the owner's decisions of 2026-11-12 (the run lookup, `pending`, the cancel option, `cancelOnlyRefused`) 272 s and 159 s (the pending gate prunes runner starts); after the final review (`cancelOnlyRefused` reading the truth, a new assignment classified afresh, the `IGNORE_PENDING` and `CANCEL_ANY` mutants) 322 s and 203 s; at GH-00S (the hook, the refusal sink and the pin in place of the hold) 259 s and 155 s; simulation about 85 s per seed and relation | KER-06, GH-00R, GH-00S |
| event_store | `formal/event_store.qnt` | the journal (stream heads, histories, the identity registry, receipts with intent digests and refusals bound to every stream read, the inbox, stream epochs), units of work judged in the port's reporting order and committed in one step or as split transactions with head locks in `StreamId` order and recorded waits, the `AggregateStore` write path (its epoch check and receipt read as one step or with another writer between them), the after-commit kick and a crash before it, the directive relay as a durable consumer (marks, checkpoints, fenced leases, workers), and the online rebuild's shadow generation, dual writes and cutover fence (event-store §7.3, ADRs 0034, 0041) | `ES_I1_atomicUnit`, `ES_I2_streamOrder`, `ES_I3_independentStreams`, `ES_I4_globalEventIdentity`, `ES_I5_commandIdentity`, `refusalReplayedAtSameRevision`, `ES_I6_messageIdentity`, `ES_I7_historyPreservation`, `ES_I11_rebuildEquivalence`, `ES_I12_epochSafety`, `replayWithinEpoch`, `ES_I14_durableDelivery`, `ES_I15_noSkip`, `ES_I16_lockOrder`, `directivesFromJournal`, `reactorsNeverReplayed` | `step`, `implStepSerial`, `relayStep`, `storeOnlyStep`, `deliveryOnlyStep` | 3 steps for `step` and `implStepSerial`, 4 for `relayStep` and `storeOnlyStep`, 8 for `deliveryOnlyStep` | about 12 min for `step`, 12 min for `implStepSerial`, 3.7 min for `relayStep` (re-run at ES-05F with `retire`, which ES-05 added to these three relations: no violation; `step` and `relayStep` two at a time, then `implStepSerial`, beside another lane's builds, 4-CPU sandbox), 78 min for `storeOnlyStep`, 42 min for `deliveryOnlyStep` (Apalache, two runs at a time beside another lane's builds; 4-CPU sandbox, measured at ES-06F, after the model gained `request`, `decide`, `rawRefusal` and intent 3: `step` and `implStepSerial` took 4 min at ES-06); `storeOnlyStep` at depth 5 had not finished checking its fourth state after 40 min, and depth 4 of `step` took 23 min at ES-06: checking the 31 invariant conditions at the last state is most of it; simulation about 3 min per seed and relation (300 traces of 50 steps of `step` ran at 30 traces per second) | ES-06, ES-06F, ES-05F |
| image_store | `formal/image_store.qnt` | one image base on one host as the data plane's image store keeps it (ADR 0087): its registry entry and state machine (Reserved, Cloning, Customizing, Verifying, Ready, Retiring, Quarantined), the VM Tart lists under its name, job clones and the clone intents recorded before them, a crash between any two steps with recovery by observation, and the writers mayfly does not control (a foreign VM under the name, a look-alike in the base's place, a modified disk), and the control plane's view of its `MaterialiseImage` resolved from the image listing (DP-08) | `cloneAllowedOnlyWhenOwnedReadyIntact` (CloneAllowed ⇒ Owned ∧ Ready ∧ IntegrityValid), `deleteAllowedOnlyWhenOwnedStoppedFree` (DeleteAllowed ⇒ Owned ∧ Stopped ∧ NoUnresolvedDependents, never Quarantined), `clonesRecorded`, `materialisedOnlyFromReady`, `noDirectiveWhileUnknown` | `step` | 10 steps | 9 s, then 12 s for the stage once `recoverStop` and the Quarantined guard joined, 9 s once eviction became two steps (Apalache, beside another lane's build; 4-CPU sandbox, measured at DP-07); simulation about 32 s per seed of 5000 traces of 50 steps; at DP-08 (five more actions) not re-verified by Apalache in the lane (the weekly `formal.yml` run measures it), `quint test` of its 24 scenarios under a second and simulations of 5000 traces of 50 steps clean | DP-07, DP-08 |

**Not modelled: the image catalog** (`Image`, `ImagePlacement`, KER-11). Each is one stream with
no rule over another stream's state: `Image`'s rules are a monotonic status and immutable fields,
and `ImagePlacement`'s single pull is its one-directive `pending` overlay, the same mechanism the
lifecycle model already checks for an attempt. Both are pinned instead by their Phase × Command
matrices, their properties over random command sequences
(`tests/image/properties.rs`, `tests/placement/properties.rs` in `mayfly-control-domain`) and
their determinism cases. The cross-stream rules belong to CP-07: admission on a host only once
the pin's base is materialised there and eviction only of a base no live lease references are
`formal/scheduling.qnt`'s `placedOn`, `materialise`, `repin` and `evict`, checked by
`leasesKeepTheirBase` with two negative controls (`startWithoutPlacementViolatesTest`, a start
without the placement check after a repin; `evictWhileReferencedViolatesTest`, an eviction that
does not count leases); withdrawal refused while pinned is a read of every pool
stream at the save (`mayfly_control::placement::withdraw_image`), and a pin refused for a
withdrawn image a read of the image stream at the pin's save (`mayfly_control::images::pin_image`,
OPS-02), neither modelled: each is one unit's read condition, pinned by its component test, and
together they still let a pin of a pool created after the withdrawal's listing commit first
(OPS-02's card records it and the two-step withdrawal that would close it).

**The data plane's image store** (DP-07, ADR 0087) is modelled in `formal/image_store.qnt`: no
model covered it, and its safety rests on the order of durable writes and effects across crashes,
which tests alone sample. One base is enough: the rules are per base (`BaseRecords::hold` and the
admission lock serialise everything on one base), and two job clones show dependents and
intents. The checks read the world exactly (file identity, fingerprint, MAC, stopped state are
observations); a directory swapped between a check and its effect is outside the model as it is
outside the trust model (only root and the daemon's user write `TART_HOME`, checked at start-up),
and clone lineage made outside mayfly is not modelled because Tart keeps none. The simulation
found one modelling gap at DP-07, now in the model: a dropped entry (its base vanished, or an
operator resolved its quarantine) takes its clones off its books, since a base rebuilt under the
name has a new `base_id` and they are not its dependents. A crash while the base was booted
is `recoverStop` (`tart stop`) before `retire`; a Quarantined base is never evicted. Eviction is
two steps, `evict` (writes Retiring) and `retire` (the delete, shared with recovery and with
the same dependents guard), so a crash between them is reached (`crashWhileRetiringTest`).
Negative controls: `evictIgnoringDependentsIsFlaggedTest`, `evictQuarantinedIsFlaggedTest`,
`evictByNameIsFlaggedTest` (no ownership or identity proof), `cloneIgnoringIntegrityIsFlaggedTest`.
**The control plane's view (DP-08):** `cp` is the outcome of the base's one `MaterialiseImage`,
`seen` the last image listing the data plane sent beside a complete inventory (the base only while
it is Ready, proven and intact, and any VM under its name), and `link` the data plane's session,
the session the directive was sent in and a ghost count of issues. Observations carry no order,
so `seen` may be read long after it was taken; `cpEvicted` closes the evict and re-materialise
cycle a stale listing could be read across, and `newSession` ends a session. `resolve` turns an
unknown outcome into materialised only from a base listed by a later session than the send,
into not applied only when nothing is listed under the name, and otherwise does nothing; `issue`
never fires while it is unknown. The build is not tied to the directive (a superset of what a
directive does), so every interleaving of work, crashes and recovery with the view is reached.
`materialisedOnlyFromReady` (every materialisation rests on a base observed Ready, ours and intact
after the directive was issued) and `noDirectiveWhileUnknown` hold; the negative controls are
`listingBeforeReadyIsFlaggedTest` (a base being built listed as a base),
`staleListingIsFlaggedTest` (a listing of the evicted base read as completion of the new
directive) and `issueWhileUnknownIsFlaggedTest`. A stale listing can still make "not applied"
wrong; the model reaches it and no invariant claims that outcome exact (DP-08's residual: one
more, idempotent, directive). How a directive sent before a restart becomes unknown is DP-08's
open question; `answerLost` stands for an outcome that became unknown (today only at the
directive's deadline). The pull is not modelled; the inbox gives it the same later-session rule,
since a listed image can be removed too (a later pull's failed check deletes the entry Tart linked
it to).
**Conformance seam:** each action is one step of `mayfly-adapter-tart`'s `images` (`reserve`,
`beginClone`, `cloneEffect`, `prove`, `boot`, `installAndStop`, `markReady`, the `recover*`
branches of `settle`), `provider::clone_vm` (`jobClone`, `jobCloneIntentOnly`), `evict` and
`retire`; the
observation is the registry entry's state and identity, Tart's listing of the name, and the clone
names Tart lists. Until the driver exists (VER-08), the `ImageStore` contract (IC-01..IC-14) and
the fault cases of `tests/tart/images.rs` pin the Rust side, one per scenario of the model.

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
saga's `IssueNext`, `relaySend` the relay's `DirectiveSent` (CP-02: the directive went on the
link; the attempt's `sent` flag), and `directiveTimeout` the saga's `RecordDirectiveTimeout`
(enabled only once sent, `canTimeOut`; the effect applied or not, then `Unknown`; its own
`timeout` stimulus, so I10 checks that a timeout never leaves `Unknown`); `replySucceeded`, `replyRefused`, `replyProofRefused`, `replyGuestRejected`,
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
evidence about the Rust code through the matrix golden, the Rust tests named below and,
since VER-01, the conformance replay: `mayfly-conformance`'s lifecycle driver runs every action
of a trace as a stimulus of the `Attempt` aggregate through `AggregateStore` over
`MemoryJournal` and compares the folded state with the model's `a` at every step, the initial
one included (`crates/mayfly-conformance/src/lifecycle.rs` names each compared field and what is
abstracted). The replay checks only what the model does: a guard the Rust code lacks is caught
when a trace takes the model's path past it, not when the model disables an action (the driver
never tries an action the model did not take). The committed traces are the scripts of
`lifecycle_traces` (the named runs above as `List[Stim]`, exported to
`formal/traces/regression/`); generated ones come from `step`, `serveStep` and `activationStep`
under a printed seed (`cargo xtask conformance`, ver §6.2).

**Divergences (VER-01, resolved by VER-01F).** The first replays found four disagreements
between the model and the aggregate. VER-01F decided each (which side is right, by the rules
the attempt protects) and fixed the wrong side; each script stays as a regression trace that now
replays clean, and `KNOWN_DIVERGENCES` (`crates/mayfly-conformance/src/lifecycle.rs`) is empty,
its mechanism kept for the next one (a replay that ends in a listed divergence is reported
`KNOWN`, and `should_reproduce_each_known_divergence_when_its_regression_trace_is_replayed`
fails as soon as one no longer reproduces).

| Was | Right side | Fix | Script and pins |
|:--|:--|:--|:--|
| a `StopVm` of unknown outcome resolved by a listing that shows the VM stopped kept the stop counted in the model's `tries`; the aggregate records `VmStopped` and resets them | the aggregate: the listing observes the stop took effect, which is the stop's success (one event, `VmStopped`, as its reply records), so the delete gets its own budget like every directive after a success | model: `inventoryPresent` resets `tries` | `unknownStopResolvedByListing`; `unknownStopResolvedByListingTest`; `tests/attempt/lifecycle.rs::should_resolve_an_unknown_start_or_stop_when_the_inventory_shows_the_vm` |
| a retirement requested while activating outlives a failed fresh verification (`Backoff(activate)`) or a runner exit after a launch of unknown outcome (`Finished`); the model confirmed it there, the aggregate refused `no_retirement_requested` and kept the request set, so the deregistration's confirmation was lost | the model: `ConfirmRetirement` follows the deregistration (I21, P11), so retiring is safe wherever the request still waits; refusing it strands a deregistered runner's attempt with a request nothing can answer | Rust: `ConfirmRetirement` and `WithdrawRetirement` are decided in `Backoff` and `Finished` too; model: neither is enabled once the attempt is terminal (`Lost` keeps the flag; the model resurrected a lost attempt into `Retiring`) | `confirmRetirementInBackoff`, `confirmRetirementAfterExit`; the runs of the same name; `tests/attempt/lifecycle.rs::should_retire_when_a_retirement_is_confirmed_after_the_attempt_backed_off`, `should_retire_when_a_retirement_is_confirmed_after_the_runner_exited`, `should_withdraw_when_a_busy_answer_arrives_after_the_attempt_backed_off`, `should_refuse_a_confirmation_when_the_vm_was_lost_while_a_retirement_waited` |
| a `CloneVm` whose lost reply took effect, then whose VM someone removed, was answered success with instance `NONE`: `Booting` with no instance id | the aggregate: a `VmCloned` carries the instance id of a listed VM (plan §8.2, DP-01: a clone is re-observed by name first, and a delivery that finds no VM clones) | model: `applyEffect` clones whenever no VM of the name exists, so the re-delivery's reply carries the new VM's instance id | `cloneAnsweredAfterRemoval`; `cloneAnsweredAfterRemovalTest` |
| the model's `vanished` was never cleared, so after an operator resolved a vanished case any later quarantine read as vanished again (and a later absence would not release it) | the aggregate: the reason belongs to the quarantine the attempt is in | model: `retire` clears `vanished` (an operator's resolution is the only way out of a vanished quarantine); `vanishedStaysQuarantined` is strengthened to `vanished implies Quarantined`, with no history, so it now also covers a second vanished case after a resolved one | `requarantineAfterVanished`; `requarantineAfterVanishedTest` |

**Deregister before stop in Backoff (KER-04G).** VER-01F's report noted that `requestRetirement`
in `Backoff(activate)` retired at once while a JIT registration was held. It was a real I21 gap,
in the model and in the aggregate alike: with a launch of unknown outcome (or a launch answered
after a restart orphaned its handle), a runner may be registered and idle in the VM; a restart
makes the next fresh verification due, and when it is refused the attempt backs off holding the
registration. A retirement requested there (and, in Rust, also a second request while one
already waited there or in Finished) recorded `AttemptRetired` and the saga stopped the VM before
any deregistration, so a job GitHub assigned meanwhile would have been killed. A spent activation
budget did the same without any request (`AttemptRetired{stage_failed}`). The model had no
invariant for it: KER-04G added `i21DeregisterBeforeStop`, and Apalache found the gap at depth 8
of `activationStep` (`prepareAtOnce`, `recordJitHandle`, `issueNext` (launch), `adopt`,
`replyLaunchUnknown`, `issueNext` (verify), `replyRefused`, `requestRetirement` → `Retiring`).
The fix, in both: Backoff with the registration held waits like Activating and Listening, a
spent activation budget with the registration held backs off and requests the retirement, a
Backoff whose retirement waits issues nothing, and a second request is refused wherever one
waits. Scripts `retirementInBackoffWaitsForDeregistration` and
`stageFailureHoldingRegistrationWaitsForDeregistration` keep it as regression traces.

**Deregister before the quarantine stop (DEC-06, ADR 0076).** KER-04G left the automatic stop of
a quarantined VM outside `i21DeregisterBeforeStop`; the owner decided on 2026-11-10 that it
deregisters first ("deregister"). The ghost now also flags a `StopVm` issued in `Quarantined`
while a runner may be registered and idle in the VM (`runnerMayLive`, not exited) before any
`ConfirmRetirement`, unless an operator resolved a quarantine before. On the model as it stood
the extended invariant fails: simulation of `activationStep` (seed 7) found Listening,
`quarantineCommand`, `issueNext` → `StopVm` in 1.2 s, and `quarantineStopWaitsForDeregistrationTest`
failed. The fix: the attempt's `dereg` (`none`, `requested`, `confirmed`, `busy`) is set to
`requested` on entering quarantine when `stopNeedsDeregistration` holds (a JIT issued, the
runner not exited, not Retiring or Deleting, the VM running with an instance id, no live job);
`nextDirective` issues the quarantine stop only once `stopCleared` (`none`, `confirmed`, or the
runner exited); `confirmRetirement` and `withdrawRetirement` answer it inside quarantine (to
`confirmed` and `busy`, the attempt staying quarantined); and `runnerExited` is recorded inside
quarantine, as the aggregate already did. Scenarios `quarantineStopWaitsForDeregistrationTest`,
`quarantineStopBusyKeepsRunningTest`, the negative controls
`quarantineStopWithoutRegistrationNeedsNoDeregistrationTest` and
`quarantineStopAfterRunnerExitedNeedsNoDeregistrationTest`, and the positive control
`quarantineStopBeforeDeregistrationIsFlaggedTest`; scripts `quarantineStopWaitsForDeregistration`
and `quarantineStopBusyKeepsRunning` keep it as regression traces, and the conformance driver
compares `dereg` with the aggregate's `stop_deregistration`. `registration.qnt` gains
`requestQuarantineStop`, served by P11 like a retirement under `deregisterBeforeStop`.
DEC-06F aligned two differences the review found. `requestQuarantineStop` required no retirement
pending, while the aggregate requests the stop's deregistration when quarantined during a pending
retirement and keys P11's answer on the newer event; the model now re-keys too (enabled unless
the retirement is confirmed), with `quarantineDuringPendingRetirementTest` (the in-flight removal
answers for the stop) and `quarantineDuringPendingRetirementBusyTest` (a busy answer to the old
key does not withdraw the stop; it asks again), and Rust's
`should_request_the_stop_deregistration_when_quarantined_during_a_pending_retirement`.
`runnerExited` inside quarantine required a JIT, while the aggregate accepts it whatever the JIT;
the model now accepts it too (with no JIT the stop reads nothing it changes).

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
evidence about the Rust code through the matrix golden, the Rust tests named below and,
since VER-01, the conformance replay: `mayfly-conformance`'s lifecycle driver runs every action
of a trace as a stimulus of the `Attempt` aggregate through `AggregateStore` over
`MemoryJournal` and compares the folded state with the model's `a` at every step, the initial
one included (`crates/mayfly-conformance/src/lifecycle.rs` names each compared field and what is
abstracted). The replay checks only what the model does: a guard the Rust code lacks is caught
when a trace takes the model's path past it, not when the model disables an action (the driver
never tries an action the model did not take). The committed traces are the scripts of
`lifecycle_traces` (the named runs above as `List[Stim]`, exported to
`formal/traces/regression/`); generated ones come from `step`, `serveStep` and `activationStep`
under a printed seed (`cargo xtask conformance`, ver §6.2).

**Known divergences (VER-01).** The first replays found four disagreements between the model
and the aggregate, each reproduced by a `divergence*` script and listed with its follow-up in
`KNOWN_DIVERGENCES` (`crates/mayfly-conformance/src/lifecycle.rs`); a replay that ends in one is
reported `KNOWN` and not failed, and a test fails as soon as one no longer reproduces
(`should_reproduce_each_known_divergence_when_its_regression_trace_is_replayed`). Neither side
was changed: which side is wrong is the follow-up's decision (ver §6.2 item 9).

| Id | Model | Aggregate |
|:--|:--|:--|
| `unknown-stop-resolved-by-listing-keeps-tries` | an Unknown `StopVm` resolved by a listing showing the VM stopped keeps the stop counted in `tries` | records `VmStopped` and resets `tries`, as a stop's success reply does in both |
| `confirm-retirement-refused-outside-activating-or-listening` | a retirement requested while activating can be confirmed from `Backoff` (a failed fresh verification) or `Finished` (the runner exited after a launch of unknown outcome) | refuses `ConfirmRetirement` there (`no_retirement_requested`) and keeps `retirement_requested` set |
| `clone-answered-after-removal-records-no-instance` | a clone whose lost reply took effect, then whose VM was removed, is answered success with the world's instance `NONE`: `Booting` with no instance id | a `VmCloned` always carries an instance id (the model allows a reply the data plane cannot give) |
| `requarantine-after-resolved-vanished-keeps-the-flag` | `vanished` is never cleared: after an operator resolves a vanished quarantine, any later quarantine reads as vanished again | records the new quarantine's reason |

| Id | Invariant | Pinned by |
|:--|:--|:--|
| `i06CompletedNeverServesAgain` | I06: no job is bound and no activation begins after the attempt finished or was retired | `formal-verify`; `tests/attempt/lifecycle.rs::should_never_return_to_prepared_when_a_runner_has_served` |
| `busyNeverWarm` | no path from Listening, Busy or Finished back to Prepared | `formal-verify`; the same Rust test |
| `i10UnknownLeavesOnlyOnObservation` | I10: when the pending outcome stops being `Unknown`, the last stimulus was an observation | `formal-verify`; `tests/attempt/lifecycle.rs::should_leave_unknown_only_on_an_observation_when_every_command_is_tried` (mutant 27) |
| `i13NoDuplicateClones` | I13: no directive issued while one is pending, every clone targets the one (name, generation), clones bounded by the retry budget | `formal-verify`; `tests/attempt/lifecycle.rs::should_keep_name_and_generation_when_a_stage_is_retried` |
| `i14OneJob` | I14: at most one job is ever bound | `formal-verify`; `tests/attempt/lifecycle.rs::should_bind_one_job_when_a_second_is_offered` |
| `i21DeregisterBeforeStop` | I21 (attempt half, KER-04G): while a runner of the registration may be registered and idle in the VM (a launch sent and not refused, the runner not seen exiting), the attempt retires with its VM running, which commits to its stop, only on `ConfirmRetirement` (P11 had the deregistration's answer; `registration.qnt`'s `deregisterBeforeStop` says when it may confirm); the automatic stop of a quarantined VM likewise waits for that answer (ADR 0076); an operator's resolution is outside it | `formal-verify`; `tests/attempt/properties.rs::should_keep_the_lifecycle_invariants_when_any_two_stimuli_are_accepted` (matrix states `backoff/registered`, `backoff/retirement_waiting`, `quarantined/deregistering`, `quarantined/busy`); `tests/attempt/lifecycle.rs::should_wait_for_the_deregistration_when_a_retirement_is_requested_in_backoff_holding_a_registration`, `should_request_the_retirement_when_the_activation_budget_is_spent_holding_a_registration`, `should_stop_a_quarantined_vm_only_after_the_deregistration_when_a_runner_may_be_registered`, `should_keep_a_quarantined_vm_running_when_github_answers_busy_for_its_deregistration`, `should_keep_waiting_when_the_deregistration_of_a_quarantine_stop_is_not_answered`, `should_stop_a_quarantined_vm_at_once_when_no_runner_can_be_registered_in_it` |
| `i22JitMintedOnceAfterGrantAndVerification` | I22 (attempt half): a JIT handle only with the grant and a verification of the current epoch, at most one launch, no job once the JIT is presumed leaked (a launch of unknown outcome, or a handle a restart orphaned, even with its launch in flight) | `formal-verify`; `tests/attempt/lifecycle.rs::should_refuse_a_jit_handle_when_activation_lacks_a_grant_or_a_fresh_verification`, `should_never_launch_again_when_a_launch_outcome_is_unknown`, `should_record_a_late_launch_and_never_bind_a_job_when_a_restart_orphaned_its_handle` |
| `i30GuestNeverDecidesOwnership` | I30: the recorded instance id is one the provider reported, never a guest's claim | `formal-verify`; `tests/attempt/lifecycle.rs::should_carry_the_recorded_instance_id_when_a_delete_is_issued` |
| `quarantineTerminal` | automation never takes an attempt out of quarantine (only an operator's resolution or an observed absence) | `formal-verify`; `tests/attempt/lifecycle.rs::should_never_delete_when_quarantined_until_an_operator_resolves` |
| `quarantineNeverAutoDeleted` | I28: no `DeleteVm` while quarantined and unresolved | the same |
| `mismatchStaysQuarantined` | DEC-07 (i): while a VM is present under the attempt's name with another instance id than the recorded one, a quarantined, unresolved attempt stays quarantined | `formal-verify`; `tests/attempt/lifecycle.rs::should_keep_a_mismatched_vm_quarantined_when_it_is_listed_until_it_is_absent`; control `mismatchReleasedFromQuarantineIsFlaggedTest` |
| `vanishedClosedForGood` | DEC-07 (ii), ADR 0066 as amended 2026-11-11: a recorded VM absent from a complete listing without our delete is a recorded desync (`vanished`, never cleared), and the attempt is `Lost` then `Closed` with nothing issued after; a recorded VM's absence is never a silent `Lost`, and an unrecorded VM never vanishes. Replaces DEC-05's `vanishedStaysQuarantined` | `formal-verify`; `tests/attempt/lifecycle.rs::should_close_as_vanished_when_a_stop_or_start_finds_its_vm_absent`, `should_close_as_vanished_when_the_inventory_shows_a_recorded_vm_absent`, `should_record_no_vanish_when_the_refusal_is_not_a_recorded_vm_s_absence`; `matrix/attempt.txt` (`lost/vanished_on_start`); controls `silentLossOfRecordedVmIsFlaggedTest`, `resurrectionAfterVanishIsFlaggedTest`; conformance `should_catch_a_forgotten_desync_at_the_first_divergent_step_when_the_inventory_finds_a_vm_gone` |
| `noAbsenceFromIncomplete` | DEC-07 (iii), I25: an incomplete listing (`listingIncomplete`: the inventory's `InventoryIncomplete`, or `Unknown{listing_incomplete}` for a re-observing directive) moves no phase and records no vanish; at most it makes the pending outcome `Unknown` | `formal-verify`; `tests/attempt/lifecycle.rs::should_conclude_no_absence_when_a_stop_is_reobserved_on_an_incomplete_listing`; control `vanishFromIncompleteListingIsFlaggedTest` |
| `retryKeepsNameAndGeneration` | every `CloneVm`, first or retried, targets the name and generation fixed at start | `formal-verify`; `tests/attempt/lifecycle.rs::should_keep_name_and_generation_when_a_stage_is_retried` |

Tests (`quint test`, stage `formal-test`, all deterministic): `happyPathTest`,
`unknownCloneResolvedByInventoryTest`, `unknownCloneNotAppliedBacksOffTest`,
`stageFailureBacksOffThenRetiresTest`, `guestRejectedIsQuarantinedTest`,
`activationNeedsGrantAndFreshVerificationTest`, `launchUnknownNeverServesTest`,
`busyServesOneJobTest`, `retirementWaitsForDeregistrationTest`,
`replacedVmQuarantinesOnDeleteTest`, and from KER-04F `adoptDuringLaunchNeverServesTest` (the
review's counterexample, kept as a regression), `deleteRefusedRepeatedlyQuarantinesTest`,
`quarantinedStopsAreBoundedTest`, `prepareAtOnceTest`, and from DEC-04 (ADR 0053)
`deleteOfAbsentVmIsDoneTest` and `deleteLostThenAnsweredDeletedTest`: a delete's success reply
is enabled when the provider holds no VM (the data plane answers a delete whose target a
complete listing does not list `VmDeleted`), so a lost delete reply is resolved by the answer
to its re-delivery as well as by the inventory (`inventoryAbsent` already read absence as
`Deleted` for a pending delete; a stop of a stopped VM was already a success), and from DEC-05
(ADR 0066) `stopOfVanishedVmQuarantinesTest` and `startOfVanishedVmQuarantinesTest` (renamed
by DEC-07 `stopOfVanishedVmIsClosedAsDesyncTest` and `startOfVanishedVmIsClosedAsDesyncTest`),
and
from CP-02 `timeoutIsUnknownNeverSuccessTest` (a sent clone whose deadline passed is `Unknown`,
issues nothing new, and is resolved by the inventory) and `heldDirectiveNeverTimesOutTest` (an
issued, unsent directive cannot time out; its tries are unchanged when it is sent), with the
invariants `timeoutOnlyAfterSend` and `timeoutNeverAdvances` (a timeout leaves the phase where it
was and only makes the outcome `Unknown`); negative controls tried: dropping `x.sent` from
`canTimeOut` makes simulation of `step` report `[violation]` of `timeoutOnlyAfterSend`, and
letting the timeout apply the success transition reports `[violation]` of
`timeoutNeverAdvances`, each within a second. Replies are not guarded by `sent`: the relay records
`DirectiveSent` after the link took the frame, so an answer may be recorded first, which the
model's unguarded replies cover: a stop or
start whose recorded VM the provider no longer holds is answered `target_absent`
(`replyTargetAbsent`), which DEC-05 quarantined as a desync and DEC-07 (ADR 0066 as amended
2026-11-11) records and closes instead (`vanish`: `Lost` with the `vanished` flag, inside
quarantine too; `inventoryAbsent` does the same for a recorded VM, and releases an unrecorded
quarantined one to a plain `Lost`); and a start may be
refused its proof too (`replyProofRefused`, an over-approximation for a start: the model's world
has no listed-but-unproven VM), and from VER-01F `unknownStopResolvedByListingTest`,
`confirmRetirementInBackoffTest`, `confirmRetirementAfterExitTest`,
`cloneAnsweredAfterRemovalTest` and `requarantineAfterVanishedTest` (the resolved divergences
above; DEC-07 renamed the last `requarantineAfterResolutionTest`, a quarantine for a replaced
VM, since a vanish no longer quarantines), and from KER-04G `retirementInBackoffWaitsForDeregistrationTest`,
`stageFailureHoldingRegistrationWaitsForDeregistrationTest` (both regression traces too),
`stopAfterRefusedLaunchNeedsNoDeregistrationTest`, `stopAfterRunnerExitedNeedsNoDeregistrationTest`
and `stopBeforeDeregistrationIsFlaggedTest` (controls of `i21DeregisterBeforeStop`), and from
DEC-07 `inventoryFindsVanishedVmTest`, `quarantinedVmVanishedIsClosedTest`,
`mismatchedVmStaysQuarantinedTest` and `incompleteListingConcludesNothingTest` (each a
regression trace too, with the two renamed vanish scripts), and the controls
`vanishFromIncompleteListingIsFlaggedTest`, `silentLossOfRecordedVmIsFlaggedTest`,
`resurrectionAfterVanishIsFlaggedTest` and `mismatchReleasedFromQuarantineIsFlaggedTest`.
`listingIncomplete` (DEC-07) is the incomplete listing: the conformance driver sends
`OutcomeUnknown` for it only when a clone, start, stop or delete is in flight, and nothing
otherwise (the inventory's is never routed to the attempt). Stops and deletes count against the
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
`i10UnknownLeavesOnlyOnObservation`; letting `inventoryAbsent` release a vanished quarantine
(DEC-05) made simulation of `step` report `[violation]` of the then `vanishedStaysQuarantined`
within a second. DEC-07's scratch mutations, each found by simulation of `step` in about half a
second and by its own invariant only: `inventoryAbsent` reading a recorded VM's absence as a
plain `Lost` (`vanishedClosedForGood`), `listingIncomplete` concluding the stop's target absent
(`noAbsenceFromIncomplete`), and `inventoryPresent` closing a quarantined attempt whose VM is
listed under another instance id (`mismatchStaysQuarantined`).

**The saga (CP-02).** `mayfly_control_domain::attempt::next_step` is `issueNext`'s guard read as
a policy: it issues exactly when `canIssueNext` holds (`issuable`, pinned against the aggregate by
`tests/attempt/saga.rs::should_issue_exactly_when_issue_next_would_be_accepted`, and since
VER-01F against the model too: the lifecycle driver issues the model's `issueNext` only when
`next_step` answers `Issue`, else records the refusal `saga_idle`), waits on an
in-flight directive until its deadline, closes a deleted or lost attempt, and does nothing while
an outcome is `Unknown` or the pending directive is not sent yet. **Not modelled:** time (the
deadline per directive kind fixed at the first send, the reply grace,
the aggregate's refusal of a timeout before the deadline, `deadline_not_reached`, pinned by
`tests/attempt/lifecycle.rs::should_make_the_outcome_unknown_never_success_when_a_deadline_passes`);
the saga's timers being memory that startup re-arms (`AttemptSaga::reconsider`, CP-04); and the
saga reading the attempt's current state rather than the event that woke it, so a late or repeated
event issues nothing the model's `issueNext` would not (`crates/mayfly-control/tests/store/saga.rs`).

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
| `i09NoStaleEpochExecutes` | I09: no frame of an epoch older than the floor executes | `formal-verify`; `crates/mayfly-protocol/src/handshake.rs::should_fence_frames_by_the_session_epoch_when_a_directive_arrives`; the data plane's fencing table `crates/mayfly-data-domain/tests/data_plane/matrix.rs::should_fence_every_frame_when_it_meets_each_session_state_and_table_entry` and `executor.rs::should_answer_unknown_stale_epoch_and_execute_nothing_when_a_frame_is_of_an_older_session` (mutant 13); the link contract's LK-15 (`crates/mayfly-testkit-shared/src/link_contract.rs`: a transport hands up a directive of any epoch, so the fencing stays the executor's) |
| `executesAtMostOncePerEpoch` | each directive id executes at most once per epoch, under re-sends and duplicates | `formal-verify`; `crates/mayfly-data-domain/tests/data_plane/executor.rs::should_execute_once_and_answer_the_first_outcome_when_a_directive_id_repeats` (mutant 17); later the link contract |
| `refusedMeansNothingHappened` | a definite refusal names an (epoch, directive) that never executes; an applied answer one that did (ADR 0042) | `formal-verify`; `staleFrameIsNeverRefusedTest`; the golden `observation/v1.unknown-stale-epoch.json` |
| `i17EpochsIncrease` | sessions are entered at strictly increasing epochs, the floor equals the open session's epoch and never exceeds what was minted | `formal-verify`; `handshake.rs::should_mint_above_the_journal_and_the_floor_when_a_session_opens`, `should_enter_only_above_the_floor_when_a_welcome_arrives` |
| `i18InboxFence` | the inbox applies no answer older than the newest it applied | `formal-verify`; `olderAnswerIgnoredTest`; the `Attempt` epoch fence (KER-04) |
| `i29SafeModeHoldsKinds` | in a safe session no held kind executes | `formal-verify`; `directive.rs::should_hold_exactly_the_effect_kinds_when_in_safe_mode`; `crates/mayfly-data-domain/tests/data_plane/executor.rs::should_refuse_held_kinds_and_run_the_rest_when_the_session_is_safe` |
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

**The data plane's executor (KER-08)** implements this model's `dpWelcome` and `dpDeliver` in
`mayfly-data-domain` (`link::step`, `DataPlane::receive`): discard a frame of another version
or of a session not entered, answer an older session's frame `OutcomeUnknown{stale_epoch}`,
answer a repeated id from the session's table, refuse a held kind in safe mode, then execute.
The proof re-checked against the re-observed instance id is `lifecycle`'s (`replyProofRefused`:
a stop or delete runs only while the provider holds the recorded instance; a delete whose
target is gone is done, ADR 0053), and the fit is `scheduling`'s `dpFits` (nothing starts
while the memory it needs is unknown, ADR 0052). This model is unchanged by DEC-04: it states
fencing and the outcome table, not the effects; `listing_incomplete` is one more `Unknown`. **Not modelled** (each pinned by the data plane's unit tables only,
fixtures `crates/mayfly-data-domain/tests/fixtures/matrix/*.txt`): the order of the admission
checks after the table (deadline, bounded strings, `protected_target`, the proof's own
consistency, the fit), which `dpDeliver` folds into one nondeterministic effect; execution as
two steps, so a repeat while a directive runs (`Pending`) and the reservations held for an
`Unknown` outcome until a complete inventory; and the VM supervisor (`supervisor::state`: the
provider's listing × the shim's lock and `exit.json`, `ProcessLost`, ADR 0037), which no model
states. A two-step `dpDeliver` (start, finish) is the extension to make for the bounded concurrency
DP-01 added; the supervisor joins `recovery` when CP-04 re-adopts VMs from it. **Not modelled
either (DP-01, `mayfly-data::executor`)**, each pinned by the executor's component tests
(`crates/mayfly-data/tests/executor/`): the concurrency bound and one directive per target at a
time; a stage whose deadline passed while it waited is refused `deadline_expired`; an effect
still running at its deadline is answered `Unknown{deadline}` and keeps its target, slot and the
listing gate until it completes; a `CloneVm` or `StartVm` re-observed by name before its effect
(a re-delivery answered from the listing); the observer's listings serialised against effects,
no new listing-changing effect while a pass is due or running, an overlapped listing discarded
(the `gate`); `ReportInventory` answered from the next clean listing or
`InventoryIncomplete{deadline}`. None of these changes what `dpDeliver` may answer; a two-step model would add them.

**The relay's sink (CP-02, `mayfly_control::delivery::LinkSink`)** is this model's `relaySend`
with three restrictions the model leaves out, each removing behaviours and adding none: it sends a
directive only while it is the attempt's pending, in-flight directive decided under the session's
epoch (so a late directive of a stale relay worker is dropped at the control plane; the one that
was already on its way is answered from the data plane's table, `executesAtMostOncePerEpoch`); it
holds the kinds `held_in_safe_mode` names while the host's session is safe, before the data
plane's own hold (`i29SafeModeHoldsKinds` therefore holds twice); and it re-sends only on a
redelivery of the relay, never on a timer: "until the answer is definite" is the deadline, past
which the saga's timeout makes the outcome `Unknown` (`lifecycle`'s `directiveTimeout`). Pinned by
`crates/mayfly-control/tests/store/link_sink.rs` and, across both planes,
`crates/mayfly-composition/tests/directive_loop/main.rs`.

### scheduling

One host and two pools (`linux` with 1-unit VMs, `macos` with 2-unit VMs and G3's two guest
slots), three attempt ids and three slots. The data plane (`dp`, `vms`, `sent`) is the truth:
foreign consumption, our VMs and the capacity reports it has sent, which the host applies late
and out of order (`recordReport`: a newer `id` wins, an older one is ignored). The host
(`host`, `leases`, `slotGen`, `terms`) is the journal: `reserve` takes the lowest free slot
under its next generation if the pool's applied terms are open, the gate is open, the pool is
under `max_running` (quarantined leases counted) and the VM fits the last report on every
dimension it uses; `grant` (with the gate and the pool's terms open, and, as the reconciler's
pass plans it, only a started lease and only while the pool has fewer granted leases than
`host.jobs`, the jobs a runner may start for: the fewer of the shortfall and the room
`demand::runner_may_start` leaves, which the world's `jobLeaves` lowers; one atomic step
because the pass saves it with the host stream expected where it read it and reads of the other
host streams, the attempt, the pool, the scale set and the pool's retiring runners, CP-09; one
host, a second host's grant being covered by those reads in the Rust tests, and a runner's
retirement withdrawn after the read, which the model does not represent, pinned by
`crates/mayfly-control/tests/store/activation.rs::should_grant_nothing_when_a_retiring_runner_binds_a_job_after_the_read`), `quarantine`, `stopQuarantined` (a stopped quarantined lease
shrinks to disk and slot), `release` (only on the observed absence of a VM that was created),
`releaseUnstarted` (`attempt_never_started`: only while the attempt stream is absent, the
read the reconciler's save carries, CP-03F), the gate, the drain
and `applyTerms` (policy P4, only at a newer revision; one atomic step because the Rust pass saves it with a read of the pool stream at the revision it read, CP-08) are its other commands. The pools
(`pools`) pause, resume, count failures and trip (I27), and reset by an operator.
`startAttempt` is P2's `StartAttempt`, saved with a read of the host stream that shows the
lease reserved (CP-03F). `dpCreate` starts a started, unquarantined attempt's VM (the data plane
sees no lease) only if it fits the data plane's own ledger (I33); `foreignChanges` and
`vmVanishes` are the world. `planOf` is `capacity::plan`: free capacity after every live
lease, carved in pool-name order up to each pool's headroom within what the open pools before
it keep (`max_running` minus their live runners, whatever they were carved), and `reportedMaxOf`
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
| `i33DataPlaneNeverOvercommits` | I33: the data plane starts nothing that does not fit its ledger, whatever the host allocated | `formal-verify`; `crates/mayfly-data-domain/tests/data_plane/executor.rs::should_refuse_insufficient_resources_whatever_the_cp_allocated_when_a_clone_does_not_fit` (mutant 24), `should_refuse_the_second_clone_when_the_first_reserved_the_last_slot`; the tables `matrix/admission.txt` and `matrix/ledger.txt` |
| `noVmWithoutLease` | CP-03F: the data plane never creates a VM for a lease the host released, so a slot is never reserved again under a live VM (`staleUnstartedReadKeepsTheSlotTest`, `releasedLeaseNeverStartsTest`; its negative control `staleReleaseWithoutGuardViolatesTest`) | `crates/mayfly-control/tests/store/reconciler.rs::should_keep_the_slot_when_the_attempt_starts_between_the_read_and_the_release`, `should_release_an_unstarted_lease_and_refuse_its_late_start_when_the_release_commits_first`; the CP-03 simulation's "no lease" check |
| `noStartWhileCapacityUnknown` | ADR 0052: the data plane starts nothing while the memory it needs is not known | `formal-verify`; `crates/mayfly-data-domain/tests/data_plane/ledger.rs::should_refuse_capacity_unknown_and_never_undercount_when_a_running_vm_has_unknown_memory`; `matrix/ledger.txt` |
| `macosGuestsAtMostTwo` | G3: at most two running macOS guests, foreign included; no reservation counts a third | `tests/host/rules.rs::should_admit_two_macos_guests_at_most_when_foreign_guests_run`, `tests/capacity.rs::should_never_admit_a_third_macos_guest_when_inputs_are_random` |
| `i07NeverAdvertiseMore` | I07: a pool's `reported_max` is within what it can deliver | `tests/capacity.rs::should_fit_every_carve_out_together_when_inputs_are_random` |
| `i24CarveOutsFitTheHost` | I24: the carve-outs fit the host's free capacity together | the same |
| `externalConsumptionShrinksAdmissible` | more foreign consumption never frees capacity or carves more | `tests/capacity.rs::should_never_admit_more_when_foreign_consumption_grows` |
| `carveMonotone` (pure, `carveMonotoneTest`) | no pool's carve grows when the free capacity shrinks or shrinks when an earlier pool's running or stopped quarantined lease is released, in either order of a larger and a smaller VM; the carve-outs fit together; each pool is carved at least what KER-05H's keep carved (KER-05H: the state space carves the smaller Linux VM first only, where handing a later pool the leftover is monotone, so the greedy carve's fault was out of its reach; KER-05D: pool by pool, and the quarantine keep of ADR 0069) | `tests/capacity.rs::should_carve_no_more_when_foreign_use_leaves_a_large_vm_unfit`, `should_carve_no_less_when_a_quarantine_of_an_earlier_pool_is_released`, `should_never_admit_more_when_foreign_consumption_grows`, `should_carve_no_pool_more_when_foreign_consumption_grows_and_inputs_are_random`, `should_carve_no_pool_less_when_a_quarantined_lease_is_released_and_inputs_are_random`, `should_carve_every_pool_at_least_the_whole_place_keep_when_inputs_are_random`, `should_hold_quarantined_capacity_when_inputs_are_random` |
| `i08NothingAfterDrain` | I08: no reservation and no grant after drain start; the drain completes with no lease | `tests/host/rules.rs::should_admit_nothing_when_the_host_drains`, `tests/capacity.rs::should_admit_nothing_and_report_live_runners_when_admission_is_closed` |
| `grantsWithinDemand` | CP-09 (P9): no grant leaves its pool with more granted leases than jobs a runner may start for at its commit, racing passes and jobs leaving included (`racingGrantKeepsTheBoundTest`, `grantOnlyAStartedAttemptTest`, `jobLeavingBeforeTheGrantKeepsTheBoundTest`; negative controls `grantOnStaleReadViolatesTest` and `grantOnStaleScaleSetReadViolatesTest`, grants saved without the read) | `crates/mayfly-control-domain/tests/reconcile.rs::should_grant_within_the_shortfall_and_the_trusted_room_when_attempts_are_prepared`, `should_grant_within_the_shortfall_and_the_trusted_room_when_inputs_are_random`; `crates/mayfly-control/tests/store/activation.rs::should_grant_no_second_attempt_when_a_racing_pass_read_before_the_first_grant`, `should_grant_no_second_attempt_when_a_racing_pass_on_another_host_read_before_the_first_grant`, `should_grant_nothing_when_a_job_leaves_the_scale_set_before_the_pass_applies_its_capacity`, `should_grant_no_more_than_the_shortfall_when_more_attempts_are_prepared`; the fleet simulation's P9 check, behind the journal too (`should_grant_within_the_demand_when_a_seeded_simulation_reads_behind_the_journal`) |
| `i26NoAdmissionWhileClosed`, `noGrantWhileClosed` | I26: no reservation and no grant with the gate or the pool's terms closed | `tests/host/rules.rs::should_refuse_a_grant_when_any_term_or_cause_is_closed` |
| `i27BreakerBounded` | I27: failures bounded by the threshold, a tripped breaker published closed, nothing reserved under terms that show it | `tests/pool/rules.rs::should_trip_at_the_threshold_and_count_nothing_more_when_failures_repeat`, `should_close_the_terms_when_the_breaker_of_the_pin_trips` |
| `i28QuarantineNeverAutoDeleted`, `quarantineHoldsCapacity` | I28: a quarantined VM is never deleted by automation, its lease released only on the VM's absence, counted against capacity and `max_running` | `tests/host/rules.rs::should_hold_capacity_when_a_lease_is_quarantined`, `tests/capacity.rs::should_hold_quarantined_capacity_when_inputs_are_random` |
| `i13NoDuplicateClones` | I13: at most one VM is created per lease | `formal-verify` (the attempt's retries are `lifecycle`'s) |
| `carveOutAdmissible` | whatever the policy carves out, the host admits | `tests/host/properties.rs::should_admit_every_planned_reservation_when_the_host_decides_them` |

Tests (`quint test`, stage `formal-test`, all deterministic so each runs once):
`fillsThenRefusesTest`, `generationsIncreaseTest`, `macosAtMostTwoTest`,
`staleReportIgnoredTest`, `drainAdmitsNothingTest`, `quarantineHoldsCapacityTest`,
`quarantineConsumesPoolTest`, `breakerTripsTest`, `pauseReportsLiveOnlyTest`,
`termsFollowEveryBumpTest` (CP-08: P4's terms after each bump, never a revision twice),
`unplacedStartWaitsForItsBaseTest` and `unplacedLeaseReleasedAfterTheGraceTest` (CP-10: the
reconciler's pass drives `startAttempt`, planning it only where the host would admit and the pin
is placed, stricter than its guard; a lease whose pin is not placed waits, and one never placed
is released after the start grace),
`racingGrantKeepsTheBoundTest`, `grantOnlyAStartedAttemptTest`,
`jobLeavingBeforeTheGrantKeepsTheBoundTest` and the negative controls
`grantOnStaleReadViolatesTest` and `grantOnStaleScaleSetReadViolatesTest` (CP-09: `host.jobs`,
chosen by `init` from 0 to `MAX_RUNNING` per pool and lowered only by `jobLeaves`, in both
relations; `initWith` gives every pool `MAX_RUNNING`),
`dataPlaneRefusesWhatDoesNotFitTest`, and from DEC-04 (ADR 0052)
`unknownMemoryStartsNothingTest`, from KER-05H `carveMonotoneTest`, and from KER-05D the
candidate carve policies of `doc/domain-model.md` §6.3.1 (ADR 0069): `carvePoliciesTest` (each
candidate against safety, monotonicity pool by pool and in total, and release monotonicity, over
a small host), `carveScenariosTest` (the usefulness table) and `sustainedLoadTest` (which
candidates starve a waiting macOS job under sustained Linux load). The candidates are pure
functions beside `planOf`; `capacity::plan` implements `EXACT` (`carveTwo`) and, from DEC-06
(owner, 2026-11-10: "implement both now"), `DEMAND` per pool. The policy is a parameter of the
state machine: `host.policy` and `host.waiting` give each pool its policy and the jobs it is told
wait for it, chosen by `init` (both policies, 0 to `MAX_RUNNING` jobs) and fixed for the run, and
`shapeOf` cuts a `DEMAND` pool's headroom to them (`demandShape`), so every invariant of
`allInvariants`, Apalache's bounded runs included, is checked under both policies for a fixed
demand; `initWith` keeps every pool `EXACT` for the scenarios. DEC-06's tests:
`carveMonotoneDemandTest` (`carveMonotone`'s properties for `DEMAND` with every fixed number of
waiting jobs, and the later pool carved at least what `EXACT` carves it),
`demandFixesThe13GiBCaseTest` (S1 and S2), `demandStarvesWhenDemandIsInvisibleTest` (sustained
load seen only through `maxCapacity`: the real behaviour, since GitHub assigns nothing to a
scale set advertising 0, `doc/github-scaleset.md` §5) and
`demandLetsTheLaterPoolInTest` (the state machine: an idle `DEMAND` Linux pool keeps nothing and
the macOS pool is carved and admitted, where `EXACT` carves it nothing).

**Two modules (DEC-06).** `formal/scheduling.qnt` holds the state machine in
`scheduling_machine` (state, actions, relations, invariants) and everything else in `scheduling`,
its main module, which imports it: the candidate policies, `carveMonotone`, its variants and every
scenario. `quint test` and `quint run` read `scheduling`; `formal-verify` gives Apalache
`--main scheduling_machine` (`VERIFY_MAIN` in `tools/xtask/src/formal_verify.rs`). Apalache's
inliner expands every definition of the module it is given and copies an operator's argument at
every use: since KER-05D the candidate policies (a carve that depends on another carve, folded over
every free capacity) and the plan's `Shape` read field by field through `less` and `fitIn` ran it
out of its 3 GB heap in `InlinePass` before it checked anything, even with a single invariant at
depth 1 and with 11 GB (bisected at DEC-06: KER-05H's model checked `admitStep` to depth 2 in 74 s,
KER-05D's ran out of memory). So the state machine's `planOf` carves from counts (`carveCounts`:
the keep and the fit as sums of indicators over `1..MAX_RUNNING`, no floors, no nested minima),
and `carveCountsTest` checks that it equals `carveTwo` over the pools' shapes (`shapeOfCounts`)
on every free capacity of a small host, both orders, every count of runners and of running and
stopped quarantined leases of either pool, every cap of either pool (`EXACT`, and `DEMAND` with
every number of waiting jobs) and either pool closed (`carveCountsAgree`), and that those shapes
are KER-05D's, folded from the leases (`shapeOf`), for every assignment of leases, either policy,
every waiting count and the gate and terms open or closed (`shapesFromLeasesAgree`, DEC-06F; the
test takes about two minutes); `admitStep` then checks to depth 2 in 24 s.
`dp.memUnknown` abstracts every fact the ledger lacks
(`vmFactsChange` toggles it in both relations): `dpFits` is false and `dpReport` delivers no
memory while it holds, and the invariant `noStartWhileCapacityUnknown` (pinned by
`crates/mayfly-data-domain/tests/data_plane/ledger.rs::should_refuse_capacity_unknown_and_never_undercount_when_a_running_vm_has_unknown_memory`
and the `matrix/ledger.txt` rows) states that the data plane starts nothing meanwhile. The
other unknown dimensions (guest, disk, a listing not read) are pinned by the data plane's
tables only: a model limitation, kept because each further flag doubles the nondeterministic
state of both relations whose Apalache runs already take about 7.7 and 4 minutes, while the
ledger's `matrix/ledger.txt` enumerates every dimension (DEC-04F). The invariant is implied by
`dpFits`' `not(dp.memUnknown)` conjunct, so it is only as strong as that guard: mutant 28
(verification §9) removes the conjunct, and simulation of `step` then reports `[violation]` of
`noStartWhileCapacityUnknown` (`vmFactsChange` to unknown, then `dpCreate`) and
`unknownMemoryStartsNothingTest` fails at its first `expect`; `i33DataPlaneNeverOvercommits`
alone would not see it, since it reads the same `dpFits`. Negative controls tried: removing the capacity guard of
`reserve` (mutant 1) makes simulation of `admitStep` report `[violation]` of
`allocationWithinReport`; reserving every slot at generation 1 (mutant 2) makes it report
`i02NoSharedSlotGeneration`; dropping the gate guard of `grant` makes simulation of `step`
report `noGrantWhileClosed`. Simulation runs at about 38 traces of 50 steps per second
(TypeScript evaluator, 4-CPU sandbox).

### recovery

One host after an earlier run of epoch 1 ended, cleanly or not: two attempt records (each in
any of Reserved, Cloning, Verifying, Prepared, Activating, Listening, Busy, Finished, Deleting,
Deleted or Quarantined, with its pending directive's outcome unknown), the provider's VMs (each running or stopped, under a live shim or not, with
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
record, ack). In safe mode `proposePlan` draws a plan bound to the journal's heads per stream (`j.heads`:
the host run's and each attempt's revision), the inventory and the epoch, an operator approves
it, and each action is an intent (`planIntentWith`, only while bound), the attempt command's own
unit (`planActWith`, which reads every bound head and is disabled once any moved: ADR 0041) and
the outcome (`planOutcomeWith`, which advances only the action's own head and only from where the
plan bound it, KER-10F); `foreignWriteWith` is another writer appending to an attempt's stream
in safe mode,
the plan is refused when the world moved (`refusePlan`), and `completePlan` leaves safe mode for
a verified restart. `restore` puts the pre-migration backup back while the daemon is down (ADR
0043) and `worldChange` makes a VM vanish, be replaced under its name, stop or lose its shim.

`classify` is the abstraction of `mayfly_control_domain::operations::recovery::classify` for
the inputs the model generates, every name included: the `mf-*` name no record names goes
through it too (`classifyStray`), so I04 reads its decision. GitHub's view (`GH_VIEWS`) stands
for the Rust `GithubView`: what the journal records of the registration (`not_registered`,
`issued_not_delivered`) and what GitHub shows of the runner; the registration's own lifecycle
(issue, deliver, deregister before stop) is the registration model's (KER-09). Two orderings
are decided on safety grounds and are the same in both: a Prepared attempt reads its
registration before its VM's liveness, so a runner GitHub knows for an attempt that never
activated quarantines the VM (`jit_presumed_leaked`) stopped or not, keeping the evidence,
where retiring a stopped one would delete it on the doubt and leave a runner with no journaled
id that the reaper may only report (ADRs 0025, 0029;
`preparedStoppedWithAnUnknownRunnerQuarantinedTest`,
`tests/recovery.rs::should_quarantine_when_a_prepared_vm_has_a_runner_github_knows`); and an
Activating attempt whose runner GitHub shows busy is adopted and monitored, never retired under
its job (F13), while every other registered view presumes the JIT leaked (I22, P13; the
deregistration that precedes any stop waits for GitHub, so a job hidden behind `unreachable`
is refused 422, never killed) (`activatingBusyRunnerAdoptedTest`,
`activatingIssuedNotDeliveredPresumedLeakedTest`,
`tests/recovery.rs::should_decide_each_plan_row_when_its_inputs_are_given`).

Rust-only rows, which the model leaves out because they add states and no behaviour: `NotStarted`
and `Closed` (the stream's two ends: a present VM is reported, an absent one ignored), pinned by
the golden `tests/fixtures/matrix/recovery.txt`
(`tests/recovery.rs::should_define_every_row_when_each_view_meets_each_other`); and
`Backoff{stage}`, which decides exactly as the phase its retry re-enters
(`tests/recovery.rs::should_decide_as_its_stage_when_the_attempt_backs_off`). The golden lists
only the (phase, pending directive, recorded instance id) triples the attempt aggregate can
reach (`tests/recovery.rs::reachable`); the rules range over every combination.

`step` offers every action; `restartStep` leaves out GitHub and the plan, so a bounded
check spends its depth on the pipeline, its crashes and its effects; `repairStep` leaves out the
saga and the data plane (which safe mode holds anyway), because a uniform walk of `step` almost
never strings proposal, approval, two intents, two outcomes and completion together without a
fault in between.

| Id | Invariant | Pinned by |
|:--|:--|:--|
| `i04UnrecognisedNeverDeleted` | I04: the name no record names is only reported or ignored by `classify`, no directive names it, and none deletes it | `formal-verify`; `crates/mayfly-control-domain/tests/recovery.rs::should_never_delete_when_an_mf_vm_is_unknown_to_the_journal` |
| `i05BaseNeverDeletable` | I05: no directive targets the image base, which always exists | `formal-verify`; `tests/recovery.rs::should_never_touch_when_the_vm_is_a_base_or_foreign` |
| `i09NoStaleEpochExecutes` | I09: no frame takes effect outside its own session or below the epoch its attempt accepted | `formal-verify` (the link's own fencing is `link`'s) |
| `i10UnknownLeavesOnlyOnObservation` | I10: an `Unknown` outcome is cleared only by a classification or an observation of a complete inventory | `formal-verify`; `tests/recovery.rs::should_assert_no_outcome_when_any_row_is_decided` |
| `noAssertion` | every resolution of an `Unknown` outcome agrees with the inventory: a present VM is never read as a clone not applied or as deleted, an absent one never as a clone applied | the same |
| `i11RecoveryCompletes` | I11 (bounded): a start the journal trusts never stalls before admission, and one with no fault completes within the pipeline's nine steps with every record classified | `formal-verify` |
| `i12RecordBeforeAck` | I12: a GitHub message is acked only after its durable record, across crashes | `formal-verify` (the listener's own test is KER-06's) |
| `i17EpochsIncrease` | I17: sessions are entered at strictly increasing epochs, the floor is raised before an epoch is used, a journal below the floor is never trusted | `formal-verify`; `tests/recovery.rs::should_enter_safe_mode_when_the_journal_is_below_the_floor_or_does_not_verify`, `tests/hostrun/rules.rs::should_mint_above_the_journal_and_the_floor_when_a_run_starts`, `should_enter_safe_mode_when_the_floor_is_above_the_journal`, `tests/hostrun/properties.rs::should_mint_strictly_increasing_epochs_when_runs_restart` |
| `i25IncompleteNeverAbsence` | I25: no decision reads absence from an incomplete inventory, and admission never opens on one | `formal-verify`; `tests/recovery.rs::should_release_nothing_when_the_inventory_is_incomplete` |
| `i29SafeModeNoEffects` | I29: in a safe session no clone or delete executes | `formal-verify` |
| `safeModeNoDestructiveAction` | in safe mode no directive is issued and no decision or plan action retires, retries or deletes | `formal-verify`; `tests/recovery.rs::should_propose_nothing_destructive_when_the_journal_is_untrusted` |
| `i31PlanBound` | I31: a plan acts only against the heads, inventory and epoch it was drawn from, intent before outcome | `formal-verify`; `crates/mayfly-control-domain/tests/recovery_plan/rules.rs::should_refuse_the_plan_when_it_is_applied_after_the_world_moved` (F45), `tests/recovery_plan/properties.rs::should_record_no_intent_when_a_binding_moved`, `should_record_each_outcome_after_its_intent_when_commands_are_random` |
| `deleteNeedsProof` | a delete carries the recorded instance id and executes only on the VM that has it | `formal-verify`; `tests/recovery.rs::should_delete_only_a_leaked_vm_with_proof_when_any_row_is_decided`, `should_never_delete_when_the_instance_id_differs` |
| `quarantineNeverDeleted` | I28 at recovery: every decision and plan action on a quarantined record is keep (wait on an incomplete inventory), so none retires, deletes, adopts or releases it, and no directive targets it | `formal-verify`; `tests/recovery.rs::should_keep_when_the_attempt_is_quarantined` |
| `busyNeverRetiredWhileRunning` | F13 at recovery: a running VM whose runner may serve a job is never retired: a Busy attempt's whatever GitHub says, or a Prepared, Activating, Listening or Deleting attempt's whose runner GitHub shows busy | `formal-verify`; `tests/recovery.rs::should_never_retire_when_a_busy_vm_runs` |

Tests (`quint test`, stage `formal-test`, all deterministic): one per row of plan §10 the model
expresses (`busyAdoptedTest`, `unreachableIsNotAbsentTest`, `stoppedBusyRetiresWithProofTest`,
`unknownCloneAbsentRetriesTest`, `unknownCloneAppliedAdoptsTest`, `deletingAbsentConfirmedTest`,
`activeRunnerSuspendsCleanupTest`, `incompleteInventoryWaitsTest`, `incompleteThenCompleteTest`,
`instanceMismatchQuarantinesTest`, `quarantinedKeptTest`, `leakedVmDeletedWithProofTest`,
`strayReportedNeverDeletedTest`, `preparedAdoptedTest`,
`preparedWithAnUnknownRunnerQuarantinedTest`, `preparedStoppedWithAnUnknownRunnerQuarantinedTest`,
`activatingIssuedNotDeliveredPresumedLeakedTest`, `activatingBusyRunnerAdoptedTest`,
`listeningAbsentRunnerRetiresTest`, `reservedVerifyingFinishedTest`), and the crashes and faults around them
(`staleFrameNeverExecutesTest`, `replacedVmNeverDeletedTest`, `crashBeforeFloorRaisedTest`,
`rolledBackJournalIsSafeModeTest`, `restoredBackupIsSafeModeTest`, `unknownKindIsSafeModeTest`,
`planRefusedWhenTheWorldMovesTest`, `foreignWriteBeforeTheActionRefusesTest`,
`foreignWriteBeforeTheOutcomeRefusesTest`, `foreignWriteOnTheOwnStreamRefusesTest`, the negative
control `foreignWriteAbsorbedViolatesTest`, `recordBeforeAckTest`, `safeSessionExecutesNothingTest`).
Negative controls tried: reading an incomplete inventory as the table's input (dropping
`classify`'s first row) makes simulation of `restartStep` report `[violation]` of
`i25IncompleteNeverAbsence`; retiring a Busy attempt whenever GitHub shows its runner absent
makes it report `busyNeverRetiredWhileRunning`. At KER-07F, Apalache on `restartStep` at depth 6
(about 30 s each) reports a violation for each of: dropping the quarantine row of `classify`
(`quarantineNeverDeleted`), retiring a Busy attempt on `offline` or `absent` whether or not its
VM runs (`busyNeverRetiredWhileRunning`), presuming leaked an Activating attempt whose runner
GitHub shows busy, the pre-KER-07F row (`busyNeverRetiredWhileRunning`); and simulation reports
deleting the unrecorded `mf-*` VM (`i04UnrecognisedNeverDeleted`). Simulation runs at about 60 traces of 40 steps
per second (TypeScript evaluator, 4-CPU sandbox).

**Not modelled (KER-10).** The `HostRun`, `RecoveryPlan` and `QuarantineCase` aggregates and
`operations::mode` refine the model's `j.epoch`, `j.safe`, `plan` and `quarantineNeverDeleted`
without adding a model change; what they decide beyond it is pinned by Rust tests only:

- `HostRun`'s phases (`Starting`, `Recovering`, `Operating`, `ShuttingDown`, `Ended`),
  `PreviousRunEndedUnclean`, and the shutdown pair. The model's `mint` (above `max(j.epoch,
  floor)`) and `verify` (a floor above the journal is safe mode) are one command here,
  `MintEpoch`, which records the epoch and safe mode in one unit
  (`tests/hostrun/rules.rs`, `tests/hostrun/properties.rs`).
- A **verified restart** as a second exit from safe mode: the model leaves safe mode only by
  `completePlan` followed by a restart that re-verifies. The aggregate also lets a later run
  whose start-up verification trusts the journal leave safe mode entered for an unknown event
  kind, a failed verification or a crash loop, never for a rolled-back journal (`needs_plan`),
  which still needs a completed plan, as the model's `rolledBackJournalIsSafeModeTest` does
  (`tests/hostrun/rules.rs::should_refuse_a_verified_restart_when_the_journal_was_rolled_back`).
  The model has no upgrade step that would make an unknown kind known, so this exit has no
  model counterpart to check.
- The plan's sequential order (the model applies actions in any order), its expiry, a failed
  action that stops it resumably, the operator-only single approval and the GitHub binding (the
  model binds the heads, the epoch and the inventory). KER-10's outcome took the heads the caller
  reported into the binding, absorbing another writer's append; KER-10F modelled the action as
  intent, own write and outcome with a foreign writer, and the Rust outcome now carries the
  plan's own write only (`foreignWriteBeforeTheActionRefusesTest`,
  `foreignWriteBeforeTheOutcomeRefusesTest`, `foreignWriteOnTheOwnStreamRefusesTest`; the
  absorbing outcome, `planOutcomeAbsorbingWith`, fails `i31PlanBound` in
  `foreignWriteAbsorbedViolatesTest`).
- The crash loop (P25) and the verified-restart exit that it must block (KER-10F M2): the model
  has neither a crash-loop input nor a restart exit from safe mode, so `MintEpoch.crash_loop`
  is pinned by `tests/hostrun/rules.rs::should_refuse_a_verified_restart_when_the_run_is_crash_looping`
  and the property `should_leave_safe_mode_only_by_its_two_exits_when_commands_are_random` only.
  The plan exit agrees with the model: a completed plan leaves safe mode and the daemon restarts
  and verifies again before recovery completes (`completePlan` takes it down;
  `tests/hostrun/rules.rs::should_require_a_restart_when_a_completed_plan_left_safe_mode`).
- `QuarantineCase` entirely: opening, inspection, escalation and the operator's resolution
  (`tests/quarantine/`). The model's `quarantineNeverDeleted` covers recovery's side only.
- The mode precedence (`tests/mode.rs`, an exhaustive table).

**Code diverges from the model (CP-04, until CP-04C):** a held cleanup. In the model,
`suspend_cleanup` adopts the attempt with its effect held (`apply` sets `hold`, `nextDirective`
issues nothing while it holds; `activeRunnerSuspendsCleanupTest`), so it is seen and
`complete` holds. In the code the `Attempt` aggregate cannot hold a directive in flight after an
adoption (adopting lets the relay's sink send the pending delete again, and `Quarantine` is
refused while a directive is in flight), so start-up leaves a held attempt **unadopted**: the
sink holds its delete (`attempt_not_adopted`), its lease keeps its slot, recovery completes and
reopens admission with the attempt reported held, and every later inventory classifies it again
(`Startup::resume_held`), adopting it and resuming its cleanup once GitHub reports the runner
offline or gone; a runner GitHub cannot be read for stays held
(`crates/mayfly-control/tests/store/recovery.rs::should_resume_a_held_cleanup_when_github_reports_the_runner_offline`,
`should_keep_the_cleanup_held_when_github_cannot_be_read`).
CP-04C closes the gap model first.

### registration

Two attempts, each with its registration stream (`regs`) and the retirement side of its
`Attempt` (`atts`: retirement requested or confirmed, the VM running, a launch of unknown
outcome). The world is GitHub's runner list (`gh`, keyed by runner id; a runner's id is its
attempt's number, since a name is minted at most once), whether a runner process lives in each VM
(`vmRunner`) and whether GitHub answers (`ghUp`). A foreign runner of our name shape that mayfly
never journaled is listed from the start. The mint answers (`mintAnswered`), is lost with or
without effect (`mintLost`: `IssueUnknown`, never minted again) or is refused (`mintRefused`:
absent); the reaper lists by name (`reaperByName`: found, journaled and presumed leaked, or
absent). GitHub's listings lag behind a lost mint (CP-05): such a runner exists before any listing
shows it (`listed`, caught up by `listingCatchesUp`), so the reaper remembers a first empty listing
(`emptyListings`, policy memory a restart loses: `reaperRestarts` resets it) and records the registration absent only on a
second one after the grace period (`graceElapses`, which assumes the grace is at least the lag and
so lists every runner of the name). Delivery succeeds (`deliver`) or has an unknown outcome (`deliverUnknown`, after which P13
presumes the registration leaked); the runner connects (`runnerConnects`) and the confirmation
reads the issued id online (`confirmOnline`, else `presumeLeaked`). GitHub gives an online idle
runner its one job (`jobStarts`, also while a removal is in flight) and removes the ephemeral
runner when it completes (`jobCompletes`). P11 is `retirementStep(reg, retirement)`, the pure
`registration::retirement_step` of the aggregate: a busy answer for this retirement withdraws it;
nothing issued, an unknown issue (the config never left the control plane), absent or
deregistered confirms; a pending mint or removal waits; otherwise deregister first. The removal
(`deregisterCall`) is by the journaled id: removed, not found (the service already removed it),
busy, or a lost reply that leaves the request pending. A busy removal is named busy, never by its
code (409 on the Actions-service path, 422 on REST: `doc/github-scaleset.md` §7). `stopVm` stops
the VM only after a confirmed retirement. `step` offers every stimulus for either attempt and
GitHub outages; `raceStep` is the biased relation: one attempt, GitHub up, every mint answered,
so a walk delivers and connects a runner and meets its retirement, a job starting during the
removal, busy answers, lost replies, an unknown launch and the leak path.

Simulation of `raceStep` found that the first draft let job evidence clear a busy refusal, so the
same retirement went on to deregister the finished runner and stop the VM after GitHub had
answered busy for it (`i21BusyCancelsRetirement`); a busy answer is now final for the retirement
it served, recorded per retirement (`busyRetirementStaysCancelledAfterTheJobTest`). The Rust
property test then found that keeping only the last busy retirement forgot an earlier one; the
aggregate keeps the set.

| Id | Invariant | Pinned by |
|:--|:--|:--|
| `i19OnlyJournaledIdsDeregistered` | I19: a runner is removed only by an id a registration journaled; the foreign runner stays | `tests/registration/rules.rs::should_refuse_a_deregistration_when_no_runner_id_is_journaled` |
| `i19NameIssuedOnce` | I19: a runner name is minted at most once | `tests/registration/rules.rs::should_never_issue_a_runner_name_twice_when_any_command_follows`, `should_refuse_a_runner_name_when_it_is_not_the_attempts_vm_name` |
| `noRemintAfterUnknown`, `issueUnknownLeavesOnlyOnObservation` | an unknown issue is never minted again and leaves `IssueUnknown` only on the reaper's listing | `tests/registration/rules.rs::should_leave_an_unknown_issue_only_on_the_reapers_observation_when_every_command_is_tried` |
| `i20SettledOrOnReaperList` | I20: once its VM is stopped, a registration is settled or on the reaper's list | `formal-verify`; `mayfly-control`'s `tests/store/registration.rs::should_find_the_runner_by_name_on_the_second_listing_when_the_first_lagged`, `should_presume_leaked_and_remove_by_id_when_a_crash_left_an_issued_config_without_handle` |
| `absentOnlyWhenNoRunner` | I20 (CP-05): a registration recorded absent has no runner of its name on GitHub, listed or not; the negative control `absentOnOneListingViolatesTest` (one empty listing trusted) violates it | `formal-verify`; `mayfly-control`'s `tests/store/registration.rs::should_record_absent_only_after_a_second_empty_listing_when_the_grace_passed` |
| `deregisterBeforeStop` | I21: no VM is stopped while its runner is still listed | `tests/registration/rules.rs::should_never_confirm_a_retirement_when_a_runner_may_still_be_registered` (mutant 21) |
| `i21BusyCancelsRetirement` | I21: a busy answer is final for its retirement | `tests/registration/rules.rs::should_withdraw_the_retirement_when_github_answered_busy_for_it`, `should_refuse_the_same_retirement_when_github_answered_busy` |
| `i22IssuedAndDeliveredOnce` | I22 (registration half): one mint and one launch per registration, none once leaked, and the online confirmation names the issued id | `tests/registration/rules.rs::should_confirm_only_the_issued_runner_id_when_a_runner_is_seen_online`, `should_stay_leaked_and_deregister_when_a_delivery_is_presumed_leaked` |

Every invariant is also asserted on the aggregate over every accepted transition two stimuli deep
from every matrix state (`tests/registration/properties.rs`).

Tests (`quint test`, all deterministic): `happyPathTest`, `idleRunnerDeregisteredBeforeStopTest`,
`busyAnswerWithdrawsRetirementTest`, `busyRetirementStaysCancelledAfterTheJobTest`,
`lostRemovalReplyIsRetriedTest`, `issueUnknownFoundByNameTest`, `issueUnknownAbsentTest`,
`issueUnknownWaitsForGitHubTest`, `deliveryUnknownPresumedLeakedTest`,
`notSeenOnlinePresumedLeakedTest`, `reaperSparesTheForeignRunnerTest`, `nameIssuedOnceTest`,
`mutant21StopBeforeDeregisterViolatesTest`: mutant 21 (verification §9) as the action
`confirmRetirementStopFirst`, kept out of every step relation, which stops the VM before the
runner is deregistered and violates `deregisterBeforeStop`; and from DEC-06 (ADR 0076)
`quarantineStopDeregistersFirstTest`, `quarantineStopBusyKeepsRunningTest` and
`quarantineStopWaitsForGitHubTest` for `requestQuarantineStop` (a quarantine stop asks P11 like a
retirement, in any registration phase: a busy answer withdraws it and the VM keeps running, an
unreachable GitHub leaves it waiting), with the negative control
`quarantineStopAtOnceViolatesTest` (KER-04F's immediate stop, `quarantineStopAtOnce`, kept out of
every relation, violates `deregisterBeforeStop`). `requestQuarantineStop` is in `step` and
`raceStep`; the Apalache times of the table predate it (simulation of both, 3000 traces of 30
steps, found no violation). Negative controls tried in simulation:
confirming a retirement without P11's step (mutant 21) makes `raceStep` and `step` report
`deregisterBeforeStop`; a reaper that sends an unknown issue back to `Requested` makes `step`
report `noRemintAfterUnknown`; dropping the "not leaked" guard of delivery makes `step` report
`i22IssuedAndDeliveredOnce`.

### scaleset

One pool's scale set. The world is GitHub: which flow it runs (`directFlow`, fixed per trace:
`init` is `initAcquire` or `initDirect`), the `maxCapacity` it received on the last poll
(`ghMax`), the scale set's id (`ghSetGen`), mayfly's runners that have no job yet (`idle`), each
job's status (`jobs`: available, assigned to the set by an acquire or directly, running, done), the job events not yet in a message (`buffer`), the messages it emitted (`msgs`,
ids increasing, each with the statistics of its moment), the unacknowledged ones (`queue`,
redelivered until deleted) and the live session (`ghSession`). The journal is the stream's state
(`ss`: phase, live and stale session, cursor, latest statistics, capacity input, desired, the
pending and acquired reservations, drift) and the inbox receipts (`inbox`, keyed `(session,
message)`); the listener holds at most one message. Three jobs start available, two trusted and
one `pull_request_target`. In the acquire flow (GHES) the first batch holds two offers (`CAP = 2`
stands for the service's 50), the third waits, so the backlog is truncated; in the direct flow
(github.com) nothing is offered and GitHub assigns jobs to the set (`ghAssigns`) while fewer than
`ghMax` are assigned, so a poll advertising 0 gets nothing assigned (the cap ADR 0069's `demand`
starves on, `doc/github-scaleset.md` §5).

`poll` answers the oldest unacknowledged message (a recorded one whose ack was lost included);
`record` is `RecordMessage`: the receipt (a duplicate only when the receipt exists and the id is
the cursor's, since only the cursor can be redelivered within a session; a reused id with a
receipt is not one); a session's first message strictly below its floor
starts a new numbering (`MessageIdsRestarted`: applied as new, cursor and floor restart from it);
any other message at or below the floor (the cursor at its open) applies nothing, and unless it
is the stream's last record redelivered as the session's first message it degrades the session
(A1 refuted); any other message applies its
statistics if its id is above the cursor, then the batch folded in arrival order through
`itemStep`, where an offer goes through `offerStep` (`demand::decide_offer`: tier, no second
reservation, held modes and a degraded session, then the budget `assigned + open reservations <
deliverable`), a `JobAssigned` not acquired by mayfly is classified (`assignedStep`: trusted, or
refused, `AssignedJobRefused`), and `JobStarted`/`JobCompleted` release a reservation, a
classification and a refusal; `ack` deletes after the record (a lost ack leaves it queued); `acquireCall` acquires
the pending ids still available while open (a lost answer leaves them pending); `startVm`
starts a runner only for a job classified trusted that waits for one (acquired, or assigned and
classified), never more idle runners than such jobs, and only while open and active; nothing
else holds it (ADR 0090, amended 2026-11-12); `lookupRun` classifies a pending assignment from
its workflow run (a failed lookup refuses); `cancelRun` (with `cancel_refused_runs`, chosen per
trace by `initDirectWith`) asks GitHub to cancel a refused run once, its effect and its answer
each possibly lost; `startSink` starts a refusal sink for a refused job (without the option,
once per refusal, one at a time, only while no trusted or pending assignment is known), and the
wire max is pinned at the assigned count while it is up (`wireMax`); `sinkEnds` deletes it.
GitHub's side
adds `JobAssigned` for each acquisition, lets a runner take any job assigned to the set
(`runnerStarts`: runners are interchangeable; its in-guest hook runs a trusted job and fails any
other before a step), lets the sink take any assigned job (`sinkTakes`: its hook fails every
job), finishes jobs, releases an assigned job no runner
took as canceled and requeues it under the same request id (offered again in the acquire flow,
assigned again in the direct one), failing it at the third release (`released`,
`MAX_RELEASES`), and on github.com may send a `JobCompleted` that names no job (`named` false,
[open: HW-02]), sends unknown types, once (`idsRestart`, A1 refuted, with no
message of the old numbering queued) numbers a batch from 1 again, and once deletes a scale set
no session holds (`dormantDeleted`: its queue and idle runners go, its assigned jobs return to
GitHub's queue).
Sessions end on their own (`sessionEnds`: an expired token, or an abandoned session timing
out), are lost by the listener (`sessionLoss`) or a restart, deleted only when journaled
(`deleteStale`), re-created after a dormant deletion (`recreateScaleSet`: the create answered
404, so `ensure` journals the new id, the numbering starts afresh and the old set's reservations,
classifications and refusals are dropped on that observation), and re-opened (`openSession`: a 409 while another lives, counted and escalated
at `CONFLICT_LIMIT` in a row, else the new session's initial statistics recompute desired and
the cursor becomes its floor); `openLost` leaves a session mayfly has no record of. `applyCapacity` is `ApplyCapacity` with a mode: open, paused, draining or safe;
`journalToggles` makes the journal unwritable, which stops polling. `step` offers every stimulus;
`messageStep` is the biased relation without session churn, capacity changes or outages, so a
walk goes deep into redelivery, lost acks and acquire answers, truncation and reassignment.

The model states the safe side of six facts `doc/github-scaleset.md` leaves client-assumed or
[open: HW-02] (GH-00R), named A1–A6 in its header: within a numbering ids increase and a message
may be redelivered in a later session, oldest first; a message at or below a session's floor
applies nothing, a session's first message strictly below it starts a new numbering, and any
other A1 does not explain degrades the session (A1); a message's statistics may not count mayfly's
latest acquisitions, so the budget counts every open reservation on top (A2); a 409 on create
backs off, persistent 409s are escalated, and only a journaled session is deleted (A3); acquire
answers a subset and a lost answer leaves its ids pending (A4); an unclassifiable offer is
declined, an unclassifiable assignment refused (A5); a message emitted before an open and recorded after it cannot be told from a newer
one, so statistics are ordered by message id only and such a message's older statistics apply
until the backlog is consumed (A6).

`desiredFromLatestStatistics` reads "latest" from GitHub, not from the stream: desired is
`min(deliverable, totalAssignedJobs)` of the applied statistics, and those are GitHub's own count
(`statsOf(jobs)`) whenever the stream has seen everything GitHub emitted (`quiescent`: no job
event waits for a message, every emitted message is recorded). KER-06's version compared the
stream with a ghost keyed by the ids the stream itself had recorded, so it could not see a
message emitted before an open and recorded after it (the KER-06 review's MED1). The
deterministic `crashBeforeRecordThenReopenConvergesTest` walks that path: the older statistics
apply, I23 holds because the acquisitions are still reservations (A2), and once the backlog is
recorded desired is GitHub's count again. The alternative the review suggested, keeping the
opening's statistics until a message is known to be newer, is the model-local mutant `OPEN_WINS`:
nothing in a message or a create answer tells mayfly where the backlog ends, so it never
converges after a change at GitHub (`mutantOpenWinsNeverConvergesTest`); simulation does not
reach that state (quiescence after a change is rare in a random walk), the deterministic test
does.

Simulation found nothing in the model itself. Negative controls tried in simulation: `record` as
mutant 19 (acquire every offer) makes `messageStep` report a violation at once; resetting the
statistics cursor at each session open (a per-session cursor, the alternative to A1) makes
`step` report `desiredFromLatestStatistics`; `record` as the model-local mutant `REAPPLY` (KER-06's
rule: a redelivered message folded like a new one) makes `step` report a violation within 3000
samples. The Rust property test then found that an envelope without statistics must not move the
statistics' floor
(`should_apply_statistics_when_only_a_newer_message_without_statistics_was_recorded`); the model's
messages all carry statistics, so the aggregate keeps the floor apart from the cursor.

Coverage of the bounded run: both relations start from `init`, an Active session in either flow
(the acquire flow's first message queued, or the direct flow with nothing yet), not from
`Ensured`. The simulation of GH-00R's first draft found that a dormant deletion changes GitHub's
count with no message, so `quiescent` now also requires the journaled scale set to be GitHub's.
The review of GH-00R found two more: a reused id whose `(session, id)` receipt existed was
dropped as a duplicate (now a duplicate needs the cursor's id), and `quiescent` never held again
after a restart (it now requires every reused id recorded and applied as new, `reusedIgnored`
false: a restart read as a redelivery or degraded is the operator's, not a convergence). Its
final review added a ghost for `cancelOnlyRefused` that reads the truth, and the simulation then
found a stale refusal surviving a job's reassignment (a release naming no job, then a new
assignment): a new assignment now replaces the earlier classification. At 6 steps `step` reaches the crash before a record and
the reopen that records it (poll, restart, delete, open, poll, record) and the 409 escalation
(restart, delete, lost create, two 409s), but not a redelivery after a lost ack in a new session
(8 steps) nor the A1 refutation (9); `messageStep` has no session churn at all. Those paths are
covered by the deterministic tests above and by simulation (50 steps), not by Apalache's bound.

| Id | Invariant | Pinned by |
|:--|:--|:--|
| `i12RecordBeforeAck` | I12: a message is acked only once its record (or receipt) is durable | the model (mutant 4); `tests/scaleset/rules.rs::should_record_every_message_when_a_session_is_live_in_any_mode`; the ack itself is GH-04's (component ack-after-record) |
| `noDoubleReservationPerRequest` | a request id is reserved at most once, whatever redelivery in a later session, a repeat offer or a reassignment shows | `tests/scaleset/rules.rs::should_reserve_a_request_once_when_a_message_is_recorded_again_in_a_new_session`, `should_reserve_a_requeued_job_once_when_its_completion_precedes_the_offer`; `tests/demand.rs::should_never_reserve_a_request_twice_when_it_is_offered_again` |
| `i23AcquireBelowDeliverable` | I23: acquisition only while assigned < deliverable, checked against GitHub's own count at the plan | `tests/demand.rs::should_acquire_only_while_assigned_is_below_deliverable_when_offers_exceed_it` (mutant 19), `tests/scaleset/rules.rs::should_acquire_only_while_assigned_is_below_deliverable_when_a_batch_exceeds_it` |
| `i32TrustedOnly` | I32: only offers the pool's tier admits are reserved | `tests/demand.rs::should_decline_untrusted_and_unclassifiable_offers_when_the_pool_is_trusted`, `tests/scaleset/rules.rs::should_decline_untrusted_offers_when_a_message_is_recorded` |
| `desiredFromLatestStatistics` | desired = min(deliverable, `totalAssignedJobs`) of the applied statistics (never a count of messages, never plus running jobs), and the applied statistics are GitHub's own once the stream has seen everything GitHub emitted (A6) | `tests/demand.rs::should_take_demand_from_statistics_when_a_batch_shows_fewer_jobs_than_assigned` (mutant 5), `should_not_add_running_jobs_when_desired_is_computed` (mutant 6); `tests/scaleset/rules.rs::should_keep_newer_statistics_when_an_older_message_is_recorded`, `should_recompute_desired_from_initial_statistics_when_a_session_opens`, `should_stay_within_deliverable_when_a_message_emitted_before_the_open_is_recorded_after_it`; the model (`OPEN_WINS`) |
| `redeliveryAppliesNothing` | A1: a message recorded in an earlier session and redelivered applies nothing again (no statistics, plan, release or drift) | `tests/scaleset/rules.rs::should_apply_nothing_when_a_message_from_an_earlier_session_is_redelivered`, `should_count_no_drift_again_when_a_redelivered_message_carries_unknown_types`; the model (`REAPPLY`) |
| `a1RefutationIsFlagged` | a message emitted anew under an id at or below the session's floor is never dropped or applied silently: a session's first message below the floor starts a numbering recorded as `MessageIdsRestarted`, any other degrades the session (except a restarted id equal to the floor as the session's first message, which reads as the redelivery) | `tests/scaleset/rules.rs::should_restart_the_numbering_when_a_new_session_delivers_an_id_below_the_floor` (the renumbered branch), `should_degrade_the_session_when_an_id_at_or_below_the_cursor_comes_later_in_a_session` (the degrade branch), `tests/scaleset/trust_properties.rs::should_hold_the_trust_invariants_when_assignments_are_random`; the model (`a1RestartIsRecoveredTest`, `a1RefutedMidSessionDegradesTest`) |
| `persistentConflictsAreFlagged` | A3: `CONFLICT_LIMIT` creates in a row answered 409 are escalated to the operator | `tests/scaleset/rules.rs::should_escalate_once_when_creates_keep_answering_conflict` |
| `vmOnlyForAJobClassifiedTrusted` | ADR 0090: a runner starts only for a job mayfly classified trusted that waits for one, never more idle runners than such jobs | the model (`directAssignmentNeedsAdvertisedRoomTest`, mutant `RUN_ASSIGNED`); `tests/demand.rs::should_start_a_runner_only_for_a_trusted_job_when_one_waits`, `tests/scaleset/trust.rs::should_start_a_runner_only_after_the_lookup_when_a_job_is_assigned_directly`, `should_start_no_runner_when_an_assignment_is_pending_or_refused`, `tests/scaleset/trust_properties.rs` (property and the `RUN_ASSIGNED` negative control); the reconciler's use of the gate is GH-08's |
| `noStepOfAnUntrustedJobRuns` | ADR 0090 (amended 2026-11-12): no step of an untrusted job runs, wherever it lands; the in-guest hook is the guard | the model (mutant `HOOK_PASS`, `refusedJobHoldsNothingAndTheHookFailsItTest`); the hook's tests (IMG-02) |
| `sinkRunsNoStep` | ADR 0090: a refusal sink runs no job's steps | the model (mutant `SINK_RUNS`); IMG-03 |
| `sinkOnlyForARefusedJob` | ADR 0090: a sink starts only for a job truly untrusted or whose lookup failed, without the cancel option, while no trusted or pending assignment is known | the model (`refusalSinkFailsTheRefusedJobTest`, `noSinkBesideATrustedJobTest`); `tests/scaleset/trust.rs::should_start_a_sink_only_when_no_trusted_or_pending_job_is_known`, `should_start_no_sink_when_the_pool_is_held`, `tests/scaleset/trust_properties.rs` |
| `refusedJobNeverGetsAVm` (removed by GH-00S) | the hold of the first decision: no runner starts while a job mayfly saw assigned and did not classify trusted is still assigned | the model (`assignedUntrustedJobIsRefusedTest`, mutants `RUN_ASSIGNED` and `IGNORE_REFUSALS`); KER-06G |
| `assignedJobsClassified` | ADR 0090 and I32 for assignments: no untrusted assignment accepted, and each one seen and not released is pending its lookup or refused, a journaled fact | the model (mutant `RUN_ASSIGNED`, `failedLookupRefusesTest`); `tests/demand.rs::should_trust_only_an_allowed_event_of_an_allowed_repository_when_an_assignment_is_classified`, `tests/scaleset/trust.rs::should_refuse_an_assignment_when_its_lookup_fails`, `should_classify_a_reassigned_job_afresh_when_it_is_assigned_again`, `tests/scaleset/trust_properties.rs` (property and the `RUN_ASSIGNED` negative control); the lookup itself is GH-10's |
| `cancelOnlyRefused` | ADR 0090: a run is cancelled only with `cancel_refused_runs`, and only for a job that is truly untrusted or whose lookup failed | the model (`cancelledRefusedRunTest`, `failedCancelFallsBackToGitHubsReleaseTest`, mutant `CANCEL_ANY`); `tests/scaleset/trust.rs::should_cancel_only_a_refused_run_when_the_option_is_on`, `tests/scaleset/trust_properties.rs` (property and the `CANCEL_ANY` negative control); the call is GH-10's |
| `i07WireWithinDeliverable`, `heldAcquiresNothing` | I07 and ADR 0029: the wire max never exceeds deliverable and is 0 while draining or in safe mode; nothing is planned or acquired while paused, draining or in safe mode, nor planned while the session is degraded (P21); no poll while the journal is unwritable | `tests/demand.rs::should_send_zero_when_draining_or_in_safe_mode`, `should_acquire_nothing_when_paused_draining_or_in_safe_mode`, `should_acquire_nothing_when_the_session_is_frozen`; `tests/scaleset/rules.rs::should_acquire_nothing_and_send_the_mode_wire_max_when_held`, `should_plan_no_offer_when_the_session_is_degraded_or_closing` |
| `messageAppliesOnce` | I13 for demand: a message applies once per receipt | the model (mutant 7); the receipt store's inbox dedupe (ES-01) |
| `acquireUnknownLeavesOnlyOnObservation` | a pending acquisition leaves only on an acquire answer or a job fact | `tests/scaleset/rules.rs::should_keep_acquisitions_pending_when_no_observation_names_them` |
| `neverStealsASession` | A3: only a journaled session is deleted | `tests/scaleset/rules.rs::should_refuse_to_delete_a_session_when_mayfly_holds_no_record_of_it`, `should_refuse_a_create_when_a_journaled_session_is_not_deleted` |

Every invariant except I12's ack and the receipt dedupe is also asserted on the aggregate over
random command sequences (`tests/scaleset/properties.rs`).

Tests (`quint test`, all deterministic): `acquireWithinDeliverableTest`,
`declinedPastDeliverableAndTierTest`, `truncatedBacklogDemandFromStatisticsTest`,
`redeliveryAfterLostAckIsDuplicateTest`, `redeliveryInNewSessionReservesOnceTest`,
`crashBeforeRecordThenReopenConvergesTest`, `changeAfterOpenReachesDesiredTest`,
`redeliveryCountsNoDriftTwiceTest`, `a1RestartIsRecoveredTest`,
`a1RefutedMidSessionDegradesTest`, `a1RefutedAtTheFloorReadsAsARedeliveryTest`, `persistentConflictsEscalateTest`,
`degradedSessionPlansNothingTest`, `sessionConflictBacksOffTest`, `sessionExpiryReopensWithInitialStatisticsTest`,
`unknownTypesDegradeTheSessionTest`, `drainSendsZeroAndAcquiresNothingTest`,
`pauseSendsRunnersAndAcquiresNothingTest`, `safeModeRecordsButNeverAcquiresTest`,
`requeuedJobIsReservedOnceAgainTest`, `lostAcquireAnswerStaysPendingTest`, GH-00R's
`directAssignmentNeedsAdvertisedRoomTest`, `dormantScaleSetIsRecreatedTest`,
`reassignedJobIsClassifiedAfreshTest`, and GH-00S's `refusedJobHoldsNothingAndTheHookFailsItTest`,
`untrustedJobAssignedAfterTheRunnerStartedFailsInTheHookTest`,
`untrustedJobAssignedBeforeTheRunnerStartedFailsInTheHookTest`,
`refusalSinkFailsTheRefusedJobTest`, `noSinkBesideATrustedJobTest`,
`sinkMayFailATrustedJobItCouldNotKnowOfTest` (the sink's limit; `g.trustedFailedBySink` is a
ghost, not an invariant), `failedLookupRefusesTest`, `cancelledRefusedRunTest` and
`failedCancelFallsBackToGitHubsReleaseTest`, and
`restartWithinASessionDegradesTest` (a reused id with a
receipt in the same session degrades instead of reading as a duplicate), and the curated
mutants of verification §9, each an action or a `recordWith` variant kept out of every step
relation: `mutant4AckBeforeRecordViolatesTest` (`i12RecordBeforeAck`),
`mutant5CountMessagesViolatesTest` and `mutant6AddRunningViolatesTest`
(`desiredFromLatestStatistics`), `mutant7IgnoreInboxViolatesTest` (`messageAppliesOnce`) and
`mutant19AcquireEveryViolatesTest` (`i23AcquireBelowDeliverable`, then `i32TrustedOnly`); and
two model-local mutants of the KER-06 review: `mutantOpenWinsNeverConvergesTest`
(`desiredFromLatestStatistics`) and `mutantReapplyCountsDriftTwiceTest`
(`redeliveryAppliesNothing`); and two of ADR 0090: `mutantRunAssignedViolatesTest` (an assigned
job accepted unclassified: `assignedJobsClassified`, then `vmOnlyForAJobClassifiedTrusted`),
`mutantHookPassViolatesTest` (a hook that lets every job through:
`noStepOfAnUntrustedJobRuns`), `mutantSinkRunsViolatesTest` (a sink that runs its job:
`sinkRunsNoStep`) and `mutantCancelAnyViolatesTest` (a trusted job's run cancelled with the option off:
`cancelOnlyRefused`, whose ghost reads the job's true class and the option, not the guard).

GH-00S's negative controls in simulation: `record` as `RUN_ASSIGNED` and `cancelRun` as
`CANCEL_ANY` each report a violation within 5000 samples of 40 steps of `step`, and
`runnerStarts` as `HOOK_PASS` and `sinkTakes` as `SINK_RUNS` within 5000 of 50 steps of
`messageStep` (`step` did not reach them in 5000 of 40: the paths are long) (GH-00R's `IGNORE_REFUSALS` and `IGNORE_PENDING` went with
the hold); the correct model reports none under `step`
or `messageStep` (three seeds of 5000 traces of 50 steps each). The Rust aggregate does not yet have the
direct flow, the trust gate, the sink, the renumbering branch or the re-create: KER-06G, then
VER-02's driver; the hook and the sink are IMG-02 and IMG-03.

### event_store

Two streams `a` < `b` (the `StreamId` order) of up to three revisions, four event ids, two
command ids with two intents, two inbox keys, builds supporting epoch 1 or 2, two client slots
and two delivery workers. The journal (`j`) holds the heads, the log (each event with its id,
required epoch, whether it is a `DirectiveIssued`, and the commit that wrote it), the stream
epochs, the identity registry, the receipts (an acceptance with what it appended, or a refusal
bound to every stream the decision read: ADR 0041) and the inbox. A slot (`slots`) prepares one
unit: `command` is `AggregateStore::save` up to the append (the epoch of its stream, then the
receipt read with the epochs of the streams a replay would answer for: `CommandIdReused`, a
replay refused `StreamEpochUnsupported` when one of them is above the build, the replay, or the
decision outside any transaction: a pure function of the event counts it read, so a refusal can
turn into an acceptance once a stream it read moves; intent 1 appends a `DirectiveIssued`,
intent 2 is refused until its streams hold an event, intent 3 is accepted with no event; a
refusal carries no inbox key); `request` and `decide` are the same save with its epoch check
and its receipt read as two steps, so another writer can commit between them (the race of
event-store §3.7, review MED1); `rawUnit` is a unit a journal caller builds (any expected
revision, a batch of one or two events, one or two streams, any epoch, no receipt); `rawRefusal`
a refusal receipt a caller builds, with or without an inbox key (malformed with one, JC-34);
`noteMessage` records only an inbox key. The unit then commits
in one step (`appendUnit`: the memory journal) or as a split transaction: `begin`, one `lock`
per head in `StreamId` order (shared for a read, exclusive for an append; a held head is a
recorded wait), `write` (the receipt, inbox and identity keys, waiting on a key another open
transaction wrote, then the judgement), `commit`, or `rollback` and `timeout` (`Busy`). The
judgement (`judge`) follows `MemoryJournal::refusal` in the port's reporting order: malformed,
epoch above build, `CommandIdReused`, a replay above the build (`StreamEpochUnsupported`),
`CommandReplayed`, duplicate message, duplicate event id, wrong expected revision, stream epoch
unsupported, then ES-12's generation fence. A commit kicks the reactors once (`kick`, named by
the unit: its lowest appended stream and that stream's new head; the commit counter is ghost
and no guard reads it, ES-I17); a `crash` drops prepared units, open transactions, pending kicks
and workers, never a commit. The directive relay (`d`) is registered, bootstraps streams written
before it, and its workers `claim` a marked stream with the next fence, run the `effect` of the
next revision under the live lease, `advance` the checkpoint only up and clear the mark only up
to the delivered revision, `release`, or `failHandle` (released, nothing skipped); leases
`expire` and workers crash alone (`crashWorker`); the consumer is `retire`d (ES-05: its marks and
checkpoints go, its leases end and keep their fences, so registered again it takes higher ones). The inline projection (`p`) counts each
stream's events, computed outside the transaction (`withRows`); `startRebuild`,
`rebuildStream` and `cutover` are ES-12's online rebuild: units dual-write streams already
rebuilt, and a unit computed for another generation set, or before its stream was rebuilt, is
refused at commit. Snapshots (`ES_I8`) join with ES-08.

**ES-04 against the model.** The Rust inline projection keeps the model's rule: a unit's rows
are the fold of its stream at the unit's expected revision plus its batch (`rowAfter`), computed
outside the transaction from the events the decision was made on
(`mayfly_control::projection::Projection::inline`), never from the rows of now, and guarded by
the expected revision like the events, so a unit whose revision another unit reached in between
is refused `WrongExpectedRevision` with its rows (PC-01, PC-03). Gaps, none of which an invariant
of the model depends on: the model's projection is one count per stream, so it has no row keys,
no deletes and no cross-stream write to refuse (ES-I10 is pinned by PC-05 and
`ARCH-projection-locality`, not by the model); its rebuild is ES-12's online one, and the offline
`prepare` of ES-04 (version or build mismatch, run before any writer) is not modelled; the
applied revision the store keeps per stream (`projection_stream`) is the row itself in the model;
and the conformance driver (ES-07) observes no projection rows yet. ES-12 closes the rebuild gap
when it implements `startRebuild`, `rebuildStream` and `cutover`.

`step` offers every stimulus with concurrent held writers (memory and PostgreSQL);
`implStepSerial` is the same with at most one open transaction (SQLite's `BEGIN IMMEDIATE`): a
unit begins, or appends in one step, only while no other transaction is open; `relayStep` is
the biased relation: commands of intents 1 and 2 by one build, one-step appends, kicks, crashes
and every consumer stimulus (registration among them), because a uniform walk of `step` rarely
strings registration, a commit, a claim, an effect and an advance together. `retire` (ES-05) is offered by
`step`, `implStepSerial` and `relayStep`, not by `deliveryOnlyStep`, whose depth is spent on one
registration; the Apalache times below predate it and were not measured again. Two component
relations exist for Apalache (ES-06F, review MED4): `storeOnlyStep` is `step` without the
consumer and the rebuild, and `deliveryOnlyStep` is the consumer over a journal grown by
`DirectiveIssued` commands of one build appended in one step from one slot.

What each check reaches. Apalache covers every trace of a relation up to its depth, so at depth
3 (`step`, `implStepSerial`) and 4 (`relayStep`) it reaches no delivered effect (registration,
a command, its append, a claim and an effect take five steps): `ES_I14_durableDelivery`,
`ES_I15_noSkip` and `directivesFromJournal` hold there only vacuously. `deliveryOnlyStep` at
depth 8 is what Apalache proves them to: every trace of registration, commits, claims,
effects, advances, releases, failures, expiries and worker crashes of that length.
`storeOnlyStep` at depth 4 covers two units interleaving their commands (as one step or as
a request and a decision), one-step appends and split transactions, kicks and crashes. Its
depth is the owner's decision (2026-11-10, DEC-06: "depth 4"): `BOUNDED` keeps it at 4, run only
in the weekly formal lane (`formal.yml`, about 78 min), and depth 5, which had not checked its
fourth state after 40 min, is not pursued. Beyond
those depths the invariants are checked by the scenarios (every one runs `allInvariants` at its
end) and by simulation (`formal-verify-simulate`: 3 seeds of 5000 traces of 50 steps per
relation), which samples and proves nothing.

| Id | Invariant | Pinned by |
|:--|:--|:--|
| `ES_I1_atomicUnit` | a commit wrote every event of its unit (the `DirectiveIssued` among them), its receipt and its inbox entry, or nothing | `rollbackTest`, `batchFailsAfterSecondEventTest`, `multiStreamConflictTest`, `directiveInUnitTest`; JC-10, JC-23, JC-32 |
| `ES_I2_streamOrder` | revisions consecutive from 0, the head at the last; at most one unit commits per (stream, expected revision) | `lostRaceTest`, `noStreamVersusEmptyTest`, `headQueueTest`; JC-02..JC-06, JC-21 |
| `ES_I3_independentStreams` | a transaction waits only on one holding, in a conflicting mode (one of the two appends to it; two readers never conflict), a head it names, or a key it writes; a unit that cannot append in one step is held up only by such a slot | `independentStreamsTest`, `independentHeldWritersTest`; JC-22 |
| `ES_I4_globalEventIdentity` | an event id is bound to exactly the place its event is | `duplicateAcrossStreamsTest`, `repeatedWithinBatchTest`, `idWaitTest`; JC-07, JC-08, JC-16, JC-31 |
| `ES_I5_commandIdentity` | one accepted outcome per command id; a replay answers the same intent only; an accepted receipt is never replaced, a refusal receipt only once stale | `receiptReplayTest`, `racingCommandsTest`, `refusedThenAcceptedTest`; JC-12..JC-14, JC-28, JC-29, JC-29b |
| `refusalReplayedAtSameRevision` | a refusal is answered from its receipt only while every stream it read is at the revision read; otherwise decided afresh | `refusalReplayedAtSameRevisionTest` (two reads, the other stream moves); JC-28 (two-read half) |
| `ES_I6_messageIdentity` | an inbox key is consumed by at most one commit, never by a refusal (ADR 0041 rule 6) | `inboxRedeliveryTest`, `refusedRedeliveryTest`, `refusalWithInboxTest`; JC-15, JC-30, JC-34 |
| `ES_I7_historyPreservation` | no committed event changes or disappears | every test; JC-24, JC-27 |
| `ES_I11_rebuildEquivalence` | the active rows are the fold of every stream; a rebuilt shadow row too | `shadowRebuildDualWriteTest`, `cutoverFenceTest` (ES-12's PC-07..PC-09 to come) |
| `ES_I12_epochSafety` | a stream's epoch never decreases; nothing is appended above its build or to a stream above it; `replayWithinEpoch` | `epochRaiseTest`, `refusedAboveEpochTest`, `epochRaceTest`; JC-18, `tests/store/save.rs::should_refuse_a_newer_epoch_before_the_receipt_when_a_newer_build_wrote_the_stream` |
| `replayWithinEpoch` | no receipt answers a build for a stream above it (those the receipt appended or is bound to, or for an acceptance with no event the streams the repeat names), also when a newer build commits between the older build's epoch check and its receipt read | `epochRaceTest`, negative world `staleBuildReplayViolatesTest`; JC-33, `tests/store/save.rs::should_refuse_the_replay_when_a_newer_build_recorded_the_receipt_after_the_epoch_check` |
| `ES_I14_durableDelivery` | the checkpoint never passes the head nor moves back (checked on every consumer step, whichever action it is; `retire` removes a consumer's checkpoints with it); an undelivered revision of a covered stream is marked; no effect and no advance without the live lease | `bootstrapAfterWritesTest`, `commitBetweenFetchAndApplyTest`, `markDuringProcessingTest`, `staleWorkerFencedTest`, `workerCrashRedeliversTest`, `retireThenRegisterTest`; DC-01..DC-17, DC-19..DC-27 (ES-05, ES-05F) |
| `ES_I15_noSkip` | the delivered revisions of a stream are a prefix of its committed ones | `failingConsumerNoSkipTest`; DC-08, DC-13, DC-18 (ES-05) |
| `ES_I16_lockOrder` | no wait-for cycle | `oppositeOrderUnitsTest`, `racingCommandsTest`, `idWaitTest`; JC-22's two-unit case |
| `directivesFromJournal` | a directive is sent only for a committed `DirectiveIssued` event, and only committed events are delivered (ADR 0034) | `crashBeforeReactorsTest`, `directiveInUnitTest` |
| `reactorsNeverReplayed` | the after-commit kick runs at most once per commit and never for a replay; a crash before it loses only the kick. Structural in the relations (only `apply` sets a kick; `kick` and `crash` clear them); the negative world shows it is not vacuous | `crashBeforeReactorsTest`, `zeroEventAcceptanceTest`, negative world `replayingRestartViolatesTest` |

Fairness of delivery is not modelled (ES-05G). Every invariant above is a safety property; neither
`relayStep` nor `deliveryOnlyStep` states that a marked stream is eventually delivered, nor that a
stream that keeps failing (`failHandle` again and again) leaves the other streams' claims free, and
the model has no fairness condition on `claim` (it picks any claimable stream, so it also admits
the starving schedule). The code's discovery is round-robin from the last stream claimed, and a
drain handles a stream it gave up on at most once; DC-28 pins that, with `CursorlessLedger` as its
negative control. A temporal property (`claim` weakly fair per stream) would need Apalache's
liveness checking or a lasso search, which the bounded runs above do not do, so it is left out.

Tests (`quint test`, all deterministic): one per scenario of event-store §7.3 that the model
expresses before ES-08 (`lostRaceTest`, `noStreamVersusEmptyTest`, `independentStreamsTest`,
`independentHeldWritersTest`, `batchFailsAfterSecondEventTest`, `rollbackTest`,
`duplicateAcrossStreamsTest`, `repeatedWithinBatchTest`, `epochRaiseTest`,
`refusedAboveEpochTest`, `receiptReplayTest`, `racingCommandsTest`, `refusedThenAcceptedTest`,
`inboxRedeliveryTest`, `multiStreamConflictTest`, `readSetInvalidatedTest`, `headQueueTest`,
`idWaitTest`, `oppositeOrderUnitsTest`, `bootstrapAfterWritesTest`,
`commitBetweenFetchAndApplyTest`, `markDuringProcessingTest`, `staleWorkerFencedTest`,
`workerCrashRedeliversTest`, `failingConsumerNoSkipTest`), plus `refusalReplayedAtSameRevisionTest`,
`crashBeforeReactorsTest`, `directiveInUnitTest`, `shadowRebuildDualWriteTest` and
`cutoverFenceTest`, then (ES-06F) `epochRaceTest`, `refusedRedeliveryTest`,
`refusalWithInboxTest` and `zeroEventAcceptanceTest`, then (ES-05) `retireThenRegisterTest`, and the negative worlds, each an action
kept out of every step relation: `headTrustingReaderViolatesTest` (`advanceToHead`, the
`HeadTrustingWorker`, breaks `ES_I15_noSkip`) and `unfencedWorkerViolatesTest`
(`effectUnfenced`, the `UnfencedLedger`, breaks `ES_I14_durableDelivery`), both of §7.3, and
`staleBuildReplayViolatesTest` (`decideBlind`, the store before ES-06F, breaks
`replayWithinEpoch`) and `replayingRestartViolatesTest` (`replayingRestart`, a restart that
kicks every commit again, breaks `reactorsNeverReplayed`). A scenario is a `List[Stim]` read by
both its `run` (through `stim`, an `if` chain, so `quint test` samples it once) and its script
in the module `event_store_traces` (through `recorded`, which records each argument as a pick of
`step`), so the regression trace in `formal/traces/regression/` (listed in its `manifest.toml`)
takes exactly the steps of the test; ES-07's driver replays them (`mbt::actionTaken` is the model action,
`last` its outcome).

Negative controls tried (ES-06; scratch mutations, not committed). Each fails the scenario named
and makes `quint run` of the relation named (3000 traces of 20 steps, only that invariant)
report `[violation]`, except where a directed run is named: a commit that writes only stream `a`
of its unit (`ES_I1_atomicUnit`; `rollbackTest`, `step`); no expected-revision check
(`ES_I2_streamOrder`; `lostRaceTest`, `step`); any held head blocking every unit
(`ES_I3_independentStreams`; `independentHeldWritersTest`, `step`); no identity check
(`ES_I4_globalEventIdentity`; `duplicateAcrossStreamsTest`, `step`); intent-blind receipts in the
store and the journal (`ES_I5_commandIdentity`; `receiptReplayTest`, `relayStep`); a refusal
replayed whatever its streams did, and one compared on its lowest bound stream only
(`refusalReplayedAtSameRevision`; `refusedThenAcceptedTest` and
`refusalReplayedAtSameRevisionTest`, `step`); a forgetful inbox (`ES_I6_messageIdentity`;
`inboxRedeliveryTest`, `step`); a crash that loses the newest event of a stream
(`ES_I7_historyPreservation`; `crashBeforeReactorsTest`, `relayStep`); no dual write, and no
cutover fence (`ES_I11_rebuildEquivalence`; `shadowRebuildDualWriteTest`, `cutoverFenceTest`,
`step`); a head that takes its batch's epoch instead of the maximum (`ES_I12_epochSafety`;
`epochRaiseTest`; simulation misses it, a directed run of two appends reports it); an advance
that clears the mark whatever its revision (`ES_I14_durableDelivery`;
`markDuringProcessingTest`, `relayStep`); a claim that starts one revision past the checkpoint
(`ES_I15_noSkip`; `commitBetweenFetchAndApplyTest`, `relayStep`); one slot locking heads in
reverse order (`ES_I16_lockOrder`; `oppositeOrderUnitsTest`; simulation misses the eight-step
interleaving, a directed run reports the cycle); a relay reading one revision past its mark
(`directivesFromJournal`; a directed run of an effect after the advance); a restart that replays
every commit's kick (`reactorsNeverReplayed`; `crashBeforeReactorsTest`, `relayStep`). The
first Apalache run of `step` and `implStepSerial` found a bug of the model itself at depth 4:
inline rows computed from the rows of now instead of the rows at the unit's expected revision,
so a unit whose expected revision another unit reached in between wrote a stale count; the
row is now the fold at the expected revision (`rowAfter`), as §3.2 says.

Negative controls tried at ES-06F (scratch mutations, not committed): the journal answering a
replay without the epoch gate (`epochRaceTest` fails; `quint run` of `step` reports a
`replayWithinEpoch` violation); the store's decision without it (`epochRaceTest`, `step`); a
refusal carrying an inbox key accepted (`refusalWithInboxTest`; `step` breaks
`ES_I6_messageIdentity`); a held head blocking whatever its mode (`step` breaks
`ES_I3_independentStreams`: two readers of one stream); a worker crash that resets its stream's
checkpoint (`deliveryOnlyStep` breaks `ES_I14_durableDelivery`; `relayStep` at 3000 traces of 20
steps misses it); a relay that sends every delivered event, not only `DirectiveIssued`
(`relayStep` at 5000 traces of 40 steps breaks `directivesFromJournal`; 20 steps miss it). The
review's other points: the first conjunct of `directivesFromJournal` is kept, as it pins the
relay's filter (the mutant just named); `ckpt >= ckptSeen` became the per-step `ckptBack` ghost;
kicks are named by their unit and the commit counter moved to the ghost (ES-I17).

Negative control tried at ES-05 (scratch mutation, not committed): a `retire` that resets each
lease's fence fails `retireThenRegisterTest` (the new claim's fence is 1, not 2, the fence the
retired consumer's claim already handed out).

## Promoting a counterexample

A counterexample from `formal-verify` or a fault-lab verdict is promoted as verification §6.2
step 9 says: reproduce it with the seed, decide which side is wrong (the model first, if the
model is), reduce it to a named run in the model's tests, and keep it as a regression.
