// -*- mode: Bluespec; -*-
/**
 * The lifecycle model (KER-04): one `Attempt` stream (doc/domain-model.md §6.4, plan §6) with
 * the saga that issues its directives, the data plane that executes them and the inventory that
 * observes the provider. The Rust aggregate is `mayfly_control_domain::attempt`; the
 * conformance driver (VER-01) maps each action below onto one of its stimuli.
 *
 * The world (`w`) is what the provider really holds; the attempt (`a`) is what the journal
 * records, and it changes only on a command or an observation of the world. Each action is one
 * stimulus: `saga` (IssueNext), `command` (BeginActivation, RecordJitHandle, BindJob,
 * RequestRetirement, ConfirmRetirement, WithdrawRetirement, Quarantine, Adopt,
 * ResolveQuarantine, Close), `observation` (RecordObservation) or `world` (the provider changes
 * on its own; the journal does not move). A data-plane reply is merged with the execution of
 * its effect, so a lost reply is two actions: the effect happened or not, and the outcome is
 * Unknown either way.
 *
 * Ghost state (`g`) records history for the invariants and never feeds a guard.
 */
module lifecycle {
  // ---- constants ----------------------------------------------------------------------------

  /// Directives a stage may issue before a stage failure retires the attempt.
  pure val BUDGET = 2
  /// VM names and slot generations the reservation may hand out (fixed for the attempt).
  pure val NAMES = Set(1, 2)
  pure val GENERATIONS = Set(1, 2)
  /// Instance ids the provider assigns to a clone (the random MAC of ADR 0026).
  pure val INSTANCES = Set(1, 2)
  /// What a guest may claim about itself: a superset of what the provider reports (I30).
  pure val CLAIMS = Set(1, 2, 3)
  /// GitHub job request ids.
  pure val JOBS = Set(1, 2)
  pure val NONE = 0

  // Phases (`vm_state` of dm §6.4, plus the stream's end).
  pure val RESERVED = "Reserved"
  pure val CLONING = "Cloning"
  pure val BOOTING = "Booting"
  pure val VERIFYING = "Verifying"
  pure val PREPARED = "Prepared"
  pure val ACTIVATING = "Activating"
  pure val LISTENING = "Listening"
  pure val BUSY = "Busy"
  pure val FINISHED = "Finished"
  pure val RETIRING = "Retiring"
  pure val DELETING = "Deleting"
  pure val DELETED = "Deleted"
  pure val LOST = "Lost"
  pure val BACKOFF = "Backoff"
  pure val QUARANTINED = "Quarantined"
  pure val CLOSED = "Closed"

  // Directive kinds; "none" when nothing is pending.
  pure val NO_DIRECTIVE = "none"
  pure val CLONE_VM = "CloneVm"
  pure val START_VM = "StartVm"
  pure val VERIFY_GUEST = "VerifyGuest"
  pure val LAUNCH_RUNNER = "LaunchRunner"
  pure val EXPORT_DIAGNOSTICS = "ExportDiagnostics"
  pure val STOP_VM = "StopVm"
  pure val DELETE_VM = "DeleteVm"

  // ---- state --------------------------------------------------------------------------------

  type Attempt = {
    phase: str,
    // the stage a Backoff returns to: "clone" | "boot" | "verify" | "activate"
    stage: str,
    // directives issued in the current stage
    tries: int,
    // the pending overlay: kind and whether its outcome is Unknown
    pending: str,
    unknown: bool,
    name: int,
    generation: int,
    // recorded from VmCloned; NONE before
    instance: int,
    vmRunning: bool,
    epoch: int,
    granted: bool,
    // the epoch of the last GuestVerified while Activating; NONE when none
    freshEpoch: int,
    // "none" | "issued" | "taken" | "orphaned"
    jit: str,
    // the JIT config is presumed leaked (a launch of unknown outcome, or a handle orphaned by a
    // restart): the runner, if one starts, never gets a job
    jitLeaked: bool,
    job: int,
    retireRequested: bool,
    // "none" | "exported" | "lost"
    diagnostics: str,
    runnerExited: bool,
    // quarantined because a stop or start found its recorded VM absent on a complete listing:
    // we know it was there and did not remove it, a desync kept for review (ADR 0066)
    vanished: bool,
  }

  /// The provider's truth, and whether the pending directive has taken effect yet.
  type World = { exists: bool, instance: int, running: bool, applied: bool }

  type Ghost = {
    // what the last action was: "init" | "saga" | "command" | "observation" | "world"
    lastKind: str,
    // whether the pending outcome was Unknown before the last action
    prevUnknown: bool,
    // (name, generation) of every CloneVm issued
    cloneTargets: Set[(int, int)],
    // CloneVm directives issued
    clones: int,
    // a directive issued while another was pending
    doubleIssue: bool,
    launches: int,
    jobsBound: Set[int],
    // a JIT handle minted without a grant or a fresh verification of the current epoch
    illegalMint: bool,
    // a launch outcome Unknown or an orphaned handle: the JIT is presumed leaked
    leaked: bool,
    // reached Listening, Busy or Finished
    served: bool,
    // AttemptRetired recorded, or Finished reached
    done: bool,
    // a job bound, or an activation begun, after `done`
    servedAfterDone: bool,
    quarantined: bool,
    operatorResolved: bool,
    // a DeleteVm issued while quarantined and not resolved by an operator
    autoDelete: bool,
    // instance ids the provider reported
    providerInstances: Set[int],
  }

  var a: Attempt
  var w: World
  var g: Ghost

  // ---- helpers ------------------------------------------------------------------------------

  pure def idle(x: Attempt): bool = x.pending == NO_DIRECTIVE

  pure def terminal(x: Attempt): bool = Set(DELETED, LOST, CLOSED).contains(x.phase)

  pure def stagePhase(stage: str): str =
    if (stage == "clone") CLONING
    else if (stage == "boot") BOOTING
    else if (stage == "verify") VERIFYING
    else ACTIVATING

  pure def stageDirective(stage: str): str =
    if (stage == "clone") CLONE_VM
    else if (stage == "boot") START_VM
    else VERIFY_GUEST

  /// The stage the pending stage directive belongs to.
  pure def pendingStage(x: Attempt): str =
    if (x.pending == CLONE_VM) "clone"
    else if (x.pending == START_VM) "boot"
    else if (x.phase == ACTIVATING) "activate"
    else "verify"

  pure def jobLive(x: Attempt): bool = x.job != NONE and not(x.runnerExited)

  /// The directive IssueNext issues in this state, or NO_DIRECTIVE.
  pure def nextDirective(x: Attempt): str =
    if (x.phase == RESERVED or x.phase == CLONING) CLONE_VM
    else if (x.phase == BOOTING) START_VM
    else if (x.phase == VERIFYING) VERIFY_GUEST
    else if (x.phase == BACKOFF) stageDirective(x.stage)
    else if (x.phase == ACTIVATING)
      if (x.freshEpoch != x.epoch) VERIFY_GUEST
      else if (x.jit == "issued") LAUNCH_RUNNER
      else NO_DIRECTIVE
    else if (x.phase == RETIRING)
      if (x.vmRunning and x.diagnostics == "none") EXPORT_DIAGNOSTICS
      else if (x.vmRunning) STOP_VM
      else if (x.instance != NONE) DELETE_VM
      else NO_DIRECTIVE
    else if (x.phase == DELETING) DELETE_VM
    else if (x.phase == QUARANTINED)
      // stopped with proof, never deleted; a VM without a recorded instance id is not touched
      if (x.vmRunning and not(jobLive(x)) and x.instance != NONE) STOP_VM else NO_DIRECTIVE
    else NO_DIRECTIVE

  pure def isStageDirective(d: str): bool = Set(CLONE_VM, START_VM, VERIFY_GUEST).contains(d)

  pure def isCleanupDirective(d: str): bool = Set(STOP_VM, DELETE_VM).contains(d)

  /// Directives counted against the retry budget: the stage directives, stops and deletes.
  pure def isCounted(d: str): bool = isStageDirective(d) or isCleanupDirective(d)

  /// Retirement decided: cleanup begins, or ends at once when no VM was ever cloned.
  pure def retire(x: Attempt): Attempt =
    { ...x, phase: if (x.instance == NONE) DELETED else RETIRING, retireRequested: false,
      pending: NO_DIRECTIVE, unknown: false, tries: 0 }

  /// Quarantine; entering it restarts the count of stops, staying in it does not.
  pure def quarantine(x: Attempt): Attempt =
    { ...x, phase: QUARANTINED, pending: NO_DIRECTIVE, unknown: false, retireRequested: false,
      tries: if (x.phase == QUARANTINED) x.tries else 0 }

  /// Quarantine because the recorded VM vanished (`vm_vanished_externally`, ADR 0066): it is
  /// not running, so no stop is issued for it, and its absence is the reason, not a release.
  pure def quarantineVanished(x: Attempt): Attempt =
    { ...quarantine(x), vmRunning: false, vanished: true }

  /// A stage directive failed definitely (refused, or observed not applied).
  pure def stageFailed(x: Attempt): Attempt =
    if (x.tries < BUDGET)
      { ...x, phase: BACKOFF, stage: pendingStage(x), pending: NO_DIRECTIVE, unknown: false }
    else retire(x)

  /// The pending directive did not take effect.
  pure def notApplied(x: Attempt): Attempt =
    if (isStageDirective(x.pending)) stageFailed(x)
    else if (x.pending == LAUNCH_RUNNER) retire(x)
    else if (x.pending == EXPORT_DIAGNOSTICS)
      { ...x, diagnostics: "lost", pending: NO_DIRECTIVE, unknown: false }
    // a stop or delete is retried within the budget; spent, cleanup ends in quarantine
    // (`delete_refused_repeatedly`), and inside quarantine the stops simply stop
    else if (x.tries < BUDGET or x.phase == QUARANTINED)
      { ...x, pending: NO_DIRECTIVE, unknown: false }
    else quarantine(x)

  pure def servedPhase(phase: str): bool = Set(LISTENING, BUSY, FINISHED).contains(phase)

  /// Records the history every action shares.
  pure def ghostAfter(g0: Ghost, kind: str, before: Attempt, after: Attempt): Ghost = {
    ...g0,
    lastKind: kind,
    prevUnknown: before.unknown,
    served: g0.served or servedPhase(after.phase),
    done: g0.done or after.phase == RETIRING or after.phase == FINISHED
      or (before.phase != DELETED and after.phase == DELETED),
    quarantined: g0.quarantined or after.phase == QUARANTINED,
  }

  action commit(kind: str, after: Attempt, ghost: Ghost): bool = all {
    a' = after,
    w' = w,
    g' = ghostAfter(ghost, kind, a, after),
  }

  action commitWorld(kind: str, after: Attempt, world: World, ghost: Ghost): bool = all {
    a' = after,
    w' = world,
    g' = ghostAfter(ghost, kind, a, after),
  }

  // ---- guards (shared by the actions and the scenarios) -------------------------------------

  pure def canIssueNext(x: Attempt): bool = {
    val d = nextDirective(x)
    and {
      idle(x),
      d != NO_DIRECTIVE,
      not(isCleanupDirective(d)) or x.tries < BUDGET,
    }
  }

  pure def canBeginActivation(x: Attempt): bool = x.phase == PREPARED and idle(x)

  pure def canRecordJitHandle(x: Attempt): bool = and {
    x.phase == ACTIVATING,
    idle(x),
    x.granted,
    x.freshEpoch == x.epoch,
    x.jit == "none",
  }

  pure def canBindJob(x: Attempt): bool = and {
    x.phase == LISTENING,
    x.job == NONE,
    idle(x),
    not(x.jitLeaked),
  }

  pure def canRequestRetirement(x: Attempt): bool = and {
    idle(x),
    Set(RESERVED, BOOTING, VERIFYING, BACKOFF, PREPARED, ACTIVATING, LISTENING, FINISHED)
      .contains(x.phase),
    not(x.retireRequested),
  }

  pure def canQuarantine(x: Attempt): bool = idle(x) and not(terminal(x)) and x.phase != QUARANTINED

  // ---- init ---------------------------------------------------------------------------------

  /// `StartAttempt` from `SlotReserved`: the name and generation are fixed here, for good.
  action initWith(name: int, generation: int): bool = {
    all {
      a' = {
        phase: RESERVED, stage: "clone", tries: 0, pending: NO_DIRECTIVE, unknown: false,
        name: name, generation: generation, instance: NONE, vmRunning: false, epoch: 1,
        granted: false, freshEpoch: NONE, jit: "none", jitLeaked: false, job: NONE,
        retireRequested: false,
        diagnostics: "none", runnerExited: false, vanished: false,
      },
      w' = { exists: false, instance: NONE, running: false, applied: false },
      g' = {
        lastKind: "init", prevUnknown: false, cloneTargets: Set(), clones: 0,
        doubleIssue: false, launches: 0, jobsBound: Set(), illegalMint: false, leaked: false,
        served: false, done: false, servedAfterDone: false, quarantined: false,
        operatorResolved: false, autoDelete: false, providerInstances: Set(),
      },
    }
  }

  action init = {
    nondet name = NAMES.oneOf()
    nondet generation = GENERATIONS.oneOf()
    initWith(name, generation)
  }

  // ---- saga ---------------------------------------------------------------------------------

  /// `IssueNext`: `DirectiveIssued` for the stage's directive, one at a time per target.
  action issueNext = {
    val d = nextDirective(a)
    all {
      canIssueNext(a),
      commitWorld("saga",
        {
          ...a,
          pending: d,
          unknown: false,
          phase:
            if (a.phase == RESERVED) CLONING
            else if (a.phase == BACKOFF) stagePhase(a.stage)
            else if (d == DELETE_VM) DELETING
            else a.phase,
          tries: if (isCounted(d)) a.tries + 1 else a.tries,
        },
        { ...w, applied: false },
        {
          ...g,
          doubleIssue: g.doubleIssue or not(idle(a)),
          cloneTargets:
            if (d == CLONE_VM) g.cloneTargets.union(Set((a.name, a.generation)))
            else g.cloneTargets,
          launches: if (d == LAUNCH_RUNNER) g.launches + 1 else g.launches,
          clones: if (d == CLONE_VM) g.clones + 1 else g.clones,
          autoDelete: g.autoDelete
            or (d == DELETE_VM and g.quarantined and not(g.operatorResolved)),
        }),
    }
  }

  // ---- the data plane executes and replies ------------------------------------------------

  /// The world after the pending directive took effect, and whether it created a VM.
  pure def applyEffect(x: Attempt, world: World, fresh: int): World =
    if (x.pending == CLONE_VM)
      // clones are idempotent by name: an existing VM of this name is kept
      if (world.applied or world.exists) { ...world, applied: true }
      else { exists: true, instance: fresh, running: false, applied: true }
    else if (world.applied) world
    else if (x.pending == START_VM) { ...world, running: world.exists, applied: true }
    else if (x.pending == STOP_VM) { ...world, running: false, applied: true }
    else if (x.pending == DELETE_VM) { exists: false, instance: NONE, running: false, applied: true }
    else { ...world, applied: true }

  /// The attempt after the success reply of its pending directive (`world` already applied).
  pure def succeeded(x: Attempt, world: World): Attempt = {
    val cleared = { ...x, pending: NO_DIRECTIVE, unknown: false }
    if (x.pending == CLONE_VM)
      { ...cleared, phase: BOOTING, instance: world.instance, tries: 0 }
    else if (x.pending == START_VM)
      { ...cleared, phase: VERIFYING, vmRunning: true, tries: 0 }
    else if (x.pending == VERIFY_GUEST)
      if (x.phase == ACTIVATING) { ...cleared, freshEpoch: x.epoch, tries: 0 }
      else { ...cleared, phase: PREPARED, tries: 0 }
    // a launch answered after a restart orphaned its handle keeps the handle orphaned
    else if (x.pending == LAUNCH_RUNNER)
      { ...cleared, phase: LISTENING, jit: if (x.jit == "issued") "taken" else x.jit }
    else if (x.pending == EXPORT_DIAGNOSTICS) { ...cleared, diagnostics: "exported" }
    else if (x.pending == STOP_VM) { ...cleared, vmRunning: false, tries: 0 }
    else { ...cleared, phase: DELETED, vmRunning: false }
  }

  /// The effect happened and the reply says so (VmCloned, VmBooted, GuestVerified, ...).
  /// A verification reply carries the guest's own claims, which never set an ownership field.
  action replySucceededWith(fresh: int, claim: int): bool = {
    val world = applyEffect(a, w, fresh)
    all {
      not(idle(a)),
      // a stop or delete whose target is replaced is refused by the proof check; a delete
      // whose target a complete listing shows absent is done, whoever removed it (ADR 0053)
      not(Set(STOP_VM, DELETE_VM).contains(a.pending)) or (w.exists and w.instance == a.instance)
        or (a.pending == DELETE_VM and not(w.exists)),
      // the guest answers only a running VM
      not(Set(VERIFY_GUEST, LAUNCH_RUNNER, EXPORT_DIAGNOSTICS).contains(a.pending)) or w.running,
      a.pending != START_VM or w.exists,
      // the guest's claim is read by no field of the attempt
      CLAIMS.contains(claim),
      commitWorld("observation", succeeded(a, world), world,
        {
          ...g,
          providerInstances:
            if (a.pending == CLONE_VM) g.providerInstances.union(Set(world.instance))
            else g.providerInstances,
        }),
    }
  }

  action replySucceeded = {
    nondet fresh = INSTANCES.oneOf()
    nondet claim = CLAIMS.oneOf()
    replySucceededWith(fresh, claim)
  }

  /// `DirectiveRefused`: definitely nothing happened.
  action replyRefused = all {
    not(idle(a)),
    not(w.applied),
    commit("observation", notApplied(a), g),
  }

  /// `DirectiveRefused{proof_mismatch}` for a stop, a delete, a launch or a start whose
  /// target's instance id differs from the recorded one: quarantine (ADR 0026, ADR 0038); inside
  /// quarantine it is one more refused stop. A start is refused so when its exact name is listed
  /// but unproven, or, by a data plane older than protocol 1.2, when its VM is absent (ADR 0066);
  /// the model's world has neither, so for a start this reply over-approximates the data plane.
  action replyProofRefused = all {
    Set(STOP_VM, DELETE_VM, LAUNCH_RUNNER, START_VM).contains(a.pending),
    not(w.exists and w.instance == a.instance),
    w.exists,
    commit("observation", if (a.phase == QUARANTINED) notApplied(a) else quarantine(a), g),
  }

  /// `DirectiveRefused{target_absent}` for a stop or a start whose recorded VM a complete
  /// listing shows absent: we know it was there and did not remove it, so the journal and the
  /// provider have desynced, and the attempt is quarantined for review
  /// (`vm_vanished_externally`, ADR 0066), never read as stopped or retried. A delete of an
  /// absent VM is done instead (`replySucceededWith`, ADR 0053). Inside quarantine it is one
  /// more refused stop.
  action replyTargetAbsent = all {
    Set(STOP_VM, START_VM).contains(a.pending),
    a.instance != NONE,
    not(w.exists),
    commit("observation",
      if (a.phase == QUARANTINED) notApplied(a) else quarantineVanished(a), g),
  }

  /// `GuestRejected`: the guest failed its identity challenge (U3).
  action replyGuestRejected = all {
    a.pending == VERIFY_GUEST,
    w.running,
    commit("observation", quarantine(a), g),
  }

  /// The reply was lost (deadline): `DirectiveOutcomeUnknown`; the effect happened or not.
  action replyLostWith(fresh: int, applied: bool): bool = {
    val world = if (applied) applyEffect(a, w, fresh) else w
    all {
      not(idle(a)),
      not(a.unknown),
      not(Set(STOP_VM, DELETE_VM).contains(a.pending)) or (w.exists and w.instance == a.instance)
        or not(applied),
      commitWorld("observation", { ...a, unknown: true }, world, g),
    }
  }

  action replyLost = {
    nondet fresh = INSTANCES.oneOf()
    nondet applied = Set(true, false).oneOf()
    replyLostWith(fresh, applied)
  }

  /// `RunnerLaunchUnknown`: the launcher may have taken the JIT config; it is presumed leaked
  /// and never launched again (I22).
  action replyLaunchUnknown = all {
    a.pending == LAUNCH_RUNNER,
    commit("observation",
      { ...a, pending: NO_DIRECTIVE, unknown: false,
        jit: if (a.jit == "issued") "taken" else a.jit, jitLeaked: true },
      { ...g, leaked: true }),
  }

  // ---- inventory and other observations ---------------------------------------------------

  /// A complete inventory lists the VM (name, instance id, running).
  action inventoryPresent = {
    val x = a
    val result =
      if (x.instance != NONE and w.instance != x.instance) quarantine(x)
      else if (x.unknown and x.pending == CLONE_VM)
        { ...x, phase: BOOTING, instance: w.instance, pending: NO_DIRECTIVE, unknown: false,
          tries: 0 }
      else if (x.unknown and x.pending == START_VM)
        if (w.running)
          { ...x, phase: VERIFYING, vmRunning: true, pending: NO_DIRECTIVE, unknown: false,
            tries: 0 }
        else notApplied(x)
      else if (x.unknown and x.pending == STOP_VM)
        if (w.running) notApplied(x)
        else { ...x, vmRunning: false, pending: NO_DIRECTIVE, unknown: false }
      else if (x.unknown and x.pending == DELETE_VM) notApplied(x)
      else if (x.instance == NONE and idle(x) and not(terminal(x)) and x.phase != QUARANTINED)
        // a VM of our exact name that we never recorded cloning
        quarantine(x)
      else x
    all {
      w.exists,
      not(terminal(a)),
      commit("observation", result,
        { ...g, providerInstances:
            if (a.unknown and a.pending == CLONE_VM) g.providerInstances.union(Set(w.instance))
            else g.providerInstances }),
    }
  }

  /// A complete inventory does not list the VM.
  action inventoryAbsent = {
    val x = a
    val result =
      if (x.pending == DELETE_VM) { ...x, phase: DELETED, vmRunning: false, pending: NO_DIRECTIVE,
                                    unknown: false }
      else if (x.pending == CLONE_VM) (if (x.unknown) notApplied(x) else x)
      // a quarantined VM of our name that we never recorded is released when it is gone
      else if (x.instance == NONE and x.phase != QUARANTINED) x
      // a VM quarantined because it vanished stays quarantined for review: its absence is
      // the reason, not a release (ADR 0066)
      else if (x.phase == QUARANTINED and x.vanished) x
      else { ...x, phase: LOST, vmRunning: false, pending: NO_DIRECTIVE, unknown: false }
    all {
      not(w.exists),
      not(terminal(a)),
      commit("observation", result, g),
    }
  }

  /// The provider reports the VM stopped, not by our directive (`VmObservedStopped`): a
  /// serving attempt is retired; with a directive pending, the fact is only recorded.
  action vmObservedStopped = {
    val x = { ...a, vmRunning: false }
    val result =
      if (idle(a) and Set(VERIFYING, PREPARED, ACTIVATING, LISTENING, BUSY).contains(a.phase))
        retire(x)
      else x
    all {
      w.running,
      not(terminal(a)),
      commitWorld("observation", result, { ...w, running: false }, g),
    }
  }

  /// `RunnerExited{code}`: the runner process ended.
  action runnerExited = all {
    Set(LISTENING, BUSY).contains(a.phase) or (a.phase == ACTIVATING and a.jit == "taken"),
    not(a.runnerExited),
    commit("observation", { ...a, phase: FINISHED, runnerExited: true }, g),
  }

  // ---- the world changes on its own ---------------------------------------------------------

  /// Someone deletes the VM behind our back, or replaces it with another of the same name.
  action vmReplacedOrRemovedWith(other: int, removed: bool): bool = {
    all {
      w.exists,
      commitWorld("world", a,
        if (removed) { ...w, exists: false, instance: NONE, running: false }
        else { ...w, instance: other },
        g),
    }
  }

  action vmReplacedOrRemoved = {
    nondet other = INSTANCES.oneOf()
    nondet removed = Set(true, false).oneOf()
    vmReplacedOrRemovedWith(other, removed)
  }

  // ---- commands -----------------------------------------------------------------------------

  /// `BeginActivation{grant_ref}` after the Host's grant (P9).
  action beginActivation = all {
    canBeginActivation(a),
    commit("command", { ...a, phase: ACTIVATING, granted: true, tries: 0 },
      { ...g, servedAfterDone: g.servedAfterDone or g.done }),
  }

  /// `RecordJitHandle`: needs the grant and a fresh verification in the current epoch (I22).
  action recordJitHandle = all {
    canRecordJitHandle(a),
    commit("command", { ...a, jit: "issued" },
      { ...g, illegalMint: g.illegalMint or not(a.granted) or a.freshEpoch != a.epoch }),
  }

  /// `BindJob{request_id}`: GitHub bound a job to the runner (Listening → Busy).
  action bindJobWith(job: int): bool = {
    all {
      canBindJob(a),
      commit("command", { ...a, phase: BUSY, job: job, retireRequested: false },
        { ...g, jobsBound: g.jobsBound.union(Set(job)),
          servedAfterDone: g.servedAfterDone or g.done }),
    }
  }

  action bindJob = {
    nondet job = JOBS.oneOf()
    bindJobWith(job)
  }

  /// `RequestRetirement{reason}`: a registered runner waits for its deregistration (I21).
  action requestRetirement = all {
    canRequestRetirement(a),
    commit("command",
      if (Set(ACTIVATING, LISTENING).contains(a.phase) and a.jit != "none")
        { ...a, retireRequested: true }
      else retire(a),
      g),
  }

  /// `ConfirmRetirement` after the registration was deregistered.
  action confirmRetirement = all {
    idle(a),
    a.retireRequested,
    commit("command", retire(a), g),
  }

  /// `WithdrawRetirement` after GitHub answered 422 (busy).
  action withdrawRetirement = all {
    idle(a),
    a.retireRequested,
    commit("command", { ...a, retireRequested: false }, g),
  }

  /// `Quarantine{reason}` by a policy (identity doubt after delivery, redaction, ...).
  action quarantineCommand = all {
    canQuarantine(a),
    commit("command", quarantine(a), g),
  }

  /// `Adopt{inventory_ref}` under a new run epoch (restart): the broker is empty by
  /// construction, so a handle issued and not taken is orphaned and presumed leaked, even with
  /// its launch in flight (the launcher may have taken it); a late launch reply is still
  /// recorded, but that runner never gets a job. A verification from the old epoch is no
  /// longer fresh. Adoption is allowed with a directive pending: it moves nothing.
  action adopt = all {
    not(terminal(a)),
    a.epoch < 3,
    commit("command",
      { ...a, epoch: a.epoch + 1, jit: if (a.jit == "issued") "orphaned" else a.jit,
        jitLeaked: a.jitLeaked or a.jit == "issued" },
      { ...g, leaked: g.leaked or a.jit == "issued" }),
  }

  /// `ResolveQuarantine` by an operator: delete with proof (the data plane re-checks it).
  action resolveQuarantine = all {
    a.phase == QUARANTINED,
    idle(a),
    a.instance != NONE,
    commit("command", retire(a), { ...g, operatorResolved: true }),
  }

  /// `Close` once the VM is deleted or lost.
  action close = all {
    Set(DELETED, LOST).contains(a.phase),
    commit("command", { ...a, phase: CLOSED }, g),
  }

  // ---- the relation -------------------------------------------------------------------------

  action step = any {
    issueNext,
    replySucceeded,
    replyRefused,
    replyProofRefused,
    replyTargetAbsent,
    replyGuestRejected,
    replyLost,
    replyLaunchUnknown,
    inventoryPresent,
    inventoryAbsent,
    vmObservedStopped,
    runnerExited,
    vmReplacedOrRemoved,
    beginActivation,
    recordJitHandle,
    bindJob,
    requestRetirement,
    confirmRetirement,
    withdrawRetirement,
    quarantineCommand,
    adopt,
    resolveQuarantine,
    close,
  }

  /// The serving path, biased towards a job: the stimuli that move an attempt through
  /// activation, binding and reclamation, with lost replies and their resolution by inventory,
  /// restarts and a dying guest, but no policy retirement before the job ends and no
  /// interference with the provider. Random simulation of `step` rarely strings fifteen
  /// stimuli of the happy path together; this relation reaches Busy, Finished and Closed.
  action serveStep = any {
    issueNext,
    replySucceeded,
    replyLost,
    replyLaunchUnknown,
    inventoryPresent,
    inventoryAbsent,
    vmObservedStopped,
    runnerExited,
    beginActivation,
    recordJitHandle,
    bindJob,
    all { Set(FINISHED, ACTIVATING).contains(a.phase), requestRetirement },
    confirmRetirement,
    adopt,
    close,
  }

  /// The whole preparation at once (clone, boot and verification all answered), from
  /// `Reserved`, optionally followed by the grant and the fresh verification of activation: a
  /// shortcut for `activationStep` only, so a bounded check spends its depth on activation,
  /// restarts, refusals and quarantine instead of on the preparation `step` and `serveStep`
  /// already cover. It records what those stimuli record.
  action prepareAtOnceWith(instance: int, activated: bool): bool = all {
    a.phase == RESERVED,
    idle(a),
    not(w.exists),
    commitWorld("observation",
      {
        ...a,
        phase: if (activated) ACTIVATING else PREPARED,
        instance: instance,
        vmRunning: true,
        tries: 0,
        granted: activated,
        freshEpoch: if (activated) a.epoch else NONE,
      },
      { exists: true, instance: instance, running: true, applied: true },
      {
        ...g,
        cloneTargets: g.cloneTargets.union(Set((a.name, a.generation))),
        clones: g.clones + 1,
        providerInstances: g.providerInstances.union(Set(instance)),
      }),
  }

  action prepareAtOnce = {
    nondet instance = INSTANCES.oneOf()
    nondet activated = Set(true, false).oneOf()
    prepareAtOnceWith(instance, activated)
  }

  /// Activation under stress: after a preparation (and possibly the grant and fresh
  /// verification) done at once, the JIT handle, the launch and the job meet restarts, lost
  /// replies, launch outcomes, refused proofs, a replaced VM, quarantine and retirement. This
  /// is where a restart with a launch in flight once let a presumed-leaked runner get a job
  /// (`adoptDuringLaunchNeverServesTest`); `step` needed more depth than Apalache reaches to
  /// find it.
  action activationStep = any {
    prepareAtOnce,
    issueNext,
    replySucceeded,
    replyRefused,
    replyProofRefused,
    replyTargetAbsent,
    replyLost,
    replyLaunchUnknown,
    inventoryPresent,
    vmReplacedOrRemoved,
    beginActivation,
    recordJitHandle,
    bindJob,
    requestRetirement,
    confirmRetirement,
    quarantineCommand,
    adopt,
  }

  // ---- invariants ---------------------------------------------------------------------------

  /// I06: a completed attempt never serves a new job, and a retired one never activates.
  val i06CompletedNeverServesAgain = not(g.servedAfterDone)

  /// busyNeverWarm: no path from Listening, Busy or Finished back to Prepared.
  val busyNeverWarm = g.served implies a.phase != PREPARED

  /// I10: `Unknown` leaves only on an observation: no command, no saga step, no timeout.
  val i10UnknownLeavesOnlyOnObservation =
    (g.prevUnknown and not(a.unknown)) implies g.lastKind == "observation"

  /// I13: at most one in-flight directive per target, every clone is of the one (name,
  /// generation), so the provider's names keep it to one VM, and the clones are bounded by the
  /// stage's retry budget.
  val i13NoDuplicateClones = and {
    not(g.doubleIssue),
    g.cloneTargets.size() <= 1,
    g.clones <= BUDGET,
  }

  /// I14: one VM serves at most one job.
  val i14OneJob = g.jobsBound.size() <= 1

  /// I22 (attempt half): a JIT handle only after the grant and a fresh verification in the
  /// current epoch; delivered at most once; once presumed leaked, no job is ever bound.
  val i22JitMintedOnceAfterGrantAndVerification = and {
    not(g.illegalMint),
    g.launches <= 1,
    g.leaked implies g.jobsBound.size() == 0,
  }

  /// I30: ownership fields come from the control plane and the provider, never from the guest.
  val i30GuestNeverDecidesOwnership =
    a.instance == NONE or g.providerInstances.contains(a.instance)

  /// quarantineTerminal: automation never takes an attempt out of quarantine; only an operator
  /// resolution or an observed absence does.
  val quarantineTerminal =
    (g.quarantined and not(g.operatorResolved)) implies Set(QUARANTINED, LOST, CLOSED).contains(a.phase)

  /// quarantineNeverAutoDeleted: no delete directive while quarantined and unresolved (I28).
  val quarantineNeverAutoDeleted = not(g.autoDelete)

  /// vanishedStaysQuarantined: an attempt quarantined because its recorded VM vanished stays
  /// quarantined until an operator resolves it; no inventory, saga step or retry takes it out
  /// (ADR 0066: a desync is reported, never absorbed).
  val vanishedStaysQuarantined =
    (a.vanished and not(g.operatorResolved)) implies a.phase == QUARANTINED

  /// retryKeepsNameAndGeneration: every clone, first or retried, targets the name and slot
  /// generation fixed at start.
  val retryKeepsNameAndGeneration =
    g.cloneTargets.forall(t => t == (a.name, a.generation))

  // Every invariant of the model, so a new one cannot be left out of a stage.
  val allInvariants = and {
    i06CompletedNeverServesAgain,
    busyNeverWarm,
    i10UnknownLeavesOnlyOnObservation,
    i13NoDuplicateClones,
    i14OneJob,
    i22JitMintedOnceAfterGrantAndVerification,
    i30GuestNeverDecidesOwnership,
    quarantineTerminal,
    quarantineNeverAutoDeleted,
    vanishedStaysQuarantined,
    retryKeepsNameAndGeneration,
  }

  // ---- scenarios ----------------------------------------------------------------------------

  /// A success reply with fixed choices (instance 1, a guest claiming 3).
  action ok = replySucceededWith(1, 3)

  /// Clone, boot and verify: the attempt is Prepared.
  action toPrepared = issueNext.then(ok).then(issueNext).then(ok)
    .then(issueNext).then(ok)

  /// From Prepared: grant, fresh verification, mint, launch.
  action toListening = beginActivation.then(issueNext).then(ok).then(recordJitHandle)
    .then(issueNext).then(ok)

  // The happy path of dm §2.1: prepared, activated, bound, finished, retired, deleted, closed.
  run happyPathTest = {
    initWith(1, 1).then(toPrepared)
      .expect(a.phase == PREPARED and a.vmRunning and a.instance != NONE)
      .then(toListening)
      .expect(a.phase == LISTENING and a.jit == "taken")
      .then(bindJobWith(1)).expect(a.phase == BUSY)
      .then(runnerExited).expect(a.phase == FINISHED)
      .then(requestRetirement).expect(a.phase == RETIRING)
      .then(issueNext).expect((a.pending == EXPORT_DIAGNOSTICS)).then(ok)
      .then(issueNext).expect((a.pending == STOP_VM)).then(ok)
      .then(issueNext).expect(a.phase == DELETING and a.pending == DELETE_VM)
      .then(ok).expect(a.phase == DELETED and not(w.exists))
      .then(close).expect(a.phase == CLOSED and allInvariants)
  }

  // U1: a lost clone reply leaves Unknown; no command clears it; the inventory does.
  run unknownCloneResolvedByInventoryTest = {
    initWith(1, 1).then(issueNext).then(replyLostWith(1, true))
      .expect(a.unknown and a.pending == CLONE_VM)
      .expect(not(canIssueNext(a)))
      .expect(not(canRequestRetirement(a)))
      .expect(not(canQuarantine(a)))
      .then(adopt).expect(a.unknown)
      .then(inventoryPresent)
      .expect((not(a.unknown) and a.phase == BOOTING and a.instance == 1
        and i10UnknownLeavesOnlyOnObservation))
  }

  // U1: a lost clone reply whose effect did not happen: the inventory's absence backs off.
  run unknownCloneNotAppliedBacksOffTest = {
    initWith(1, 1).then(issueNext).then(replyLostWith(1, false))
      .then(inventoryAbsent)
      .expect(not(a.unknown) and a.phase == BACKOFF and a.stage == "clone")
      .then(issueNext).expect(a.phase == CLONING and a.tries == 2 and retryKeepsNameAndGeneration)
  }

  // U2: a stage that keeps failing backs off with the same name and generation, then retires.
  run stageFailureBacksOffThenRetiresTest = {
    initWith(1, 1).then(issueNext).then(replyRefused)
      .expect(a.phase == BACKOFF and a.stage == "clone")
      .then(issueNext).expect(a.phase == CLONING and a.tries == 2)
      .then(replyRefused)
      .expect(a.phase == DELETED and g.clones == 2 and not(w.exists) and retryKeepsNameAndGeneration)
  }

  // U3: a guest that fails its challenge is quarantined; automation never deletes it.
  run guestRejectedIsQuarantinedTest = {
    initWith(1, 1).then(issueNext).then(ok).then(issueNext).then(ok)
      .then(issueNext).then(replyGuestRejected)
      .expect(a.phase == QUARANTINED)
      .then(issueNext).expect((a.pending == STOP_VM)).then(ok)
      .expect(not(canIssueNext(a)))
      .expect(not(canRequestRetirement(a)))
      .then(resolveQuarantine).expect(a.phase == RETIRING and g.operatorResolved)
      .then(issueNext).expect(a.pending == DELETE_VM and quarantineNeverAutoDeleted)
  }

  // I22: a JIT handle is refused before a fresh verification, and after a restart.
  run activationNeedsGrantAndFreshVerificationTest = {
    initWith(1, 1).then(toPrepared)
      .expect(not(canRecordJitHandle(a)))
      .then(beginActivation)
      .expect(not(canRecordJitHandle(a)))
      .then(issueNext).then(ok)
      .then(adopt)
      .expect(not(canRecordJitHandle(a)))
      .then(issueNext).expect((a.pending == VERIFY_GUEST)).then(ok)
      .then(recordJitHandle).expect(a.jit == "issued")
  }

  // U5: a launch outcome Unknown: never launched again, never bound to a job.
  run launchUnknownNeverServesTest = {
    initWith(1, 1).then(toPrepared).then(beginActivation).then(issueNext).then(ok)
      .then(recordJitHandle).then(issueNext).then(replyLaunchUnknown)
      .expect(a.phase == ACTIVATING and g.leaked)
      .expect(not(canIssueNext(a)))
      .expect(not(canBindJob(a)))
      .then(requestRetirement).expect(a.retireRequested)
      .then(confirmRetirement).expect(a.phase == RETIRING and g.launches == 1)
  }

  // I14 and busyNeverWarm: one job, and no way back to Prepared.
  run busyServesOneJobTest = {
    initWith(1, 1).then(toPrepared).then(toListening).then(bindJobWith(1))
      .expect(not(canBindJob(a)))
      .expect(not(canRequestRetirement(a)))
      .expect(not(canBeginActivation(a)))
      .expect(a.phase == BUSY and i14OneJob and busyNeverWarm)
  }

  // U6/I21: a registered runner waits for its deregistration; a 422 withdraws the retirement.
  run retirementWaitsForDeregistrationTest = {
    initWith(1, 1).then(toPrepared).then(toListening)
      .then(requestRetirement).expect(a.phase == LISTENING and a.retireRequested)
      .expect(not(canIssueNext(a)))
      .then(withdrawRetirement).expect(not(a.retireRequested))
      .then(requestRetirement).then(bindJobWith(1))
      .expect(a.phase == BUSY and not(a.retireRequested))
  }

  // ADR 0026: a delete refused because the instance id changed quarantines the attempt.
  run replacedVmQuarantinesOnDeleteTest = {
    initWith(1, 1).then(toPrepared).then(requestRetirement)
      .then(issueNext).then(ok).then(issueNext).then(ok)
      .then(issueNext).expect(a.pending == DELETE_VM)
      .then(vmReplacedOrRemovedWith(2, false))
      .then(replyProofRefused)
      .expect(a.phase == QUARANTINED and allInvariants)
  }

  // H1 of the KER-04 review: a restart while the launch is in flight orphans the handle; the
  // late launch reply is recorded, and that runner never gets a job.
  run adoptDuringLaunchNeverServesTest = {
    initWith(1, 1).then(toPrepared).then(beginActivation).then(issueNext).then(ok)
      .then(recordJitHandle).then(issueNext).expect(a.pending == LAUNCH_RUNNER)
      .then(adopt).expect(a.jit == "orphaned" and a.jitLeaked and a.pending == LAUNCH_RUNNER)
      .then(ok).expect(a.phase == LISTENING and a.jit == "orphaned")
      .expect(not(canBindJob(a)))
      .then(requestRetirement).then(confirmRetirement)
      .expect(a.phase == RETIRING and g.jobsBound.size() == 0 and allInvariants)
  }

  // A delete the data plane keeps refusing ends in quarantine, within the budget.
  run deleteRefusedRepeatedlyQuarantinesTest = {
    initWith(1, 1).then(toPrepared).then(requestRetirement)
      .then(issueNext).then(ok).then(issueNext).then(ok)
      .then(issueNext).expect(a.pending == DELETE_VM and a.tries == 1)
      .then(replyRefused).expect(a.phase == DELETING and a.pending == NO_DIRECTIVE)
      .then(issueNext).expect(a.tries == 2)
      .then(replyRefused).expect(a.phase == QUARANTINED)
      .expect(not(canIssueNext(a)) and quarantineNeverAutoDeleted)
  }

  // Inside quarantine, refused stops (a proof refused included) are bounded too.
  run quarantinedStopsAreBoundedTest = {
    initWith(1, 1).then(issueNext).then(ok).then(issueNext).then(ok)
      .then(issueNext).then(replyGuestRejected).expect(a.phase == QUARANTINED and a.tries == 0)
      .then(issueNext).then(replyRefused)
      .then(vmReplacedOrRemovedWith(2, false))
      .then(issueNext).then(replyProofRefused)
      .expect(a.phase == QUARANTINED and a.tries == 2 and not(canIssueNext(a)))
  }

  // ADR 0053: a delete whose target is already gone is answered deleted, never refused.
  run deleteOfAbsentVmIsDoneTest = {
    initWith(1, 1).then(toPrepared).then(requestRetirement)
      .then(issueNext).then(ok).then(issueNext).then(ok)
      .then(issueNext).expect(a.pending == DELETE_VM)
      .then(vmReplacedOrRemovedWith(1, true))
      .then(ok).expect(a.phase == DELETED and not(w.exists) and allInvariants)
  }

  // ADR 0053: a delete whose reply was lost leaves Unknown; the answer to its re-delivery,
  // the target absent on a complete listing, is the observation that resolves it (I10).
  run deleteLostThenAnsweredDeletedTest = {
    initWith(1, 1).then(toPrepared).then(requestRetirement)
      .then(issueNext).then(ok).then(issueNext).then(ok)
      .then(issueNext).then(replyLostWith(1, true))
      .expect(a.unknown and a.pending == DELETE_VM and not(w.exists))
      .then(ok)
      .expect(a.phase == DELETED and not(a.unknown) and i10UnknownLeavesOnlyOnObservation
        and allInvariants)
  }

  // ADR 0066: a stop whose recorded VM vanished is refused target_absent and quarantines the
  // attempt for review; the inventory's absence does not release it, and only an operator's
  // resolution ends it (with a delete that ADR 0053 answers done).
  run stopOfVanishedVmQuarantinesTest = {
    initWith(1, 1).then(toPrepared).then(requestRetirement)
      .then(issueNext).then(ok).then(issueNext).expect(a.pending == STOP_VM)
      .then(vmReplacedOrRemovedWith(1, true))
      .then(replyTargetAbsent)
      .expect(a.phase == QUARANTINED and a.vanished and not(a.vmRunning))
      .expect(not(canIssueNext(a)))
      .then(inventoryAbsent).expect(a.phase == QUARANTINED and vanishedStaysQuarantined)
      .then(resolveQuarantine).expect(a.phase == RETIRING)
      .then(issueNext).expect(a.pending == DELETE_VM)
      .then(ok).expect(a.phase == DELETED and allInvariants)
  }

  // ADR 0066: a start whose cloned VM vanished is quarantined too, never a stage failure.
  run startOfVanishedVmQuarantinesTest = {
    initWith(1, 1).then(issueNext).then(ok).expect(a.phase == BOOTING)
      .then(vmReplacedOrRemovedWith(1, true))
      .then(issueNext).expect(a.pending == START_VM)
      .then(replyTargetAbsent)
      .expect(a.phase == QUARANTINED and a.vanished and a.tries == 0 and allInvariants)
  }

  // The activation relation's shortcut lands where the preparation does.
  run prepareAtOnceTest = {
    initWith(1, 1).then(prepareAtOnceWith(1, false))
      .expect(a.phase == PREPARED and a.vmRunning and w.running and allInvariants)
      .then(beginActivation).then(issueNext).expect(a.pending == VERIFY_GUEST)
  }
}
