// -*- mode: Bluespec; -*-
/**
 * The link model (KER-02): the CP↔DP protocol of plan §3.2 between one control plane (`cp`)
 * and one data plane (`dp`) over a network (`net`) that loses, duplicates and reorders frames
 * in both directions. The Rust published language is `mayfly_protocol`; the conformance driver
 * (DP-02, LNK-01) maps each action below onto the `inproc` and `uds` transports and the data
 * plane's executor (KER-08).
 *
 * What it states:
 *
 * - the handshake: `Hello{floor, major}` → `Welcome{epoch, mode, major}` or a refusal of an
 *   unsupported major; the control plane mints a fresh run epoch above both its journal and the
 *   floor for every hello, and the data plane enters a session only on a welcome above its
 *   floor, raising the floor first (I17, ADR 0026);
 * - delivery: the relay re-sends every directive that has no definite answer, stamped with the
 *   current session's epoch, as often as it likes (at-least-once); the network keeps a copy of a
 *   delivered frame (a duplicate), drops frames, and has no order (a set);
 * - fencing: the data plane executes only frames of its current session (I09); a frame of an
 *   older session is answered `Unknown{stale_epoch}`, never refused, because a definite refusal
 *   would be false when an earlier delivery of the same directive took effect before its reply
 *   was lost (`staleFrameIsNeverRefusedTest`, ADR 0042); a frame of a session it has not entered,
 *   or of another major, is discarded unanswered;
 * - per-epoch idempotency: a directive id executes at most once per epoch, and a repeat answers
 *   the recorded outcome; across epochs a directive may run again, and the effect's own
 *   idempotency by target name (KER-08, the lifecycle model's world) makes that a re-observation;
 * - safe mode: a session welcomed in `safe` mode refuses the held kinds (I29);
 * - the control plane's inbox applies an answer only if its epoch is not older than the newest
 *   it has applied (I18), and a definite answer is final for the directive.
 *
 * Two directives stand for the kinds: id 1 is a `CloneVm` (held in safe mode, needs a resource
 * fit), id 2 a `ReportInventory` (never held). Ghost state (`g`) records history for the
 * invariants and never feeds a guard.
 */
module link {
  // ---- constants ----------------------------------------------------------------------------

  pure val NONE = 0
  /// Directive ids the control plane may journal.
  pure val IDS = Set(1, 2)
  pure val CLONE_VM = "CloneVm"
  pure val REPORT_INVENTORY = "ReportInventory"
  /// The majors the control plane speaks, and the majors a data plane may offer (2 stands for
  /// a newer data plane the control plane cannot speak to).
  pure val CP_MAJORS = Set(1)
  pure val DP_MAJORS = Set(1, 2)
  pure val NORMAL = "normal"
  pure val SAFE = "safe"
  pure val MODES = Set(NORMAL, SAFE)

  // Frame tags.
  pure val HELLO = "hello"
  pure val WELCOME = "welcome"
  pure val HANDSHAKE_REFUSED = "handshakeRefused"
  pure val DIRECTIVE = "directive"
  pure val ANSWER = "answer"

  // Answer outcomes (`DirectiveOutcome` of the protocol).
  pure val APPLIED = "applied"
  pure val REFUSED = "refused"
  pure val UNKNOWN = "unknown"

  /// How the data plane's execution of a current, new, unheld directive ends: applied; refused
  /// definitely (it does not fit, I33: nothing happened); or Unknown (a deadline or a lost
  /// process) with the effect applied or not.
  pure val EFFECTS = Set("apply", "refuse", "unknownApplied", "unknownNotApplied")

  pure def kindOf(id: int): str = if (id == 1) CLONE_VM else REPORT_INVENTORY

  /// I29: the kinds safe mode holds (create, start, stop, delete, pull, evict, materialise and
  /// the launch that takes a secret); `mayfly_protocol::DirectiveKind::held_in_safe_mode`.
  pure def held(kind: str): bool = kind == CLONE_VM

  pure def max(x: int, y: int): int = if (x > y) x else y

  // ---- state --------------------------------------------------------------------------------

  /// One frame. Unused fields are 0 or "".
  type Msg = {
    tag: str,
    // HELLO: the floor it reports; WELCOME, DIRECTIVE, ANSWER: the epoch it carries
    epoch: int,
    // DIRECTIVE, ANSWER: the directive
    id: int,
    // the major the frame is encoded with
    major: int,
    // WELCOME: the session's mode
    mode: str,
    // ANSWER: APPLIED | REFUSED | UNKNOWN
    outcome: str,
    // ANSWER: why ("", "safe_mode", "insufficient_resources", "stale_epoch", "deadline")
    reason: str,
  }

  /// What the control plane applied for one directive.
  type Belief = { outcome: str, epoch: int }

  type Cp = {
    // the highest run epoch minted (the journal's HostRun)
    minted: int,
    // the current session's epoch and major, NONE before the first welcome
    session: int,
    major: int,
    issued: Set[int],
    // the newest epoch of an answer the inbox applied (I18)
    inboxHigh: int,
    beliefs: int -> Belief,
  }

  type Dp = {
    // the epoch floor in TART_HOME: survives a restart
    floor: int,
    // the session; everything below is memory, lost on a restart
    epoch: int,
    major: int,
    mode: str,
    awaiting: bool,
    // outcomes recorded in this session, by directive id
    table: int -> str,
  }

  type Net = { toDp: Set[Msg], toCp: Set[Msg] }

  type Ghost = {
    lastAction: str,
    // (epoch, directive id) of every execution: the effect ran
    executions: Set[(int, int)],
    // an execution of an (epoch, id) that had already executed
    doubleExec: bool,
    // an execution of a frame older than the floor
    staleExec: bool,
    // an execution of a held kind in a safe session
    heldExec: bool,
    // an execution of a frame of another major than the session's
    wrongMajorExec: bool,
    // a session entered at an epoch not above every earlier one
    epochRegressed: bool,
    // an answer applied with an epoch older than the newest applied
    inboxRegressed: bool,
  }

  var cp: Cp
  var dp: Dp
  var net: Net
  var g: Ghost

  pure def frame(tag: str, epoch: int, id: int, major: int): Msg =
    { tag: tag, epoch: epoch, id: id, major: major, mode: "", outcome: "", reason: "" }

  pure def answer(epoch: int, id: int, major: int, outcome: str, reason: str): Msg =
    { ...frame(ANSWER, epoch, id, major), outcome: outcome, reason: reason }

  pure def final(b: Belief): bool = b.outcome == APPLIED or b.outcome == REFUSED

  pure def settled(c: Cp, id: int): bool = c.beliefs.keys().contains(id) and final(c.beliefs.get(id))

  // ---- init ---------------------------------------------------------------------------------

  action init = all {
    cp' = { minted: NONE, session: NONE, major: NONE, issued: Set(), inboxHigh: NONE, beliefs: Map() },
    dp' = { floor: NONE, epoch: NONE, major: NONE, mode: NORMAL, awaiting: false, table: Map() },
    net' = { toDp: Set(), toCp: Set() },
    g' = {
      lastAction: "init", executions: Set(), doubleExec: false, staleExec: false,
      heldExec: false, wrongMajorExec: false, epochRegressed: false, inboxRegressed: false,
    },
  }

  /// Takes `m` off a channel, or leaves a copy behind for a duplicate delivery.
  pure def take(s: Set[Msg], m: Msg, keep: bool): Set[Msg] = if (keep) s else s.exclude(Set(m))

  // ---- data plane ---------------------------------------------------------------------------

  /// `Hello` (a start, or a reconnect): the session ends, its table with it; the floor stays.
  action dpHelloWith(major: int): bool = all {
    DP_MAJORS.contains(major),
    dp' = { ...dp, epoch: NONE, major: NONE, awaiting: true, table: Map() },
    net' = { ...net, toCp: net.toCp.union(Set({ ...frame(HELLO, dp.floor, NONE, major) })) },
    cp' = cp,
    g' = { ...g, lastAction: "dpHello" },
  }

  /// The data plane process restarts: everything but the floor is lost.
  action dpRestart = all {
    dp' = { ...dp, epoch: NONE, major: NONE, mode: NORMAL, awaiting: false, table: Map() },
    cp' = cp,
    net' = net,
    g' = { ...g, lastAction: "dpRestart" },
  }

  /// A `Welcome` is entered only while a hello waits, only above the floor (strictly: a session
  /// is never re-entered, because its table is gone), and only in a major the data plane speaks.
  /// The floor is raised (and fsynced) before the session is used (I17, ADR 0026).
  action dpWelcomeWith(w: Msg, keep: bool): bool = all {
    net.toDp.contains(w),
    w.tag == WELCOME,
    val enter = dp.awaiting and w.epoch > dp.floor and DP_MAJORS.contains(w.major)
    all {
      dp' = if (enter) { floor: w.epoch, epoch: w.epoch, major: w.major, mode: w.mode, awaiting: false, table: Map() } else dp,
      net' = { ...net, toDp: take(net.toDp, w, keep) },
      cp' = cp,
      g' = { ...g, lastAction: "dpWelcome", epochRegressed: g.epochRegressed or (enter and w.epoch <= dp.floor) },
    },
  }

  /// The data plane takes a directive frame. `effect` is how a current, new, unheld execution
  /// ends.
  action dpDeliverWith(m: Msg, keep: bool, effect: str): bool = all {
    net.toDp.contains(m),
    m.tag == DIRECTIVE,
    EFFECTS.contains(effect),
    val rest = take(net.toDp, m, keep)
    def reply(outcome, reason) = net.toCp.union(Set(answer(dp.epoch, m.id, dp.major, outcome, reason)))
    if (dp.epoch == NONE or m.major != dp.major or m.epoch > dp.epoch) all {
      // no session, another major, or a session not entered yet: discarded unexecuted and
      // unanswered (the relay re-sends under the current session)
      net' = { ...net, toDp: rest },
      dp' = dp,
      cp' = cp,
      g' = { ...g, lastAction: "dpDiscard" },
    } else if (m.epoch < dp.epoch) all {
      // fenced (I09): an older session's frame is never executed and never refused
      net' = { toDp: rest, toCp: reply(UNKNOWN, "stale_epoch") },
      dp' = dp,
      cp' = cp,
      g' = { ...g, lastAction: "dpFence" },
    } else if (dp.table.keys().contains(m.id)) all {
      // a repeat in the same session answers the recorded outcome
      net' = { toDp: rest, toCp: reply(dp.table.get(m.id), "repeat") },
      dp' = dp,
      cp' = cp,
      g' = { ...g, lastAction: "dpRepeat" },
    } else if (dp.mode == SAFE and held(kindOf(m.id))) all {
      net' = { toDp: rest, toCp: reply(REFUSED, "safe_mode") },
      dp' = { ...dp, table: dp.table.put(m.id, REFUSED) },
      cp' = cp,
      g' = { ...g, lastAction: "dpHold" },
    } else {
      val runs = effect == "apply" or effect == "unknownApplied"
      val outcome = if (effect == "apply") APPLIED else if (effect == "refuse") REFUSED else UNKNOWN
      val reason = if (effect == "refuse") "insufficient_resources" else if (outcome == UNKNOWN) "deadline" else ""
      all {
        net' = { toDp: rest, toCp: reply(outcome, reason) },
        dp' = { ...dp, table: dp.table.put(m.id, outcome) },
        cp' = cp,
        g' = {
          ...g,
          lastAction: "dpExecute",
          executions: if (runs) g.executions.union(Set((m.epoch, m.id))) else g.executions,
          doubleExec: g.doubleExec or (runs and g.executions.contains((m.epoch, m.id))),
          staleExec: g.staleExec or (runs and m.epoch < dp.floor),
          heldExec: g.heldExec or (runs and dp.mode == SAFE and held(kindOf(m.id))),
          wrongMajorExec: g.wrongMajorExec or (runs and m.major != dp.major),
        },
      }
    },
  }

  // ---- control plane ------------------------------------------------------------------------

  /// A hello in a major the control plane speaks mints a fresh run epoch above its journal and
  /// the reported floor, and opens the session; any other major is refused (no epoch minted).
  action cpWelcomeWith(h: Msg, mode: str, keep: bool): bool = all {
    net.toCp.contains(h),
    h.tag == HELLO,
    MODES.contains(mode),
    val rest = take(net.toCp, h, keep)
    if (CP_MAJORS.contains(h.major)) {
      val e = max(cp.minted, h.epoch) + 1
      all {
        cp' = { ...cp, minted: e, session: e, major: h.major },
        net' = { toCp: rest, toDp: net.toDp.union(Set({ ...frame(WELCOME, e, NONE, h.major), mode: mode })) },
        dp' = dp,
        g' = { ...g, lastAction: "cpWelcome" },
      }
    } else all {
      cp' = cp,
      net' = { toCp: rest, toDp: net.toDp.union(Set(frame(HANDSHAKE_REFUSED, NONE, NONE, h.major))) },
      dp' = dp,
      g' = { ...g, lastAction: "cpRefuseHandshake" },
    },
  }

  /// `DirectiveIssued`: the intent is journaled before anything is sent.
  action cpIssueWith(id: int): bool = all {
    IDS.contains(id),
    not(cp.issued.contains(id)),
    cp' = { ...cp, issued: cp.issued.union(Set(id)) },
    dp' = dp,
    net' = net,
    g' = { ...g, lastAction: "cpIssue" },
  }

  /// The relay sends (again) a directive with no definite answer, stamped with the current
  /// session's epoch and major at send time.
  action relaySendWith(id: int): bool = all {
    cp.issued.contains(id),
    cp.session != NONE,
    not(settled(cp, id)),
    net' = { ...net, toDp: net.toDp.union(Set(frame(DIRECTIVE, cp.session, id, cp.major))) },
    cp' = cp,
    dp' = dp,
    g' = { ...g, lastAction: "relaySend" },
  }

  /// The inbox takes an answer: an older epoch than the newest applied is ignored (I18), and so
  /// is any answer for a directive that already has a definite one (`StaleObservationIgnored`).
  action cpReceiveWith(o: Msg, keep: bool): bool = all {
    net.toCp.contains(o),
    o.tag == ANSWER,
    val apply = o.epoch >= cp.inboxHigh and not(settled(cp, o.id))
    all {
      cp' = if (apply) { ...cp, inboxHigh: o.epoch, beliefs: cp.beliefs.put(o.id, { outcome: o.outcome, epoch: o.epoch }) } else cp,
      net' = { ...net, toCp: take(net.toCp, o, keep) },
      dp' = dp,
      g' = { ...g, lastAction: "cpReceive", inboxRegressed: g.inboxRegressed or (apply and o.epoch < cp.inboxHigh) },
    },
  }

  // ---- the network --------------------------------------------------------------------------

  action loseToDpWith(m: Msg): bool = all {
    net.toDp.contains(m),
    net' = { ...net, toDp: net.toDp.exclude(Set(m)) },
    cp' = cp, dp' = dp, g' = { ...g, lastAction: "lose" },
  }

  action loseToCpWith(m: Msg): bool = all {
    net.toCp.contains(m),
    net' = { ...net, toCp: net.toCp.exclude(Set(m)) },
    cp' = cp, dp' = dp, g' = { ...g, lastAction: "lose" },
  }

  /// A peer that speaks another major sends a directive frame (a newer control plane, a
  /// garbled frame): the copy carries major 2.
  action foreignMajorWith(m: Msg): bool = all {
    net.toDp.contains(m),
    m.tag == DIRECTIVE,
    net' = { ...net, toDp: net.toDp.union(Set({ ...m, major: 2 })) },
    cp' = cp, dp' = dp, g' = { ...g, lastAction: "foreignMajor" },
  }

  // ---- the relations ------------------------------------------------------------------------

  action dpHello = { nondet major = DP_MAJORS.oneOf() dpHelloWith(major) }
  action dpWelcome = { nondet w = net.toDp.oneOf() nondet keep = Set(true, false).oneOf() dpWelcomeWith(w, keep) }
  action dpDeliver = {
    nondet m = net.toDp.oneOf()
    nondet keep = Set(true, false).oneOf()
    nondet effect = EFFECTS.oneOf()
    dpDeliverWith(m, keep, effect)
  }
  action cpWelcome = {
    nondet h = net.toCp.oneOf()
    nondet mode = MODES.oneOf()
    nondet keep = Set(true, false).oneOf()
    cpWelcomeWith(h, mode, keep)
  }
  action cpIssue = { nondet id = IDS.oneOf() cpIssueWith(id) }
  action relaySend = { nondet id = IDS.oneOf() relaySendWith(id) }
  action cpReceive = { nondet o = net.toCp.oneOf() nondet keep = Set(true, false).oneOf() cpReceiveWith(o, keep) }
  action loseToDp = { nondet m = net.toDp.oneOf() loseToDpWith(m) }
  action loseToCp = { nondet m = net.toCp.oneOf() loseToCpWith(m) }
  action foreignMajor = { nondet m = net.toDp.oneOf() foreignMajorWith(m) }

  /// The full contract: every peer and network behaviour plan §3.2 allows.
  action step = any {
    dpHello,
    dpRestart,
    dpWelcome,
    dpDeliver,
    cpWelcome,
    cpIssue,
    relaySend,
    cpReceive,
    loseToDp,
    loseToCp,
    foreignMajor,
  }

  /// What the composition can do (verification §6): its control plane never sends another
  /// major, and its data plane offers the majors it speaks; the network still loses,
  /// duplicates and reorders, and the data plane still restarts and reconnects.
  action implStep = any {
    dpHello,
    dpRestart,
    dpWelcome,
    dpDeliver,
    cpWelcome,
    cpIssue,
    relaySend,
    cpReceive,
    loseToDp,
    loseToCp,
  }

  // ---- invariants ---------------------------------------------------------------------------

  /// I09: no directive from an older epoch than the data plane's floor executes.
  val i09NoStaleEpochExecutes = not(g.staleExec)

  /// executesAtMostOncePerEpoch: each directive id executes at most once per epoch, however
  /// often the relay re-sends it and the network duplicates it.
  val executesAtMostOncePerEpoch = not(g.doubleExec)

  /// refusedMeansNothingHappened: a definite refusal, on the wire or applied by the control
  /// plane, names an (epoch, directive) that never executed and never will; and an applied
  /// answer names one that did.
  val refusedMeansNothingHappened = and {
    net.toCp.forall(m =>
      (m.tag == ANSWER and m.outcome == REFUSED) implies not(g.executions.contains((m.epoch, m.id)))),
    net.toCp.forall(m =>
      (m.tag == ANSWER and m.outcome == APPLIED) implies g.executions.contains((m.epoch, m.id))),
    cp.beliefs.keys().forall(id =>
      val b = cp.beliefs.get(id)
      (b.outcome == REFUSED implies not(g.executions.contains((b.epoch, id))))
        and (b.outcome == APPLIED implies g.executions.contains((b.epoch, id)))),
  }

  /// I17: sessions are entered at strictly increasing epochs, the floor is the session's epoch
  /// while one is open, and the data plane never sees an epoch the control plane did not mint.
  val i17EpochsIncrease = and {
    not(g.epochRegressed),
    dp.floor <= cp.minted,
    dp.epoch == NONE or dp.epoch == dp.floor,
  }

  /// I18 (the inbox's half): no answer older than the newest applied one is applied.
  val i18InboxFence = and {
    not(g.inboxRegressed),
    cp.beliefs.keys().forall(id => cp.beliefs.get(id).epoch <= cp.inboxHigh),
  }

  /// I29: in a safe session no held kind executes.
  val i29SafeModeHoldsKinds = not(g.heldExec)

  /// versionNegotiated: a session exists only in a major both peers speak, and no frame of
  /// another major executes.
  val versionNegotiated = and {
    dp.epoch == NONE or CP_MAJORS.contains(dp.major),
    cp.session == NONE or CP_MAJORS.contains(cp.major),
    not(g.wrongMajorExec),
  }

  // Every invariant of the model, so a new one cannot be left out of a stage.
  val allInvariants = and {
    i09NoStaleEpochExecutes,
    executesAtMostOncePerEpoch,
    refusedMeansNothingHappened,
    i17EpochsIncrease,
    i18InboxFence,
    i29SafeModeHoldsKinds,
    versionNegotiated,
  }

  // ---- scenarios ----------------------------------------------------------------------------

  pure def welcomeOf(e: int, mode: str): Msg = { ...frame(WELCOME, e, NONE, 1), mode: mode }

  /// Hello in major 1, welcomed at the next epoch in `mode`, entered.
  action connect(mode: str): bool =
    dpHelloWith(1)
      .then(cpWelcomeWith(frame(HELLO, dp.floor, NONE, 1), mode, false))
      .then(dpWelcomeWith(welcomeOf(cp.session, mode), false))

  // The handshake mints epoch 1 above an empty floor; the floor follows; a directive executes
  // and its answer is applied.
  run handshakeAndExecuteTest = {
    init.then(connect(NORMAL))
      .expect(dp.epoch == 1 and dp.floor == 1 and cp.session == 1)
      .then(cpIssueWith(1)).then(relaySendWith(1))
      .then(dpDeliverWith(frame(DIRECTIVE, 1, 1, 1), false, "apply"))
      .expect(g.executions == Set((1, 1)))
      .then(cpReceiveWith(answer(1, 1, 1, APPLIED, ""), false))
      .expect(cp.beliefs.get(1).outcome == APPLIED and allInvariants)
  }

  // An unsupported major is refused at the handshake: no epoch is minted, no session opens.
  run unsupportedMajorRefusedTest = {
    init.then(dpHelloWith(2))
      .then(cpWelcomeWith(frame(HELLO, NONE, NONE, 2), NORMAL, false))
      .expect(cp.minted == NONE and cp.session == NONE)
      .expect(net.toDp.contains(frame(HANDSHAKE_REFUSED, NONE, NONE, 2)))
      .expect(dp.epoch == NONE and allInvariants)
  }

  // A duplicated directive executes once; the duplicate answers the recorded outcome.
  run duplicateExecutesOnceTest = {
    init.then(connect(NORMAL)).then(cpIssueWith(1)).then(relaySendWith(1))
      .then(dpDeliverWith(frame(DIRECTIVE, 1, 1, 1), true, "unknownApplied"))
      .then(dpDeliverWith(frame(DIRECTIVE, 1, 1, 1), false, "apply"))
      .expect(g.executions == Set((1, 1)) and not(g.doubleExec))
      .expect(net.toCp == Set(answer(1, 1, 1, UNKNOWN, "deadline"), answer(1, 1, 1, UNKNOWN, "repeat")))
  }

  // An effect applied in epoch 1 whose answer is lost; the data plane reconnects into epoch 2;
  // the stale copy of the epoch-1 frame arrives: fenced, answered Unknown, never refused. The
  // relay's re-send under epoch 2 runs again (a re-observation by name), once.
  run staleFrameIsNeverRefusedTest = {
    init.then(connect(NORMAL)).then(cpIssueWith(1)).then(relaySendWith(1))
      .then(dpDeliverWith(frame(DIRECTIVE, 1, 1, 1), true, "apply"))
      .then(loseToCpWith(answer(1, 1, 1, APPLIED, "")))
      .then(connect(NORMAL))
      .expect(dp.epoch == 2 and dp.floor == 2)
      .then(dpDeliverWith(frame(DIRECTIVE, 1, 1, 1), false, "apply"))
      .expect(net.toCp == Set(answer(2, 1, 1, UNKNOWN, "stale_epoch")))
      .expect(g.executions == Set((1, 1)))
      .then(relaySendWith(1))
      .then(dpDeliverWith(frame(DIRECTIVE, 2, 1, 1), false, "apply"))
      .expect(g.executions == Set((1, 1), (2, 1)) and allInvariants)
  }

  // A duplicated welcome of an earlier session after a restart is not re-entered: the floor
  // is strictly above it, so a lost table can never let an epoch execute an id twice.
  run oldWelcomeAfterRestartIgnoredTest = {
    init.then(dpHelloWith(1))
      .then(cpWelcomeWith(frame(HELLO, NONE, NONE, 1), NORMAL, false))
      .then(dpWelcomeWith(welcomeOf(1, NORMAL), true))
      .then(dpRestart).then(dpHelloWith(1))
      .then(dpWelcomeWith(welcomeOf(1, NORMAL), false))
      .expect(dp.epoch == NONE and dp.floor == 1 and dp.awaiting)
  }

  // A hello that reports a floor above the journal (a restored journal) gets an epoch above
  // the floor.
  run epochMintedAboveFloorTest = {
    init.then(connect(NORMAL)).then(connect(NORMAL)).then(connect(NORMAL))
      .expect(dp.floor == 3 and cp.minted == 3)
      .then(dpHelloWith(1))
      .then(cpWelcomeWith(frame(HELLO, 3, NONE, 1), NORMAL, false))
      .expect(cp.minted == 4 and allInvariants)
  }

  // Safe mode refuses a held kind definitely and runs an unheld one.
  run safeModeHoldsCreateTest = {
    init.then(connect(SAFE)).then(cpIssueWith(1)).then(cpIssueWith(2))
      .then(relaySendWith(1)).then(relaySendWith(2))
      .then(dpDeliverWith(frame(DIRECTIVE, 1, 1, 1), false, "apply"))
      .then(dpDeliverWith(frame(DIRECTIVE, 1, 2, 1), false, "apply"))
      .expect(g.executions == Set((1, 2)))
      .expect(net.toCp.contains(answer(1, 1, 1, REFUSED, "safe_mode")))
      .expect(allInvariants)
  }

  // A frame of another major is discarded unexecuted and unanswered.
  run foreignMajorDiscardedTest = {
    init.then(connect(NORMAL)).then(cpIssueWith(1)).then(relaySendWith(1))
      .then(foreignMajorWith(frame(DIRECTIVE, 1, 1, 1)))
      .then(dpDeliverWith({ ...frame(DIRECTIVE, 1, 1, 1), major: 2 }, false, "apply"))
      .expect(g.executions == Set() and net.toCp == Set() and allInvariants)
  }

  // A frame of a session the data plane has not entered yet is discarded, and the welcome
  // still opens the session afterwards.
  run frameBeforeWelcomeDiscardedTest = {
    init.then(dpHelloWith(1))
      .then(cpWelcomeWith(frame(HELLO, NONE, NONE, 1), NORMAL, false))
      .then(cpIssueWith(2)).then(relaySendWith(2))
      .then(dpDeliverWith(frame(DIRECTIVE, 1, 2, 1), false, "apply"))
      .expect(g.executions == Set() and net.toCp == Set())
      .then(dpWelcomeWith(welcomeOf(1, NORMAL), false))
      .then(relaySendWith(2))
      .then(dpDeliverWith(frame(DIRECTIVE, 1, 2, 1), false, "apply"))
      .expect(g.executions == Set((1, 2)))
  }

  // An answer of an older session arriving after a newer one is ignored by the inbox (I18).
  run olderAnswerIgnoredTest = {
    init.then(connect(NORMAL)).then(cpIssueWith(1)).then(cpIssueWith(2))
      .then(relaySendWith(1))
      .then(dpDeliverWith(frame(DIRECTIVE, 1, 1, 1), false, "unknownNotApplied"))
      .then(connect(NORMAL)).then(relaySendWith(2))
      .then(dpDeliverWith(frame(DIRECTIVE, 2, 2, 1), false, "apply"))
      .then(cpReceiveWith(answer(2, 2, 1, APPLIED, ""), false))
      .then(cpReceiveWith(answer(1, 1, 1, UNKNOWN, "deadline"), false))
      .expect(not(cp.beliefs.keys().contains(1)) and cp.inboxHigh == 2 and allInvariants)
  }
}
