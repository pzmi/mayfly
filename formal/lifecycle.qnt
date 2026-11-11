// -*- mode: Bluespec; -*-
/**
 * The lifecycle model (KER-04): one `Attempt` stream (doc/domain-model.md §6.4, plan §6) with
 * the saga that issues its directives, the data plane that executes them and the inventory that
 * observes the provider. The Rust aggregate is `mayfly_control_domain::attempt`; the
 * conformance driver (VER-01) maps each action below onto one of its stimuli.
 *
 * The world (`w`) is what the provider really holds; the attempt (`a`) is what the journal
 * records, and it changes only on a command or an observation of the world. Each action is one
 * stimulus: `saga` (IssueNext), `relay` (the directive sent, `DirectiveSent`), `timeout` (the
 * saga's RecordDirectiveTimeout), `command` (BeginActivation,
 * RecordJitHandle, BindJob, RequestRetirement, ConfirmRetirement, WithdrawRetirement,
 * Quarantine, Adopt, ResolveQuarantine, Close), `observation` (RecordObservation) or `world` (the provider changes
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
    // the relay put the pending directive on the link (`DirectiveSent`, CP-02): only then does
    // its deadline run, so a directive held unsent never times out
    sent: bool,
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
    // the VM the attempt recorded cloning was found absent from a complete listing, by a stop's
    // or start's re-observation or by the inventory, and mayfly had not deleted it: a desync,
    // recorded (`VmVanished`) and surfaced, and the attempt closed for good; the reconciler
    // provisions a fresh one for the remaining demand (ADR 0066 as amended 2026-11-11, DEC-07)
    vanished: bool,
    // the quarantine stop's deregistration (ADR 0076): "none" (nothing to wait for), "requested"
    // (a runner of the attempt's registration may be registered and idle in the running VM, so
    // P11 deregisters it before the stop), "confirmed" (P11 had the answer: removed or not
    // found) or "busy" (GitHub answered busy: a job runs there, and the VM is not stopped)
    dereg: str,
  }

  /// The provider's truth, and whether the pending directive has taken effect yet.
  type World = { exists: bool, instance: int, running: bool, applied: bool }

  type Ghost = {
    // what the last action was: "init" | "saga" | "relay" | "timeout" | "command" |
    // "observation" | "world"
    lastKind: str,
    // whether the pending outcome was Unknown before the last action
    prevUnknown: bool,
    // the phase, and whether the pending directive had been sent, before the last action
    prevPhase: str,
    prevSent: bool,
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
    // a LaunchRunner was issued and not refused: a runner of the attempt's registration may be
    // registered in the VM (its config may have reached the launcher)
    runnerMayLive: bool,
    // a retirement was confirmed: P11 had the deregistration's answer (`ConfirmRetirement`)
    deregistered: bool,
    // retired outside quarantine with the VM running (so its stop follows) while such a runner
    // may be registered and idle in it, before the deregistration answered (I21)
    stopBeforeDeregister: bool,
    // a vanished VM was recorded (`VmVanished`): never cleared
    vanishedEver: bool,
    // a directive was issued, or the phase left Lost or Closed for another, after a vanish
    actedAfterVanish: bool,
    // an incomplete listing changed the phase or recorded a vanish: an absence concluded from
    // it (I25)
    concludedFromIncomplete: bool,
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

  /// Whether the automatic stop of a quarantined VM would leave a runner of the attempt's
  /// registration registered and idle on GitHub (ADR 0076): a JIT was issued, the runner was
  /// not seen exiting, and no retirement passed the deregistration before (a retiring attempt
  /// had P11's answer, or had nothing to wait for); only for a VM the stop would be issued for.
  pure def stopNeedsDeregistration(x: Attempt): bool = and {
    x.jit != "none",
    not(x.runnerExited),
    not(Set(RETIRING, DELETING).contains(x.phase)),
    x.vmRunning,
    x.instance != NONE,
    not(jobLive(x)),
  }

  /// The quarantine stop is not waiting on the deregistration: none was needed, P11 confirmed
  /// it, or the runner has exited since (an exited runner takes no job, I21).
  pure def stopCleared(x: Attempt): bool =
    Set("none", "confirmed").contains(x.dereg) or x.runnerExited

  /// The directive IssueNext issues in this state, or NO_DIRECTIVE.
  pure def nextDirective(x: Attempt): str =
    if (x.phase == RESERVED or x.phase == CLONING) CLONE_VM
    else if (x.phase == BOOTING) START_VM
    else if (x.phase == VERIFYING) VERIFY_GUEST
    // a retirement waiting for its deregistration does not retry the stage (KER-04G)
    else if (x.phase == BACKOFF) (if (x.retireRequested) NO_DIRECTIVE else stageDirective(x.stage))
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
      // stopped with proof, never deleted; a VM without a recorded instance id is not touched;
      // a runner that may be registered in it is deregistered first (ADR 0076)
      if (x.vmRunning and not(jobLive(x)) and x.instance != NONE and stopCleared(x)) STOP_VM
      else NO_DIRECTIVE
    else NO_DIRECTIVE

  pure def isStageDirective(d: str): bool = Set(CLONE_VM, START_VM, VERIFY_GUEST).contains(d)

  pure def isCleanupDirective(d: str): bool = Set(STOP_VM, DELETE_VM).contains(d)

  /// Directives counted against the retry budget: the stage directives, stops and deletes.
  pure def isCounted(d: str): bool = isStageDirective(d) or isCleanupDirective(d)

  /// Retirement decided: cleanup begins, or ends at once when no VM was ever cloned.
  pure def retire(x: Attempt): Attempt =
    { ...x, phase: if (x.instance == NONE) DELETED else RETIRING, retireRequested: false,
      pending: NO_DIRECTIVE, unknown: false, tries: 0, dereg: "none" }

  /// Quarantine; entering it restarts the count of stops, staying in it does not. Entering it
  /// with a runner that may be registered in the running VM requests its deregistration, which
  /// the stop waits for (ADR 0076); staying in it keeps the request where it is.
  pure def quarantine(x: Attempt): Attempt =
    { ...x, phase: QUARANTINED, pending: NO_DIRECTIVE, unknown: false, retireRequested: false,
      tries: if (x.phase == QUARANTINED) x.tries else 0,
      dereg:
        if (x.phase == QUARANTINED) x.dereg
        else if (stopNeedsDeregistration(x)) "requested"
        else "none" }

  /// The recorded VM vanished (`VmVanished`, ADR 0066 as amended 2026-11-11, DEC-07): a VM the
  /// attempt recorded cloning, which mayfly did not delete, is absent from a complete listing.
  /// Like a controller restoring desired state, the attempt does not wait on it: the desync is
  /// recorded, the attempt is lost (its slot released, `Close` follows) and never resurrected;
  /// the reconciler provisions a fresh attempt for the demand that remains. Whatever was
  /// pending is moot, its target gone, and so is a quarantine stop's deregistration (ADR 0076):
  /// there is no VM left to stop. Quarantined or not, the same: quarantine holds only a VM that
  /// is present and does not match the record.
  pure def vanish(x: Attempt): Attempt =
    { ...x, phase: LOST, vmRunning: false, pending: NO_DIRECTIVE, unknown: false, vanished: true,
      dereg: "none" }

  /// A stage directive failed definitely (refused, or observed not applied). Once the budget is
  /// spent the attempt retires, but one holding a registration (a JIT was issued: the activate
  /// stage) requests its retirement and waits in Backoff for the deregistration, as a policy's
  /// request would (I21, KER-04G).
  pure def stageFailed(x: Attempt): Attempt = {
    val backoff =
      { ...x, phase: BACKOFF, stage: pendingStage(x), pending: NO_DIRECTIVE, unknown: false }
    if (x.tries < BUDGET) backoff
    else if (x.jit != "none") { ...backoff, retireRequested: true }
    else retire(x)
  }

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
    prevPhase: before.phase,
    prevSent: before.sent,
    served: g0.served or servedPhase(after.phase),
    done: g0.done or after.phase == RETIRING or after.phase == FINISHED
      or (before.phase != DELETED and after.phase == DELETED),
    quarantined: g0.quarantined or after.phase == QUARANTINED,
    vanishedEver: g0.vanishedEver or after.vanished,
    actedAfterVanish: g0.actedAfterVanish
      or (g0.vanishedEver and not(Set(LOST, CLOSED).contains(after.phase))),
    concludedFromIncomplete: g0.concludedFromIncomplete or (kind == "incomplete" and or {
      after.phase != before.phase,
      after.vanished != before.vanished,
    }),
    // retiring a running VM commits to its stop (diagnostics, then StopVm): that decision is
    // where deregister-before-stop is kept or lost, so it is checked there, at the bounded
    // depth Apalache reaches (KER-04G)
    stopBeforeDeregister: g0.stopBeforeDeregister or and {
      before.phase != RETIRING,
      after.phase == RETIRING,
      after.vmRunning,
      not(g0.quarantined),
      g0.runnerMayLive,
      not(before.runnerExited),
      not(g0.deregistered),
    } or and {
      // the automatic stop of a quarantined VM is issued while such a runner may be registered
      // and idle in it, before the deregistration answered (ADR 0076)
      before.pending == NO_DIRECTIVE,
      after.phase == QUARANTINED,
      after.pending == STOP_VM,
      g0.runnerMayLive,
      not(after.runnerExited),
      not(g0.deregistered),
      not(g0.operatorResolved),
    },
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
        sent: false,
        name: name, generation: generation, instance: NONE, vmRunning: false, epoch: 1,
        granted: false, freshEpoch: NONE, jit: "none", jitLeaked: false, job: NONE,
        retireRequested: false,
        diagnostics: "none", runnerExited: false, vanished: false, dereg: "none",
      },
      w' = { exists: false, instance: NONE, running: false, applied: false },
      g' = {
        lastKind: "init", prevUnknown: false, prevPhase: RESERVED, prevSent: false, cloneTargets: Set(), clones: 0,
        doubleIssue: false, launches: 0, jobsBound: Set(), illegalMint: false, leaked: false,
        served: false, done: false, servedAfterDone: false, quarantined: false,
        operatorResolved: false, autoDelete: false, providerInstances: Set(),
        runnerMayLive: false, deregistered: false, stopBeforeDeregister: false,
        vanishedEver: false, actedAfterVanish: false, concludedFromIncomplete: false,
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
          sent: false,
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
          actedAfterVanish: g.actedAfterVanish or g.vanishedEver,
          runnerMayLive: g.runnerMayLive or d == LAUNCH_RUNNER,
        }),
    }
  }

  // ---- the data plane executes and replies ------------------------------------------------

  /// The world after the pending directive took effect, and whether it created a VM.
  pure def applyEffect(x: Attempt, world: World, fresh: int): World =
    if (x.pending == CLONE_VM)
      // clones are idempotent by name: an existing VM of this name is kept, and a clone is
      // re-observed by name before its effect (plan §8.2, DP-01), so a delivery that finds no
      // VM, even one an earlier delivery cloned and someone then removed, clones afresh; a
      // `VmCloned` always carries the instance id of a listed VM (VER-01F)
      if (world.exists) { ...world, applied: true }
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
    commit("observation", notApplied(a),
      // a launch refused definitely never reached the launcher: no runner in the VM
      { ...g, runnerMayLive: g.runnerMayLive and a.pending != LAUNCH_RUNNER }),
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
  /// provider have desynced. The desync is recorded and the attempt closed (`vanish`, ADR 0066
  /// as amended 2026-11-11), never read as stopped, a stage failure or retried; inside
  /// quarantine too, since quarantine holds only a VM that is present. A delete of an absent VM
  /// is done instead (`replySucceededWith`, ADR 0053), and a re-observation on an incomplete
  /// listing concludes nothing (`listingIncomplete`).
  action replyTargetAbsent = all {
    Set(STOP_VM, START_VM).contains(a.pending),
    a.instance != NONE,
    not(w.exists),
    commit("observation", vanish(a), g),
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

  /// `DirectiveSent`: the relay put the pending directive on the link and recorded it (CP-02);
  /// from now on its deadline runs. A held directive (safe mode, no session, not adopted) is
  /// not sent, so it never reaches its timeout.
  action relaySend = all {
    not(idle(a)),
    not(a.sent),
    not(a.unknown),
    commit("relay", { ...a, sent: true }, g),
  }

  /// Whether the saga's timer may time the pending directive out: only once it was sent.
  pure def canTimeOut(x: Attempt): bool = and { not(idle(x)), x.sent, not(x.unknown) }

  /// The sent directive's deadline passed with no answer: the saga's timer issues
  /// `RecordDirectiveTimeout` (CP-02, ADR 0034), which records `DirectiveOutcomeUnknown`; the
  /// effect happened or not, and nothing is read as success. The same transition as a lost
  /// reply, but its own stimulus (`timeout`), never an observation, so I10 states that a
  /// timeout enters `Unknown` and never leaves it, and `timeoutOnlyAfterSend` and
  /// `timeoutNeverAdvances` state what it may not do. Time is abstract here: the aggregate
  /// refuses a timeout before the deadline (`deadline_not_reached`), which the Rust tests pin.
  action directiveTimeoutWith(fresh: int, applied: bool): bool = {
    val world = if (applied) applyEffect(a, w, fresh) else w
    all {
      canTimeOut(a),
      not(Set(STOP_VM, DELETE_VM).contains(a.pending)) or (w.exists and w.instance == a.instance)
        or not(applied),
      commitWorld("timeout", { ...a, unknown: true }, world, g),
    }
  }

  action directiveTimeout = {
    nondet fresh = INSTANCES.oneOf()
    nondet applied = Set(true, false).oneOf()
    directiveTimeoutWith(fresh, applied)
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
      // a stop the listing shows took effect is a stop that succeeded: `VmStopped`, as its
      // success reply records, so the next cleanup directive has its own budget (VER-01F)
      else if (x.unknown and x.pending == STOP_VM)
        if (w.running) notApplied(x)
        else { ...x, vmRunning: false, pending: NO_DIRECTIVE, unknown: false, tries: 0 }
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

  /// A complete inventory does not list the VM: our delete took effect; an Unknown clone did
  /// not; a VM we recorded and did not delete vanished (`vanish`, the same as a stop's or a
  /// start's `target_absent`, DEC-07); a quarantined VM of our name that we never recorded is
  /// released when it is gone (lost, not a desync: we never knew it was ours).
  action inventoryAbsent = {
    val x = a
    val result =
      if (x.pending == DELETE_VM) { ...x, phase: DELETED, vmRunning: false, pending: NO_DIRECTIVE,
                                    unknown: false }
      else if (x.pending == CLONE_VM) (if (x.unknown) notApplied(x) else x)
      else if (x.instance == NONE and x.phase != QUARANTINED) x
      else if (x.instance == NONE) { ...x, phase: LOST, vmRunning: false, pending: NO_DIRECTIVE,
                                     unknown: false }
      else vanish(x)
    all {
      not(w.exists),
      not(terminal(a)),
      commit("observation", result, g),
    }
  }

  /// An incomplete listing (I25): the inventory's `InventoryIncomplete`, or the data plane's
  /// `Unknown{listing_incomplete}` for a directive whose re-observation it is (a clone, start,
  /// stop or delete; the data plane touches nothing then). Nothing is concluded from it, least
  /// of all an absence: the inventory's is never routed to the attempt, and an answer makes the
  /// pending outcome Unknown, never vanished, lost or deleted (`noAbsenceFromIncomplete`).
  action listingIncomplete = {
    val reobserves = Set(CLONE_VM, START_VM, STOP_VM, DELETE_VM).contains(a.pending)
    all {
      not(terminal(a)),
      commit("incomplete", if (reobserves) { ...a, unknown: true } else a, g),
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

  /// `RunnerExited{code}`: the runner process ended. Inside quarantine it is only recorded:
  /// an exited runner takes no job, so the stop no longer waits for its deregistration. Inside
  /// quarantine it is accepted whatever the JIT, as `decide` accepts it (DEC-06F): the
  /// observation is a fact, and with no JIT issued it changes nothing the stop reads.
  action runnerExited = all {
    Set(LISTENING, BUSY).contains(a.phase) or (a.phase == ACTIVATING and a.jit == "taken")
      or a.phase == QUARANTINED,
    not(a.runnerExited),
    commit("observation",
      { ...a, phase: if (a.phase == QUARANTINED) QUARANTINED else FINISHED, runnerExited: true },
      g),
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

  /// `RequestRetirement{reason}`: a registered runner waits for its deregistration (I21),
  /// whether activating, listening or backing off from a failed fresh verification (KER-04G:
  /// Backoff retired at once and skipped the deregistration).
  action requestRetirement = all {
    canRequestRetirement(a),
    commit("command",
      if (Set(ACTIVATING, LISTENING, BACKOFF).contains(a.phase) and a.jit != "none")
        { ...a, retireRequested: true }
      else retire(a),
      g),
  }

  /// Whether P11 has a deregistration of this attempt to answer: a retirement's, or a
  /// quarantine stop's (ADR 0076).
  pure def awaitsDeregistration(x: Attempt): bool =
    x.retireRequested or (x.phase == QUARANTINED and x.dereg == "requested")

  /// `ConfirmRetirement` after the registration was deregistered. The request outlives the
  /// phase it was made in: a failed fresh verification backs off, a runner that exits finishes,
  /// and the deregistration still ends in a confirmation there (VER-01F). A lost VM ends the
  /// attempt instead: it is terminal, and nothing is confirmed after it. Inside quarantine the
  /// confirmation lets the quarantine stop go ahead and retires nothing (ADR 0076).
  action confirmRetirement = all {
    idle(a),
    awaitsDeregistration(a),
    not(terminal(a)),
    commit("command",
      if (a.phase == QUARANTINED) { ...a, dereg: "confirmed" } else retire(a),
      { ...g, deregistered: true }),
  }

  /// `WithdrawRetirement` after GitHub answered 422 (busy). Inside quarantine a job runs on the
  /// runner: the VM is not stopped (ADR 0076), until the runner is seen exiting.
  action withdrawRetirement = all {
    idle(a),
    awaitsDeregistration(a),
    not(terminal(a)),
    commit("command",
      if (a.phase == QUARANTINED) { ...a, dereg: "busy" } else { ...a, retireRequested: false },
      g),
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
    relaySend,
    directiveTimeout,
    replyLaunchUnknown,
    inventoryPresent,
    inventoryAbsent,
    listingIncomplete,
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
    relaySend,
    directiveTimeout,
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
    relaySend,
    directiveTimeout,
    replyLaunchUnknown,
    inventoryPresent,
    listingIncomplete,
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

  /// CP-02: a directive times out only after the relay sent it; one held unsent has a known
  /// outcome (nothing happened) and never becomes `Unknown` by its deadline.
  val timeoutOnlyAfterSend = g.lastKind == "timeout" implies g.prevSent

  /// CP-02: a timeout never moves the lifecycle; it only makes the pending outcome `Unknown`.
  val timeoutNeverAdvances =
    g.lastKind == "timeout" implies (a.phase == g.prevPhase and a.unknown)

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
  /// resolution or an observed absence does (a recorded VM's absence is a vanish, DEC-07).
  val quarantineTerminal =
    (g.quarantined and not(g.operatorResolved)) implies Set(QUARANTINED, LOST, CLOSED).contains(a.phase)

  /// quarantineNeverAutoDeleted: no delete directive while quarantined and unresolved (I28).
  val quarantineNeverAutoDeleted = not(g.autoDelete)

  /// mismatchStaysQuarantined (DEC-07, (i)): while a VM is present under the attempt's name with
  /// another instance id than the recorded one, a quarantined attempt stays quarantined until
  /// an operator resolves it. Quarantine is for a VM that is there and does not match the
  /// record; only its absence (a vanish) or the operator ends it.
  val mismatchStaysQuarantined =
    (and {
      g.quarantined,
      not(g.operatorResolved),
      w.exists,
      a.instance != NONE,
      w.instance != a.instance,
    }) implies a.phase == QUARANTINED

  /// vanishedClosedForGood (DEC-07, (ii)): a VM the attempt recorded and did not delete, found
  /// absent from a complete listing, is a recorded desync (`vanished`), and the attempt is
  /// closed for good: lost, then closed, with no directive issued and no phase entered after
  /// it, and the record never cleared. A recorded VM's absence is never a silent loss, and a
  /// VM never recorded never vanishes.
  val vanishedClosedForGood = and {
    a.vanished implies (Set(LOST, CLOSED).contains(a.phase) and a.instance != NONE),
    g.vanishedEver implies a.vanished,
    not(g.actedAfterVanish),
    (a.phase == LOST and a.instance != NONE) implies a.vanished,
  }

  /// noAbsenceFromIncomplete (DEC-07, (iii); I25): an incomplete listing moves no phase and
  /// records no vanish; at most it makes a pending outcome Unknown.
  val noAbsenceFromIncomplete = not(g.concludedFromIncomplete)

  /// retryKeepsNameAndGeneration: every clone, first or retried, targets the name and slot
  /// generation fixed at start.
  val retryKeepsNameAndGeneration =
    g.cloneTargets.forall(t => t == (a.name, a.generation))

  /// I21 (attempt half, KER-04G): deregister before stop. While a runner of the attempt's
  /// registration may be registered and idle in the VM (a launch was sent and not refused, and
  /// the runner was not seen exiting), the attempt retires with its VM running, which commits
  /// to stopping it, only after P11 had the deregistration's answer (`ConfirmRetirement`);
  /// `registration.qnt`'s `deregisterBeforeStop` states when that answer may confirm.
  /// The automatic stop of a quarantined VM (KER-04F) is inside it too since ADR 0076: issued
  /// while such a runner may be registered and idle in the VM, it waits for the same answer.
  /// An operator's resolution ends the protection: it retires the attempt on the operator's
  /// word (ADR 0025).
  val i21DeregisterBeforeStop = not(g.stopBeforeDeregister)

  // Every invariant of the model, so a new one cannot be left out of a stage.
  val allInvariants = and {
    i06CompletedNeverServesAgain,
    busyNeverWarm,
    i10UnknownLeavesOnlyOnObservation,
    timeoutOnlyAfterSend,
    timeoutNeverAdvances,
    i13NoDuplicateClones,
    i14OneJob,
    i21DeregisterBeforeStop,
    i22JitMintedOnceAfterGrantAndVerification,
    i30GuestNeverDecidesOwnership,
    quarantineTerminal,
    quarantineNeverAutoDeleted,
    mismatchStaysQuarantined,
    vanishedClosedForGood,
    noAbsenceFromIncomplete,
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

  // CP-02: a clone whose deadline passed unanswered is Unknown, never success: the saga's
  // timeout issues nothing new (no second name), and only the inventory resolves it.
  run timeoutIsUnknownNeverSuccessTest = {
    initWith(1, 1).then(issueNext).then(relaySend).then(directiveTimeoutWith(1, true))
      .expect(a.unknown and a.pending == CLONE_VM and a.phase == CLONING and a.instance == NONE)
      .expect(g.lastKind == "timeout" and not(canIssueNext(a)))
      .then(inventoryPresent)
      .expect(not(a.unknown) and a.phase == BOOTING and g.clones == 1
        and retryKeepsNameAndGeneration and allInvariants)
  }

  // CP-02: a directive the relay holds (safe mode) is never timed out; once sent, it may be.
  run heldDirectiveNeverTimesOutTest = {
    initWith(1, 1).then(issueNext)
      .expect(not(canTimeOut(a)) and a.tries == 1)
      .then(relaySend).expect(canTimeOut(a) and a.tries == 1 and allInvariants)
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

  // ADR 0066 as amended 2026-11-11 (DEC-07): a stop whose recorded VM vanished is refused
  // target_absent; the desync is recorded and the attempt closed, never quarantined, stopped
  // or retried; a later inventory's absence changes nothing.
  run stopOfVanishedVmIsClosedAsDesyncTest = {
    initWith(1, 1).then(toPrepared).then(requestRetirement)
      .then(issueNext).then(ok).then(issueNext).expect(a.pending == STOP_VM)
      .then(vmReplacedOrRemovedWith(1, true))
      .then(replyTargetAbsent)
      .expect(a.phase == LOST and a.vanished and not(a.vmRunning) and a.pending == NO_DIRECTIVE)
      .expect(not(canIssueNext(a)) and not(canQuarantine(a)) and not(canRequestRetirement(a)))
      .then(close).expect(a.phase == CLOSED and a.vanished and allInvariants)
  }

  // DEC-07: a start whose cloned VM vanished is closed as a desync too, never a stage failure.
  run startOfVanishedVmIsClosedAsDesyncTest = {
    initWith(1, 1).then(issueNext).then(ok).expect(a.phase == BOOTING)
      .then(vmReplacedOrRemovedWith(1, true))
      .then(issueNext).expect(a.pending == START_VM)
      .then(replyTargetAbsent)
      .expect(a.phase == LOST and a.vanished and g.clones == 1 and allInvariants)
  }

  // DEC-07: the periodic inventory finds a recorded VM gone, with nothing pending: the same
  // desync as a stop's or start's target_absent, never a silent loss.
  run inventoryFindsVanishedVmTest = {
    initWith(1, 1).then(toPrepared).then(toListening)
      .then(vmReplacedOrRemovedWith(1, true))
      .then(inventoryAbsent)
      .expect(a.phase == LOST and a.vanished and not(a.vmRunning) and allInvariants)
      .then(close).expect(a.phase == CLOSED and a.vanished and allInvariants)
  }

  // DEC-07: a quarantined attempt (the guest failed its challenge) whose recorded VM then
  // vanishes is closed as a desync: quarantine holds only a VM that is present.
  run quarantinedVmVanishedIsClosedTest = {
    initWith(1, 1).then(issueNext).then(ok).then(issueNext).then(ok)
      .then(issueNext).then(replyGuestRejected).expect(a.phase == QUARANTINED)
      .then(issueNext).expect(a.pending == STOP_VM)
      .then(vmReplacedOrRemovedWith(1, true))
      .then(replyTargetAbsent)
      .expect(a.phase == LOST and a.vanished and quarantineNeverAutoDeleted and allInvariants)
  }

  // DEC-07, (i): a VM present under our name with another instance id keeps the attempt
  // quarantined, whatever the inventory or the stops say, until the VM goes (a vanish of the
  // recorded VM) or an operator resolves it.
  run mismatchedVmStaysQuarantinedTest = {
    initWith(1, 1).then(toPrepared)
      .then(vmReplacedOrRemovedWith(2, false))
      .then(inventoryPresent).expect(a.phase == QUARANTINED and not(a.vanished))
      .then(issueNext).expect(a.pending == STOP_VM)
      .then(replyProofRefused).expect(a.phase == QUARANTINED)
      .then(inventoryPresent).expect(a.phase == QUARANTINED and mismatchStaysQuarantined)
      .then(vmReplacedOrRemovedWith(2, true))
      .then(inventoryAbsent).expect(a.phase == LOST and a.vanished and allInvariants)
  }

  // DEC-07, (iii): an incomplete listing concludes no absence: a stop re-observed on one is
  // Unknown, never vanished; an inventory one moves nothing. The complete listing decides.
  run incompleteListingConcludesNothingTest = {
    initWith(1, 1).then(toPrepared).then(requestRetirement)
      .then(issueNext).then(ok).then(issueNext).expect(a.pending == STOP_VM)
      .then(vmReplacedOrRemovedWith(1, true))
      .then(listingIncomplete)
      .expect(a.phase == RETIRING and a.unknown and not(a.vanished) and noAbsenceFromIncomplete)
      .then(listingIncomplete).expect(a.phase == RETIRING and not(a.vanished))
      .then(inventoryAbsent)
      .expect(a.phase == LOST and a.vanished and i10UnknownLeavesOnlyOnObservation
        and allInvariants)
  }

  // DEC-07, negative controls: each new invariant flags the behaviour it forbids, forced here
  // by a transition no action allows.
  run vanishFromIncompleteListingIsFlaggedTest = {
    initWith(1, 1).then(toPrepared)
      .then(commit("incomplete", vanish(a), g))
      .expect(not(noAbsenceFromIncomplete))
  }

  run silentLossOfRecordedVmIsFlaggedTest = {
    initWith(1, 1).then(toPrepared)
      .then(commit("observation", { ...a, phase: LOST, vmRunning: false }, g))
      .expect(not(vanishedClosedForGood))
  }

  run resurrectionAfterVanishIsFlaggedTest = {
    initWith(1, 1).then(toPrepared).then(vmReplacedOrRemovedWith(1, true))
      .then(inventoryAbsent).expect(a.vanished)
      .then(commit("command", { ...a, phase: RESERVED, vanished: false }, g))
      .expect(not(vanishedClosedForGood))
  }

  run mismatchReleasedFromQuarantineIsFlaggedTest = {
    initWith(1, 1).then(toPrepared)
      .then(vmReplacedOrRemovedWith(2, false))
      .then(inventoryPresent).expect(a.phase == QUARANTINED)
      .then(commit("observation", vanish(a), g))
      .expect(not(mismatchStaysQuarantined))
  }

  // The activation relation's shortcut lands where the preparation does.
  run prepareAtOnceTest = {
    initWith(1, 1).then(prepareAtOnceWith(1, false))
      .expect(a.phase == PREPARED and a.vmRunning and w.running and allInvariants)
      .then(beginActivation).then(issueNext).expect(a.pending == VERIFY_GUEST)
  }

  // VER-01F: a lost stop reply resolved by a listing that shows the VM stopped is the stop's
  // success (`VmStopped`): the count of tries restarts, and the delete has its own budget.
  run unknownStopResolvedByListingTest = {
    initWith(1, 1).then(toPrepared).then(requestRetirement)
      .then(issueNext).then(ok).then(issueNext).expect(a.pending == STOP_VM and a.tries == 1)
      .then(replyLostWith(1, true)).expect(a.unknown)
      .then(inventoryPresent)
      .expect(not(a.unknown) and a.pending == NO_DIRECTIVE and not(a.vmRunning) and a.tries == 0
        and i10UnknownLeavesOnlyOnObservation)
      .then(issueNext).expect(a.pending == DELETE_VM and a.tries == 1 and allInvariants)
  }

  // VER-01F: a retirement requested while activating outlives a failed fresh verification;
  // the deregistration's confirmation retires the attempt from Backoff.
  run confirmRetirementInBackoffTest = {
    initWith(1, 1).then(prepareAtOnceWith(1, true)).then(recordJitHandle).then(adopt)
      .then(requestRetirement).expect(a.phase == ACTIVATING and a.retireRequested)
      .then(issueNext).expect(a.pending == VERIFY_GUEST)
      .then(replyRefused)
      .expect(a.phase == BACKOFF and a.stage == "activate" and a.retireRequested)
      .then(confirmRetirement)
      .expect(a.phase == RETIRING and not(a.retireRequested) and allInvariants)
  }

  // VER-01F: and a runner that exits after a launch of unknown outcome finishes with the
  // request still waiting for its deregistration; the confirmation retires it from Finished.
  run confirmRetirementAfterExitTest = {
    initWith(1, 1).then(prepareAtOnceWith(1, true)).then(recordJitHandle)
      .then(issueNext).expect(a.pending == LAUNCH_RUNNER)
      .then(replyLaunchUnknown).then(requestRetirement).expect(a.retireRequested)
      .then(runnerExited).expect(a.phase == FINISHED and a.retireRequested)
      .then(confirmRetirement)
      .expect(a.phase == RETIRING and not(a.retireRequested) and allInvariants)
  }

  /// A launch of unknown outcome (a runner may be registered in the VM), then a restart, whose
  /// fresh verification is refused: Backoff in the activate stage with the registration held.
  action toBackoffHoldingRegistration =
    prepareAtOnceWith(1, true).then(recordJitHandle).then(issueNext).then(replyLaunchUnknown)
      .then(adopt).then(issueNext).then(replyRefused)

  // KER-04G/I21: a retirement requested in Backoff while the registration is held waits for
  // the deregistration like one in Activating or Listening; no stop before its confirmation.
  // Before KER-04G the request retired at once and the stop followed (a violation of
  // `i21DeregisterBeforeStop`).
  run retirementInBackoffWaitsForDeregistrationTest = {
    initWith(1, 1).then(toBackoffHoldingRegistration)
      .expect(a.phase == BACKOFF and a.stage == "activate" and a.jit == "taken"
        and not(a.retireRequested))
      .then(requestRetirement)
      .expect(a.phase == BACKOFF and a.retireRequested)
      // a retirement waiting for its deregistration does not retry the stage, nor ask twice
      .expect(not(canIssueNext(a)) and not(canRequestRetirement(a)))
      .then(confirmRetirement).expect(a.phase == RETIRING and not(a.retireRequested))
      .then(issueNext).expect(a.pending == EXPORT_DIAGNOSTICS).then(ok)
      .then(issueNext).expect(a.pending == STOP_VM and allInvariants)
  }

  // KER-04G/I21: an activation whose budget is spent while the registration is held does not
  // retire at once: it requests the retirement and waits for the deregistration.
  run stageFailureHoldingRegistrationWaitsForDeregistrationTest = {
    initWith(1, 1).then(toBackoffHoldingRegistration)
      .then(issueNext).expect(a.phase == ACTIVATING and a.tries == BUDGET)
      .then(replyRefused)
      .expect(a.phase == BACKOFF and a.stage == "activate" and a.retireRequested)
      .expect(not(canIssueNext(a)))
      .then(confirmRetirement).expect(a.phase == RETIRING)
      .then(issueNext).then(ok)
      .then(issueNext).expect(a.pending == STOP_VM and allInvariants)
  }

  // KER-04G, negative controls of `i21DeregisterBeforeStop`: a launch refused definitely
  // leaves no runner in the VM, and a runner that exited can take no job there, so either
  // retirement stops the VM at once without a violation.
  run stopAfterRefusedLaunchNeedsNoDeregistrationTest = {
    initWith(1, 1).then(prepareAtOnceWith(1, true)).then(recordJitHandle).then(issueNext)
      .then(replyRefused).expect(a.phase == RETIRING and not(g.runnerMayLive))
      .then(issueNext).then(ok)
      .then(issueNext).expect(a.pending == STOP_VM and i21DeregisterBeforeStop)
  }

  run stopAfterRunnerExitedNeedsNoDeregistrationTest = {
    initWith(1, 1).then(prepareAtOnceWith(1, true)).then(recordJitHandle).then(issueNext)
      .then(ok).then(runnerExited).then(requestRetirement).expect(a.phase == RETIRING)
      .then(issueNext).then(ok)
      .then(issueNext).expect(a.pending == STOP_VM and i21DeregisterBeforeStop)
  }

  // KER-04G, positive control of `i21DeregisterBeforeStop`: the invariant flags a retirement
  // that skips the deregistration (forced here by retiring a listening runner directly, which
  // no action allows; before KER-04G `requestRetirement` in Backoff did the same).
  run stopBeforeDeregistrationIsFlaggedTest = {
    initWith(1, 1).then(prepareAtOnceWith(1, true)).then(recordJitHandle).then(issueNext)
      .then(ok).expect(a.phase == LISTENING)
      .then(commit("command", retire(a), g))
      .expect(a.phase == RETIRING and not(i21DeregisterBeforeStop))
  }

  // ADR 0076: a quarantined VM whose runner may still be registered is stopped only after
  // the deregistration answered; before ADR 0076 the stop was issued at once (a violation of
  // `i21DeregisterBeforeStop` once the invariant covers quarantine stops).
  run quarantineStopWaitsForDeregistrationTest = {
    initWith(1, 1).then(toPrepared).then(toListening)
      .then(quarantineCommand)
      .expect(a.phase == QUARANTINED and a.vmRunning and a.dereg == "requested")
      .expect(not(canIssueNext(a)))
      .then(confirmRetirement)
      .expect(a.phase == QUARANTINED and a.dereg == "confirmed" and allInvariants)
      .then(issueNext).expect(a.pending == STOP_VM and allInvariants)
  }

  // ADR 0076: GitHub answered busy (a job runs on the runner): the quarantined VM keeps
  // running and is never stopped under the job; once the runner is seen exiting, it is.
  run quarantineStopBusyKeepsRunningTest = {
    initWith(1, 1).then(toPrepared).then(toListening)
      .then(quarantineCommand).then(withdrawRetirement)
      .expect(a.phase == QUARANTINED and a.dereg == "busy" and a.vmRunning)
      .expect(not(canIssueNext(a)))
      .then(runnerExited).expect(a.phase == QUARANTINED and a.runnerExited)
      .then(issueNext).expect(a.pending == STOP_VM and allInvariants)
  }

  // ADR 0076, negative controls: a quarantine with no runner that could be registered (no JIT
  // issued, or the runner exited) stops at once, and so does one whose VM was already stopped
  // or whose job is live (it waits for the job instead, as before).
  run quarantineStopWithoutRegistrationNeedsNoDeregistrationTest = {
    initWith(1, 1).then(toPrepared).then(quarantineCommand)
      .expect(a.dereg == "none")
      .then(issueNext).expect(a.pending == STOP_VM and i21DeregisterBeforeStop)
  }

  run quarantineStopAfterRunnerExitedNeedsNoDeregistrationTest = {
    initWith(1, 1).then(toPrepared).then(toListening).then(runnerExited)
      .then(quarantineCommand).expect(a.dereg == "none")
      .then(issueNext).expect(a.pending == STOP_VM and i21DeregisterBeforeStop)
  }

  // ADR 0076, positive control: the KER-04F stop, issued at once for a quarantined VM whose
  // runner may still be registered (forced here: no action issues it now), is flagged.
  run quarantineStopBeforeDeregistrationIsFlaggedTest = {
    initWith(1, 1).then(toPrepared).then(toListening).then(quarantineCommand)
      .then(commitWorld("saga", { ...a, pending: STOP_VM, tries: a.tries + 1 }, w, g))
      .expect(a.pending == STOP_VM and not(i21DeregisterBeforeStop))
  }

  // VER-01F: a clone whose lost reply took effect, and whose VM someone then removed, is
  // cloned afresh by its re-delivery: the reply carries the new VM's instance id.
  run cloneAnsweredAfterRemovalTest = {
    initWith(1, 1).then(issueNext).then(replyLostWith(1, true))
      .then(vmReplacedOrRemovedWith(1, true)).expect(a.unknown and not(w.exists))
      .then(ok)
      .expect(a.phase == BOOTING and a.instance == 1 and w.exists and w.instance == 1
        and i10UnknownLeavesOnlyOnObservation and allInvariants)
  }

  // VER-01F, as DEC-07 left it: an operator's resolution and a policy's quarantine record no
  // vanish; only a recorded VM's absence does.
  run requarantineAfterResolutionTest = {
    initWith(1, 1).then(toPrepared).then(requestRetirement)
      .then(issueNext).then(ok).then(issueNext).then(replyProofRefusedAfterReplace)
      .expect(a.phase == QUARANTINED and not(a.vanished))
      .then(resolveQuarantine).expect(a.phase == RETIRING and not(a.vanished))
      .then(quarantineCommand)
      .expect(a.phase == QUARANTINED and not(a.vanished) and allInvariants)
  }

  /// The VM replaced behind our back, then the pending stop refused for its proof.
  action replyProofRefusedAfterReplace = vmReplacedOrRemovedWith(2, false).then(replyProofRefused)
}

/**
 * Named runs of `lifecycle` as scripted step relations, so `quint run --mbt` exports each as an
 * ITF regression trace carrying the action and picks of every step (ver §6.2; the pattern of
 * `event_store_traces`). The conformance driver (VER-01) replays them against the `Attempt`
 * aggregate in `mayfly-conformance`'s tests, without Node. A script is a `List[Stim]`, one step
 * per stimulus; `recorded` turns each stimulus' arguments into picks named as the model's own
 * relations name them (`fresh`, `claim`, `applied`, `job`, `other`, `removed`, `instance`,
 * `activated`), and its `any` makes the model action taken `mbt::actionTaken`.
 *
 * The scripts follow the `run`s of `lifecycle` of the same name. Four of them reproduced the
 * divergences the first replays found between the model and the aggregate (VER-01); VER-01F
 * resolved each and keeps its script as a regression trace (`unknownStopResolvedByListing`,
 * `confirmRetirementInBackoff`, `cloneAnsweredAfterRemoval`, and `requarantineAfterVanished`,
 * renamed `requarantineAfterResolution` when DEC-07 made a vanish close the attempt).
 *
 * Each trace is regenerated into `formal/traces/regression/` from the repository root (the
 * scripts are deterministic; the seed is recorded for byte-for-byte freshness, VER-01):
 *
 *   npx --prefix formal --no -- quint run --backend typescript --main lifecycle_traces \
 *     --init tinit --step <script> --max-steps <its length> --mbt --seed 0x5ce1 \
 *     --invariant allInvariants --out-itf formal/traces/regression/lifecycle-<script>.itf.json \
 *     formal/lifecycle.qnt
 */
module lifecycle_traces {
  import lifecycle as L

  /// The `allInvariants` of the model, checked on every exported state.
  val allInvariants = L::allInvariants

  /// The script's position: the stimulus it takes next.
  var pc: int

  /// One stimulus: the action's name in `step` and every argument any action takes.
  type Stim = {
    kind: str, fresh: int, claim: int, applied: bool, job: int, other: int, removed: bool,
    instance: int, activated: bool,
  }

  pure val S0: Stim = {
    kind: "", fresh: 1, claim: 3, applied: true, job: 1, other: 1, removed: false, instance: 1,
    activated: false,
  }

  pure def s(kind: str): Stim = { ...S0, kind: kind }

  pure val ISSUE = s("issueNext")
  /// The success reply of the scenarios (`ok`: instance 1, a guest claiming 3).
  pure val OK = s("replySucceeded")
  pure def lost(applied: bool): Stim = { ...S0, kind: "replyLost", applied: applied }
  pure val SENT = s("relaySend")
  pure def timedOut(applied: bool): Stim = { ...S0, kind: "directiveTimeout", applied: applied }
  pure def replaced(other: int, removed: bool): Stim =
    { ...S0, kind: "vmReplacedOrRemoved", other: other, removed: removed }
  pure def prepare(activated: bool): Stim = { ...S0, kind: "prepareAtOnce", activated: activated }

  pure val TO_PREPARED = [ISSUE, OK, ISSUE, OK, ISSUE, OK]
  pure val TO_LISTENING = [s("beginActivation"), ISSUE, OK, s("recordJitHandle"), ISSUE, OK]
  /// Prepared, then retired: diagnostics exported, and the stop issued.
  pure val TO_STOPPING = TO_PREPARED.concat([s("requestRetirement"), ISSUE, OK, ISSUE])

  pure val HAPPY_PATH = TO_PREPARED.concat(TO_LISTENING).concat([
    s("bindJob"), s("runnerExited"), s("requestRetirement"), ISSUE, OK, ISSUE, OK, ISSUE, OK,
    s("close"),
  ])
  /// CP-02: a clone sent and timed out is Unknown, never cloned, and the inventory resolves it.
  pure val TIMEOUT_IS_UNKNOWN_NEVER_SUCCESS =
    [ISSUE, SENT, timedOut(true), s("inventoryPresent")]
  pure val UNKNOWN_CLONE_RESOLVED_BY_INVENTORY =
    [ISSUE, lost(true), s("adopt"), s("inventoryPresent")]
  pure val STAGE_FAILURE_BACKS_OFF_THEN_RETIRES =
    [ISSUE, s("replyRefused"), ISSUE, s("replyRefused")]
  pure val GUEST_REJECTED_IS_QUARANTINED = [
    ISSUE, OK, ISSUE, OK, ISSUE, s("replyGuestRejected"), ISSUE, OK, s("resolveQuarantine"),
    ISSUE,
  ]
  pure val LAUNCH_UNKNOWN_NEVER_SERVES = TO_PREPARED.concat([
    s("beginActivation"), ISSUE, OK, s("recordJitHandle"), ISSUE, s("replyLaunchUnknown"),
    s("requestRetirement"), s("confirmRetirement"),
  ])
  pure val RETIREMENT_WAITS_FOR_DEREGISTRATION = TO_PREPARED.concat(TO_LISTENING).concat([
    s("requestRetirement"), s("withdrawRetirement"), s("requestRetirement"), s("bindJob"),
  ])
  pure val REPLACED_VM_QUARANTINES_ON_DELETE = TO_PREPARED.concat([
    s("requestRetirement"), ISSUE, OK, ISSUE, OK, ISSUE, replaced(2, false),
    s("replyProofRefused"),
  ])
  pure val ADOPT_DURING_LAUNCH_NEVER_SERVES = TO_PREPARED.concat([
    s("beginActivation"), ISSUE, OK, s("recordJitHandle"), ISSUE, s("adopt"), OK,
    s("requestRetirement"), s("confirmRetirement"),
  ])
  pure val DELETE_REFUSED_REPEATEDLY_QUARANTINES = TO_PREPARED.concat([
    s("requestRetirement"), ISSUE, OK, ISSUE, OK, ISSUE, s("replyRefused"), ISSUE,
    s("replyRefused"),
  ])
  pure val DELETE_LOST_THEN_ANSWERED_DELETED = TO_PREPARED.concat([
    s("requestRetirement"), ISSUE, OK, ISSUE, OK, ISSUE, lost(true), OK, s("close"),
  ])
  pure val STOP_OF_VANISHED_VM_IS_CLOSED_AS_DESYNC =
    TO_STOPPING.concat([replaced(1, true), s("replyTargetAbsent"), s("close")])
  /// DEC-07: a start whose cloned VM vanished.
  pure val START_OF_VANISHED_VM_IS_CLOSED_AS_DESYNC =
    [ISSUE, OK, replaced(1, true), ISSUE, s("replyTargetAbsent")]
  /// DEC-07: the inventory finds a listening attempt's VM gone.
  pure val INVENTORY_FINDS_VANISHED_VM = TO_PREPARED.concat(TO_LISTENING).concat([
    replaced(1, true), s("inventoryAbsent"), s("close"),
  ])
  /// DEC-07: a quarantined attempt whose VM vanishes under its stop.
  pure val QUARANTINED_VM_VANISHED_IS_CLOSED = [
    ISSUE, OK, ISSUE, OK, ISSUE, s("replyGuestRejected"), ISSUE, replaced(1, true),
    s("replyTargetAbsent"),
  ]
  /// DEC-07: a replaced VM keeps the attempt quarantined until it goes.
  pure val MISMATCHED_VM_STAYS_QUARANTINED = TO_PREPARED.concat([
    replaced(2, false), s("inventoryPresent"), ISSUE, s("replyProofRefused"),
    s("inventoryPresent"), replaced(2, true), s("inventoryAbsent"),
  ])
  /// DEC-07: incomplete listings conclude nothing; the complete one decides.
  pure val INCOMPLETE_LISTING_CONCLUDES_NOTHING = TO_STOPPING.concat([
    replaced(1, true), s("listingIncomplete"), s("listingIncomplete"), s("inventoryAbsent"),
  ])
  pure val ACTIVATION_AT_ONCE = [
    prepare(true), s("recordJitHandle"), ISSUE, OK, s("bindJob"), s("vmObservedStopped"),
  ]

  // The divergences VER-01 found and VER-01F resolved (see the module docs).
  /// A stop whose reply was lost, resolved by a listing showing the VM stopped.
  pure val UNKNOWN_STOP_RESOLVED_BY_LISTING =
    TO_STOPPING.concat([lost(true), s("inventoryPresent"), ISSUE])
  /// A retirement requested while activating, confirmed after the fresh verification failed.
  pure val CONFIRM_RETIREMENT_IN_BACKOFF = [
    prepare(true), s("recordJitHandle"), s("adopt"), s("requestRetirement"), ISSUE,
    s("replyRefused"), s("confirmRetirement"),
  ]
  /// A retirement requested after a launch of unknown outcome, confirmed after the exit.
  pure val CONFIRM_RETIREMENT_AFTER_EXIT = [
    prepare(true), s("recordJitHandle"), ISSUE, s("replyLaunchUnknown"),
    s("requestRetirement"), s("runnerExited"), s("confirmRetirement"),
  ]
  /// A clone whose reply was lost and whose VM was then removed, answered by a re-delivery.
  pure val CLONE_ANSWERED_AFTER_REMOVAL =
    [ISSUE, lost(true), replaced(1, true), OK]
  /// A quarantine resolved by an operator, then quarantined again by a policy: no vanish is
  /// recorded either way (VER-01F's `requarantineAfterVanished`, as DEC-07 left it).
  pure val REQUARANTINE_AFTER_RESOLUTION = TO_STOPPING.concat([
    replaced(2, false), s("replyProofRefused"), s("resolveQuarantine"), s("quarantineCommand"),
  ])

  // KER-04G: deregister before stop in Backoff (I21), see `i21DeregisterBeforeStop`.
  /// A launch of unknown outcome, a restart, and its fresh verification refused: Backoff in the
  /// activate stage with the registration held.
  pure val TO_BACKOFF_HOLDING_REGISTRATION = [
    prepare(true), s("recordJitHandle"), ISSUE, s("replyLaunchUnknown"), s("adopt"), ISSUE,
    s("replyRefused"),
  ]
  /// A retirement requested there waits for the deregistration; its confirmation stops the VM.
  pure val RETIREMENT_IN_BACKOFF_WAITS_FOR_DEREGISTRATION = TO_BACKOFF_HOLDING_REGISTRATION
    .concat([s("requestRetirement"), s("confirmRetirement"), ISSUE, OK, ISSUE])
  /// The activation's budget spent there requests the retirement instead of retiring at once.
  pure val STAGE_FAILURE_HOLDING_REGISTRATION_WAITS_FOR_DEREGISTRATION =
    TO_BACKOFF_HOLDING_REGISTRATION
      .concat([ISSUE, s("replyRefused"), s("confirmRetirement"), ISSUE, OK, ISSUE])

  // ADR 0076: the quarantine stop deregisters first, see `i21DeregisterBeforeStop`.
  /// A listening runner quarantined: the stop waits for the deregistration's confirmation.
  pure val QUARANTINE_STOP_WAITS_FOR_DEREGISTRATION = TO_PREPARED.concat(TO_LISTENING)
    .concat([s("quarantineCommand"), s("confirmRetirement"), ISSUE])
  /// A busy answer keeps the quarantined VM running until its runner is seen exiting.
  pure val QUARANTINE_STOP_BUSY_KEEPS_RUNNING = TO_PREPARED.concat(TO_LISTENING)
    .concat([s("quarantineCommand"), s("withdrawRetirement"), s("runnerExited"), ISSUE])

  action tinit = {
    nondet name = Set(1).oneOf()
    nondet generation = Set(1).oneOf()
    all { L::initWith(name, generation), pc' = 0 }
  }

  /// One stimulus, its arguments recorded as the picks of `step`.
  action recorded(x: Stim): bool = {
    nondet fresh = Set(x.fresh).oneOf()
    nondet claim = Set(x.claim).oneOf()
    nondet applied = Set(x.applied).oneOf()
    nondet job = Set(x.job).oneOf()
    nondet other = Set(x.other).oneOf()
    nondet removed = Set(x.removed).oneOf()
    nondet instance = Set(x.instance).oneOf()
    nondet activated = Set(x.activated).oneOf()
    any {
      all { x.kind == "issueNext", L::issueNext },
      all { x.kind == "replySucceeded", L::replySucceededWith(fresh, claim) },
      all { x.kind == "replyRefused", L::replyRefused },
      all { x.kind == "replyProofRefused", L::replyProofRefused },
      all { x.kind == "replyTargetAbsent", L::replyTargetAbsent },
      all { x.kind == "replyGuestRejected", L::replyGuestRejected },
      all { x.kind == "replyLost", L::replyLostWith(fresh, applied) },
      all { x.kind == "relaySend", L::relaySend },
      all { x.kind == "directiveTimeout", L::directiveTimeoutWith(fresh, applied) },
      all { x.kind == "replyLaunchUnknown", L::replyLaunchUnknown },
      all { x.kind == "inventoryPresent", L::inventoryPresent },
      all { x.kind == "inventoryAbsent", L::inventoryAbsent },
      all { x.kind == "listingIncomplete", L::listingIncomplete },
      all { x.kind == "vmObservedStopped", L::vmObservedStopped },
      all { x.kind == "runnerExited", L::runnerExited },
      all { x.kind == "vmReplacedOrRemoved", L::vmReplacedOrRemovedWith(other, removed) },
      all { x.kind == "beginActivation", L::beginActivation },
      all { x.kind == "recordJitHandle", L::recordJitHandle },
      all { x.kind == "bindJob", L::bindJobWith(job) },
      all { x.kind == "requestRetirement", L::requestRetirement },
      all { x.kind == "confirmRetirement", L::confirmRetirement },
      all { x.kind == "withdrawRetirement", L::withdrawRetirement },
      all { x.kind == "quarantineCommand", L::quarantineCommand },
      all { x.kind == "adopt", L::adopt },
      all { x.kind == "resolveQuarantine", L::resolveQuarantine },
      all { x.kind == "close", L::close },
      all { x.kind == "prepareAtOnce", L::prepareAtOnceWith(instance, activated) },
    }
  }

  action script(S: List[Stim]): bool = all { pc < S.length(), recorded(S[pc]), pc' = pc + 1 }

  action happyPath = script(HAPPY_PATH)
  action timeoutIsUnknownNeverSuccess = script(TIMEOUT_IS_UNKNOWN_NEVER_SUCCESS)
  action unknownCloneResolvedByInventory = script(UNKNOWN_CLONE_RESOLVED_BY_INVENTORY)
  action stageFailureBacksOffThenRetires = script(STAGE_FAILURE_BACKS_OFF_THEN_RETIRES)
  action guestRejectedIsQuarantined = script(GUEST_REJECTED_IS_QUARANTINED)
  action launchUnknownNeverServes = script(LAUNCH_UNKNOWN_NEVER_SERVES)
  action retirementWaitsForDeregistration = script(RETIREMENT_WAITS_FOR_DEREGISTRATION)
  action replacedVmQuarantinesOnDelete = script(REPLACED_VM_QUARANTINES_ON_DELETE)
  action adoptDuringLaunchNeverServes = script(ADOPT_DURING_LAUNCH_NEVER_SERVES)
  action deleteRefusedRepeatedlyQuarantines = script(DELETE_REFUSED_REPEATEDLY_QUARANTINES)
  action deleteLostThenAnsweredDeleted = script(DELETE_LOST_THEN_ANSWERED_DELETED)
  action stopOfVanishedVmIsClosedAsDesync = script(STOP_OF_VANISHED_VM_IS_CLOSED_AS_DESYNC)
  action startOfVanishedVmIsClosedAsDesync = script(START_OF_VANISHED_VM_IS_CLOSED_AS_DESYNC)
  action inventoryFindsVanishedVm = script(INVENTORY_FINDS_VANISHED_VM)
  action quarantinedVmVanishedIsClosed = script(QUARANTINED_VM_VANISHED_IS_CLOSED)
  action mismatchedVmStaysQuarantined = script(MISMATCHED_VM_STAYS_QUARANTINED)
  action incompleteListingConcludesNothing = script(INCOMPLETE_LISTING_CONCLUDES_NOTHING)
  action activationAtOnce = script(ACTIVATION_AT_ONCE)
  action unknownStopResolvedByListing = script(UNKNOWN_STOP_RESOLVED_BY_LISTING)
  action confirmRetirementInBackoff = script(CONFIRM_RETIREMENT_IN_BACKOFF)
  action confirmRetirementAfterExit = script(CONFIRM_RETIREMENT_AFTER_EXIT)
  action cloneAnsweredAfterRemoval = script(CLONE_ANSWERED_AFTER_REMOVAL)
  action requarantineAfterResolution = script(REQUARANTINE_AFTER_RESOLUTION)
  action retirementInBackoffWaitsForDeregistration =
    script(RETIREMENT_IN_BACKOFF_WAITS_FOR_DEREGISTRATION)
  action stageFailureHoldingRegistrationWaitsForDeregistration =
    script(STAGE_FAILURE_HOLDING_REGISTRATION_WAITS_FOR_DEREGISTRATION)
  action quarantineStopWaitsForDeregistration = script(QUARANTINE_STOP_WAITS_FOR_DEREGISTRATION)
  action quarantineStopBusyKeepsRunning = script(QUARANTINE_STOP_BUSY_KEEPS_RUNNING)

}
