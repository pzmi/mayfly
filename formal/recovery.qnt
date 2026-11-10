// -*- mode: Bluespec; -*-
/**
 * The recovery model (KER-07): the startup use case of one host (plan §10) across restarts,
 * and the recovery decision table it applies to each VM. The Rust table is
 * `mayfly_control_domain::operations::recovery::classify`; `classify` below is its
 * abstraction for the inputs this model generates, and the conformance driver (VER-01) maps
 * each action onto the startup use case.
 *
 * Durable state survives a crash: the journal (`j`: the attempts' records, the highest minted
 * epoch, the safe-mode marker of `HostRun`, the GitHub messages recorded), the backup taken
 * before every migration (`backup`), the external epoch floor in `TART_HOME` (`floor`), the
 * provider (`world`, with whether each VM runs and under a live shim), the frames on the link (`net`) and
 * the recovery plan's stream (`plan`). The run (`run`) and the data plane's session (`dp`)
 * are volatile. A crash can happen between any two actions, so it happens at every await of
 * the startup sequence: after the backup, after the epoch is minted and before the floor is
 * raised, between the inventory and each classification, between a GitHub message's record
 * and its ack.
 *
 * Every reply of the data plane is lost: an effect is learned only from an inventory, which
 * is the hardest case for I10. A frame stays on the link after it executes (at-least-once) and
 * may arrive in a later session, where it is never executed (ADR 0042).
 *
 * Ghost state (`g`) records history for the invariants and never feeds a guard.
 */
module recovery {
  // ---- constants ----------------------------------------------------------------------------

  /// Attempt ids; attempt `a`'s VM is named `a`.
  pure val ATTEMPTS = Set(1, 2)
  /// The image base (`mfb-*`), a VM no attempt names (I05).
  pure val BASE = 0
  /// An `mf-*` name no attempt records (managed-looking, I04).
  pure val STRAY = 3
  pure val NAMES = Set(BASE, 1, 2, STRAY)
  /// Provider instance ids (the random MAC of ADR 0026).
  pure val INSTANCES = Set(1, 2)
  /// GitHub scale-set messages.
  pure val MSGS = Set(1, 2)
  pure val NONE = 0
  /// Pipeline steps of one healthy start: open, verify, mint, welcome, inventory, two
  /// classifications, the stray, completion (I11).
  pure val PIPELINE = 9

  // Record phases (`vm_state` of dm §6.4, the ones recovery meets).
  pure val CLONING = "Cloning"
  pure val BOOTING = "Booting"
  pure val PREPARED = "Prepared"
  pure val BUSY = "Busy"
  pure val RETIRING = "Retiring"
  pure val DELETING = "Deleting"
  pure val DELETED = "Deleted"
  pure val LOST = "Lost"
  pure val QUARANTINED = "Quarantined"
  pure val INIT_PHASES = Set(CLONING, PREPARED, BUSY, DELETING, DELETED, QUARANTINED)

  pure val NO_DIRECTIVE = "none"
  pure val CLONE_VM = "CloneVm"
  pure val DELETE_VM = "DeleteVm"

  // Run phases: the startup sequence of plan §10.
  pure val DOWN = "down"
  pure val OPENED = "opened"
  pure val VERIFIED = "verified"
  pure val MINTED = "minted"
  pure val WELCOMED = "welcomed"
  pure val INVENTORIED = "inventoried"
  pure val OPERATING = "operating"

  // GitHub's view of an attempt's runner, an environment choice read at classification.
  pure val GH_VIEWS = Set("online", "busy", "offline", "absent", "unreachable")

  // Decisions (`recovery::Decision`; a proposal in safe mode is `safe:` + its action).
  pure val WAIT = "wait"
  pure val ADOPT = "adopt"
  pure val RETRY = "retry_same_name"
  pure val RETIRE = "retire"
  pure val CONFIRM_DELETED = "confirm_deleted"
  pure val MARK_LOST = "mark_lost"
  pure val DELETE_LEAKED = "delete_leaked"
  pure val SUSPEND = "suspend_cleanup"
  pure val QUARANTINE = "quarantine"
  pure val KEEP = "keep"
  pure val REPORT = "report"
  pure val IGNORE = "ignore"

  // ---- state --------------------------------------------------------------------------------

  type Record = {
    phase: str,
    // recorded from VmCloned; NONE before
    instance: int,
    // the pending overlay; every pending outcome here is Unknown (replies are lost)
    pending: str,
    unknown: bool,
    // the highest run epoch the attempt accepted (`AttemptAdopted`)
    epoch: int,
    // the run epoch of the run that classified it; NONE before
    seen: int,
    // cleanup suspended for inspection
    hold: bool,
  }

  type Journal = {
    // the highest minted epoch (`EpochMinted` of `HostRun`)
    epoch: int,
    // changes to the streams a plan binds (attempts, epochs): the heads digest
    version: int,
    recs: int -> Record,
    // `HostRun.safe_mode`
    safe: bool,
    // every stored kind is known and the structure verifies
    verifies: bool,
    // GitHub messages recorded (the listener's inbox)
    msgs: Set[int],
  }

  /// A VM as the inventory lists it. A stopped VM stands for both rows of plan §10's "stopped"
  /// (its shim wrote `exit.json`, or the host rebooted under it): the table reads them alike.
  type Vm = { exists: bool, instance: int, running: bool, shim: bool }

  type Frame = { epoch: int, name: int, kind: str, proof: int }

  type Run = {
    phase: str,
    epoch: int,
    safe: bool,
    // the last inventory: complete or not, and what it listed
    invOk: bool,
    inv: int -> Vm,
    // the stray name was classified in this run
    strayDone: bool,
  }

  type Dp = { session: int, safe: bool }

  type Plan = {
    // "none" | "proposed" | "approved" | "refused" | "completed"
    phase: str,
    jv: int,
    ep: int,
    inv: int -> Vm,
    actions: int -> str,
    intent: Set[int],
    done: Set[int],
  }

  type Gh = { pending: Set[int], acked: Set[int] }

  type Ghost = {
    // "init" | "pipeline" | "observation" | "saga" | "plan" | "env" | "fault"
    lastKind: str,
    // consecutive pipeline steps since the last fault (I11)
    progress: int,
    lastSession: int,
    sessionNotIncreasing: bool,
    trustedBelowFloor: bool,
    epochUsedAboveFloor: bool,
    staleExecuted: bool,
    effectInSafe: bool,
    destructiveInSafe: bool,
    deleteWithoutProof: bool,
    deletedUnrecorded: bool,
    baseTargeted: bool,
    quarantinedDeleted: bool,
    busyRetiredWhileRunning: bool,
    unknownClearedWithoutObservation: bool,
    inconsistentResolution: bool,
    absenceFromIncomplete: bool,
    admittedOnIncomplete: bool,
    planUnbound: bool,
    outcomeWithoutIntent: bool,
    ackWithoutRecord: bool,
  }

  var j: Journal
  var backup: Journal
  var floor: int
  var world: int -> Vm
  var net: Set[Frame]
  var daemon: Run
  var dp: Dp
  var plan: Plan
  var gh: Gh
  var g: Ghost

  // ---- helpers ------------------------------------------------------------------------------

  pure val NO_VM = { exists: false, instance: NONE, running: false, shim: false }
  pure val EMPTY_INV = NAMES.mapBy(_ => NO_VM)

  pure def terminal(phase: str): bool = Set(DELETED, LOST).contains(phase)

  pure def max2(x: int, y: int): int = if (x > y) x else y

  /// The decision table for one record, abstracted to this model's inputs. The rows that
  /// depend on a runner registration (Activating, Listening) belong to the registration model
  /// (KER-09) and are pinned in Rust by the table test; here a record is never Activating or
  /// Listening, and GitHub's view is only read for Prepared, Busy and Deleting. `safe`: the journal
  /// is not trusted (below the floor, unknown kind, failed verification, safe-mode marker);
  /// the decision is then a plan proposal, never destructive.
  pure def classify(safe: bool, r: Record, vm: Vm, complete: bool, view: str): str =
    if (not(complete)) WAIT
    else if (safe) proposal(r, vm)
    else if (r.phase == QUARANTINED) KEEP
    else if (vm.exists and r.instance != NONE and vm.instance != r.instance)
      if (terminal(r.phase)) REPORT else QUARANTINE
    else if (r.phase == CLONING)
      if (not(vm.exists)) RETRY
      else if (r.pending == CLONE_VM) ADOPT
      // a VM of our exact name we never asked for: ownership doubt
      else QUARANTINE
    else if (not(vm.exists))
      if (r.phase == DELETING) CONFIRM_DELETED
      else if (terminal(r.phase)) IGNORE
      else MARK_LOST
    // present, and the recorded instance id matches (or none recorded on a terminal record)
    else if (terminal(r.phase)) (if (r.instance == NONE) REPORT else DELETE_LEAKED)
    else if (r.phase == BOOTING) (if (vm.running and not(vm.shim)) RETIRE else ADOPT)
    else if (r.phase == PREPARED)
      if (not(vm.running) or not(vm.shim)) RETIRE
      else if (Set("online", "busy", "offline").contains(view)) QUARANTINE
      else ADOPT
    // a Busy VM that runs is never retired, whatever GitHub says (F13); a stopped one only
    // once GitHub shows its runner offline or gone, never on `unreachable`
    else if (r.phase == BUSY)
      if (not(vm.running) and Set("offline", "absent").contains(view)) RETIRE
      else ADOPT
    else if (r.phase == DELETING)
      if (Set("online", "busy").contains(view)) SUSPEND else ADOPT
    // Retiring: cleanup continues
    else ADOPT

  /// Safe mode: a proposal for the recovery plan, never destructive (ADR 0025: quarantine
  /// what cannot be proven; nothing is deleted on a doubt).
  pure def proposal(r: Record, vm: Vm): str =
    if (r.phase == QUARANTINED) KEEP
    else if (not(vm.exists)) (if (terminal(r.phase)) IGNORE else MARK_LOST)
    else if (terminal(r.phase)) REPORT
    else if (r.instance != NONE and vm.instance == r.instance) ADOPT
    else QUARANTINE

  pure def destructive(d: str): bool = Set(RETRY, RETIRE, DELETE_LEAKED).contains(d)

  /// Whether the decision reads the VM's absence.
  pure def readsAbsence(d: str): bool = Set(RETRY, CONFIRM_DELETED, MARK_LOST).contains(d)

  /// The record after a decision, under run epoch `e`. A pending outcome is resolved here only
  /// by what the inventory showed.
  pure def apply(r: Record, d: str, vm: Vm, e: int): Record = {
    val seen = { ...r, seen: e }
    val cleared = { ...seen, pending: NO_DIRECTIVE, unknown: false }
    if (d == WAIT) r
    else if (d == KEEP or d == REPORT or d == IGNORE) seen
    else if (d == ADOPT)
      if (r.phase == CLONING) { ...cleared, phase: BOOTING, instance: vm.instance, epoch: e }
      // a pending delete of a present VM was not applied; the saga issues it again
      else { ...cleared, epoch: e }
    else if (d == RETRY) { ...cleared, phase: CLONING, epoch: e }
    else if (d == RETIRE) { ...cleared, phase: RETIRING, epoch: e }
    else if (d == CONFIRM_DELETED) { ...cleared, phase: DELETED }
    else if (d == MARK_LOST) { ...cleared, phase: LOST }
    else if (d == DELETE_LEAKED) { ...cleared, phase: DELETING, epoch: e }
    else if (d == SUSPEND) { ...cleared, hold: true, epoch: e }
    // QUARANTINE
    else { ...cleared, phase: QUARANTINED }
  }

  /// A resolution of an Unknown outcome agrees with what the inventory listed: a present VM
  /// is never read as a clone not applied or as deleted, an absent one never as a clone applied
  /// or as a VM still there.
  pure def consistent(before: Record, after: Record, vm: Vm): bool =
    if (not(before.unknown) or after.unknown) true
    else if (before.pending == CLONE_VM)
      if (vm.exists) not(Set(CLONING, LOST).contains(after.phase))
        and (after.phase != BOOTING or after.instance == vm.instance)
      else after.phase != BOOTING
    else terminal(after.phase) == not(vm.exists)

  pure def clearedUnknown(before: Journal, after: Journal): bool =
    ATTEMPTS.exists(a => before.recs.get(a).unknown and not(after.recs.get(a).unknown))

  pure def ghostAfter(g0: Ghost, kind: str, before: Journal, after: Journal): Ghost = {
    ...g0,
    lastKind: kind,
    progress:
      if (kind == "pipeline") g0.progress + 1
      else if (kind == "fault") 0
      else g0.progress,
    unknownClearedWithoutObservation: g0.unknownClearedWithoutObservation
      or (clearedUnknown(before, after) and kind != "observation" and kind != "fault"),
  }

  action commit(kind: str, j2: Journal, daemon2: Run, ghost: Ghost): bool = all {
    j' = j2,
    daemon' = daemon2,
    backup' = backup,
    floor' = floor,
    world' = world,
    net' = net,
    dp' = dp,
    plan' = plan,
    gh' = gh,
    g' = ghostAfter(ghost, kind, j, j2),
  }

  /// The plan's bindings hold: journal heads, epoch and a fresh complete inventory (I31).
  val bound = and {
    daemon.phase == INVENTORIED,
    daemon.invOk,
    j.version == plan.jv,
    daemon.epoch == plan.ep,
    daemon.inv == plan.inv,
  }

  // ---- init ---------------------------------------------------------------------------------

  pure def recWith(phase: str, instance: int): Record = {
    phase: phase,
    instance: if (phase == CLONING) NONE else instance,
    pending: if (phase == CLONING) CLONE_VM else if (phase == DELETING) DELETE_VM else NO_DIRECTIVE,
    unknown: phase == CLONING or phase == DELETING,
    epoch: 1,
    seen: NONE,
    hold: false,
  }

  /// The host after an earlier run of epoch 1 ended (cleanly or not): two attempt records and
  /// the provider as the world left it, the floor (2 if a newer daemon's journal was rolled back
  /// by a restore), whether every stored kind is known, and the earlier daemon's in-flight frames.
  action initWith(r1: Record, r2: Record, w1: Vm, w2: Vm, stray: Vm, fl: int, verifies: bool,
                  frames: Set[Frame]): bool = {
    val journal = {
      epoch: 1, version: 0, recs: Map(1 -> r1, 2 -> r2), safe: false, verifies: verifies,
      msgs: Set(),
    }
    all {
      j' = journal,
      backup' = journal,
      floor' = fl,
      world' = Map(BASE -> { exists: true, instance: 1, running: false, shim: false },
                   1 -> w1, 2 -> w2, STRAY -> stray),
      net' = frames,
      daemon' = { phase: DOWN, epoch: NONE, safe: false, invOk: false, inv: EMPTY_INV,
               strayDone: false },
      dp' = { session: fl, safe: false },
      plan' = { phase: "none", jv: 0, ep: 0, inv: EMPTY_INV, actions: ATTEMPTS.mapBy(_ => KEEP),
                intent: Set(), done: Set() },
      gh' = { pending: Set(), acked: Set() },
      g' = {
        lastKind: "init", progress: 0, lastSession: fl, sessionNotIncreasing: false,
        trustedBelowFloor: false, epochUsedAboveFloor: false, staleExecuted: false,
        effectInSafe: false, destructiveInSafe: false, deleteWithoutProof: false,
        deletedUnrecorded: false, baseTargeted: false, quarantinedDeleted: false,
        busyRetiredWhileRunning: false,
        unknownClearedWithoutObservation: false, inconsistentResolution: false,
        absenceFromIncomplete: false, admittedOnIncomplete: false, planUnbound: false,
        outcomeWithoutIntent: false, ackWithoutRecord: false,
      },
    }
  }

  pure def vmWith(present: bool, instance: int, running: bool, shim: bool): Vm =
    if (present) { exists: true, instance: instance, running: running, shim: shim and running }
    else NO_VM

  /// The frame of a record's pending directive, sent by the run of epoch 1.
  pure def oldFrame(a: int, r: Record): Frame =
    { epoch: 1, name: a, kind: r.pending, proof: r.instance }

  action init = {
    nondet p1 = INIT_PHASES.oneOf()
    nondet p2 = INIT_PHASES.oneOf()
    nondet i1 = Set(NONE, 1).oneOf()
    nondet i2 = INSTANCES.oneOf()
    nondet e1 = Set(true, false).oneOf()
    nondet e2 = Set(true, false).oneOf()
    nondet es = Set(true, false).oneOf()
    nondet v1 = INSTANCES.oneOf()
    nondet v2 = INSTANCES.oneOf()
    nondet run1 = Set(true, false).oneOf()
    nondet run2 = Set(true, false).oneOf()
    nondet shim1 = Set(true, false).oneOf()
    nondet shim2 = Set(true, false).oneOf()
    nondet fl = Set(1, 2).oneOf()
    nondet verifies = Set(true, false).oneOf()
    nondet sent = Set(true, false).oneOf()
    val r1 = recWith(p1, if (p1 == QUARANTINED) i1 else 1)
    val r2 = recWith(p2, i2)
    initWith(r1, r2,
      vmWith(e1, v1, run1, shim1), vmWith(e2, v2, run2, shim2),
      vmWith(es, 2, true, true), fl, verifies,
      if (sent) ATTEMPTS.filter(a => Map(1 -> r1, 2 -> r2).get(a).pending != NO_DIRECTIVE)
                  .map(a => oldFrame(a, Map(1 -> r1, 2 -> r2).get(a)))
      else Set())
  }

  // ---- the startup pipeline (plan §10) ------------------------------------------------------

  /// Lock the state directory and open the store; the automatic backup precedes any migration.
  action start = all {
    daemon.phase == DOWN,
    j' = j,
    backup' = j,
    floor' = floor,
    world' = world,
    net' = net,
    daemon' = { ...daemon, phase: OPENED, safe: false, invOk: false, inv: EMPTY_INV, strayDone: false },
    dp' = dp,
    plan' = plan,
    gh' = gh,
    g' = ghostAfter({ ...g, progress: 0 }, "pipeline", j, j),
  }

  /// Verify the epoch floor, the stored kinds, the structure and the safe-mode marker. A
  /// journal below the floor was rolled back (I17): safe mode, recorded before anything else.
  action verify = {
    val safe = j.epoch < floor or not(j.verifies) or j.safe
    all {
      daemon.phase == OPENED,
      commit("pipeline", { ...j, safe: safe }, { ...daemon, phase: VERIFIED, safe: safe },
        { ...g, trustedBelowFloor: g.trustedBelowFloor or (not(safe) and j.epoch < floor) }),
    }
  }

  /// `StartRun` and `MintEpoch` above both the journal and the floor.
  action mint = {
    val e = max2(j.epoch, floor) + 1
    all {
      daemon.phase == VERIFIED,
      commit("pipeline", { ...j, epoch: e, version: j.version + 1 },
        { ...daemon, phase: MINTED, epoch: e }, g),
    }
  }

  /// The handshake: the data plane raises and fsyncs the floor, then enters the session
  /// strictly above it, in the mode the control plane sends (`Welcome{epoch, mode}`).
  action welcome = all {
    daemon.phase == MINTED,
    daemon.epoch > floor,
    j' = j,
    backup' = backup,
    floor' = daemon.epoch,
    world' = world,
    net' = net,
    daemon' = { ...daemon, phase: WELCOMED },
    dp' = { session: daemon.epoch, safe: daemon.safe },
    plan' = plan,
    gh' = gh,
    g' = { ...ghostAfter(g, "pipeline", j, j),
           lastSession: daemon.epoch,
           sessionNotIncreasing: g.sessionNotIncreasing or daemon.epoch <= g.lastSession },
  }

  /// The data plane's inventory: `tart list`, each shim's lock and `exit.json`. An incomplete
  /// one (a failed or unparseable listing) is a fault and lists nothing; it is never absence.
  action inventoryWith(complete: bool): bool = all {
    Set(WELCOMED, INVENTORIED, OPERATING).contains(daemon.phase),
    // during recovery a complete inventory is taken once; again only after an incomplete one
    daemon.phase != INVENTORIED or not(daemon.invOk) or daemon.safe,
    commit(if (not(complete)) "fault" else if (daemon.phase == OPERATING or daemon.safe) "env"
           else "pipeline",
      j,
      { ...daemon, phase: if (daemon.phase == WELCOMED) INVENTORIED else daemon.phase, invOk: complete,
        inv: if (complete) world else EMPTY_INV },
      g),
  }

  action inventory = {
    nondet complete = Set(true, false).oneOf()
    inventoryWith(complete)
  }

  /// Classify one record by the decision table and record the decision (one append each).
  action classifyWith(a: int, view: str): bool = {
    val r = j.recs.get(a)
    val vm = daemon.inv.get(a)
    val d = classify(daemon.safe, r, vm, daemon.invOk, view)
    val r2 = apply(r, d, vm, daemon.epoch)
    all {
      daemon.phase == INVENTORIED,
      not(daemon.safe),
      r.seen != daemon.epoch,
      commit(if (d == WAIT) "env" else "observation",
        { ...j, recs: j.recs.set(a, r2), version: if (r2 == r) j.version else j.version + 1 },
        daemon,
        {
          ...g,
          progress: if (d == WAIT) g.progress else g.progress + 1,
          inconsistentResolution: g.inconsistentResolution or not(consistent(r, r2, vm)),
          absenceFromIncomplete: g.absenceFromIncomplete or (readsAbsence(d) and not(daemon.invOk)),
          epochUsedAboveFloor: g.epochUsedAboveFloor or (r2.epoch > floor),
          quarantinedDeleted: g.quarantinedDeleted
            or (r.phase == QUARANTINED and destructive(d)),
          busyRetiredWhileRunning: g.busyRetiredWhileRunning
            or (r.phase == BUSY and vm.running and destructive(d)),
        }),
    }
  }

  action classifyRecord = {
    nondet a = ATTEMPTS.oneOf()
    nondet view = GH_VIEWS.oneOf()
    classifyWith(a, view)
  }

  /// The `mf-*` name no attempt records: reported for operator review, never touched (I04).
  action classifyStray = all {
    daemon.phase == INVENTORIED,
    daemon.invOk,
    not(daemon.safe),
    not(daemon.strayDone),
    commit("pipeline", j, { ...daemon, strayDone: true }, g),
  }

  /// `CompleteRecovery`: admission opens, then the GitHub sessions.
  action complete = all {
    daemon.phase == INVENTORIED,
    not(daemon.safe),
    daemon.invOk,
    daemon.strayDone,
    ATTEMPTS.forall(a => j.recs.get(a).seen == daemon.epoch),
    commit("pipeline", j, { ...daemon, phase: OPERATING },
      { ...g, admittedOnIncomplete: g.admittedOnIncomplete or not(daemon.invOk) }),
  }

  // ---- after recovery: the saga, the data plane and the observer ----------------------------

  /// The directive a record's decision leads to: a clone retried by name, or a delete with the
  /// ownership proof built from the record (abstracting the stop and diagnostics before it).
  pure def nextDirective(r: Record): str =
    if (r.pending != NO_DIRECTIVE or r.hold) NO_DIRECTIVE
    else if (r.phase == CLONING) CLONE_VM
    else if (r.phase == RETIRING or r.phase == DELETING) DELETE_VM
    else NO_DIRECTIVE

  /// `IssueNext`: `DirectiveIssued`, sent under the current session; its reply is lost.
  action issueNextWith(a: int): bool = {
    val r = j.recs.get(a)
    val d = nextDirective(r)
    val frame = { epoch: daemon.epoch, name: a, kind: d, proof: r.instance }
    all {
      daemon.phase == OPERATING,
      d != NO_DIRECTIVE,
      j' = { ...j, recs: j.recs.set(a, { ...r, pending: d, unknown: true,
                                         phase: if (d == DELETE_VM) DELETING else r.phase }),
             version: j.version + 1 },
      backup' = backup,
      floor' = floor,
      world' = world,
      net' = net.union(Set(frame)),
      daemon' = daemon,
      dp' = dp,
      plan' = plan,
      gh' = gh,
      g' = { ...ghostAfter(g, "saga", j, j),
             deleteWithoutProof: g.deleteWithoutProof
               or (d == DELETE_VM and (r.instance == NONE)),
             effectInSafe: g.effectInSafe or daemon.safe,
             destructiveInSafe: g.destructiveInSafe or daemon.safe,
             epochUsedAboveFloor: g.epochUsedAboveFloor or daemon.epoch > floor,
             quarantinedDeleted: g.quarantinedDeleted or r.phase == QUARANTINED },
    }
  }

  action issueNext = {
    nondet a = ATTEMPTS.oneOf()
    issueNextWith(a)
  }

  /// The data plane takes one frame off the link, possibly leaving a duplicate behind. A frame
  /// of another session is never executed (answered Unknown, ADR 0042); in a safe session a
  /// clone or delete is refused (I29); a delete is refused unless the VM's instance id is the
  /// proof's (ADR 0026) and never names a base.
  action dpExecuteWith(f: Frame, keep: bool, fresh: int): bool = {
    val vm = world.get(f.name)
    val current = f.epoch == dp.session
    val runs = current and not(dp.safe)
    val deletes = runs and f.kind == DELETE_VM and f.name != BASE and vm.exists
      and vm.instance == f.proof
    val clones = runs and f.kind == CLONE_VM and not(vm.exists)
    val world2 =
      if (deletes) world.set(f.name, NO_VM)
      else if (clones) world.set(f.name, { exists: true, instance: fresh, running: false,
                                           shim: false })
      else world
    val recEpoch = if (ATTEMPTS.contains(f.name)) j.recs.get(f.name).epoch else NONE
    all {
      net.contains(f),
      j' = j,
      backup' = backup,
      floor' = floor,
      world' = world2,
      net' = if (keep) net else net.exclude(Set(f)),
      daemon' = daemon,
      dp' = dp,
      plan' = plan,
      gh' = gh,
      g' = {
        ...ghostAfter(g, if (keep) "fault" else "env", j, j),
        staleExecuted: g.staleExecuted or ((deletes or clones)
          and (f.epoch != dp.session
               or f.epoch < recEpoch)),
        effectInSafe: g.effectInSafe or ((deletes or clones) and dp.safe),
        deletedUnrecorded: g.deletedUnrecorded or (deletes and not(ATTEMPTS.contains(f.name))),
        baseTargeted: g.baseTargeted or f.name == BASE,
        deleteWithoutProof: g.deleteWithoutProof or (deletes and vm.instance != f.proof),
      },
    }
  }

  action dpExecute = {
    nondet f = net.oneOf()
    nondet keep = Set(true, false).oneOf()
    nondet fresh = INSTANCES.oneOf()
    all { net != Set(), dpExecuteWith(f, keep, fresh) }
  }

  /// The observer applies a complete inventory to an Unknown outcome (I10): the only way out.
  action observeWith(a: int): bool = {
    val r = j.recs.get(a)
    val vm = daemon.inv.get(a)
    val r2 =
      if (r.pending == CLONE_VM)
        if (vm.exists) { ...r, phase: BOOTING, instance: vm.instance, pending: NO_DIRECTIVE,
                         unknown: false }
        else { ...r, pending: NO_DIRECTIVE, unknown: false }
      else if (not(vm.exists)) { ...r, phase: DELETED, pending: NO_DIRECTIVE, unknown: false }
      else if (vm.instance == r.instance) { ...r, pending: NO_DIRECTIVE, unknown: false }
      else { ...r, phase: QUARANTINED, pending: NO_DIRECTIVE, unknown: false }
    all {
      daemon.phase == OPERATING,
      daemon.invOk,
      r.unknown,
      commit("observation", { ...j, recs: j.recs.set(a, r2), version: j.version + 1 }, daemon,
        { ...g, inconsistentResolution: g.inconsistentResolution or not(consistent(r, r2, vm)),
          absenceFromIncomplete: g.absenceFromIncomplete or not(daemon.invOk) }),
    }
  }

  action observe = {
    nondet a = ATTEMPTS.oneOf()
    observeWith(a)
  }

  // ---- GitHub: the listener records before it acks (I12) ------------------------------------

  val listening = daemon.phase == OPERATING or (daemon.safe and daemon.phase == INVENTORIED)

  action ghDeliver(m: int): bool = all {
    listening,
    not(gh.acked.contains(m)),
    not(gh.pending.contains(m)),
    j' = j, backup' = backup, floor' = floor, world' = world, net' = net, daemon' = daemon, dp' = dp,
    plan' = plan,
    gh' = { ...gh, pending: gh.pending.union(Set(m)) },
    g' = ghostAfter(g, "env", j, j),
  }

  /// The message is recorded durably (the inbox entry); in safe mode nothing is acquired, but
  /// a message is still recorded and may then be acked (ADR 0029).
  action ghRecord(m: int): bool = all {
    listening,
    gh.pending.contains(m),
    not(j.msgs.contains(m)),
    commit("env", { ...j, msgs: j.msgs.union(Set(m)) }, daemon, g),
  }

  action ghAck(m: int): bool = all {
    listening,
    gh.pending.contains(m),
    j.msgs.contains(m),
    j' = j, backup' = backup, floor' = floor, world' = world, net' = net, daemon' = daemon, dp' = dp,
    plan' = plan,
    gh' = { pending: gh.pending.exclude(Set(m)), acked: gh.acked.union(Set(m)) },
    g' = { ...ghostAfter(g, "env", j, j),
           ackWithoutRecord: g.ackWithoutRecord or not(j.msgs.contains(m)) },
  }

  action github = {
    nondet m = MSGS.oneOf()
    any { ghDeliver(m), ghRecord(m), ghAck(m) }
  }

  // ---- safe mode: the recovery plan (I31) ---------------------------------------------------

  /// `mayfly recover --dry-run`: a plan bound to the journal heads, the inventory and the epoch.
  action proposePlan = all {
    daemon.safe,
    daemon.phase == INVENTORIED,
    daemon.invOk,
    Set("none", "refused").contains(plan.phase),
    j' = j, backup' = backup, floor' = floor, world' = world, net' = net, daemon' = daemon, dp' = dp,
    plan' = {
      phase: "proposed", jv: j.version, ep: daemon.epoch, inv: daemon.inv,
      actions: ATTEMPTS.mapBy(a => classify(true, j.recs.get(a), daemon.inv.get(a), true, "unreachable")),
      intent: Set(), done: Set(),
    },
    gh' = gh,
    g' = { ...ghostAfter(g, "plan", j, j),
           destructiveInSafe: g.destructiveInSafe
             or ATTEMPTS.exists(a => destructive(classify(true, j.recs.get(a), daemon.inv.get(a),
                                                          true, "unreachable"))) },
  }

  /// `--apply <plan-id>` by an operator.
  action approvePlan = all {
    plan.phase == "proposed",
    j' = j, backup' = backup, floor' = floor, world' = world, net' = net, daemon' = daemon, dp' = dp,
    plan' = { ...plan, phase: "approved" },
    gh' = gh,
    g' = ghostAfter(g, "plan", j, j),
  }

  /// The intent of one action, recorded on the plan's stream before the action.
  action planIntentWith(a: int): bool = all {
    plan.phase == "approved",
    not(plan.intent.contains(a)),
    bound,
    j' = j, backup' = backup, floor' = floor, world' = world, net' = net, daemon' = daemon, dp' = dp,
    plan' = { ...plan, intent: plan.intent.union(Set(a)) },
    gh' = gh,
    g' = { ...ghostAfter(g, "plan", j, j), planUnbound: g.planUnbound or not(bound) },
  }

  /// The action itself (an ordinary command on the attempt), then its outcome on the plan.
  action planOutcomeWith(a: int): bool = {
    val r = j.recs.get(a)
    val d = plan.actions.get(a)
    val r2 = apply(r, d, plan.inv.get(a), daemon.epoch)
    val j2 = { ...j, recs: j.recs.set(a, r2), version: j.version + 1 }
    all {
      plan.phase == "approved",
      not(plan.done.contains(a)),
      plan.intent.contains(a),
      bound,
      j' = j2, backup' = backup, floor' = floor, world' = world, net' = net, daemon' = daemon, dp' = dp,
      plan' = { ...plan, done: plan.done.union(Set(a)), jv: j2.version },
      gh' = gh,
      g' = { ...ghostAfter(g, "observation", j, j2),
             planUnbound: g.planUnbound or not(bound),
             outcomeWithoutIntent: g.outcomeWithoutIntent or not(plan.intent.contains(a)),
             inconsistentResolution: g.inconsistentResolution
               or not(consistent(r, r2, plan.inv.get(a))),
             destructiveInSafe: g.destructiveInSafe or destructive(d),
             quarantinedDeleted: g.quarantinedDeleted
               or (r.phase == QUARANTINED and destructive(d)) },
    }
  }

  /// The world, the journal or the epoch moved since the plan was drawn: refused.
  action refusePlan = all {
    plan.phase == "approved",
    not(bound),
    j' = j, backup' = backup, floor' = floor, world' = world, net' = net, daemon' = daemon, dp' = dp,
    plan' = { ...plan, phase: "refused" },
    gh' = gh,
    g' = ghostAfter(g, "plan", j, j),
  }

  /// `RecoveryPlanCompleted`, `SafeModeLeft`: the daemon starts again and re-verifies.
  action completePlan = all {
    plan.phase == "approved",
    plan.done == ATTEMPTS,
    bound,
    j.verifies,
    j' = { ...j, safe: false },
    backup' = backup, floor' = floor, world' = world, net' = net,
    daemon' = { ...daemon, phase: DOWN },
    dp' = dp,
    plan' = { ...plan, phase: "completed" },
    gh' = gh,
    g' = { ...ghostAfter(g, "plan", j, j), planUnbound: g.planUnbound or not(bound) },
  }

  action planStep = {
    nondet a = ATTEMPTS.oneOf()
    any { proposePlan, approvePlan, planIntentWith(a), planOutcomeWith(a), refusePlan,
          completePlan }
  }

  // ---- faults -------------------------------------------------------------------------------

  /// The daemon dies at whatever await it was in (SIGKILL, power loss, upgrade).
  action crash = all {
    daemon.phase != DOWN,
    j' = j, backup' = backup, floor' = floor, world' = world, net' = net,
    daemon' = { phase: DOWN, epoch: NONE, safe: false, invOk: false, inv: EMPTY_INV,
             strayDone: false },
    dp' = dp, plan' = plan, gh' = gh,
    g' = ghostAfter(g, "fault", j, j),
  }

  /// `mayfly restore <backup>` while the daemon is down: a deeper rollback (ADR 0043).
  action restore = all {
    daemon.phase == DOWN,
    j' = backup, backup' = backup, floor' = floor, world' = world, net' = net, daemon' = daemon,
    dp' = dp, plan' = plan, gh' = gh,
    g' = ghostAfter(g, "fault", j, backup),
  }

  /// The provider changes behind the journal's back: a VM vanishes, is replaced by another of
  /// the same name, stops (its shim writes `exit.json`), or loses its shim.
  action worldChangeWith(n: int, change: str, other: int): bool = {
    val vm = world.get(n)
    val vm2 =
      if (change == "vanish") NO_VM
      else if (change == "replace") { ...vm, instance: other }
      else if (change == "stop") { ...vm, running: false, shim: false }
      else { ...vm, shim: false }
    all {
      n != BASE,
      vm.exists,
      j' = j, backup' = backup, floor' = floor, world' = world.set(n, vm2), net' = net,
      daemon' = daemon, dp' = dp, plan' = plan, gh' = gh,
      g' = ghostAfter(g, "fault", j, j),
    }
  }

  action worldChange = {
    nondet n = Set(1, 2, STRAY).oneOf()
    nondet change = Set("vanish", "replace", "stop", "shimDies").oneOf()
    nondet other = INSTANCES.oneOf()
    worldChangeWith(n, change, other)
  }

  // ---- the relations ------------------------------------------------------------------------

  action pipeline = any {
    start, verify, mint, welcome, inventory, classifyRecord, classifyStray, complete,
  }

  action step = any {
    pipeline,
    issueNext,
    dpExecute,
    observe,
    github,
    planStep,
    crash,
    restore,
    worldChange,
  }

  /// Restarts under stress: the startup sequence with crashes at every await, restores,
  /// incomplete inventories, stale frames and a changing provider, but no GitHub traffic and
  /// no plan, so a bounded check spends its depth on the pipeline itself.
  action restartStep = any {
    pipeline,
    issueNext,
    dpExecute,
    observe,
    crash,
    restore,
    worldChange,
  }

  /// Repair: safe mode and the recovery plan, under crashes, restores, a moving provider and
  /// GitHub traffic, without the saga and the data plane, which safe mode holds anyway. A
  /// uniform walk of `step` almost never strings proposal, approval, two intents, two outcomes
  /// and completion together without a fault in between; this relation does.
  action repairStep = any {
    pipeline,
    planStep,
    github,
    crash,
    restore,
    worldChange,
  }

  // ---- invariants ---------------------------------------------------------------------------

  /// I04: an unrecognised VM is never deleted.
  val i04UnrecognisedNeverDeleted = not(g.deletedUnrecorded)
    and net.forall(f => ATTEMPTS.contains(f.name))

  /// I05: a base image is never the target of a directive and is never deleted.
  val i05BaseNeverDeletable = not(g.baseTargeted) and world.get(BASE).exists

  /// I09: no frame executes outside its own session, nor below the epoch the attempt it
  /// targets accepted.
  val i09NoStaleEpochExecutes = not(g.staleExecuted)

  /// I10: an Unknown outcome leaves only on an observation (a restore replaces the journal,
  /// and is a fault, not a resolution).
  val i10UnknownLeavesOnlyOnObservation = not(g.unknownClearedWithoutObservation)

  /// Some step of the startup sequence can still be taken.
  val pipelineEnabled =
    if (daemon.phase == MINTED) daemon.epoch > floor
    else if (daemon.phase == INVENTORIED)
      not(daemon.invOk) or not(daemon.strayDone)
        or ATTEMPTS.exists(a => j.recs.get(a).seen != daemon.epoch)
        or ATTEMPTS.forall(a => j.recs.get(a).seen == daemon.epoch)
    else Set(OPENED, VERIFIED, WELCOMED).contains(daemon.phase)

  /// I11 (bounded): a fault-free start completes within the pipeline's length, every record
  /// classified, and a start the journal trusts never stalls before admission opens.
  val i11RecoveryCompletes = and {
    (g.progress >= PIPELINE and not(daemon.safe) and daemon.phase != DOWN) implies and {
      daemon.phase == OPERATING,
      ATTEMPTS.forall(a => j.recs.get(a).seen == daemon.epoch),
    },
    (not(daemon.safe) and not(Set(DOWN, OPERATING).contains(daemon.phase))) implies pipelineEnabled,
  }

  /// I12: a GitHub message is acked only after its durable record.
  val i12RecordBeforeAck = not(g.ackWithoutRecord)

  /// I17: sessions are entered at strictly increasing epochs, the floor is raised before an
  /// epoch is used, and a journal below the floor is never trusted (safe mode).
  val i17EpochsIncrease = and {
    not(g.sessionNotIncreasing),
    not(g.epochUsedAboveFloor),
    not(g.trustedBelowFloor),
    dp.session <= floor,
  }

  /// I25: an incomplete inventory is never read as absence, and admission never opens on one.
  val i25IncompleteNeverAbsence = not(g.absenceFromIncomplete) and not(g.admittedOnIncomplete)

  /// I29: in a safe session no clone or delete executes.
  val i29SafeModeNoEffects = not(g.effectInSafe)

  /// I31: a plan acts only against the heads, inventory and epoch it was drawn from, and each
  /// action's intent precedes its outcome.
  val i31PlanBound = not(g.planUnbound) and not(g.outcomeWithoutIntent)

  /// safeModeNoDestructiveAction: in safe mode no directive is issued and no decision or plan
  /// action retires, retries or deletes.
  val safeModeNoDestructiveAction = not(g.destructiveInSafe)

  /// deleteNeedsProof: a delete carries the record's instance id and executes only on a VM
  /// with that instance id.
  val deleteNeedsProof = not(g.deleteWithoutProof)
    and net.forall(f => f.kind != DELETE_VM or f.proof != NONE)

  /// noAssertion: every resolution of an Unknown outcome agrees with an inventory listing.
  val noAssertion = not(g.inconsistentResolution)

  /// quarantineNeverDeleted: no decision, plan action or directive deletes a quarantined VM.
  val quarantineNeverDeleted = not(g.quarantinedDeleted)

  /// busyNeverRetiredWhileRunning: recovery never retires a Busy attempt whose VM runs (F13).
  val busyNeverRetiredWhileRunning = not(g.busyRetiredWhileRunning)

  val allInvariants = and {
    i04UnrecognisedNeverDeleted,
    i05BaseNeverDeletable,
    i09NoStaleEpochExecutes,
    i10UnknownLeavesOnlyOnObservation,
    i11RecoveryCompletes,
    i12RecordBeforeAck,
    i17EpochsIncrease,
    i25IncompleteNeverAbsence,
    i29SafeModeNoEffects,
    i31PlanBound,
    safeModeNoDestructiveAction,
    deleteNeedsProof,
    noAssertion,
    quarantineNeverDeleted,
    busyNeverRetiredWhileRunning,
  }

  // ---- scenarios (`quint test`) -------------------------------------------------------------

  pure val R_ABSENT = NO_VM
  pure def running(i: int): Vm = vmWith(true, i, true, true)
  pure def stopped(i: int): Vm = vmWith(true, i, false, false)

  /// A start up to the classification, on a complete or an incomplete inventory.
  action startTo(complete: bool): bool =
    start.then(verify).then(mint).then(welcome).then(inventoryWith(complete))

  /// The rest of a healthy start.
  action finish(v1: str, v2: str): bool =
    classifyWith(1, v1).then(classifyWith(2, v2)).then(classifyStray).then(complete)

  // plan §10 "Listening/Busy | running, shim alive | online/busy": adopt and monitor.
  run busyAdoptedTest = {
    initWith(recWith(BUSY, 1), recWith(DELETED, 2), running(1), R_ABSENT, R_ABSENT, 1, true, Set())
      .then(startTo(true))
      .then(classifyWith(1, "busy"))
      .expect(j.recs.get(1).phase == BUSY and j.recs.get(1).epoch == 2)
      .then(classifyWith(2, "unreachable")).then(classifyStray).then(complete)
      .expect(daemon.phase == OPERATING and g.progress == PIPELINE and allInvariants)
  }

  // "Listening/Busy | running | unreachable": `unreachable` is not `absent`, never delete.
  run unreachableIsNotAbsentTest = {
    initWith(recWith(BUSY, 1), recWith(DELETED, 2), running(1), R_ABSENT, R_ABSENT, 1, true, Set())
      .then(startTo(true)).then(classifyWith(1, "unreachable"))
      .expect(j.recs.get(1).phase == BUSY and allInvariants)
      .expect(classify(false, recWith(BUSY, 1), running(1), true, "absent") == ADOPT)
      .expect(classify(false, recWith(BUSY, 1), stopped(1), true, "unreachable") == ADOPT)
      .expect(classify(false, recWith(BUSY, 1), stopped(1), true, "absent") == RETIRE)
  }

  // "Busy | stopped (exit.json) | offline": retire, then delete with proof.
  run stoppedBusyRetiresWithProofTest = {
    initWith(recWith(BUSY, 1), recWith(DELETED, 2), stopped(1), R_ABSENT, R_ABSENT, 1, true, Set())
      .then(startTo(true)).then(finish("offline", "offline"))
      .expect(j.recs.get(1).phase == RETIRING)
      .then(issueNextWith(1))
      .expect(net == Set({ epoch: 2, name: 1, kind: DELETE_VM, proof: 1 }))
      .then(dpExecuteWith({ epoch: 2, name: 1, kind: DELETE_VM, proof: 1 }, false, 1))
      .expect(not(world.get(1).exists))
      .then(inventoryWith(true)).then(observeWith(1))
      .expect(j.recs.get(1).phase == DELETED and allInvariants)
  }

  // "Cloning | absent (complete inventory)": the Unknown clone was not applied; retry by name.
  run unknownCloneAbsentRetriesTest = {
    initWith(recWith(CLONING, 1), recWith(DELETED, 2), R_ABSENT, R_ABSENT, R_ABSENT, 1, true, Set())
      .then(startTo(true)).then(finish("unreachable", "unreachable"))
      .expect(j.recs.get(1).phase == CLONING and not(j.recs.get(1).unknown))
      .then(issueNextWith(1))
      .then(dpExecuteWith({ epoch: 2, name: 1, kind: CLONE_VM, proof: NONE }, false, 2))
      .then(inventoryWith(true)).then(observeWith(1))
      .expect(j.recs.get(1).phase == BOOTING and j.recs.get(1).instance == 2 and allInvariants)
  }

  // "Cloning | owned clone exists": adopt, record the observed instance id, continue.
  run unknownCloneAppliedAdoptsTest = {
    initWith(recWith(CLONING, 1), recWith(DELETED, 2), stopped(2), R_ABSENT, R_ABSENT, 1, true, Set())
      .then(startTo(true)).then(classifyWith(1, "unreachable"))
      .expect(j.recs.get(1).phase == BOOTING and j.recs.get(1).instance == 2
        and not(j.recs.get(1).unknown) and allInvariants)
  }

  // "Deleting | absent": confirm the deletion; the slot is released.
  run deletingAbsentConfirmedTest = {
    initWith(recWith(DELETING, 1), recWith(DELETED, 2), R_ABSENT, R_ABSENT, R_ABSENT, 1, true, Set())
      .then(startTo(true)).then(classifyWith(1, "unreachable"))
      .expect(j.recs.get(1).phase == DELETED and allInvariants)
  }

  // "Deleting | present | active": suspend cleanup and inspect.
  run activeRunnerSuspendsCleanupTest = {
    initWith(recWith(DELETING, 1), recWith(DELETED, 2), running(1), R_ABSENT, R_ABSENT, 1, true, Set())
      .then(startTo(true)).then(finish("busy", "unreachable"))
      .expect(j.recs.get(1).hold and not(j.recs.get(1).unknown))
      .expect(nextDirective(j.recs.get(1)) == NO_DIRECTIVE)
  }

  // "any | inventory incomplete": wait; nothing released; admission stays closed (I25).
  run incompleteInventoryWaitsTest = {
    initWith(recWith(DELETING, 1), recWith(BUSY, 2), R_ABSENT, R_ABSENT, R_ABSENT, 1, true, Set())
      .then(startTo(false)).then(classifyWith(1, "unreachable"))
      .expect(j.recs.get(1).phase == DELETING and j.recs.get(1).unknown)
      .then(classifyWith(2, "unreachable"))
      .expect(j.recs.get(2).phase == BUSY and j.recs.get(2).seen == NONE)
      .expect(not(daemon.invOk) and daemon.phase == INVENTORIED)
  }

  run incompleteThenCompleteTest = {
    initWith(recWith(DELETING, 1), recWith(DELETED, 2), R_ABSENT, R_ABSENT, R_ABSENT, 1, true, Set())
      .then(startTo(false)).then(inventoryWith(true)).then(finish("unreachable", "unreachable"))
      .expect(daemon.phase == OPERATING and j.recs.get(1).phase == DELETED and allInvariants)
  }

  // "any | name present, instance id differs": quarantine, never delete.
  run instanceMismatchQuarantinesTest = {
    initWith(recWith(PREPARED, 1), recWith(DELETED, 2), running(2), R_ABSENT, R_ABSENT, 1, true, Set())
      .then(startTo(true)).then(finish("unreachable", "unreachable"))
      .expect(j.recs.get(1).phase == QUARANTINED)
      .expect(nextDirective(j.recs.get(1)) == NO_DIRECTIVE)
  }

  // "Quarantined | any | any": keep; the case continues.
  run quarantinedKeptTest = {
    initWith(recWith(QUARANTINED, 1), recWith(DELETED, 2), running(1), R_ABSENT, R_ABSENT, 1, true, Set())
      .then(startTo(true)).then(finish("unreachable", "unreachable"))
      .expect(j.recs.get(1).phase == QUARANTINED and world.get(1).exists)
      .expect(nextDirective(j.recs.get(1)) == NO_DIRECTIVE)
  }

  // "Deleted | owned clone present": a leaked VM, deleted with proof.
  run leakedVmDeletedWithProofTest = {
    initWith(recWith(BUSY, 2), recWith(DELETED, 2), running(2), running(2), R_ABSENT, 1, true, Set())
      .then(startTo(true)).then(finish("busy", "unreachable"))
      .expect(j.recs.get(2).phase == DELETING)
      .then(issueNextWith(2))
      .then(dpExecuteWith({ epoch: 2, name: 2, kind: DELETE_VM, proof: 2 }, false, 1))
      .expect(not(world.get(2).exists) and world.get(1).exists and allInvariants)
  }

  // "no record | mf-* name": reported, never deleted (I04).
  run strayReportedNeverDeletedTest = {
    initWith(recWith(DELETED, 1), recWith(DELETED, 2), R_ABSENT, R_ABSENT, running(1), 1, true, Set())
      .then(startTo(true)).then(finish("unreachable", "unreachable"))
      .expect(daemon.strayDone and world.get(STRAY).exists and net == Set() and allInvariants)
  }

  // "Prepared | running, shim alive | no registration": adopt; re-verify before activation.
  // A runner GitHub knows for a prepared VM is an outcome the journal cannot prove.
  run preparedAdoptedTest = {
    initWith(recWith(PREPARED, 1), recWith(DELETED, 2), running(1), R_ABSENT, R_ABSENT, 1, true, Set())
      .then(startTo(true)).then(classifyWith(1, "absent"))
      .expect(j.recs.get(1).phase == PREPARED and j.recs.get(1).epoch == 2)
  }

  run preparedWithAnUnknownRunnerQuarantinedTest = {
    initWith(recWith(PREPARED, 1), recWith(DELETED, 2), running(1), R_ABSENT, R_ABSENT, 1, true, Set())
      .then(startTo(true)).then(classifyWith(1, "online"))
      .expect(j.recs.get(1).phase == QUARANTINED)
  }

  // A frame of the run before the restart never executes in the new session (I09, ADR 0042).
  run staleFrameNeverExecutesTest = {
    val old = { epoch: 1, name: 1, kind: DELETE_VM, proof: 1 }
    initWith(recWith(DELETING, 1), recWith(DELETED, 2), running(1), R_ABSENT, R_ABSENT, 1, true,
             Set(old))
      .then(startTo(true))
      .then(dpExecuteWith(old, false, 1))
      .expect(world.get(1).exists and net == Set() and allInvariants)
  }

  // A delete whose VM was replaced under the same name is refused by the proof (ADR 0026).
  run replacedVmNeverDeletedTest = {
    val old = { epoch: 1, name: 1, kind: DELETE_VM, proof: 1 }
    initWith(recWith(DELETING, 1), recWith(DELETED, 2), running(1), R_ABSENT, R_ABSENT, 1, true,
             Set(old))
      .then(worldChangeWith(1, "replace", 2))
      .then(dpExecuteWith(old, false, 1))
      .expect(world.get(1).exists and world.get(1).instance == 2)
      .then(startTo(true)).then(classifyWith(1, "unreachable"))
      .expect(j.recs.get(1).phase == QUARANTINED and allInvariants)
  }

  // A crash after the epoch is minted and before the floor is raised: the next start mints
  // above both, and the floor only ever rises (I17).
  run crashBeforeFloorRaisedTest = {
    initWith(recWith(BUSY, 1), recWith(DELETED, 2), running(1), R_ABSENT, R_ABSENT, 1, true, Set())
      .then(start).then(verify).then(mint).expect(j.epoch == 2 and floor == 1)
      .then(crash).then(startTo(true))
      .expect(j.epoch == 3 and floor == 3 and dp.session == 3 and not(daemon.safe) and allInvariants)
  }

  // "journal epoch < floor": a journal another run already moved past is safe mode; nothing
  // destructive executes; a plan bound to what it saw is applied, then a verified restart.
  run rolledBackJournalIsSafeModeTest = {
    initWith(recWith(BUSY, 1), recWith(CLONING, 2), running(1), running(2), R_ABSENT, 2, true, Set())
      .then(startTo(true))
      .expect(daemon.safe and j.safe and dp.safe and j.epoch == 3 and floor == 3)
      .then(proposePlan)
      .expect(plan.actions.get(1) == ADOPT and plan.actions.get(2) == QUARANTINE)
      .then(approvePlan)
      .then(planIntentWith(1)).then(planOutcomeWith(1))
      .then(planIntentWith(2)).then(planOutcomeWith(2))
      .then(completePlan)
      .expect(not(j.safe) and daemon.phase == DOWN and j.recs.get(2).phase == QUARANTINED)
      .then(startTo(true)).expect(not(daemon.safe))
      .then(finish("busy", "unreachable"))
      .expect(daemon.phase == OPERATING and allInvariants)
  }

  // A deeper rollback: `mayfly restore` of the pre-migration backup after a run used a newer
  // epoch is below the floor (ADR 0043).
  run restoredBackupIsSafeModeTest = {
    initWith(recWith(BUSY, 1), recWith(DELETED, 2), running(1), R_ABSENT, R_ABSENT, 1, true, Set())
      .then(startTo(true)).then(finish("busy", "unreachable"))
      .then(crash).then(restore)
      .expect(j.epoch == 1 and floor == 2)
      .then(start).then(verify).expect(daemon.safe and allInvariants)
  }

  // An unknown event kind (a newer build wrote the journal): safe mode, never left by a plan.
  run unknownKindIsSafeModeTest = {
    initWith(recWith(BUSY, 1), recWith(DELETED, 2), running(1), R_ABSENT, R_ABSENT, 1, false, Set())
      .then(startTo(true)).expect(daemon.safe)
      .then(proposePlan).then(approvePlan)
      .then(planIntentWith(1)).then(planOutcomeWith(1))
      .then(planIntentWith(2)).then(planOutcomeWith(2))
      .expect(plan.phase == "approved" and plan.done == ATTEMPTS and not(j.verifies))
  }

  // The world moved after the plan was drawn: the plan is refused, not applied (I31).
  run planRefusedWhenTheWorldMovesTest = {
    initWith(recWith(BUSY, 1), recWith(DELETED, 2), running(1), R_ABSENT, R_ABSENT, 2, true, Set())
      .then(startTo(true)).then(proposePlan).then(approvePlan)
      .then(worldChangeWith(1, "vanish", 1)).then(inventoryWith(true))
      .expect(not(bound))
      .then(refusePlan).expect(plan.phase == "refused" and allInvariants)
  }

  // A message recorded, then a crash before its ack: redelivered and acked from its record;
  // a message is never acked before it is recorded (I12).
  run recordBeforeAckTest = {
    initWith(recWith(BUSY, 1), recWith(DELETED, 2), running(1), R_ABSENT, R_ABSENT, 1, true, Set())
      .then(startTo(true)).then(finish("busy", "unreachable"))
      .then(ghDeliver(1)).expect(not(j.msgs.contains(1)))
      .then(ghRecord(1)).then(crash)
      .then(startTo(true)).then(finish("busy", "unreachable"))
      .then(ghAck(1))
      .expect(gh.acked == Set(1) and allInvariants)
  }

  // A safe session refuses the clone a stale daemon's frame asks for (I29).
  run safeSessionExecutesNothingTest = {
    initWith(recWith(CLONING, 1), recWith(DELETED, 2), R_ABSENT, R_ABSENT, R_ABSENT, 2, true, Set())
      .then(startTo(true))
      .expect(daemon.safe and dp.safe and daemon.phase != OPERATING)
  }
}
