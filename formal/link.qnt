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
 * - the JIT config on the link (LNK-02, protocol 1.6): the control plane's single-take broker
 *   holds the config minted for the launch (id 3) under the session it was minted in; the relay
 *   takes it, once, into the one frame that carries it, only when the session speaks 1.6 and only
 *   for that session's epoch (a take for another epoch discards it); a resend carries nothing; the
 *   data plane puts a config it receives into its store only for its own session's epoch, once per
 *   handle, and a launch takes it from there: delivered at most once, ever, and a launch with no
 *   config held is refused (nothing happened). The store is keyed by the handle alone, as
 *   `mayfly_data::LaunchStore` is: a launch of any epoch takes what is held for its id, so the
 *   data plane's epoch check at the put and the store emptied when a new session is entered are
 *   what keep a config in its epoch. A store entry no launch takes is dropped at its deadline
 *   (`dpExpire`); a restart empties both stores.
 *
 * Three directives stand for the kinds: id 1 is a `CloneVm` (held in safe mode, needs a resource
 * fit), id 2 a `ReportInventory` (never held), id 3 a `LaunchRunner` (held in safe mode, carries
 * the JIT config). Ghost state (`g`) records history for the invariants and never feeds a guard.
 */
module link {
  // ---- constants ----------------------------------------------------------------------------

  pure val NONE = 0
  /// Directive ids the control plane may journal.
  pure val IDS = Set(1, 2, 3)
  /// The launch: the directive whose config the broker holds (its handle).
  pure val LAUNCH_ID = 3
  pure val CLONE_VM = "CloneVm"
  pure val REPORT_INVENTORY = "ReportInventory"
  pure val LAUNCH_RUNNER = "LaunchRunner"
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

  pure def kindOf(id: int): str =
    if (id == 1) CLONE_VM else if (id == LAUNCH_ID) LAUNCH_RUNNER else REPORT_INVENTORY

  /// I29: the kinds safe mode holds (create, start, stop, delete, pull, evict, materialise and
  /// the launch that takes a secret); `mayfly_protocol::DirectiveKind::held_in_safe_mode`.
  pure def held(kind: str): bool = kind == CLONE_VM or kind == LAUNCH_RUNNER

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
    // ANSWER: why ("", "safe_mode", "insufficient_resources", "stale_epoch", "deadline",
    // "no_config")
    reason: str,
    // whether the writer speaks protocol 1.6 (`LAUNCH_CONFIG`): on a HELLO what the data plane
    // offers, on a WELCOME the session's; an older peer's frames carry `false`
    v16: bool,
    // DIRECTIVE: the frame carries the launch's JIT config (the non-`Serialize` member)
    secret: bool,
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
    // the session speaks 1.6: its launch frames may carry a config
    carries: bool,
    // the JIT broker: (directive id, epoch it was minted under) of each config held
    broker: Set[(int, int)],
    // the launch's config was minted (once: I19, I22)
    jitMinted: bool,
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
    // the session speaks 1.6
    carries: bool,
    // the LaunchStore: the directive id (its handle) of each config held, as the store keys
    // them, and every handle ever put in this process (a second put is refused)
    store: Set[int],
    used: Set[int],
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
    // the epoch the launch's config was minted under (NONE before)
    mintEpoch: int,
    // frames the relay built carrying the config (a network duplicate is the same frame)
    configSends: int,
    // (epoch, id) of every delivery of the config to a launcher
    deliveries: Set[(int, int)],
    // a delivery of a config already delivered once, in any epoch
    doubleDelivery: bool,
    // a config put in the store or delivered in another epoch than it was minted under
    wrongEpochConfig: bool,
  }

  var cp: Cp
  var dp: Dp
  var net: Net
  var g: Ghost

  pure def frame(tag: str, epoch: int, id: int, major: int): Msg =
    { tag: tag, epoch: epoch, id: id, major: major, mode: "", outcome: "", reason: "", v16: true, secret: false }

  pure def answer(epoch: int, id: int, major: int, outcome: str, reason: str): Msg =
    { ...frame(ANSWER, epoch, id, major), outcome: outcome, reason: reason }

  pure def final(b: Belief): bool = b.outcome == APPLIED or b.outcome == REFUSED

  pure def settled(c: Cp, id: int): bool = c.beliefs.keys().contains(id) and final(c.beliefs.get(id))

  // ---- init ---------------------------------------------------------------------------------

  action init = all {
    cp' = {
      minted: NONE, session: NONE, major: NONE, issued: Set(), inboxHigh: NONE, beliefs: Map(),
      carries: false, broker: Set(), jitMinted: false,
    },
    dp' = {
      floor: NONE, epoch: NONE, major: NONE, mode: NORMAL, awaiting: false, table: Map(),
      carries: false, store: Set(), used: Set(),
    },
    net' = { toDp: Set(), toCp: Set() },
    g' = {
      lastAction: "init", executions: Set(), doubleExec: false, staleExec: false,
      heldExec: false, wrongMajorExec: false, epochRegressed: false, inboxRegressed: false,
      mintEpoch: NONE, configSends: 0, deliveries: Set(), doubleDelivery: false,
      wrongEpochConfig: false,
    },
  }

  /// Takes `m` off a channel, or leaves a copy behind for a duplicate delivery.
  pure def take(s: Set[Msg], m: Msg, keep: bool): Set[Msg] = if (keep) s else s.exclude(Set(m))

  // ---- data plane ---------------------------------------------------------------------------

  /// `Hello` (a start, or a reconnect): the session ends, its table with it; the floor stays.
  /// `v16`: whether this data plane speaks protocol 1.6 (an older one offers 1.5 at most).
  action dpHelloOffering(major: int, v16: bool): bool = all {
    DP_MAJORS.contains(major),
    dp' = { ...dp, epoch: NONE, major: NONE, awaiting: true, table: Map(), carries: false },
    net' = { ...net, toCp: net.toCp.union(Set({ ...frame(HELLO, dp.floor, NONE, major), v16: v16 })) },
    cp' = cp,
    g' = { ...g, lastAction: "dpHello" },
  }

  action dpHelloWith(major: int): bool = dpHelloOffering(major, true)

  /// The data plane process restarts: everything but the floor is lost.
  action dpRestart = all {
    dp' = {
      ...dp, epoch: NONE, major: NONE, mode: NORMAL, awaiting: false, table: Map(),
      carries: false, store: Set(), used: Set(),
    },
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
      // a new session drops every config held for an earlier one (LNK-02)
      dp' = if (enter) { ...dp, floor: w.epoch, epoch: w.epoch, major: w.major, mode: w.mode, awaiting: false, table: Map(), carries: w.v16, store: Set() } else dp,
      net' = { ...net, toDp: take(net.toDp, w, keep) },
      cp' = cp,
      g' = { ...g, lastAction: "dpWelcome", epochRegressed: g.epochRegressed or (enter and w.epoch <= dp.floor) },
    },
  }

  /// The data plane takes a directive frame. `effect` is how a current, new, unheld execution
  /// ends. Before anything else, whatever the executor then does with the frame, the link's
  /// data-plane end puts a config the frame carries into the store: only a launch's, only for
  /// the session's own epoch and major, only once per handle (LNK-02); otherwise the config is
  /// dropped unread. The epoch check is the endpoint's own, not the executor's fence.
  action dpDeliverWith(m: Msg, keep: bool, effect: str): bool = all {
    net.toDp.contains(m),
    m.tag == DIRECTIVE,
    EFFECTS.contains(effect),
    val rest = take(net.toDp, m, keep)
    val puts = and {
      m.secret, dp.carries, dp.epoch != NONE, m.epoch == dp.epoch, m.major == dp.major,
      kindOf(m.id) == LAUNCH_RUNNER, not(dp.used.contains(m.id)),
    }
    val store = if (puts) dp.store.union(Set(m.id)) else dp.store
    val used = if (puts) dp.used.union(Set(m.id)) else dp.used
    val g1 = { ...g, wrongEpochConfig: g.wrongEpochConfig or (puts and m.epoch != g.mintEpoch) }
    def reply(outcome, reason) = net.toCp.union(Set(answer(dp.epoch, m.id, dp.major, outcome, reason)))
    if (dp.epoch == NONE or m.major != dp.major or m.epoch > dp.epoch) all {
      // no session, another major, or a session not entered yet: discarded unexecuted and
      // unanswered (the relay re-sends under the current session)
      net' = { ...net, toDp: rest },
      dp' = { ...dp, store: store, used: used },
      cp' = cp,
      g' = { ...g1, lastAction: "dpDiscard" },
    } else if (m.epoch < dp.epoch) all {
      // fenced (I09): an older session's frame is never executed and never refused
      net' = { toDp: rest, toCp: reply(UNKNOWN, "stale_epoch") },
      dp' = { ...dp, store: store, used: used },
      cp' = cp,
      g' = { ...g1, lastAction: "dpFence" },
    } else if (dp.table.keys().contains(m.id)) all {
      // a repeat in the same session answers the recorded outcome; a config it carries (the
      // first frame overtaken by a resend) waits in the store for its deadline
      net' = { toDp: rest, toCp: reply(dp.table.get(m.id), "repeat") },
      dp' = { ...dp, store: store, used: used },
      cp' = cp,
      g' = { ...g1, lastAction: "dpRepeat" },
    } else if (dp.mode == SAFE and held(kindOf(m.id))) all {
      net' = { toDp: rest, toCp: reply(REFUSED, "safe_mode") },
      dp' = { ...dp, table: dp.table.put(m.id, REFUSED), store: store, used: used },
      cp' = cp,
      g' = { ...g1, lastAction: "dpHold" },
    } else if (kindOf(m.id) == LAUNCH_RUNNER and not(store.contains(m.id))) all {
      // no config held for this launch (none carried, an older peer, lost to a cancelled send
      // or a restart): a definite failure, nothing was delivered (ADR 0082)
      net' = { toDp: rest, toCp: reply(REFUSED, "no_config") },
      dp' = { ...dp, table: dp.table.put(m.id, REFUSED), store: store, used: used },
      cp' = cp,
      g' = { ...g1, lastAction: "dpNoConfig" },
    } else {
      val runs = effect == "apply" or effect == "unknownApplied"
      val outcome = if (effect == "apply") APPLIED else if (effect == "refuse") REFUSED else UNKNOWN
      val reason = if (effect == "refuse") "insufficient_resources" else if (outcome == UNKNOWN) "deadline" else ""
      val launch = kindOf(m.id) == LAUNCH_RUNNER
      val delivers = launch and runs
      all {
        net' = { toDp: rest, toCp: reply(outcome, reason) },
        // the guest transport takes the config and discards the handle whatever the outcome
        dp' = {
          ...dp,
          table: dp.table.put(m.id, outcome),
          store: if (launch) store.exclude(Set(m.id)) else store,
          used: used,
        },
        cp' = cp,
        g' = {
          ...g1,
          lastAction: "dpExecute",
          executions: if (runs) g.executions.union(Set((m.epoch, m.id))) else g.executions,
          doubleExec: g.doubleExec or (runs and g.executions.contains((m.epoch, m.id))),
          staleExec: g.staleExec or (runs and m.epoch < dp.floor),
          heldExec: g.heldExec or (runs and dp.mode == SAFE and held(kindOf(m.id))),
          wrongMajorExec: g.wrongMajorExec or (runs and m.major != dp.major),
          deliveries: if (delivers) g.deliveries.union(Set((m.epoch, m.id))) else g.deliveries,
          doubleDelivery: g.doubleDelivery
            or (delivers and g.deliveries.exists(d => d._2 == m.id)),
          wrongEpochConfig: g1.wrongEpochConfig or (delivers and m.epoch != g.mintEpoch),
        },
      }
    },
  }

  /// The store's owner drops a config no launch took once its deadline passed.
  action dpExpireWith(c: int): bool = all {
    dp.store.contains(c),
    dp' = { ...dp, store: dp.store.exclude(Set(c)) },
    cp' = cp,
    net' = net,
    g' = { ...g, lastAction: "dpExpire" },
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
        cp' = { ...cp, minted: e, session: e, major: h.major, carries: h.v16 },
        net' = { toCp: rest, toDp: net.toDp.union(Set({ ...frame(WELCOME, e, NONE, h.major), mode: mode, v16: h.v16 })) },
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

  /// The registration manager mints the launch's JIT config, once, under the current session,
  /// and holds it in the broker; the journal records only its handle.
  action cpMint = all {
    cp.session != NONE,
    not(cp.jitMinted),
    cp' = { ...cp, jitMinted: true, broker: cp.broker.union(Set((LAUNCH_ID, cp.session))) },
    dp' = dp,
    net' = net,
    g' = { ...g, lastAction: "cpMint", mintEpoch: cp.session },
  }

  /// `DirectiveIssued`: the intent is journaled before anything is sent; a launch only once its
  /// config was minted.
  action cpIssueWith(id: int): bool = all {
    IDS.contains(id),
    not(cp.issued.contains(id)),
    id != LAUNCH_ID or cp.jitMinted,
    cp' = { ...cp, issued: cp.issued.union(Set(id)) },
    dp' = dp,
    net' = net,
    g' = { ...g, lastAction: "cpIssue" },
  }

  /// The relay sends (again) a directive with no definite answer, stamped with the current
  /// session's epoch and major at send time. A launch's frame takes its config from the broker,
  /// once (LNK-02): only in a session that speaks 1.6, only when it was minted under that
  /// session's epoch (a take for another epoch discards it); a resend finds nothing to take and
  /// carries nothing. In an older session nothing is taken and nothing carried.
  action relaySendWith(id: int): bool = all {
    cp.issued.contains(id),
    cp.session != NONE,
    not(settled(cp, id)),
    val held = cp.broker.filter(b => b._1 == id)
    val carry = cp.carries and held.contains((id, cp.session))
    all {
      net' = { ...net, toDp: net.toDp.union(Set({ ...frame(DIRECTIVE, cp.session, id, cp.major), secret: carry })) },
      cp' = { ...cp, broker: if (cp.carries) cp.broker.exclude(held) else cp.broker },
      dp' = dp,
      g' = { ...g, lastAction: "relaySend", configSends: if (carry) g.configSends + 1 else g.configSends },
    },
  }

  /// A mutant of `relaySendWith` for the negative control only (in no relation): the take does
  /// not remove the config, so a resend carries it again.
  action relaySendReplayingWith(id: int): bool = all {
    cp.issued.contains(id),
    cp.session != NONE,
    not(settled(cp, id)),
    val carry = cp.carries and cp.broker.contains((id, cp.session))
    all {
      net' = { ...net, toDp: net.toDp.union(Set({ ...frame(DIRECTIVE, cp.session, id, cp.major), secret: carry })) },
      cp' = cp,
      dp' = dp,
      g' = { ...g, lastAction: "relaySendReplaying", configSends: if (carry) g.configSends + 1 else g.configSends },
    },
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

  action dpHello = {
    nondet major = DP_MAJORS.oneOf()
    nondet v16 = Set(true, false).oneOf()
    dpHelloOffering(major, v16)
  }
  action dpExpire = { nondet c = dp.store.oneOf() dpExpireWith(c) }
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
    dpExpire,
    cpWelcome,
    cpMint,
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
    dpExpire,
    cpWelcome,
    cpMint,
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

  /// configSentOnce (LNK-02): the relay builds at most one frame that carries the launch's
  /// config; every resend carries nothing (a network duplicate is that same frame).
  val configSentOnce = g.configSends <= 1

  /// configDeliveredOnce (LNK-02, I22): the config reaches a launcher at most once, in any
  /// epoch, however the frame is lost, duplicated or replayed.
  val configDeliveredOnce = not(g.doubleDelivery)

  /// configOnlyInItsEpoch (LNK-02): a config is carried, put by the data plane or delivered
  /// only in the session it was minted under: no replay into another epoch.
  val configOnlyInItsEpoch = and {
    not(g.wrongEpochConfig),
    net.toDp.forall(m => m.secret implies m.epoch == g.mintEpoch),
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
    configSentOnce,
    configDeliveredOnce,
    configOnlyInItsEpoch,
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

  // ---- the JIT config on the link (LNK-02) --------------------------------------------------

  /// The launch's frame in epoch `e`, carrying the config or not.
  pure def launchFrame(e: int, secret: bool): Msg = { ...frame(DIRECTIVE, e, LAUNCH_ID, 1), secret: secret }

  /// Connected, the config minted and the launch issued.
  action launchReady: bool = connect(NORMAL).then(cpMint).then(cpIssueWith(LAUNCH_ID))

  // The first send takes the config into its frame; the data plane puts it and the launch
  // delivers it once; a resend carries nothing and answers the recorded outcome.
  run launchCarriesConfigOnceTest = {
    init.then(launchReady).then(relaySendWith(LAUNCH_ID))
      .expect(cp.broker == Set() and g.configSends == 1)
      .expect(net.toDp == Set(launchFrame(1, true)))
      .then(dpDeliverWith(launchFrame(1, true), false, "apply"))
      .expect(g.deliveries == Set((1, LAUNCH_ID)) and dp.store == Set())
      .then(relaySendWith(LAUNCH_ID))
      .expect(net.toDp == Set(launchFrame(1, false)) and g.configSends == 1)
      .then(dpDeliverWith(launchFrame(1, false), false, "apply"))
      .expect(net.toCp.contains(answer(1, LAUNCH_ID, 1, APPLIED, "repeat")))
      .expect(g.deliveries == Set((1, LAUNCH_ID)) and allInvariants)
  }

  // The frame that carried the config is lost: the resend carries nothing (the broker gave it
  // up), and the launch fails definitely with nothing delivered (P13 then deregisters by id).
  run resendAfterLostFrameCarriesNothingTest = {
    init.then(launchReady).then(relaySendWith(LAUNCH_ID))
      .then(loseToDpWith(launchFrame(1, true)))
      .then(relaySendWith(LAUNCH_ID))
      .expect(net.toDp == Set(launchFrame(1, false)))
      .then(dpDeliverWith(launchFrame(1, false), false, "apply"))
      .expect(net.toCp == Set(answer(1, LAUNCH_ID, 1, REFUSED, "no_config")))
      .expect(g.executions == Set() and g.deliveries == Set() and allInvariants)
  }

  // The network duplicates the frame that carries the config: the second copy is refused by the
  // store (the handle was used) and answers the recorded outcome; one delivery.
  run duplicateConfigFramePutOnceTest = {
    init.then(launchReady).then(relaySendWith(LAUNCH_ID))
      .then(dpDeliverWith(launchFrame(1, true), true, "apply"))
      .then(dpDeliverWith(launchFrame(1, true), false, "apply"))
      .expect(dp.store == Set() and dp.used == Set(LAUNCH_ID))
      .expect(g.deliveries == Set((1, LAUNCH_ID)) and allInvariants)
  }

  // A resend overtakes the frame that carries the config: the launch fails for want of it; the
  // late original is put but no launch takes it, and the store drops it at its deadline.
  run overtakenConfigExpiresTest = {
    init.then(launchReady).then(relaySendWith(LAUNCH_ID)).then(relaySendWith(LAUNCH_ID))
      .then(dpDeliverWith(launchFrame(1, false), false, "apply"))
      .then(dpDeliverWith(launchFrame(1, true), false, "apply"))
      .expect(dp.store == Set(LAUNCH_ID) and g.deliveries == Set())
      .then(dpExpireWith(LAUNCH_ID))
      .expect(dp.store == Set() and g.deliveries == Set() and allInvariants)
  }

  // The frame of an earlier session that carries the config arrives after a reconnect: fenced,
  // never put; the new session's resend carries nothing, and its launch finds no config. A data
  // plane that put without its epoch check would hold the stale config by its handle, and this
  // launch of epoch 2 would deliver it (`configOnlyInItsEpoch`). And a config minted in an earlier
  // session and never sent is discarded by the take, never carried into the new epoch.
  run staleSessionConfigNeverDeliveredTest = {
    init.then(launchReady).then(relaySendWith(LAUNCH_ID))
      .then(connect(NORMAL))
      .then(dpDeliverWith(launchFrame(1, true), false, "apply"))
      .expect(net.toCp == Set(answer(2, LAUNCH_ID, 1, UNKNOWN, "stale_epoch")))
      .then(relaySendWith(LAUNCH_ID))
      .then(dpDeliverWith(launchFrame(2, false), false, "apply"))
      .expect(net.toCp.contains(answer(2, LAUNCH_ID, 1, REFUSED, "no_config")))
      .expect(g.deliveries == Set() and allInvariants)
  }

  // A config held but never taken in session 1 (its frame overtaken by a resend that already
  // failed) is dropped when session 2 is entered: the store is keyed by the handle alone, so the
  // next session's launch would otherwise take a config of another epoch.
  run heldConfigNeverCrossesSessionsTest = {
    init.then(launchReady).then(relaySendWith(LAUNCH_ID)).then(relaySendWith(LAUNCH_ID))
      .then(dpDeliverWith(launchFrame(1, false), false, "apply"))
      .then(dpDeliverWith(launchFrame(1, true), false, "apply"))
      .expect(dp.store == Set(LAUNCH_ID))
      .then(connect(NORMAL))
      .expect(dp.store == Set())
      .then(relaySendWith(LAUNCH_ID))
      .then(dpDeliverWith(launchFrame(2, false), false, "apply"))
      .expect(net.toCp.contains(answer(2, LAUNCH_ID, 1, REFUSED, "no_config")))
      .expect(g.deliveries == Set() and allInvariants)
  }

  // A config minted under an earlier session and never sent: the take under the new epoch
  // discards it, so it is never carried into another epoch.
  run unsentConfigOfEarlierSessionDiscardedTest = {
    init.then(launchReady).then(connect(NORMAL))
      .expect(cp.broker == Set((LAUNCH_ID, 1)))
      .then(relaySendWith(LAUNCH_ID))
      .expect(cp.broker == Set() and g.configSends == 0 and net.toDp == Set(launchFrame(2, false)))
      .expect(allInvariants)
  }

  // A data plane older than 1.6: nothing is taken or carried, the launch fails definitely, as
  // before; the broker keeps the config until the registration settles.
  run olderPeerGetsNoConfigTest = {
    init.then(dpHelloOffering(1, false))
      .then(cpWelcomeWith({ ...frame(HELLO, NONE, NONE, 1), v16: false }, NORMAL, false))
      .then(dpWelcomeWith({ ...welcomeOf(1, NORMAL), v16: false }, false))
      .expect(not(cp.carries) and not(dp.carries))
      .then(cpMint).then(cpIssueWith(LAUNCH_ID)).then(relaySendWith(LAUNCH_ID))
      .expect(net.toDp == Set(launchFrame(1, false)) and cp.broker == Set((LAUNCH_ID, 1)))
      .then(dpDeliverWith(launchFrame(1, false), false, "apply"))
      .expect(net.toCp == Set(answer(1, LAUNCH_ID, 1, REFUSED, "no_config")))
      .expect(g.deliveries == Set() and allInvariants)
  }

  // Negative control: a relay whose take leaves the config in the broker carries it again on
  // the resend, which `configSentOnce` flags.
  run resendCarryingConfigTwiceIsFlaggedTest = {
    init.then(launchReady)
      .then(relaySendReplayingWith(LAUNCH_ID))
      .then(loseToDpWith(launchFrame(1, true)))
      .then(relaySendReplayingWith(LAUNCH_ID))
      .expect(net.toDp == Set(launchFrame(1, true)) and g.configSends == 2)
      .expect(not(configSentOnce) and not(allInvariants))
  }
}
