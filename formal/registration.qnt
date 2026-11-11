// -*- mode: Bluespec; -*-
/**
 * The registration model (KER-09): the `RunnerRegistration` streams of two attempts
 * (doc/domain-model.md §6.5), the policies around them (P9 mint and delivery, P10 issue unknown,
 * P11 deregister before stop, P12 job facts, P13 presumed leaked, P14 the reaper) and GitHub's
 * runner list (doc/github-scaleset.md §6, §7). The Rust aggregate is
 * `mayfly_control_domain::registration`, and P11's choice is its pure `retirement_step`; the
 * conformance driver (VER-02) maps each action below onto one of their stimuli.
 *
 * The world is GitHub's runner list (`gh`), whether a runner process lives in each VM
 * (`vmRunner`) and whether GitHub answers (`ghUp`); the registrations (`regs`) and the attempts'
 * retirement side (`atts`) are what the journal records, and they change only on a command or
 * an observation of the world. GitHub's runner id is modelled as the attempt's number: a name
 * is minted at most once, so one name never has two ids. A foreign runner of our name shape
 * (`FOREIGN`), with no journal record, is on GitHub from the start: the reaper must never touch
 * it (ADR 0029).
 *
 * A busy runner's removal answer is "busy": 409 `JobStillRunningException` on the
 * Actions-service path mayfly uses, 422 on the REST path (doc/github-scaleset.md §7). The model
 * and the aggregate name it busy, never by a status code.
 *
 * Ghost state (`g`) records history for the invariants and never feeds a guard.
 */
module registration {
  // ---- constants ----------------------------------------------------------------------------

  /// Attempts, each with its own registration stream; its number is also its runner name (the
  /// VM name, unique per attempt) and the id GitHub assigns to that name's runner.
  pure val ATTEMPTS = Set(1, 2)
  /// A runner of our name shape that mayfly never journaled.
  pure val FOREIGN = 9
  pure val RUNNER_IDS = Set(1, 2, FOREIGN)
  pure val NONE = 0

  // Phases of a registration (dm §6.5; `PresumedLeaked` and `DeregistrationRequested` are the
  // `leaked` and `dereg` overlays below, as in the aggregate).
  pure val UNREQUESTED = "Unrequested"
  pure val REQUESTED = "Requested"
  pure val ISSUE_UNKNOWN = "IssueUnknown"
  pure val ISSUED = "Issued"
  pure val DELIVERED = "Delivered"
  pure val CONNECTED = "Connected"
  pure val CONSUMED = "Consumed"
  pure val COMPLETED = "Completed"
  pure val DEREGISTERED = "Deregistered"
  pure val ABSENT = "Absent"

  // The deregistration overlay.
  pure val NO_DEREG = "none"
  pure val DEREG_REQUESTED = "requested"

  // An attempt's retirement.
  pure val NOT_RETIRING = "none"
  pure val RETIRE_REQUESTED = "requested"
  pure val RETIRE_CONFIRMED = "confirmed"

  // P11's choice for a requested retirement (`retirement_step` in the aggregate).
  pure val DEREGISTER = "deregister"
  pure val CONFIRM = "confirm"
  pure val WITHDRAW = "withdraw"
  pure val WAIT = "wait"

  // ---- state --------------------------------------------------------------------------------

  type Reg = {
    phase: str,
    // journaled from RegistrationIssued or RegistrationFoundByName; NONE before
    runnerId: int,
    // presumed leaked (P13, or found by name after an unknown issue)
    leaked: bool,
    dereg: str,
    // the retirement the pending request serves; NONE for a leaked runner's (P13, P14)
    deregFor: int,
    // the retirement GitHub answered busy for (I21: final for that retirement); NONE if none
    busyFor: int,
  }

  type Att = {
    retire: str,
    vm: bool,
    // RequestRetirement commands so far: each retirement's number
    retirements: int,
    // the attempt recorded RunnerLaunchUnknown
    launchUnknown: bool,
  }

  /// One runner on GitHub's list.
  type Runner = { present: bool, name: int, online: bool, busy: bool, jobDone: bool }

  type Ghost = {
    // what the last action was: "init" | "command" | "observation" | "world"
    lastKind: str,
    // the kind of the last action, if it took a registration out of IssueUnknown; "" otherwise
    unknownLeftBy: str,
    // generatejitconfig calls per runner name
    mints: int -> int,
    // names whose issue outcome was ever Unknown
    unknownNames: Set[int],
    remintAfterUnknown: bool,
    // a runner removed by an id no registration journaled
    deletedUnjournaled: bool,
    // a VM stopped while its runner was still on GitHub's list
    stoppedWhileRegistered: bool,
    // (attempt, retirement) answered busy, and confirmed
    busyRetirements: Set[(int, int)],
    confirmedRetirements: Set[(int, int)],
    // launches of a JIT config per attempt, unknown outcomes included
    deliveries: int -> int,
    deliveredWhileLeaked: bool,
    // runner ids seen online by the confirmation
    seenOnline: Set[int],
  }

  var regs: int -> Reg
  var atts: int -> Att
  var gh: int -> Runner
  var vmRunner: int -> bool
  var ghUp: bool
  var g: Ghost

  // ---- helpers ------------------------------------------------------------------------------

  /// Nothing more happens at GitHub for it: never requested, deregistered or absent.
  pure def settled(r: Reg): bool = Set(UNREQUESTED, DEREGISTERED, ABSENT).contains(r.phase)

  /// The registration holds a runner id GitHub may still list.
  pure def holdsRunner(r: Reg): bool = r.runnerId != NONE and not(settled(r))

  /// On the reaper's list (P10, P13, P14): an unknown issue, or a journaled runner presumed
  /// leaked or waiting for its deregistration.
  pure def onReaperList(r: Reg): bool =
    r.phase == ISSUE_UNKNOWN or (holdsRunner(r) and (r.leaked or r.dereg == DEREG_REQUESTED))

  /// P11: what retirement `k` of the attempt does next, given its registration. A busy answer
  /// for this retirement withdraws it; a runner whose JIT config may have reached the VM is
  /// deregistered first; with no runner that could be in the VM the retirement is confirmed.
  pure def retirementStep(r: Reg, k: int): str =
    if (r.busyFor == k) WITHDRAW
    else if (Set(UNREQUESTED, ISSUE_UNKNOWN, ABSENT, DEREGISTERED).contains(r.phase)) CONFIRM
    else if (r.phase == REQUESTED) WAIT
    else if (r.dereg == DEREG_REQUESTED) WAIT
    else DEREGISTER

  /// P11's step for the attempt's current retirement.
  pure def stepOf(r: Reg, at: Att): str = retirementStep(r, at.retirements)

  pure def leftUnknown(before: int -> Reg, after: int -> Reg): bool =
    ATTEMPTS.exists(i => before.get(i).phase == ISSUE_UNKNOWN and after.get(i).phase != ISSUE_UNKNOWN)

  pure def ghostAfter(g0: Ghost, kind: str, before: int -> Reg, after: int -> Reg): Ghost = {
    ...g0,
    lastKind: kind,
    unknownLeftBy: if (leftUnknown(before, after)) kind else "",
  }

  action commit(kind: str, r: int -> Reg, at: int -> Att, runners: int -> Runner,
                inVm: int -> bool, ghost: Ghost): bool = all {
    regs' = r,
    atts' = at,
    gh' = runners,
    vmRunner' = inVm,
    ghUp' = ghUp,
    g' = ghostAfter(ghost, kind, regs, r),
  }

  // ---- init ---------------------------------------------------------------------------------

  action init = all {
    regs' = ATTEMPTS.mapBy(_ => { phase: UNREQUESTED, runnerId: NONE, leaked: false, dereg: NO_DEREG, deregFor: NONE,
      busyFor: NONE }),
    atts' = ATTEMPTS.mapBy(_ => { retire: NOT_RETIRING, vm: true, retirements: 0, launchUnknown: false }),
    gh' = RUNNER_IDS.mapBy(id =>
      if (id == FOREIGN) { present: true, name: FOREIGN, online: true, busy: false, jobDone: false }
      else { present: false, name: id, online: false, busy: false, jobDone: false }),
    vmRunner' = ATTEMPTS.mapBy(_ => false),
    ghUp' = true,
    g' = {
      lastKind: "init", unknownLeftBy: "", mints: ATTEMPTS.mapBy(_ => 0), unknownNames: Set(),
      remintAfterUnknown: false, deletedUnjournaled: false, stoppedWhileRegistered: false,
      busyRetirements: Set(), confirmedRetirements: Set(), deliveries: ATTEMPTS.mapBy(_ => 0),
      deliveredWhileLeaked: false, seenOnline: Set(),
    },
  }

  // ---- registration and mint (P9, P10) ------------------------------------------------------

  /// `RequestRegistration{attempt_id, runner_name}` once the attempt is activated.
  action requestRegistration(i: int): bool = all {
    regs.get(i).phase == UNREQUESTED,
    atts.get(i).vm,
    atts.get(i).retire == NOT_RETIRING,
    commit("command", regs.set(i, { ...regs.get(i), phase: REQUESTED }), atts, gh, vmRunner, g),
  }

  /// The mint's ghost: one more `generatejitconfig` call for this name.
  pure def minted(g0: Ghost, i: int): Ghost = {
    ...g0,
    mints: g0.mints.set(i, g0.mints.get(i) + 1),
    remintAfterUnknown: g0.remintAfterUnknown or g0.unknownNames.contains(i),
  }

  pure def registered(runners: int -> Runner, i: int): int -> Runner =
    runners.set(i, { present: true, name: i, online: false, busy: false, jobDone: false })

  /// `generatejitconfig` answered: `RecordIssued{runner_id}`.
  action mintAnswered(i: int): bool = all {
    ghUp,
    regs.get(i).phase == REQUESTED,
    commit("observation",
      regs.set(i, { ...regs.get(i), phase: ISSUED, runnerId: i }),
      atts, registered(gh, i), vmRunner, minted(g, i)),
  }

  /// The mint's reply was lost: GitHub may or may not have registered the runner;
  /// `RecordIssueUnknown`. Never retried with the same name.
  action mintLost(i: int, applied: bool): bool = all {
    regs.get(i).phase == REQUESTED,
    commit("command",
      regs.set(i, { ...regs.get(i), phase: ISSUE_UNKNOWN }),
      atts, if (applied and ghUp) registered(gh, i) else gh, vmRunner,
      { ...minted(g, i), unknownNames: g.unknownNames.union(Set(i)) }),
  }

  /// A definite refusal of the mint (nothing was created): `RecordAbsent`.
  action mintRefused(i: int): bool = all {
    ghUp,
    regs.get(i).phase == REQUESTED,
    commit("observation", regs.set(i, { ...regs.get(i), phase: ABSENT }), atts, gh, vmRunner,
      minted(g, i)),
  }

  /// P10: the reaper lists runners by the unknown issue's name: `RecordFoundByName{runner_id}`
  /// (journaled, presumed leaked, then deregistered) or `RecordAbsent`.
  action reaperByName(i: int): bool = {
    val r = regs.get(i)
    val found = RUNNER_IDS.exists(id => gh.get(id).present and gh.get(id).name == i)
    all {
      ghUp,
      r.phase == ISSUE_UNKNOWN,
      commit("observation",
        regs.set(i,
          if (found) { ...r, phase: ISSUED, runnerId: i, leaked: true }
          else { ...r, phase: ABSENT }),
        atts, gh, vmRunner, g),
    }
  }

  // ---- delivery and confirmation (P9, P13) --------------------------------------------------

  pure def canDeliver(r: Reg, at: Att, delivered: int): bool = and {
    r.phase == ISSUED,
    not(r.leaked),
    at.retire == NOT_RETIRING,
    at.vm,
    delivered == 0,
  }

  /// `LaunchRunner{handle}` succeeded: the runner lives in the VM; `RecordDelivered`.
  action deliver(i: int): bool = all {
    canDeliver(regs.get(i), atts.get(i), g.deliveries.get(i)),
    commit("observation",
      regs.set(i, { ...regs.get(i), phase: DELIVERED }),
      atts, gh, vmRunner.set(i, true),
      { ...g, deliveries: g.deliveries.set(i, g.deliveries.get(i) + 1),
        deliveredWhileLeaked: g.deliveredWhileLeaked or regs.get(i).leaked }),
  }

  /// The launch's outcome is Unknown (`RunnerLaunchUnknown` on the attempt): the runner may or
  /// may not live in the VM; the registration stays Issued until P13 presumes it leaked.
  action deliverUnknown(i: int, applied: bool): bool = all {
    canDeliver(regs.get(i), atts.get(i), g.deliveries.get(i)),
    commit("command", regs,
      atts.set(i, { ...atts.get(i), launchUnknown: true }), gh, vmRunner.set(i, applied),
      { ...g, deliveries: g.deliveries.set(i, g.deliveries.get(i) + 1),
        deliveredWhileLeaked: g.deliveredWhileLeaked or regs.get(i).leaked }),
  }

  /// The world: a runner in a running VM connects to GitHub.
  action runnerConnects(i: int): bool = all {
    vmRunner.get(i),
    atts.get(i).vm,
    gh.get(i).present,
    not(gh.get(i).online),
    commit("world", regs, atts, gh.set(i, { ...gh.get(i), online: true }), vmRunner, g),
  }

  /// The confirmation (ADR 0038): the issued runner id is online; `RecordConnected{runner_id}`.
  action confirmOnline(i: int): bool = {
    val r = regs.get(i)
    all {
      ghUp,
      r.phase == DELIVERED,
      gh.get(r.runnerId).present,
      gh.get(r.runnerId).online,
      commit("observation", regs.set(i, { ...r, phase: CONNECTED }), atts, gh, vmRunner,
        { ...g, seenOnline: g.seenOnline.union(Set(r.runnerId)) }),
    }
  }

  /// P13: `PresumeLeaked{reason}` after a launch of unknown outcome, or when a delivered runner
  /// is not seen online by the deadline (taken at any time: the deadline is not modelled).
  action presumeLeaked(i: int): bool = {
    val r = regs.get(i)
    all {
      Set(ISSUED, DELIVERED, CONNECTED, CONSUMED, COMPLETED).contains(r.phase),
      not(r.leaked),
      atts.get(i).launchUnknown or r.phase == DELIVERED,
      commit("command", regs.set(i, { ...r, leaked: true }), atts, gh, vmRunner, g),
    }
  }

  // ---- jobs (P12) ---------------------------------------------------------------------------

  /// GitHub gives an online idle runner its one job; `JobStarted` is recorded (`RecordConsumed`).
  action jobStarts(i: int): bool = {
    val r = regs.get(i)
    all {
      gh.get(i).present,
      gh.get(i).online,
      not(gh.get(i).busy),
      not(gh.get(i).jobDone),
      commit("observation",
        regs.set(i,
          if (Set(ISSUED, DELIVERED, CONNECTED).contains(r.phase)) { ...r, phase: CONSUMED }
          else r),
        atts, gh.set(i, { ...gh.get(i), busy: true }), vmRunner, g),
    }
  }

  /// The job completes; the service removes the ephemeral runner; `RecordCompleted{result}`.
  action jobCompletes(i: int): bool = {
    val r = regs.get(i)
    all {
      gh.get(i).busy,
      commit("observation",
        regs.set(i, { ...r, phase: if (r.phase == CONSUMED) COMPLETED else r.phase }),
        atts,
        gh.set(i, { ...gh.get(i), present: false, online: false, busy: false, jobDone: true }),
        vmRunner, g),
    }
  }

  // ---- retirement and deregistration (P11, P13, P14) ----------------------------------------

  /// `RequestRetirement` on the attempt (never while its runner is busy).
  action requestRetirement(i: int): bool = {
    val at = atts.get(i)
    all {
      at.retire == NOT_RETIRING,
      at.vm,
      regs.get(i).phase != CONSUMED,
      commit("command", regs,
        atts.set(i, { ...at, retire: RETIRE_REQUESTED, retirements: at.retirements + 1 }),
        gh, vmRunner, g),
    }
  }

  /// The automatic stop of a quarantined VM (KER-04F) asks for the deregistration first, as a
  /// retirement does, and P11 serves it the same way (ADR 0076: the attempt's
  /// `StopDeregistrationRequested` is its key). Unlike a retirement it is asked whatever the
  /// registration's phase: a quarantine does not wait for the job to end, and a job the attempt
  /// never saw bound may be running; GitHub then answers busy, the stop is withdrawn and the VM
  /// keeps running (its quarantine holds its capacity anyway). While GitHub cannot be reached
  /// nothing answers, and the stop waits. A quarantine during a pending retirement re-keys it:
  /// the attempt's `AttemptQuarantined` clears its `RetirementRequested` and its
  /// `StopDeregistrationRequested` is the new key (DEC-06F), so a busy answer to the old
  /// retirement's request does not withdraw the stop, and a removal still in flight for it
  /// serves the stop when it answers (`quarantineDuringPendingRetirementTest`). Only a confirmed
  /// retirement is not re-keyed: P11 has already answered it (`Retiring` in the attempt).
  action requestQuarantineStop(i: int): bool = {
    val at = atts.get(i)
    all {
      at.retire != RETIRE_CONFIRMED,
      at.vm,
      commit("command", regs,
        atts.set(i, { ...at, retire: RETIRE_REQUESTED, retirements: at.retirements + 1 }),
        gh, vmRunner, g),
    }
  }

  /// `RequestDeregistration{reason}`: for a retirement (P11), or for a leaked runner (P13, P14);
  /// only with a journaled runner id (I19).
  action requestDeregistration(i: int): bool = {
    val r = regs.get(i)
    val forRetirement = atts.get(i).retire == RETIRE_REQUESTED and stepOf(r, atts.get(i)) == DEREGISTER
    all {
      holdsRunner(r),
      r.dereg == NO_DEREG,
      forRetirement or r.leaked,
      commit("command",
        regs.set(i, { ...r, dereg: DEREG_REQUESTED,
          deregFor: if (forRetirement) atts.get(i).retirements else NONE }),
        atts, gh, vmRunner, g),
    }
  }

  /// The removal call by the journaled id: 204 or 404 deregisters; busy refuses; a lost reply
  /// leaves the request pending (it is retried by id, which is idempotent).
  action deregisterCall(i: int, replyLost: bool): bool = {
    val r = regs.get(i)
    val id = r.runnerId
    val runner = gh.get(id)
    val busy = runner.present and runner.busy
    val after = if (busy) gh else gh.set(id, { ...runner, present: false, online: false })
    val at = atts.get(i)
    all {
      ghUp,
      r.dereg == DEREG_REQUESTED,
      commit("observation",
        if (replyLost) regs
        else if (busy)
          regs.set(i, { ...r, dereg: NO_DEREG, deregFor: NONE,
            busyFor: if (r.deregFor != NONE) r.deregFor else r.busyFor })
        else regs.set(i, { ...r, phase: DEREGISTERED, dereg: NO_DEREG, deregFor: NONE }),
        atts, after, vmRunner,
        {
          ...g,
          deletedUnjournaled: g.deletedUnjournaled
            or (not(busy) and runner.present and not(ATTEMPTS.exists(j => regs.get(j).runnerId == id))),
          busyRetirements:
            if (busy and not(replyLost) and r.deregFor != NONE)
              g.busyRetirements.union(Set((i, r.deregFor)))
            else g.busyRetirements,
        }),
    }
  }

  /// P11: `ConfirmRetirement` when the registration allows the stop.
  action confirmRetirement(i: int): bool = {
    val at = atts.get(i)
    all {
      at.retire == RETIRE_REQUESTED,
      stepOf(regs.get(i), at) == CONFIRM,
      commit("command", regs, atts.set(i, { ...at, retire: RETIRE_CONFIRMED }), gh, vmRunner,
        { ...g, confirmedRetirements: g.confirmedRetirements.union(Set((i, at.retirements))) }),
    }
  }

  /// P11: `WithdrawRetirement` after a busy answer (I21).
  action withdrawRetirement(i: int): bool = {
    val at = atts.get(i)
    all {
      at.retire == RETIRE_REQUESTED,
      stepOf(regs.get(i), at) == WITHDRAW,
      commit("command", regs, atts.set(i, { ...at, retire: NOT_RETIRING }), gh, vmRunner, g),
    }
  }

  /// The attempt's cleanup stops the VM: its runner, if any, dies and goes offline.
  action stopVm(i: int): bool = {
    val at = atts.get(i)
    val stillListed = RUNNER_IDS.exists(id => gh.get(id).present and gh.get(id).name == i)
    all {
      at.retire == RETIRE_CONFIRMED,
      at.vm,
      commit("observation", regs, atts.set(i, { ...at, vm: false }),
        if (gh.get(i).present) gh.set(i, { ...gh.get(i), online: false }) else gh,
        vmRunner.set(i, false),
        { ...g, stoppedWhileRegistered: g.stoppedWhileRegistered or (vmRunner.get(i) and stillListed) }),
    }
  }

  /// GitHub becomes unreachable, or reachable again.
  action ghToggles = all {
    regs' = regs,
    atts' = atts,
    gh' = gh,
    vmRunner' = vmRunner,
    ghUp' = not(ghUp),
    g' = ghostAfter(g, "world", regs, regs),
  }

  // ---- step relations -----------------------------------------------------------------------

  /// Every stimulus, for either attempt.
  action step = {
    nondet i = ATTEMPTS.oneOf()
    nondet flag = Set(true, false).oneOf()
    any {
      requestRegistration(i),
      mintAnswered(i),
      mintLost(i, flag),
      mintRefused(i),
      reaperByName(i),
      deliver(i),
      deliverUnknown(i, flag),
      runnerConnects(i),
      confirmOnline(i),
      presumeLeaked(i),
      jobStarts(i),
      jobCompletes(i),
      requestRetirement(i),
      requestQuarantineStop(i),
      requestDeregistration(i),
      deregisterCall(i, flag),
      confirmRetirement(i),
      withdrawRetirement(i),
      stopVm(i),
      ghToggles,
    }
  }

  /// The removal race, biased: one attempt, GitHub up and every mint answered, so a walk
  /// delivers and connects a runner and then meets its retirement, a job that starts while the
  /// deregistration is in flight, busy answers, lost replies, a launch of unknown outcome and
  /// the leak path. A uniform walk of `step` spends most of its depth on the other attempt,
  /// outages and unknown issues.
  action raceStep = {
    nondet flag = Set(true, false).oneOf()
    any {
      requestRegistration(1),
      mintAnswered(1),
      deliver(1),
      deliverUnknown(1, flag),
      runnerConnects(1),
      confirmOnline(1),
      presumeLeaked(1),
      jobStarts(1),
      jobCompletes(1),
      requestRetirement(1),
      requestQuarantineStop(1),
      requestDeregistration(1),
      deregisterCall(1, flag),
      confirmRetirement(1),
      withdrawRetirement(1),
      stopVm(1),
    }
  }

  // ---- invariants ---------------------------------------------------------------------------

  /// I19: a runner is removed only by an id a registration journaled; the foreign runner of our
  /// name shape stays.
  val i19OnlyJournaledIdsDeregistered = and {
    not(g.deletedUnjournaled),
    gh.get(FOREIGN).present,
  }

  /// I19: a runner name is minted at most once, ever.
  val i19NameIssuedOnce = ATTEMPTS.forall(i => g.mints.get(i) <= 1)

  /// noRemintAfterUnknown: an issue of unknown outcome is never retried with the same name.
  val noRemintAfterUnknown = not(g.remintAfterUnknown)

  /// I20: once its VM is stopped, a registration is settled or on the reaper's list.
  val i20SettledOrOnReaperList =
    ATTEMPTS.forall(i => not(atts.get(i).vm) implies (settled(regs.get(i)) or onReaperList(regs.get(i))))

  /// I21, deregisterBeforeStop: no VM is stopped while its runner is still on GitHub's list.
  val deregisterBeforeStop = not(g.stoppedWhileRegistered)

  /// I21: a busy answer cancels that retirement: it is never confirmed.
  val i21BusyCancelsRetirement = g.busyRetirements.intersect(g.confirmedRetirements) == Set()

  /// I22 (registration half): a JIT config is minted and launched at most once per
  /// registration, never once it is presumed leaked, and the online confirmation names the
  /// issued runner id.
  val i22IssuedAndDeliveredOnce = and {
    ATTEMPTS.forall(i => g.deliveries.get(i) <= 1),
    not(g.deliveredWhileLeaked),
    ATTEMPTS.forall(i => regs.get(i).phase == CONNECTED implies g.seenOnline.contains(regs.get(i).runnerId)),
  }

  /// An issue of unknown outcome leaves IssueUnknown only on an observation (the reaper's
  /// listing), never on a command or a timeout.
  val issueUnknownLeavesOnlyOnObservation = g.unknownLeftBy == "" or g.unknownLeftBy == "observation"

  // Every invariant of the model, so a new one cannot be left out of a stage.
  val allInvariants = and {
    i19OnlyJournaledIdsDeregistered,
    i19NameIssuedOnce,
    noRemintAfterUnknown,
    i20SettledOrOnReaperList,
    deregisterBeforeStop,
    i21BusyCancelsRetirement,
    i22IssuedAndDeliveredOnce,
    issueUnknownLeavesOnlyOnObservation,
  }

  // ---- scenarios ----------------------------------------------------------------------------

  /// Registration, mint and delivery of attempt `i`, its runner online and confirmed.
  action toConnected(i: int): bool =
    requestRegistration(i).then(mintAnswered(i)).then(deliver(i)).then(runnerConnects(i))
      .then(confirmOnline(i))

  // The happy path of dm §2.1: connected, one job, completed, deregistered (404: the service
  // removed the ephemeral runner), then the VM is stopped.
  run happyPathTest = {
    init.then(toConnected(1))
      .expect(regs.get(1).phase == CONNECTED and regs.get(1).runnerId == 1)
      .then(jobStarts(1)).expect(regs.get(1).phase == CONSUMED)
      .then(jobCompletes(1)).expect(regs.get(1).phase == COMPLETED and not(gh.get(1).present))
      .then(requestRetirement(1)).expect(stepOf(regs.get(1), atts.get(1)) == DEREGISTER)
      .then(requestDeregistration(1)).then(deregisterCall(1, false))
      .expect(regs.get(1).phase == DEREGISTERED)
      .then(confirmRetirement(1)).then(stopVm(1))
      .expect(not(atts.get(1).vm) and allInvariants)
  }

  // I21: an idle registered runner is deregistered (204) before its VM is stopped.
  run idleRunnerDeregisteredBeforeStopTest = {
    init.then(toConnected(1)).then(requestRetirement(1))
      .expect(stepOf(regs.get(1), atts.get(1)) == DEREGISTER)
      .then(requestDeregistration(1)).expect(stepOf(regs.get(1), atts.get(1)) == WAIT)
      .then(deregisterCall(1, false))
      .expect(regs.get(1).phase == DEREGISTERED and not(gh.get(1).present))
      .then(confirmRetirement(1)).then(stopVm(1))
      .expect(deregisterBeforeStop and allInvariants)
  }

  // I21, the removal race: a job starts while the deregistration is in flight; GitHub answers
  // busy, the retirement is withdrawn and never confirmed; after the job the runner is
  // deregistered (404) and the VM stopped.
  run busyAnswerWithdrawsRetirementTest = {
    init.then(toConnected(1)).then(requestRetirement(1)).then(requestDeregistration(1))
      .then(jobStarts(1)).expect(regs.get(1).phase == CONSUMED)
      .then(deregisterCall(1, false))
      .expect(regs.get(1).busyFor == 1 and stepOf(regs.get(1), atts.get(1)) == WITHDRAW)
      .then(withdrawRetirement(1)).expect(atts.get(1).retire == NOT_RETIRING)
      .then(jobCompletes(1)).expect(regs.get(1).phase == COMPLETED and regs.get(1).dereg == NO_DEREG)
      .then(requestRetirement(1)).expect(stepOf(regs.get(1), atts.get(1)) == DEREGISTER)
      .then(requestDeregistration(1)).then(deregisterCall(1, false))
      .expect(regs.get(1).phase == DEREGISTERED)
      .then(confirmRetirement(1)).then(stopVm(1))
      .expect(i21BusyCancelsRetirement and allInvariants)
  }

  // A counterexample of the first draft (simulation of raceStep): job evidence after a busy
  // answer let the same retirement deregister the finished runner and stop the VM. A busy answer
  // stays final for its retirement; only a new retirement deregisters.
  run busyRetirementStaysCancelledAfterTheJobTest = {
    init.then(toConnected(1)).then(requestRetirement(1)).then(requestDeregistration(1))
      .then(jobStarts(1)).then(deregisterCall(1, false)).then(jobCompletes(1))
      .expect(atts.get(1).retire == RETIRE_REQUESTED and stepOf(regs.get(1), atts.get(1)) == WITHDRAW)
      .then(withdrawRetirement(1)).expect(i21BusyCancelsRetirement and allInvariants)
  }

  // A lost removal reply leaves the request pending; the retry by id answers 404.
  run lostRemovalReplyIsRetriedTest = {
    init.then(toConnected(1)).then(requestRetirement(1)).then(requestDeregistration(1))
      .then(deregisterCall(1, true))
      .expect(regs.get(1).dereg == DEREG_REQUESTED and not(gh.get(1).present))
      .expect(stepOf(regs.get(1), atts.get(1)) == WAIT)
      .then(deregisterCall(1, false)).expect(regs.get(1).phase == DEREGISTERED)
      .then(confirmRetirement(1)).then(stopVm(1)).expect(allInvariants)
  }

  // P10: an issue of unknown outcome is never minted again; the reaper finds the runner by
  // name, journals its id and deregisters it; the attempt, which never got the config, stops.
  run issueUnknownFoundByNameTest = {
    init.then(requestRegistration(1)).then(mintLost(1, true))
      .expect(regs.get(1).phase == ISSUE_UNKNOWN and gh.get(1).present)
      .expect(not(canDeliver(regs.get(1), atts.get(1), g.deliveries.get(1))) and not(holdsRunner(regs.get(1))))
      .then(requestRetirement(1)).expect(stepOf(regs.get(1), atts.get(1)) == CONFIRM)
      .then(confirmRetirement(1)).then(stopVm(1))
      .expect(i20SettledOrOnReaperList and deregisterBeforeStop)
      .then(reaperByName(1))
      .expect(regs.get(1).phase == ISSUED and regs.get(1).leaked and regs.get(1).runnerId == 1)
      .expect(issueUnknownLeavesOnlyOnObservation)
      .then(requestDeregistration(1)).then(deregisterCall(1, false))
      .expect(regs.get(1).phase == DEREGISTERED and noRemintAfterUnknown and allInvariants)
  }

  // P10: the reaper finds nothing by name: the registration is absent.
  run issueUnknownAbsentTest = {
    init.then(requestRegistration(1)).then(mintLost(1, false))
      .then(reaperByName(1))
      .expect(regs.get(1).phase == ABSENT and g.mints.get(1) == 1 and allInvariants)
  }

  // While GitHub is down the unknown issue stays on the reaper's list.
  run issueUnknownWaitsForGitHubTest = {
    init.then(requestRegistration(1)).then(ghToggles).then(mintLost(1, false))
      .expect(not(ghUp) and regs.get(1).phase == ISSUE_UNKNOWN)
      .then(ghToggles).then(reaperByName(1)).expect(regs.get(1).phase == ABSENT)
  }

  // P13: a launch of unknown outcome is presumed leaked, never launched again, deregistered
  // before the VM is stopped.
  run deliveryUnknownPresumedLeakedTest = {
    init.then(requestRegistration(1)).then(mintAnswered(1)).then(deliverUnknown(1, true))
      .expect(not(canDeliver(regs.get(1), atts.get(1), g.deliveries.get(1))))
      .then(presumeLeaked(1)).expect(regs.get(1).leaked and onReaperList(regs.get(1)))
      .then(runnerConnects(1))
      .then(requestRetirement(1)).expect(stepOf(regs.get(1), atts.get(1)) == DEREGISTER)
      .then(requestDeregistration(1)).then(deregisterCall(1, false))
      .then(confirmRetirement(1)).then(stopVm(1))
      .expect(regs.get(1).phase == DEREGISTERED and allInvariants)
  }

  // ADR 0038: a delivered runner not seen online is presumed leaked and deregistered by its id.
  run notSeenOnlinePresumedLeakedTest = {
    init.then(requestRegistration(1)).then(mintAnswered(1)).then(deliver(1))
      .expect(not(gh.get(1).online))
      .then(presumeLeaked(1)).then(requestDeregistration(1)).then(deregisterCall(1, false))
      .expect(regs.get(1).phase == DEREGISTERED and allInvariants)
  }

  // ADR 0029: the reaper deregisters only journaled ids; the foreign runner of our name shape
  // stays on GitHub's list.
  run reaperSparesTheForeignRunnerTest = {
    init.then(toConnected(1)).then(requestRegistration(2)).then(mintAnswered(2))
      .then(deliver(2)).then(presumeLeaked(2))
      .then(requestDeregistration(2)).then(deregisterCall(2, false))
      .expect(gh.get(FOREIGN).present and regs.get(2).phase == DEREGISTERED)
      .expect(i19OnlyJournaledIdsDeregistered and allInvariants)
  }

  // I19: one name, one mint: no second registration for an attempt.
  run nameIssuedOnceTest = {
    init.then(requestRegistration(1)).then(mintAnswered(1))
      .expect(regs.get(1).phase == ISSUED)
      .expect(g.mints.get(1) == 1 and i19NameIssuedOnce)
  }

  /// Mutant 21 (verification §9), a negative control kept out of every step relation: P11
  /// confirms a retirement without asking the registration, so the VM is stopped before its
  /// runner is deregistered.
  action confirmRetirementStopFirst(i: int): bool = {
    val at = atts.get(i)
    all {
      at.retire == RETIRE_REQUESTED,
      commit("command", regs, atts.set(i, { ...at, retire: RETIRE_CONFIRMED }), gh, vmRunner,
        { ...g, confirmedRetirements: g.confirmedRetirements.union(Set((i, at.retirements))) }),
    }
  }

  // Mutant 21 violates deregisterBeforeStop: the model's invariant kills it.
  run mutant21StopBeforeDeregisterViolatesTest = {
    init.then(toConnected(1)).then(requestRetirement(1))
      .then(confirmRetirementStopFirst(1)).then(stopVm(1))
      .expect(not(deregisterBeforeStop) and not(allInvariants))
  }

  // ADR 0076: a quarantined VM's idle registered runner is deregistered before the stop.
  run quarantineStopDeregistersFirstTest = {
    init.then(toConnected(1)).then(requestQuarantineStop(1))
      .expect(stepOf(regs.get(1), atts.get(1)) == DEREGISTER)
      .then(requestDeregistration(1)).then(deregisterCall(1, false))
      .then(confirmRetirement(1)).then(stopVm(1))
      .expect(not(atts.get(1).vm) and deregisterBeforeStop and allInvariants)
  }

  // ADR 0076: a job runs on the quarantined VM's runner: GitHub answers busy, the stop is
  // withdrawn, and the VM keeps running under its job.
  run quarantineStopBusyKeepsRunningTest = {
    init.then(toConnected(1)).then(jobStarts(1)).then(requestQuarantineStop(1))
      .then(requestDeregistration(1)).then(deregisterCall(1, false))
      .expect(stepOf(regs.get(1), atts.get(1)) == WITHDRAW)
      .then(withdrawRetirement(1))
      .expect(atts.get(1).vm and atts.get(1).retire == NOT_RETIRING and allInvariants)
  }

  // ADR 0076: while GitHub cannot be reached the removal is never answered and the quarantined
  // VM keeps running; once it answers, the stop follows the deregistration.
  run quarantineStopWaitsForGitHubTest = {
    init.then(toConnected(1)).then(ghToggles).then(requestQuarantineStop(1))
      .then(requestDeregistration(1))
      .expect(not(ghUp) and stepOf(regs.get(1), atts.get(1)) == WAIT and atts.get(1).vm)
      .then(ghToggles).then(deregisterCall(1, false))
      .then(confirmRetirement(1)).then(stopVm(1))
      .expect(not(atts.get(1).vm) and allInvariants)
  }

  // DEC-06F: a quarantine while a retirement's removal is in flight re-keys the request; the
  // removal answers 204 and the stop follows it.
  run quarantineDuringPendingRetirementTest = {
    init.then(toConnected(1)).then(requestRetirement(1)).then(requestDeregistration(1))
      .expect(regs.get(1).deregFor == 1)
      .then(requestQuarantineStop(1))
      .expect(atts.get(1).retirements == 2 and stepOf(regs.get(1), atts.get(1)) == WAIT)
      .then(deregisterCall(1, false)).expect(stepOf(regs.get(1), atts.get(1)) == CONFIRM)
      .then(confirmRetirement(1)).then(stopVm(1))
      .expect(not(atts.get(1).vm) and deregisterBeforeStop and allInvariants)
  }

  // ...and when a job started under the old retirement's removal, its busy answer belongs to the
  // old key: the stop asks again, is answered busy itself, and only then is withdrawn.
  run quarantineDuringPendingRetirementBusyTest = {
    init.then(toConnected(1)).then(requestRetirement(1)).then(requestDeregistration(1))
      .then(jobStarts(1)).then(requestQuarantineStop(1))
      .then(deregisterCall(1, false))
      .expect(regs.get(1).busyFor == 1 and stepOf(regs.get(1), atts.get(1)) == DEREGISTER)
      .then(requestDeregistration(1)).then(deregisterCall(1, false))
      .expect(stepOf(regs.get(1), atts.get(1)) == WITHDRAW)
      .then(withdrawRetirement(1))
      .expect(atts.get(1).vm and atts.get(1).retire == NOT_RETIRING and allInvariants)
  }

  /// KER-04F's quarantine stop before ADR 0076, a negative control kept out of every step
  /// relation: the stop of a quarantined VM went ahead without asking the registration.
  action quarantineStopAtOnce(i: int): bool = {
    val at = atts.get(i)
    all {
      at.retire == NOT_RETIRING,
      at.vm,
      commit("command", regs,
        atts.set(i, { ...at, retire: RETIRE_CONFIRMED, retirements: at.retirements + 1 }),
        gh, vmRunner, g),
    }
  }

  // ...which `deregisterBeforeStop` flags with an idle runner registered in the VM.
  run quarantineStopAtOnceViolatesTest = {
    init.then(toConnected(1)).then(quarantineStopAtOnce(1)).then(stopVm(1))
      .expect(not(deregisterBeforeStop) and not(allInvariants))
  }
}
