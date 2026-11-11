// -*- mode: Bluespec; -*-
/**
 * The image-store model (DP-07, ADR 0087): one image base on one host, the data plane's
 * registry entry for it (`rec`), the VM Tart lists under its name (`vm`), the job clones made
 * from it and the clone intents recorded before them, and the writers mayfly does not control (a
 * foreign VM created under the base's name, a modified disk). The Rust side is
 * `mayfly-adapter-tart`'s `images` (the state machine and recovery), `proof` (the clone and
 * delete proofs) and `provider::clone_vm`; the `ImageStore` contract and the fault cases of
 * `tests/tart/images.rs` pin it.
 *
 * Every action is one atomic step of the implementation: the registry writes a state before the
 * effect it permits, and a crash may fall between any two actions, so recovery (`recover*`)
 * reads only what persists: `rec` and the world. Checks read the world exactly (the file
 * identity, the fingerprint, the MAC and the stopped state are observations); a writer swapping
 * a directory between a check and its effect is outside the model, as it is outside the trust
 * model (ADR 0087: only root and the daemon's user may write `TART_HOME`, checked at start-up).
 * Clone lineage made outside mayfly is not modelled: Tart keeps none, and eviction cannot see it.
 *
 * Ghost state (`g`) records the facts at each clone and delete for the invariants and never
 * feeds a guard.
 */
module image_store {
  // ---- constants ----------------------------------------------------------------------------

  pure val ABSENT = "Absent"
  pure val RESERVED = "Reserved"
  pure val CLONING = "Cloning"
  pure val CUSTOMIZING = "Customizing"
  pure val VERIFYING = "Verifying"
  pure val READY = "Ready"
  pure val RETIRING = "Retiring"
  pure val QUARANTINED = "Quarantined"

  /// Job clones (attempts' VMs) that may be made from the base.
  pure val JOBS = Set(1, 2)

  // ---- state --------------------------------------------------------------------------------

  type Entry = { state: str, identity: bool }
  type Vm = { exists: bool, ours: bool, running: bool, intact: bool }
  type Facts = {
    owned: bool, ready: bool, integrity: bool, stopped: bool, noDeps: bool, quarantined: bool,
  }
  type Ghost = { clonedWith: Set[Facts], deletedWith: Set[Facts] }

  var rec: Entry
  var vm: Vm
  var clones: Set[int]
  var intents: Set[int]
  var g: Ghost

  pure val NO_VM = { exists: false, ours: false, running: false, intact: false }

  // ---- observations (what the implementation can check) -------------------------------------

  /// The VM under the name is the one recorded: file identity and MAC (`proof::identity_holds`).
  val identityHolds = vm.exists and vm.ours and rec.identity
  /// Its files are as fingerprinted when it became Ready (`proof::ready_for_clone`).
  val integrityHolds = vm.exists and vm.intact
  /// The facts an effect on the base is taken under.
  val facts: Facts = {
    owned: vm.exists and vm.ours,
    ready: rec.state == READY,
    integrity: integrityHolds,
    stopped: not(vm.running),
    noDeps: clones == Set(),
    quarantined: rec.state == QUARANTINED,
  }

  action init = all {
    rec' = { state: ABSENT, identity: false },
    vm' = NO_VM,
    clones' = Set(),
    intents' = Set(),
    g' = { clonedWith: Set(), deletedWith: Set() },
  }

  action unchangedClones = all { clones' = clones, intents' = intents }

  // ---- the build (images::materialise) ------------------------------------------------------

  /// A name may be reserved: no entry, and a complete listing shows it absent.
  val canReserve = rec.state == ABSENT and not(vm.exists)

  /// CloneAllowed: Ready, the recorded identity holds, the fingerprint holds, stopped.
  def canJobClone(j: int): bool =
    not(intents.contains(j)) and rec.state == READY and identityHolds and integrityHolds
      and not(vm.running)

  /// DeleteAllowed: Ready (never Quarantined), the recorded identity holds, stopped, no recorded
  /// clone or intent exists.
  val canEvict = rec.state == READY and rec.identity and identityHolds and not(vm.running)
    and intents.intersect(clones) == Set()

  /// A complete listing shows the name absent: the entry is written `Reserved`.
  action reserve = all {
    canReserve,
    rec' = { state: RESERVED, identity: false },
    vm' = vm, unchangedClones, g' = g,
  }

  /// `Cloning` is written before `tart clone`.
  action beginClone = all {
    rec.state == RESERVED, not(vm.exists),
    rec' = { ...rec, state: CLONING },
    vm' = vm, unchangedClones, g' = g,
  }

  /// `tart clone <reference> <base>`: the VM exists, ours, unproven until its MAC is recorded.
  action cloneEffect = all {
    rec.state == CLONING, not(vm.exists),
    vm' = { exists: true, ours: true, running: false, intact: true },
    rec' = rec, unchangedClones, g' = g,
  }

  /// Its own MAC and file identity recorded: `Customizing`.
  action prove = all {
    rec.state == CLONING, vm.exists, vm.ours,
    rec' = { state: CUSTOMIZING, identity: true },
    vm' = vm, unchangedClones, g' = g,
  }

  /// One boot under the shim, after the fresh clone's content was verified.
  action boot = all {
    rec.state == CUSTOMIZING, identityHolds, not(vm.running), vm.intact,
    vm' = { ...vm, running: true },
    rec' = rec, unchangedClones, g' = g,
  }

  /// The runner installed and checked, then `tart stop`, then `Verifying` written.
  action installAndStop = all {
    rec.state == CUSTOMIZING, vm.running,
    vm' = { ...vm, running: false },
    rec' = { ...rec, state: VERIFYING },
    unchangedClones, g' = g,
  }

  /// Stopped, identity checked, the final fingerprint taken: `Ready`.
  action markReady = all {
    rec.state == VERIFYING, identityHolds, not(vm.running),
    rec' = { ...rec, state: READY },
    vm' = { ...vm, intact: true },
    unchangedClones, g' = g,
  }

  // ---- job clones (provider::clone_vm) and their VMs ----------------------------------------

  /// Under the admission lock: revalidate (Ready, stopped, identity, fingerprint), record the
  /// intent, clone. CloneAllowed ⇒ Owned ∧ Ready ∧ IntegrityValid.
  action jobClone(j: int): bool = all {
    canJobClone(j),
    intents' = intents.union(Set(j)),
    clones' = clones.union(Set(j)),
    g' = { ...g, clonedWith: g.clonedWith.union(Set(facts)) },
    rec' = rec, vm' = vm,
  }

  /// A crash after the intent was written and before (or during) `tart clone`: the clone may
  /// exist with no instance id recorded; the intent stays.
  action jobCloneIntentOnly(j: int): bool = all {
    canJobClone(j),
    intents' = intents.union(Set(j)),
    clones' = clones,
    rec' = rec, vm' = vm, g' = g,
  }

  /// An attempt's VM is deleted (DeleteVm, with its own proof).
  action jobGone(j: int): bool = all {
    clones.contains(j),
    clones' = clones.exclude(Set(j)), intents' = intents,
    rec' = rec, vm' = vm, g' = g,
  }

  // ---- eviction (images::evict) -------------------------------------------------------------

  /// Under the admission lock: identity, stopped, and no recorded clone or intent that still
  /// exists; `Retiring`, then `tart delete`. DeleteAllowed ⇒ Owned ∧ Stopped ∧ NoDeps.
  /// The first half of an eviction: `Retiring` is written (absent clones pruned); the delete is
  /// [`retire`], a separate step, so a crash between them is a state the model reaches.
  action evict = all {
    canEvict,
    rec' = { ...rec, state: RETIRING },
    vm' = vm, clones' = clones, intents' = intents.intersect(clones), g' = g,
  }

  // ---- recovery after a crash (images::settle), from what persists ---------------------------

  /// `Reserved` or `Cloning` with a VM under the name: unproven, preserved, quarantined.
  action recoverUnproven = all {
    rec.state.in(Set(RESERVED, CLONING)), vm.exists,
    rec' = { ...rec, state: QUARANTINED },
    vm' = vm, unchangedClones, g' = g,
  }

  /// `Cloning` and the name absent on a complete listing: retry from `Reserved`.
  action recoverRetry = all {
    rec.state == CLONING, not(vm.exists),
    rec' = { ...rec, state: RESERVED },
    vm' = vm, unchangedClones, g' = g,
  }

  /// `Customizing`, `Verifying` or `Retiring`, proven and still running (a crash while it was
  /// booted): stopped with `tart stop` first.
  action recoverStop = all {
    rec.state.in(Set(CUSTOMIZING, VERIFYING, RETIRING)), identityHolds, vm.running,
    vm' = { ...vm, running: false },
    rec' = rec, unchangedClones, g' = g,
  }

  /// The delete (`images::retire`), after an eviction's `Retiring` or in recovery from
  /// `Customizing`, `Verifying` or `Retiring`: proven, stopped, and no recorded clone or intent
  /// still listed (the same guard as eviction; by construction a base not Ready has none, and
  /// the implementation checks it all the same).
  action retire = all {
    rec.state.in(Set(CUSTOMIZING, VERIFYING, RETIRING)), identityHolds, not(vm.running),
    intents.intersect(clones) == Set(),
    g' = { ...g, deletedWith: g.deletedWith.union(Set(facts)) },
    vm' = NO_VM,
    rec' = { state: ABSENT, identity: false },
    unchangedClones,
  }

  /// A recorded base no longer listed: its entry is dropped (an observed complete absence).
  /// Clones made from it stay VMs, but are no dependents of any base rebuilt under the name
  /// (a new `base_id`), so they leave this base's books with it.
  action recoverAbsent = all {
    rec.state.in(Set(CUSTOMIZING, VERIFYING, RETIRING, READY)), not(vm.exists),
    rec' = { state: ABSENT, identity: false },
    vm' = vm, clones' = Set(), intents' = Set(), g' = g,
  }

  /// A recorded base whose identity no longer holds: quarantined, never deleted.
  action recoverChanged = all {
    rec.state.in(Set(CUSTOMIZING, VERIFYING, RETIRING, READY)), vm.exists, not(identityHolds),
    rec' = { ...rec, state: QUARANTINED },
    vm' = vm, unchangedClones, g' = g,
  }

  /// A Ready base whose fingerprint changed: quarantined at the next clone attempt.
  action quarantineTampered = all {
    rec.state == READY, identityHolds, not(integrityHolds),
    rec' = { ...rec, state: QUARANTINED },
    vm' = vm, unchangedClones, g' = g,
  }

  // ---- writers mayfly does not control ------------------------------------------------------

  /// A foreign VM appears under the base's name.
  action foreignCreate = all {
    not(vm.exists),
    vm' = { exists: true, ours: false, running: false, intact: false },
    rec' = rec, unchangedClones, g' = g,
  }

  /// The base's VM is deleted and a look-alike made in its place (another file identity).
  action replace = all {
    vm.exists, vm.ours, not(vm.running),
    vm' = { exists: true, ours: false, running: false, intact: false },
    rec' = rec, unchangedClones, g' = g,
  }

  /// The base's disk is modified after it was fingerprinted.
  action tamper = all {
    vm.exists, vm.intact,
    vm' = { ...vm, intact: false },
    rec' = rec, unchangedClones, g' = g,
  }

  /// An operator removes a VM that is not mayfly's (by hand, never mayfly).
  action foreignRemove = all {
    vm.exists, not(vm.ours),
    vm' = NO_VM,
    rec' = rec, unchangedClones, g' = g,
  }

  /// An operator resolves a quarantine: the entry is dropped, the VM left to them.
  action operatorResolves = all {
    rec.state == QUARANTINED,
    rec' = { state: ABSENT, identity: false },
    vm' = { ...vm, ours: false },
    clones' = Set(), intents' = Set(), g' = g,
  }

  action step = {
    nondet j = JOBS.oneOf()
    any {
      reserve, beginClone, cloneEffect, prove, boot, installAndStop, markReady,
      jobClone(j), jobCloneIntentOnly(j), jobGone(j),
      evict,
      recoverUnproven, recoverRetry, recoverStop, retire, recoverAbsent, recoverChanged,
      quarantineTampered,
      foreignCreate, replace, tamper, foreignRemove, operatorResolves,
    }
  }

  // ---- invariants ---------------------------------------------------------------------------

  /// CloneAllowed(b) ⇒ Owned ∧ Ready ∧ IntegrityValid: every job clone was made under those.
  val cloneAllowedOnlyWhenOwnedReadyIntact =
    g.clonedWith.forall(f => f.owned and f.ready and f.integrity and f.stopped)

  /// DeleteAllowed(b) ⇒ Owned ∧ Stopped ∧ NoUnresolvedDependents, and never a quarantined
  /// base: every deletion of the base.
  val deleteAllowedOnlyWhenOwnedStoppedFree =
    g.deletedWith.forall(f => f.owned and f.stopped and f.noDeps and not(f.quarantined))

  /// Every clone that exists was recorded first (as an intent or a reference).
  val clonesRecorded = clones.subseteq(intents)

  // Every invariant of the model, so a new one cannot be left out of a stage.
  val allInvariants = and {
    cloneAllowedOnlyWhenOwnedReadyIntact,
    deleteAllowedOnlyWhenOwnedStoppedFree,
    clonesRecorded,
  }

  // ---- scenarios ----------------------------------------------------------------------------

  action toReady = reserve.then(beginClone).then(cloneEffect).then(prove).then(boot)
    .then(installAndStop).then(markReady)

  // The build reaches Ready, a job clone is made from it, and once the job's VM is gone the
  // base is evicted.
  run buildCloneEvictTest = {
    init.then(toReady).expect(rec.state == READY and identityHolds)
      .then(jobClone(1)).expect(clones == Set(1))
      .expect(not(canEvict))
      .then(jobGone(1)).then(evict).expect(rec.state == RETIRING).then(retire)
      .expect(not(vm.exists) and allInvariants)
  }

  // A crash after the clone and before its MAC: the clone is preserved and quarantined, never
  // cloned from and never evicted.
  run crashAfterCloneQuarantinesTest = {
    init.then(reserve).then(beginClone).then(cloneEffect)
      .then(recoverUnproven).expect(rec.state == QUARANTINED and vm.exists)
      .expect(not(canJobClone(1)))
      .expect(not(canEvict))
  }

  // A crash while the base was booted: recovery stops it with `tart stop`, then retires it.
  run crashWhileBootedTest = {
    init.then(reserve).then(beginClone).then(cloneEffect).then(prove).then(boot)
      .expect(vm.running)
      .then(recoverStop).then(retire)
      .expect(rec.state == ABSENT and not(vm.exists) and allInvariants)
  }

  // A crash between `Retiring` and the delete: recovery deletes it; one whose VM a complete
  // listing no longer shows is settled by that absence.
  run crashWhileRetiringTest = {
    init.then(toReady).then(evict).expect(rec.state == RETIRING and vm.exists)
      .expect(not(canJobClone(1)))
      .then(retire).expect(rec.state == ABSENT and not(vm.exists) and allInvariants)
  }

  // A quarantined base whose identity still holds is never evicted.
  run quarantinedNeverEvictedTest = {
    init.then(toReady).then(tamper).then(quarantineTampered)
      .expect(identityHolds and not(canEvict))
  }

  // A crash while customizing: a proven, stopped base is retired and the build starts again.
  run crashWhileCustomizingRetiresTest = {
    init.then(reserve).then(beginClone).then(cloneEffect).then(prove)
      .then(retire).expect(rec.state == ABSENT and not(vm.exists))
      .then(toReady).expect(rec.state == READY and allInvariants)
  }

  // A crash after the clone intent and before the clone: the intent blocks eviction only while a
  // VM it names exists; with none listed, eviction proceeds.
  run lostIntentDoesNotBlockForeverTest = {
    init.then(toReady).then(jobCloneIntentOnly(1)).expect(intents == Set(1) and clones == Set())
      .then(evict).then(retire).expect(not(vm.exists) and allInvariants)
  }

  // A disk modified after Ready: no clone; the base is quarantined.
  run tamperedBaseIsNeverClonedTest = {
    init.then(toReady).then(tamper)
      .expect(not(canJobClone(1)))
      .then(quarantineTampered).expect(rec.state == QUARANTINED)
  }

  // A look-alike in the base's place is never evicted and never cloned from.
  run lookAlikeNeverTouchedTest = {
    init.then(toReady).then(replace)
      .expect(not(canEvict))
      .expect(not(canJobClone(1)))
      .then(recoverChanged).expect(rec.state == QUARANTINED and vm.exists)
  }

  // A foreign VM under the name is never reserved over.
  run foreignNameNeverReservedTest = {
    init.then(foreignCreate).expect(not(canReserve))
  }

  // ---- negative controls: a guard dropped is caught by its invariant ------------------------

  /// An eviction that ignores the clones that depend on the base.
  action evictIgnoringDependents = all {
    identityHolds, not(vm.running),
    g' = { ...g, deletedWith: g.deletedWith.union(Set(facts)) },
    vm' = NO_VM, rec' = { state: ABSENT, identity: false },
    clones' = clones, intents' = intents,
  }

  /// A clone that skips the fingerprint.
  action cloneIgnoringIntegrity(j: int): bool = all {
    rec.state == READY, identityHolds, not(vm.running),
    intents' = intents.union(Set(j)), clones' = clones.union(Set(j)),
    g' = { ...g, clonedWith: g.clonedWith.union(Set(facts)) },
    rec' = rec, vm' = vm,
  }

  /// An eviction that skips the state guard: deletes a quarantined base.
  action evictQuarantined = all {
    identityHolds, not(vm.running), intents.intersect(clones) == Set(),
    g' = { ...g, deletedWith: g.deletedWith.union(Set(facts)) },
    vm' = NO_VM, rec' = { state: ABSENT, identity: false },
    clones' = Set(), intents' = Set(),
  }

  /// An eviction by name: no ownership or identity proof.
  action evictByName = all {
    vm.exists, not(vm.running), intents.intersect(clones) == Set(),
    g' = { ...g, deletedWith: g.deletedWith.union(Set(facts)) },
    vm' = NO_VM, rec' = { state: ABSENT, identity: false },
    clones' = Set(), intents' = Set(),
  }

  run evictQuarantinedIsFlaggedTest = {
    init.then(toReady).then(tamper).then(quarantineTampered).then(evictQuarantined)
      .expect(not(deleteAllowedOnlyWhenOwnedStoppedFree))
  }

  run evictByNameIsFlaggedTest = {
    init.then(toReady).then(replace).then(evictByName)
      .expect(not(deleteAllowedOnlyWhenOwnedStoppedFree))
  }

  run evictIgnoringDependentsIsFlaggedTest = {
    init.then(toReady).then(jobClone(1)).then(evictIgnoringDependents)
      .expect(not(deleteAllowedOnlyWhenOwnedStoppedFree))
  }

  run cloneIgnoringIntegrityIsFlaggedTest = {
    init.then(toReady).then(tamper).then(cloneIgnoringIntegrity(1))
      .expect(not(cloneAllowedOnlyWhenOwnedReadyIntact))
  }
}
