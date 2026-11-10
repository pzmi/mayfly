// -*- mode: Bluespec; -*-
/**
 * The event-store model (ES-06; doc/event-store.md §7.3, ADRs 0034 and 0041): the journal's
 * stream heads, histories, event identities, command receipts and inbox, its units of work
 * judged in the port's reporting order (§3.3), committed in one step (the memory journal) or as
 * a transaction split into begin, head locks in `StreamId` order, write and commit with its
 * waits recorded (SQLite, PostgreSQL); the `AggregateStore` write path in front of it (§3.7:
 * epoch before receipt, receipt replay or `CommandIdReused`, decide outside any transaction,
 * refusal receipts bound to every stream the decision read); the after-commit reactor kick and
 * a crash between the commit and it; one durable consumer, the directive relay (§5.1: marks,
 * checkpoints, leases with fences, workers); and the online rebuild of the inline projection
 * (§3.6, E9: a shadow generation, dual writes and the cutover fence), which ES-12 builds.
 * Snapshots (`ES_I8`) join with ES-08.
 *
 * The Rust code it describes: `mayfly_control::journal` (`UnitOfWork`, `AppendError` and its
 * order), `mayfly_testkit::journal::MemoryJournal` (judge, then apply all or nothing),
 * `mayfly-adapter-sqlite` (one `BEGIN IMMEDIATE` at a time: `implStepSerial`) and
 * `mayfly_control::store::AggregateStore::save` (`command`). The conformance driver (ES-07)
 * maps each action below onto a call of the ports.
 *
 * Abstractions, each a choice of the model, not of the code:
 * - two streams `a` < `b` (the `StreamId` order), three revisions each, four event ids, two
 *   command ids with two intents, two inbox keys, builds that support epoch 1 or 2;
 * - a command's decision is a pure function of the event counts it read
 *   (`accepts`): intent 1 is always accepted, intent 2 only once its streams hold an event, so
 *   a refusal can turn into an acceptance when a stream it read moves; an accepted command
 *   appends one event whose id is its command id (the derived id of `(command_id, 0)`) and
 *   which is a `DirectiveIssued` for intent 1 (ADR 0034: the directive is an event of the unit
 *   that decided it, so it is atomic with it like any event);
 * - the inline projection is the event count of each stream (`rows`): a unit's row is the fold
 *   at its expected revision plus its batch (`rowAfter`), for the generation set it saw when it
 *   was prepared outside the transaction (`withRows`);
 * - read-set heads take a shared lock, appended heads an exclusive one (§4.2: `FOR SHARE`,
 *   `FOR NO KEY UPDATE`); receipt, inbox and identity keys are taken at `write`, after every
 *   head (§4.2's order), and a key another open transaction has written makes the writer wait.
 *
 * Ghost state (`g`) records history for the invariants and never feeds a guard. `last` is the
 * stimulus just taken and its outcome, for the scenarios and the conformance driver.
 */
module event_store {
  // ---- constants ----------------------------------------------------------------------------

  /// Streams; `StreamId` order is `a` before `b` (`lock`).
  pure val STREAMS = Set("a", "b")
  pure def other(s: str): str = if (s == "a") "b" else "a"
  /// Client slots: each prepares at most one unit and runs it as one transaction.
  pure val SLOTS = Set(1, 2)
  pure val MAXREV = 2
  pure val REVS = 0.to(MAXREV)
  pure val IDS = 1.to(4)
  pure val CMDS = Set(1, 2)
  pure val INTENTS = Set(1, 2)
  pure val MSGS = Set(1, 2)
  /// A build's supported epoch, and an event's required epoch.
  pure val BUILDS = Set(1, 2)
  pure val WORKERS = Set(1, 2)
  pure val GENS = Set(1, 2)
  /// Commits are numbered 1..MAXC (a bound of the model: kicks are kept per commit).
  pure val MAXC = 6
  pure val COMMITS = 1.to(MAXC)

  /// `ExpectedRevision::NoStream`, and an absent stream's head.
  pure val NOSTREAM = -1
  /// A stream the unit does not name (in `app`, `rd`, a receipt's `bound` and `at`).
  pure val ABSENT = -2
  /// A shadow projection row its rebuild has not written yet.
  pure val UNBUILT = -1
  /// No revision: no mark, no checkpoint, nothing delivered.
  pure val NOREV = -1

  // Slot phases.
  pure val FREE = "free"
  pure val PREPARED = "prepared"
  pure val LOCKING = "locking"
  pure val WRITTEN = "written"

  // Outcomes (`AppendError` in its reporting order, then the store's own).
  pure val OK = "ok"
  pure val COMMITTED = "committed"
  pure val MALFORMED = "malformed"
  pure val EPOCH_ABOVE_BUILD = "epochAboveBuild"
  pure val REPLAYED = "commandReplayed"
  pure val COMMAND_ID_REUSED = "commandIdReused"
  pure val DUPLICATE_MESSAGE = "duplicateMessage"
  pure val DUPLICATE_EVENT_ID = "duplicateEventId"
  pure val WRONG_EXPECTED_REVISION = "wrongExpectedRevision"
  pure val STREAM_EPOCH_UNSUPPORTED = "streamEpochUnsupported"
  /// ES-12's refusal of a unit computed for another generation set (§3.6).
  pure val GENERATION_MOVED = "projectionGenerationMoved"
  pure val BUSY = "busy"
  pure val STORAGE = "storage"
  pure val WAITING = "waiting"
  pure val NONE = "none"
  pure val LEASE_LOST = "leaseLost"

  // ---- state --------------------------------------------------------------------------------

  /// An event; `id` 0 is an empty slot of the log. `n` is the commit that wrote it.
  type Ev = { id: int, epoch: int, dir: bool, n: int }

  type Unit = {
    // appended streams: the expected revision (NOSTREAM or r), ABSENT if not appended
    app: str -> int,
    ev1: str -> Ev,
    // a second event of the batch (id 0: none)
    ev2: str -> Ev,
    // the validated read set: the expected revision, ABSENT if not read
    rd: str -> int,
    rcpt: bool,
    cmd: int,
    intent: int,
    refused: bool,
    // the inbox key it consumes, 0 none
    inbox: int,
    // the build's supported epoch
    build: int,
    // the generation set its rows were computed for (§3.6): the active and shadow generations
    // and which streams the shadow had rebuilt; the rows themselves are the fold at the
    // expected revision (`rowAfter`)
    act: int,
    shd: int,
    built: str -> bool,
  }

  type Slot = { phase: str, unit: Unit, locks: Set[str], waitOn: int }

  type Receipt = {
    present: bool,
    intent: int,
    refused: bool,
    // a refusal's read set: every stream the decision read, at the revision read
    bound: str -> int,
    // an acceptance's last revision per appended stream (its `Appended`)
    at: str -> int,
    n: int,
  }

  type Journal = {
    heads: str -> int,
    log: str -> int -> Ev,
    // `required_epoch` of each stream head (0: no stream)
    epochs: str -> int,
    // the identity registry: event id -> (stream, revision); revision NOREV when unbound
    idLoc: int -> (str, int),
    receipts: int -> Receipt,
    // inbox key -> the commit that consumed it, 0 none
    inbox: int -> int,
    commits: int,
  }

  /// The inline projection: per generation and stream, the events folded (UNBUILT in a shadow
  /// not yet rebuilt for the stream); the active generation and the shadow (0: none).
  type Proj = { rows: int -> str -> int, active: int, shadow: int }

  type Lease = { holder: int, fence: int, live: bool }
  /// A worker's claim: stream "" when idle; `through` the mark it claimed; `next` the revision
  /// it delivers next; `done` a delivered revision not yet advanced to (NOREV none).
  type Worker = { stream: str, fence: int, through: int, next: int, done: int }

  type Delivery = {
    registered: bool,
    // the stream is covered: bootstrapped, or appended since registration
    boot: str -> bool,
    marks: str -> int,
    ckpt: str -> int,
    lease: str -> Lease,
    workers: int -> Worker,
  }

  type Ghost = {
    // a commit wrote other than all of its unit (checked against the unit, apart from
    // `journalAfter`)
    torn: bool,
    claimBreach: bool,
    // the id ever committed at (stream, revision), 0 none
    ever: str -> int -> int,
    epochSeen: str -> int,
    epochBreach: bool,
    accepts: int -> int,
    otherIntentReplayed: bool,
    staleRefusalReplayed: bool,
    receiptBreach: bool,
    msgCommits: int -> int,
    delivered: str -> int -> bool,
    // directives sent (effects of `DirectiveIssued` events)
    sent: str -> int -> bool,
    unfenced: bool,
    ckptSeen: str -> int,
    kicked: int -> bool,
    kickBreach: bool,
  }

  type Last = { stimulus: str, slot: int, outcome: str }

  var j: Journal
  var p: Proj
  var d: Delivery
  var slots: int -> Slot
  /// Commits whose after-commit notification has not run yet (lost on a crash).
  var kicks: int -> bool
  var g: Ghost
  var last: Last

  // ---- values -------------------------------------------------------------------------------

  pure val NOEV: Ev = { id: 0, epoch: 0, dir: false, n: 0 }
  pure val NONE_S: str -> int = STREAMS.mapBy(_ => ABSENT)
  pure val NOEVS: str -> Ev = STREAMS.mapBy(_ => NOEV)
  pure val ZERO_S: str -> int = STREAMS.mapBy(_ => 0)
  pure val NOUNIT: Unit = {
    app: NONE_S, ev1: NOEVS, ev2: NOEVS, rd: NONE_S, rcpt: false, cmd: 0, intent: 0,
    refused: false, inbox: 0, build: 1, act: 0, shd: 0, built: STREAMS.mapBy(_ => false),
  }
  pure val FREE_SLOT: Slot = { phase: FREE, unit: NOUNIT, locks: Set(), waitOn: 0 }
  pure val NORECEIPT: Receipt =
    { present: false, intent: 0, refused: false, bound: NONE_S, at: NONE_S, n: 0 }
  pure val NOREVS: str -> int -> bool = STREAMS.mapBy(_ => REVS.mapBy(_ => false))
  pure val NOKICKS: int -> bool = COMMITS.mapBy(_ => false)
  pure val IDLE: Worker = { stream: "", fence: 0, through: NOREV, next: 0, done: NOREV }

  pure def imax(x: int, y: int): int = if (x > y) x else y

  // ---- units --------------------------------------------------------------------------------

  pure def appends(u: Unit, s: str): bool = u.app.get(s) != ABSENT
  pure def appended(u: Unit): Set[str] = STREAMS.filter(s => appends(u, s))
  pure def named(u: Unit): Set[str] = STREAMS.filter(s => appends(u, s) or u.rd.get(s) != ABSENT)
  pure def evs(u: Unit, s: str): int =
    if (not(appends(u, s))) 0 else if (u.ev2.get(s).id == 0) 1 else 2
  pure def total(u: Unit): int = STREAMS.fold(0, (acc, s) => acc + evs(u, s))
  pure def idsOf(u: Unit, s: str): Set[int] =
    if (not(appends(u, s))) Set()
    else if (u.ev2.get(s).id == 0) Set(u.ev1.get(s).id)
    else Set(u.ev1.get(s).id, u.ev2.get(s).id)
  pure def unitIds(u: Unit): Set[int] = STREAMS.fold(Set(), (acc, s) => acc.union(idsOf(u, s)))
  pure def expectedOf(u: Unit, s: str): int = if (appends(u, s)) u.app.get(s) else u.rd.get(s)
  pure def maxEpoch(u: Unit, s: str): int =
    if (not(appends(u, s))) 0 else imax(u.ev1.get(s).epoch, u.ev2.get(s).epoch)
  /// The unit fits the bounded log (a model bound, not a refusal).
  pure def fits(u: Unit): bool =
    appended(u).forall(s => u.app.get(s) + evs(u, s) <= MAXREV and u.app.get(s) >= NOSTREAM)

  /// `UnitOfWork::check_shape`: empty, a stream named twice, an id twice in the unit, a refusal
  /// not bound to a read or carrying an append (the shape refusals the model can build).
  pure def malformed(u: Unit): bool = or {
    appended(u).size() == 0 and not(u.rcpt) and u.inbox == 0,
    STREAMS.exists(s => appends(u, s) and u.rd.get(s) != ABSENT),
    unitIds(u).size() != total(u),
    u.rcpt and u.refused and (appended(u).size() > 0 or named(u).size() == 0),
  }

  /// `UnitOfWork::epoch_above_build`: a batch requires more than the build supports.
  pure def aboveBuild(u: Unit): bool = appended(u).exists(s => maxEpoch(u, s) > u.build)

  /// A stored receipt answers a repeat: an acceptance for ever, a refusal while every stream
  /// it is bound to is where the decision read it (ES-I5, ADR 0041).
  def answers(r: Receipt): bool =
    not(r.refused) or
      STREAMS.forall(s => r.bound.get(s) == ABSENT or r.bound.get(s) == j.heads.get(s))

  /// The ghost's own reading of a receipt's binding (written apart from `answers`, so that a
  /// mutation of the guard is caught rather than copied): every bound stream unchanged.
  def stillBound(r: Receipt): bool =
    not(r.refused) or
      STREAMS.filter(s => r.bound.get(s) != ABSENT).forall(s => j.heads.get(s) == r.bound.get(s))

  /// The inline row of an appended stream: the fold of the stream at the unit's expected
  /// revision plus its batch (the projection counts events), computed outside the transaction
  /// and guarded by the expected revision.
  pure def rowAfter(u: Unit, s: str): int = u.app.get(s) + 1 + evs(u, s)

  def builtNow(s: str): bool = p.shadow != 0 and p.rows.get(p.shadow).get(s) != UNBUILT

  /// The generation set a unit prepared now computes its rows for (§3.6).
  def withRows(u: Unit): Unit = {
    ...u,
    act: p.active,
    shd: p.shadow,
    built: STREAMS.mapBy(s => builtNow(s)),
  }

  /// The generation set the unit's rows were computed for is gone (cutover, or a stream of
  /// it rebuilt since): ES-12's fence.
  def genMoved(u: Unit): bool = appended(u).size() > 0 and or {
    u.act != p.active,
    u.shd != p.shadow,
    appended(u).exists(s => u.built.get(s) != builtNow(s)),
  }

  /// The unit's first refusal in the port's reporting order (`MemoryJournal::refusal`), or OK.
  def judge(u: Unit): str = {
    val r = j.receipts.get(u.cmd)
    if (malformed(u)) MALFORMED
    else if (aboveBuild(u)) EPOCH_ABOVE_BUILD
    else if (u.rcpt and r.present and r.intent != u.intent) COMMAND_ID_REUSED
    else if (u.rcpt and r.present and answers(r)) REPLAYED
    else if (u.inbox != 0 and j.inbox.get(u.inbox) != 0) DUPLICATE_MESSAGE
    else if (unitIds(u).exists(i => j.idLoc.get(i)._2 != NOREV)) DUPLICATE_EVENT_ID
    else if (named(u).exists(s => expectedOf(u, s) != j.heads.get(s))) WRONG_EXPECTED_REVISION
    else if (appended(u).exists(s => j.epochs.get(s) > u.build)) STREAM_EPOCH_UNSUPPORTED
    else if (genMoved(u)) GENERATION_MOVED
    else OK
  }

  // ---- contention ---------------------------------------------------------------------------

  /// Slot `o` holds the head of `s` in a mode that conflicts with what slot `t` needs on it.
  def lockConflict(t: int, s: str, o: int): bool = and {
    o != t,
    slots.get(o).locks.contains(s),
    appends(slots.get(o).unit, s) or appends(slots.get(t).unit, s),
  }

  /// Slot `o` has written a key (receipt, inbox, identity) slot `t`'s unit needs.
  def keyConflict(t: int, o: int): bool = {
    val ut = slots.get(t).unit
    val uo = slots.get(o).unit
    and {
      o != t,
      slots.get(o).phase == WRITTEN,
      or {
        ut.rcpt and uo.rcpt and ut.cmd == uo.cmd,
        ut.inbox != 0 and ut.inbox == uo.inbox,
        unitIds(ut).intersect(unitIds(uo)) != Set(),
      },
    }
  }

  /// Slot `o` holds something slot `t` needs.
  def blocks(o: int, t: int): bool =
    named(slots.get(t).unit).exists(s => lockConflict(t, s, o)) or keyConflict(t, o)

  def anyOpen(t: int): bool =
    SLOTS.exists(o => o != t and slots.get(o).phase.in(Set(LOCKING, WRITTEN)))

  /// Slots after slot `t` ends: free, and nobody waits on it any more.
  def released(t: int): int -> Slot =
    SLOTS.mapBy(o =>
      if (o == t) FREE_SLOT
      else if (slots.get(o).waitOn == t) { ...slots.get(o), waitOn: 0 }
      else slots.get(o))

  // ---- commit -------------------------------------------------------------------------------

  pure def newEv(u: Unit, s: str, r: int, n: int): Ev =
    if (appends(u, s) and r == u.app.get(s) + 1) { ...u.ev1.get(s), n: n }
    else if (appends(u, s) and u.ev2.get(s).id != 0 and r == u.app.get(s) + 2) { ...u.ev2.get(s), n: n }
    else NOEV

  pure def locOf(u: Unit, i: int, cur: (str, int)): (str, int) =
    STREAMS.fold(cur, (acc, s) =>
      if (appends(u, s) and u.ev1.get(s).id == i) (s, u.app.get(s) + 1)
      else if (appends(u, s) and u.ev2.get(s).id == i) (s, u.app.get(s) + 2)
      else acc)

  /// The journal after `u` commits as commit `n`: all of it (ES-I1).
  def journalAfter(u: Unit, n: int): Journal = {
    val heads2 = STREAMS.mapBy(s => if (appends(u, s)) u.app.get(s) + evs(u, s) else j.heads.get(s))
    {
      heads: heads2,
      log: STREAMS.mapBy(s => REVS.mapBy(r => {
        val e = newEv(u, s, r, n)
        if (e.id != 0) e else j.log.get(s).get(r)
      })),
      epochs: STREAMS.mapBy(s =>
        if (appends(u, s)) imax(j.epochs.get(s), maxEpoch(u, s)) else j.epochs.get(s)),
      idLoc: IDS.mapBy(i => locOf(u, i, j.idLoc.get(i))),
      receipts:
        if (not(u.rcpt)) j.receipts
        else if (u.refused)
          j.receipts.set(u.cmd, {
            present: true, intent: u.intent, refused: true, bound: u.rd, at: NONE_S, n: n,
          })
        else
          j.receipts.set(u.cmd, {
            present: true, intent: u.intent, refused: false, bound: NONE_S, n: n,
            at: STREAMS.mapBy(s => if (appends(u, s)) heads2.get(s) else ABSENT),
          }),
      inbox: if (u.inbox == 0) j.inbox else j.inbox.set(u.inbox, n),
      commits: n,
    }
  }

  /// Inline rows, written for the generation set the unit computed them for (dual writes).
  def projAfter(u: Unit): Proj = {
    ...p,
    rows: GENS.mapBy(gn => STREAMS.mapBy(s =>
      if (appends(u, s) and gn == u.act) rowAfter(u, s)
      else if (appends(u, s) and u.shd != 0 and gn == u.shd and u.built.get(s)) rowAfter(u, s)
      else p.rows.get(gn).get(s))),
  }

  /// Dispatch marks: the store marks the registered consumer for every appended stream.
  def deliveryAfter(u: Unit, heads2: str -> int): Delivery =
    if (not(d.registered)) d
    else {
      ...d,
      marks: STREAMS.mapBy(s => if (appends(u, s)) heads2.get(s) else d.marks.get(s)),
      boot: STREAMS.mapBy(s => d.boot.get(s) or appends(u, s)),
    }

  def ghostAfter(u: Unit, n: int, j2: Journal): Ghost = {
    val old = j.receipts.get(u.cmd)
    {
      ...g,
      torn: g.torn or or {
        appended(u).exists(s => or {
          j2.heads.get(s) != u.app.get(s) + evs(u, s),
          j2.log.get(s).get(u.app.get(s) + 1).id != u.ev1.get(s).id,
          u.ev2.get(s).id != 0 and j2.log.get(s).get(u.app.get(s) + 2).id != u.ev2.get(s).id,
        }),
        STREAMS.exists(s => not(appends(u, s)) and j2.heads.get(s) != j.heads.get(s)),
        u.rcpt and (not(j2.receipts.get(u.cmd).present) or j2.receipts.get(u.cmd).n != n),
        u.inbox != 0 and j2.inbox.get(u.inbox) != n,
      },
      // the expected revision of every appended stream was its head: at most one unit commits
      // per (stream, expected revision), since heads only grow
      claimBreach: g.claimBreach or appended(u).exists(s => u.app.get(s) != j.heads.get(s)),
      ever: STREAMS.mapBy(s => REVS.mapBy(r => {
        val e = newEv(u, s, r, n)
        if (e.id != 0) e.id else g.ever.get(s).get(r)
      })),
      epochSeen: STREAMS.mapBy(s => imax(g.epochSeen.get(s), j2.epochs.get(s))),
      epochBreach:
        g.epochBreach or aboveBuild(u) or appended(u).exists(s => j.epochs.get(s) > u.build),
      accepts:
        if (u.rcpt and not(u.refused)) g.accepts.set(u.cmd, g.accepts.get(u.cmd) + 1) else g.accepts,
      receiptBreach: g.receiptBreach or (u.rcpt and old.present and stillBound(old)),
      msgCommits:
        if (u.inbox == 0) g.msgCommits
        else g.msgCommits.set(u.inbox, g.msgCommits.get(u.inbox) + 1),
    }
  }

  /// Commits slot `t`'s unit as the next commit and frees the slot.
  action apply(name: str, t: int): bool = {
    val u = slots.get(t).unit
    val n = j.commits + 1
    val j2 = journalAfter(u, n)
    all {
      n <= MAXC,
      j' = j2,
      p' = projAfter(u),
      d' = deliveryAfter(u, j2.heads),
      slots' = released(t),
      kicks' = if (appended(u).size() > 0) kicks.set(n, true) else kicks,
      g' = ghostAfter(u, n, j2),
      last' = { stimulus: name, slot: t, outcome: COMMITTED },
    }
  }

  /// Ghost of a receipt answer to a request of intent `intent` (journal or store).
  def replayGhost(r: Receipt, intent: int): Ghost = {
    ...g,
    otherIntentReplayed: g.otherIntentReplayed or r.intent != intent,
    staleRefusalReplayed: g.staleRefusalReplayed or not(stillBound(r)),
  }

  /// Slot `t` ends with the refusal `outcome`; nothing is written.
  action refuse(name: str, t: int, outcome: str): bool = {
    val u = slots.get(t).unit
    all {
      j' = j,
      p' = p,
      d' = d,
      slots' = released(t),
      kicks' = kicks,
      g' = if (outcome == REPLAYED) replayGhost(j.receipts.get(u.cmd), u.intent) else g,
      last' = { stimulus: name, slot: t, outcome: outcome },
    }
  }

  // ---- clients: units ----------------------------------------------------------------------

  /// Slot `t` holds `u`, its rows computed now, outside any transaction.
  action prepare(name: str, t: int, u: Unit): bool = all {
    slots.get(t).phase == FREE,
    fits(u),
    j' = j,
    p' = p,
    d' = d,
    slots' = slots.set(t, { phase: PREPARED, unit: withRows(u), locks: Set(), waitOn: 0 }),
    kicks' = kicks,
    g' = g,
    last' = { stimulus: name, slot: t, outcome: NONE },
  }

  /// The store answers without a unit (steps 1 and 2 of `AggregateStore::save`).
  action answer(name: str, t: int, outcome: str, ghost: Ghost): bool = all {
    slots.get(t).phase == FREE,
    j' = j,
    p' = p,
    d' = d,
    slots' = slots,
    kicks' = kicks,
    g' = ghost,
    last' = { stimulus: name, slot: t, outcome: outcome },
  }

  pure def count(h: int): int = h + 1

  /// The decision: a pure function of what it read (see the module docs).
  def accepts(i: int, s: str, r: str): bool =
    count(j.heads.get(s)) + (if (r == "") 0 else count(j.heads.get(r))) >= i - 1

  /// `AggregateStore::save` for command `c` of intent `i` on stream `s`, having read `r` too
  /// ("" none), by a build supporting epoch `b`, consuming inbox key `m` (0 none) on acceptance:
  /// epoch, receipt, then the decision and its unit (an acceptance's events and receipt, or a
  /// refusal receipt bound to every stream read, carrying no inbox key: ADR 0041 rule 6).
  action command(t: int, c: int, i: int, s: str, r: str, b: int, m: int): bool = {
    val rc = j.receipts.get(c)
    val reads = STREAMS.mapBy(x => if (x == r) j.heads.get(x) else ABSENT)
    if (j.epochs.get(s) > b) answer("command", t, STREAM_EPOCH_UNSUPPORTED, g)
    else if (rc.present and rc.intent != i) answer("command", t, COMMAND_ID_REUSED, g)
    else if (rc.present and answers(rc)) answer("command", t, REPLAYED, replayGhost(rc, i))
    else if (accepts(i, s, r))
      prepare("command", t, {
        ...NOUNIT,
        app: NONE_S.set(s, j.heads.get(s)),
        ev1: NOEVS.set(s, { id: c, epoch: b, dir: i == 1, n: 0 }),
        rd: reads,
        rcpt: true, cmd: c, intent: i, refused: false, inbox: m, build: b,
      })
    else
      prepare("command", t, {
        ...NOUNIT,
        rd: reads.set(s, j.heads.get(s)),
        rcpt: true, cmd: c, intent: i, refused: true, inbox: 0, build: b,
      })
  }

  /// A unit built by a journal caller (the contract's view): appends to `s` at `e` the events
  /// `id1` (and `id2`, 0 none) of epoch `ep`; with `two`, also event `id3` to the other stream
  /// at its head; no receipt; build `b`.
  action rawUnit(
    t: int, s: str, e: int, id1: int, id2: int, two: bool, id3: int, ep: int, b: int,
  ): bool = {
    val o = other(s)
    prepare("rawUnit", t, {
      ...NOUNIT,
      app: if (two) NONE_S.set(s, e).set(o, j.heads.get(o)) else NONE_S.set(s, e),
      ev1: {
        val first = NOEVS.set(s, { id: id1, epoch: ep, dir: false, n: 0 })
        if (two) first.set(o, { id: id3, epoch: ep, dir: false, n: 0 }) else first
      },
      ev2: if (id2 == 0) NOEVS else NOEVS.set(s, { id: id2, epoch: ep, dir: false, n: 0 }),
      build: b,
    })
  }

  /// A message that required nothing: a unit recording only its inbox key (ES-I6).
  action noteMessage(t: int, m: int): bool = prepare("noteMessage", t, { ...NOUNIT, inbox: m })

  // ---- the journal: one step (memory) ------------------------------------------------------

  /// `Journal::append` judged and applied at once, when no open transaction holds what it needs.
  action appendUnit(t: int): bool = {
    val u = slots.get(t).unit
    val verdict = judge(u)
    all {
      slots.get(t).phase == PREPARED,
      SLOTS.forall(o => not(blocks(o, t))),
      if (verdict == OK) apply("appendUnit", t) else refuse("appendUnit", t, verdict),
    }
  }

  // ---- the journal: split transactions (SQLite, PostgreSQL) --------------------------------

  action begin(t: int): bool = all {
    slots.get(t).phase == PREPARED,
    j' = j,
    p' = p,
    d' = d,
    slots' = slots.set(t, { ...slots.get(t), phase: LOCKING }),
    kicks' = kicks,
    g' = g,
    last' = { stimulus: "begin", slot: t, outcome: NONE },
  }

  /// The next head lock in `StreamId` order, or a recorded wait on its holder.
  action lock(t: int): bool = {
    val sl = slots.get(t)
    val todo = named(sl.unit).exclude(sl.locks)
    val nextHead = if (todo.contains("a")) "a" else "b"
    val holders = SLOTS.filter(o => lockConflict(t, nextHead, o))
    all {
      sl.phase == LOCKING,
      todo.size() > 0,
      j' = j,
      p' = p,
      d' = d,
      if (holders.size() > 0) {
        // the other slot (two slots: deterministic, so a scenario samples once)
        val o = if (holders.contains(1)) 1 else 2
        all {
          sl.waitOn != o,
          slots' = slots.set(t, { ...sl, waitOn: o }),
          last' = { stimulus: "lock", slot: t, outcome: WAITING },
        }
      } else all {
        slots' = slots.set(t, { ...sl, locks: sl.locks.union(Set(nextHead)), waitOn: 0 }),
        last' = { stimulus: "lock", slot: t, outcome: NONE },
      },
      kicks' = kicks,
      g' = g,
    }
  }

  /// With every head locked: the keys (receipt, inbox, identities), waiting on a key another
  /// open transaction wrote, then the judgement; a refusal rolls back.
  action write(t: int): bool = {
    val sl = slots.get(t)
    val verdict = judge(sl.unit)
    val holders = SLOTS.filter(o => keyConflict(t, o))
    all {
      sl.phase == LOCKING,
      sl.locks == named(sl.unit),
      if (holders.size() > 0) {
        val o = if (holders.contains(1)) 1 else 2
        all {
          sl.waitOn != o,
          j' = j,
          p' = p,
          d' = d,
          slots' = slots.set(t, { ...sl, waitOn: o }),
          kicks' = kicks,
          g' = g,
          last' = { stimulus: "write", slot: t, outcome: WAITING },
        }
      } else if (verdict == OK) all {
        j' = j,
        p' = p,
        d' = d,
        slots' = slots.set(t, { ...sl, phase: WRITTEN, waitOn: 0 }),
        kicks' = kicks,
        g' = g,
        last' = { stimulus: "write", slot: t, outcome: NONE },
      } else refuse("write", t, verdict),
    }
  }

  /// `COMMIT`: the fence of the generation set is checked under the commit (ES-12).
  action commit(t: int): bool = {
    val u = slots.get(t).unit
    all {
      slots.get(t).phase == WRITTEN,
      if (genMoved(u)) refuse("commit", t, GENERATION_MOVED) else apply("commit", t),
    }
  }

  /// A failure before `COMMIT` (a statement, the connection): nothing of the unit stays.
  action rollback(t: int): bool = all {
    slots.get(t).phase.in(Set(LOCKING, WRITTEN)),
    refuse("rollback", t, STORAGE),
  }

  /// A bounded lock wait runs out: a definite `Busy`.
  action timeout(t: int): bool = all {
    slots.get(t).phase == LOCKING,
    slots.get(t).waitOn != 0,
    refuse("timeout", t, BUSY),
  }

  // ---- reactors and crashes -----------------------------------------------------------------

  /// The after-commit notification of commit `n` (a kick, at most once, never replayed).
  action kick(n: int): bool = all {
    kicks.get(n),
    j' = j,
    p' = p,
    d' = d,
    slots' = slots,
    kicks' = kicks.set(n, false),
    g' = { ...g, kicked: g.kicked.set(n, true), kickBreach: g.kickBreach or g.kicked.get(n) },
    last' = { stimulus: "kick", slot: 0, outcome: NONE },
  }

  /// The process dies: open transactions roll back, prepared units and pending kicks are lost,
  /// workers stop; leases stay until they expire. What committed stays.
  action crash: bool = all {
    j' = j,
    p' = p,
    d' = { ...d, workers: WORKERS.mapBy(_ => IDLE) },
    slots' = SLOTS.mapBy(_ => FREE_SLOT),
    kicks' = NOKICKS,
    g' = g,
    last' = { stimulus: "crash", slot: 0, outcome: NONE },
  }

  // ---- the durable consumer (the directive relay) ------------------------------------------

  action deliveryStep(name: str, d2: Delivery, g2: Ghost, outcome: str): bool = all {
    j' = j,
    p' = p,
    d' = d2,
    slots' = slots,
    kicks' = kicks,
    g' = g2,
    last' = { stimulus: name, slot: 0, outcome: outcome },
  }

  action register: bool = all {
    not(d.registered),
    deliveryStep("register", { ...d, registered: true }, g, NONE),
  }

  /// One bootstrap page: marks a stream that existed before the consumer.
  action bootstrap(s: str): bool = all {
    d.registered,
    not(d.boot.get(s)),
    deliveryStep("bootstrap",
      {
        ...d,
        boot: d.boot.set(s, true),
        marks: d.marks.set(s, imax(d.marks.get(s), j.heads.get(s))),
      },
      g, NONE),
  }

  def holdsLease(w: int): bool = {
    val wk = d.workers.get(w)
    val l = d.lease.get(wk.stream)
    wk.stream != "" and l.live and l.holder == w and l.fence == wk.fence
  }

  /// Claims a marked stream whose lease is absent or expired, with the next fence.
  action claim(w: int, s: str): bool = {
    val l = d.lease.get(s)
    all {
      d.registered,
      d.workers.get(w).stream == "",
      d.marks.get(s) > d.ckpt.get(s),
      not(l.live),
      deliveryStep("claim", {
        ...d,
        lease: d.lease.set(s, { holder: w, fence: l.fence + 1, live: true }),
        workers: d.workers.set(w, {
          stream: s, fence: l.fence + 1, through: d.marks.get(s), next: d.ckpt.get(s) + 1,
          done: NOREV,
        }),
      }, g, NONE),
    }
  }

  /// Renews the lease and runs the effect of the next revision (sending a `DirectiveIssued`).
  action effect(w: int): bool = {
    val wk = d.workers.get(w)
    val ev = j.log.get(wk.stream).get(wk.next)
    all {
      wk.stream != "",
      wk.done == NOREV,
      wk.next <= wk.through,
      holdsLease(w),
      deliveryStep("effect", { ...d, workers: d.workers.set(w, { ...wk, done: wk.next }) }, {
        ...g,
        delivered: g.delivered.set(wk.stream, g.delivered.get(wk.stream).set(wk.next, true)),
        sent: if (ev.dir) g.sent.set(wk.stream, g.sent.get(wk.stream).set(wk.next, true)) else g.sent,
        unfenced: g.unfenced or not(holdsLease(w)),
      }, NONE),
    }
  }

  /// Moves the checkpoint up to the delivered revision and clears the mark only up to it.
  action advance(w: int): bool = {
    val wk = d.workers.get(w)
    val s = wk.stream
    all {
      s != "",
      wk.done != NOREV,
      holdsLease(w),
      deliveryStep("advance", {
        ...d,
        ckpt: d.ckpt.set(s, imax(d.ckpt.get(s), wk.done)),
        marks: if (d.marks.get(s) <= wk.done) d.marks.set(s, NOREV) else d.marks,
        workers: d.workers.set(w, { ...wk, next: wk.done + 1, done: NOREV }),
      }, {
        ...g,
        ckptSeen: g.ckptSeen.set(s, imax(g.ckptSeen.get(s), wk.done)),
        unfenced: g.unfenced or not(holdsLease(w)),
      }, NONE),
    }
  }

  /// Ends the claim (its work done, or `handle` failed: `fail`); a lost lease ends it too.
  action endClaim(name: str, w: int): bool = {
    val wk = d.workers.get(w)
    all {
      wk.stream != "",
      wk.done == NOREV,
      deliveryStep(name, {
        ...d,
        lease:
          if (holdsLease(w)) d.lease.set(wk.stream, { ...d.lease.get(wk.stream), live: false })
          else d.lease,
        workers: d.workers.set(w, IDLE),
      }, g, if (holdsLease(w)) NONE else LEASE_LOST),
    }
  }

  action release(w: int): bool = endClaim("release", w)

  /// `handle` failed on the next event: the claim is released, the checkpoint stays, and the
  /// event is delivered again later. Nothing skips it (ES-I15).
  action failHandle(w: int): bool = all {
    d.workers.get(w).next <= d.workers.get(w).through,
    endClaim("failHandle", w),
  }

  /// The store clock passes the lease.
  action expire(s: str): bool = all {
    d.lease.get(s).live,
    deliveryStep("expire",
      { ...d, lease: d.lease.set(s, { ...d.lease.get(s), live: false }) }, g, NONE),
  }

  /// A worker dies alone (its lease runs out later).
  action crashWorker(w: int): bool = all {
    d.workers.get(w).stream != "",
    deliveryStep("crashWorker", { ...d, workers: d.workers.set(w, IDLE) }, g, NONE),
  }

  // ---- the online rebuild (ES-12, E9) -------------------------------------------------------

  action projStep(name: str, p2: Proj): bool = all {
    j' = j,
    p' = p2,
    d' = d,
    slots' = slots,
    kicks' = kicks,
    g' = g,
    last' = { stimulus: name, slot: 0, outcome: NONE },
  }

  /// A shadow generation appears; from now on new units dual-write it for rebuilt streams.
  action startRebuild: bool = all {
    p.shadow == 0,
    p.active == 1,
    projStep("startRebuild", { ...p, shadow: 2, rows: p.rows.set(2, STREAMS.mapBy(_ => UNBUILT)) }),
  }

  /// One enumerated stream folded from revision 0 into the shadow, recording what it applied.
  action rebuildStream(s: str): bool = all {
    p.shadow != 0,
    p.rows.get(p.shadow).get(s) == UNBUILT,
    projStep("rebuildStream", {
      ...p,
      rows: p.rows.set(p.shadow, p.rows.get(p.shadow).set(s, count(j.heads.get(s)))),
    }),
  }

  /// Every stream rebuilt: the active generation flips in one transaction (the fence).
  action cutover: bool = all {
    p.shadow != 0,
    STREAMS.forall(s => p.rows.get(p.shadow).get(s) != UNBUILT),
    projStep("cutover", { ...p, active: p.shadow, shadow: 0 }),
  }

  // ---- init and step relations --------------------------------------------------------------

  action init = all {
    j' = {
      heads: STREAMS.mapBy(_ => NOSTREAM),
      log: STREAMS.mapBy(_ => REVS.mapBy(_ => NOEV)),
      epochs: ZERO_S,
      idLoc: IDS.mapBy(_ => ("", NOREV)),
      receipts: CMDS.mapBy(_ => NORECEIPT),
      inbox: MSGS.mapBy(_ => 0),
      commits: 0,
    },
    p' = {
      rows: GENS.mapBy(gn => STREAMS.mapBy(_ => if (gn == 1) 0 else UNBUILT)),
      active: 1,
      shadow: 0,
    },
    d' = {
      registered: false,
      boot: STREAMS.mapBy(_ => false),
      marks: STREAMS.mapBy(_ => NOREV),
      ckpt: STREAMS.mapBy(_ => NOREV),
      lease: STREAMS.mapBy(_ => { holder: 0, fence: 0, live: false }),
      workers: WORKERS.mapBy(_ => IDLE),
    },
    slots' = SLOTS.mapBy(_ => FREE_SLOT),
    kicks' = NOKICKS,
    g' = {
      torn: false,
      claimBreach: false,
      ever: STREAMS.mapBy(_ => REVS.mapBy(_ => 0)),
      epochSeen: ZERO_S,
      epochBreach: false,
      accepts: CMDS.mapBy(_ => 0),
      otherIntentReplayed: false,
      staleRefusalReplayed: false,
      receiptBreach: false,
      msgCommits: MSGS.mapBy(_ => 0),
      delivered: NOREVS,
      sent: NOREVS,
      unfenced: false,
      ckptSeen: STREAMS.mapBy(_ => NOREV),
      kicked: NOKICKS,
      kickBreach: false,
    },
    last' = { stimulus: "init", slot: 0, outcome: NONE },
  }

  /// The units of the store: commands, caller-built units and inbox-only units, run in one
  /// step or as split transactions; reactors and crashes.
  action storeStep(
    t: int, c: int, i: int, s: str, r: str, b: int, m: int, e: int, id1: int, id2: int,
    two: bool, id3: int, ep: int, n: int,
  ): bool = any {
    command(t, c, i, s, r, b, m),
    rawUnit(t, s, e, id1, id2, two, id3, ep, b),
    noteMessage(t, m),
    appendUnit(t),
    begin(t),
    lock(t),
    write(t),
    commit(t),
    rollback(t),
    timeout(t),
    kick(n),
    crash,
  }

  action consumerStep(w: int, s: str): bool = any {
    register,
    bootstrap(s),
    claim(w, s),
    effect(w),
    advance(w),
    release(w),
    failHandle(w),
    expire(s),
    crashWorker(w),
  }

  action rebuildStep(s: str): bool = any { startRebuild, rebuildStream(s), cutover }

  /// The full contract: every stimulus, concurrent held writers (memory and PostgreSQL).
  action step = {
    nondet t = SLOTS.oneOf()
    nondet c = CMDS.oneOf()
    nondet i = INTENTS.oneOf()
    nondet s = STREAMS.oneOf()
    nondet r = Set("", other(s)).oneOf()
    nondet b = BUILDS.oneOf()
    nondet m = Set(0, 1, 2).oneOf()
    nondet e = Set(NOSTREAM, 0, 1).oneOf()
    nondet id1 = IDS.oneOf()
    nondet id2 = 0.to(4).oneOf()
    nondet two = Set(true, false).oneOf()
    nondet id3 = IDS.oneOf()
    nondet ep = BUILDS.oneOf()
    nondet n = COMMITS.oneOf()
    nondet w = WORKERS.oneOf()
    any {
      storeStep(t, c, i, s, r, b, m, e, id1, id2, two, id3, ep, n),
      consumerStep(w, s),
      rebuildStep(s),
    }
  }

  /// What SQLite can do: at most one held write transaction (`BEGIN IMMEDIATE`), so a unit
  /// begins, or appends in one step, only while no other transaction is open. An admitted
  /// restriction of `step`, not another model.
  action implStepSerial = {
    nondet t = SLOTS.oneOf()
    nondet c = CMDS.oneOf()
    nondet i = INTENTS.oneOf()
    nondet s = STREAMS.oneOf()
    nondet r = Set("", other(s)).oneOf()
    nondet b = BUILDS.oneOf()
    nondet m = Set(0, 1, 2).oneOf()
    nondet e = Set(NOSTREAM, 0, 1).oneOf()
    nondet id1 = IDS.oneOf()
    nondet id2 = 0.to(4).oneOf()
    nondet two = Set(true, false).oneOf()
    nondet id3 = IDS.oneOf()
    nondet ep = BUILDS.oneOf()
    nondet n = COMMITS.oneOf()
    nondet w = WORKERS.oneOf()
    any {
      command(t, c, i, s, r, b, m),
      rawUnit(t, s, e, id1, id2, two, id3, ep, b),
      noteMessage(t, m),
      all { not(anyOpen(t)), appendUnit(t) },
      all { not(anyOpen(t)), begin(t) },
      lock(t),
      write(t),
      commit(t),
      rollback(t),
      timeout(t),
      kick(n),
      crash,
      consumerStep(w, s),
      rebuildStep(s),
    }
  }

  /// The biased relation: commands on the directive path (no caller-built units, one-step
  /// appends, no rebuild), the consumer registered from the start of a walk, so a walk commits
  /// `DirectiveIssued` events and spends its depth on claims, effects, advances, failures,
  /// expiries and crashes. A uniform walk of `step` rarely strings register, a commit, a claim,
  /// an effect and an advance together.
  action relayStep = {
    nondet t = SLOTS.oneOf()
    nondet c = CMDS.oneOf()
    nondet s = STREAMS.oneOf()
    nondet n = COMMITS.oneOf()
    nondet w = WORKERS.oneOf()
    any {
      command(t, c, 2, s, "", 1, 0),
      command(t, c, 1, s, "", 1, 0),
      appendUnit(t),
      kick(n),
      crash,
      consumerStep(w, s),
    }
  }

  // ---- invariants ---------------------------------------------------------------------------

  /// ES-I1: every commit wrote all of its unit (every event, the `DirectiveIssued` among them,
  /// its receipt, its inbox entry, and nothing on another stream); a refusal writes nothing
  /// (`refuse` leaves the journal as it is).
  val ES_I1_atomicUnit = not(g.torn)

  /// ES-I2: revisions consecutive from 0, the head at the highest; of two units expecting the
  /// same revision of a stream at most one commits.
  val ES_I2_streamOrder = and {
    STREAMS.forall(s => REVS.forall(r => (j.log.get(s).get(r).id != 0) iff (r <= j.heads.get(s)))),
    not(g.claimBreach),
  }

  /// ES-I3: a transaction waits only on one holding a head it names or a key it writes, so no
  /// unit is delayed or refused (`Busy` ends only such a wait) for a stream it does not name.
  /// Stated apart from `lockConflict`, so a mutation of the lock rule is caught, not copied.
  val ES_I3_independentStreams = SLOTS.forall(t => {
    val o = slots.get(t).waitOn
    o != 0 implies or {
      slots.get(o).locks.intersect(named(slots.get(t).unit)) != Set(),
      keyConflict(t, o),
    }
  })

  /// ES-I4: an event id is bound to exactly the one place its event is.
  val ES_I4_globalEventIdentity = and {
    IDS.forall(i => {
      val at = j.idLoc.get(i)
      at._2 != NOREV implies j.log.get(at._1).get(at._2).id == i
    }),
    STREAMS.forall(s => REVS.forall(r => {
      val id = j.log.get(s).get(r).id
      id != 0 implies j.idLoc.get(id) == (s, r)
    })),
  }

  /// ES-I5: one accepted outcome per command id; a replay answers only the same intent, and a
  /// refusal only while every stream it read is at the revision read (otherwise it is decided
  /// afresh); an accepted receipt is never replaced, a refusal receipt only once stale.
  val ES_I5_commandIdentity = and {
    CMDS.forall(c => g.accepts.get(c) <= 1),
    not(g.otherIntentReplayed),
    not(g.receiptBreach),
  }

  /// ES-I5's refusal half on its own (ADR 0041): `refusalReplayedAtSameRevision`.
  val refusalReplayedAtSameRevision = not(g.staleRefusalReplayed)

  /// ES-I6: an inbox key is consumed by at most one committed unit.
  val ES_I6_messageIdentity = MSGS.forall(m => g.msgCommits.get(m) <= 1)

  /// ES-I7: no committed event changes or disappears.
  val ES_I7_historyPreservation =
    STREAMS.forall(s => REVS.forall(r => {
      val id = g.ever.get(s).get(r)
      id != 0 implies j.log.get(s).get(r).id == id
    }))

  /// ES-I11 (ES-12's part): the active rows are the fold of every stream; a rebuilt shadow row
  /// is too, so the cutover switches to rows equal to the live ones.
  val ES_I11_rebuildEquivalence = and {
    STREAMS.forall(s => p.rows.get(p.active).get(s) == count(j.heads.get(s))),
    p.shadow != 0 implies STREAMS.forall(s =>
      p.rows.get(p.shadow).get(s).in(Set(UNBUILT, count(j.heads.get(s))))),
  }

  /// ES-I12: a stream's epoch never decreases; nothing is appended above the build or to a
  /// stream above it.
  val ES_I12_epochSafety = and {
    STREAMS.forall(s => j.epochs.get(s) >= g.epochSeen.get(s)),
    not(g.epochBreach),
  }

  /// ES-I14: the checkpoint never passes the head nor moves back; an undelivered revision of a
  /// covered stream is marked; no effect starts and no checkpoint moves without the live lease;
  /// a claim's fence is the lease's newest.
  val ES_I14_durableDelivery = and {
    STREAMS.forall(s => d.ckpt.get(s) <= j.heads.get(s) and d.ckpt.get(s) >= g.ckptSeen.get(s)),
    STREAMS.forall(s =>
      (d.registered and d.boot.get(s) and d.ckpt.get(s) < j.heads.get(s))
        implies d.marks.get(s) == j.heads.get(s)),
    not(g.unfenced),
    WORKERS.forall(w => {
      val wk = d.workers.get(w)
      wk.stream != "" implies wk.fence <= d.lease.get(wk.stream).fence
    }),
  }

  /// ES-I15: the applied revisions of a stream are a prefix of its committed ones: no skip.
  val ES_I15_noSkip =
    STREAMS.forall(s => REVS.forall(r => r <= d.ckpt.get(s) implies g.delivered.get(s).get(r)))

  /// ES-I16: no wait-for cycle.
  val ES_I16_lockOrder = not(slots.get(1).waitOn == 2 and slots.get(2).waitOn == 1)

  /// ADR 0034: a directive is sent only for a committed `DirectiveIssued` event, and only
  /// committed events are delivered.
  val directivesFromJournal = and {
    STREAMS.forall(s => REVS.forall(r => g.sent.get(s).get(r) implies j.log.get(s).get(r).dir)),
    STREAMS.forall(s => REVS.forall(r =>
      g.delivered.get(s).get(r) implies j.log.get(s).get(r).id != 0)),
  }

  /// §5.5: the after-commit notification runs at most once per commit and never for a replay
  /// (a crash between commit and notification loses it; the marks still deliver).
  val reactorsNeverReplayed = and {
    not(g.kickBreach),
    COMMITS.forall(n => kicks.get(n) implies n <= j.commits),
  }

  // Every invariant of the model, so a new one cannot be left out of a stage.
  val allInvariants = and {
    ES_I1_atomicUnit,
    ES_I2_streamOrder,
    ES_I3_independentStreams,
    ES_I4_globalEventIdentity,
    ES_I5_commandIdentity,
    refusalReplayedAtSameRevision,
    ES_I6_messageIdentity,
    ES_I7_historyPreservation,
    ES_I11_rebuildEquivalence,
    ES_I12_epochSafety,
    ES_I14_durableDelivery,
    ES_I15_noSkip,
    ES_I16_lockOrder,
    directivesFromJournal,
    reactorsNeverReplayed,
  }

  // ---- negative worlds (kept out of every step relation) ------------------------------------

  /// `HeadTrustingWorker`: advances to the stream's head, not to the revision it delivered.
  action advanceToHead(w: int): bool = {
    val wk = d.workers.get(w)
    val s = wk.stream
    all {
      s != "",
      wk.done != NOREV,
      holdsLease(w),
      deliveryStep("advanceToHead", {
        ...d,
        ckpt: d.ckpt.set(s, j.heads.get(s)),
        marks: d.marks.set(s, NOREV),
        workers: d.workers.set(w, IDLE),
      }, { ...g, ckptSeen: g.ckptSeen.set(s, imax(g.ckptSeen.get(s), j.heads.get(s))) }, NONE),
    }
  }

  /// `UnfencedLedger`: a worker whose lease expired or was taken over still starts its effect.
  action effectUnfenced(w: int): bool = {
    val wk = d.workers.get(w)
    val ev = j.log.get(wk.stream).get(wk.next)
    all {
      wk.stream != "",
      wk.done == NOREV,
      wk.next <= wk.through,
      deliveryStep("effectUnfenced", { ...d, workers: d.workers.set(w, { ...wk, done: wk.next }) }, {
        ...g,
        delivered: g.delivered.set(wk.stream, g.delivered.get(wk.stream).set(wk.next, true)),
        sent: if (ev.dir) g.sent.set(wk.stream, g.sent.get(wk.stream).set(wk.next, true)) else g.sent,
        unfenced: g.unfenced or not(holdsLease(w)),
      }, NONE),
    }
  }

  // ---- scenarios ----------------------------------------------------------------------------

  /// One stimulus of a scenario and the outcome it must have ("" unchecked). Scenarios are data
  /// so that a `run` below and its exported regression trace (`event_store_traces`) take
  /// exactly the same steps.
  type Stim = {
    kind: str, t: int, c: int, i: int, s: str, r: str, b: int, m: int, e: int, id1: int,
    id2: int, two: bool, id3: int, ep: int, n: int, w: int, want: str,
  }

  pure val X: Stim = {
    kind: "", t: 1, c: 1, i: 1, s: "a", r: "", b: 1, m: 0, e: NOSTREAM, id1: 1, id2: 0,
    two: false, id3: 1, ep: 1, n: 1, w: 1, want: "",
  }

  /// `command(t, c, i, s, r, b, m)`, expecting `want` ("" when it only prepares a unit).
  pure def Cmd(t: int, c: int, i: int, s: str, r: str, b: int, m: int, want: str): Stim =
    { ...X, kind: "command", t: t, c: c, i: i, s: s, r: r, b: b, m: m, want: want }
  /// A caller-built unit on one stream: events `id1` (and `id2`) of epoch `ep`, build `b`.
  pure def Raw(t: int, s: str, e: int, id1: int, id2: int, ep: int, b: int): Stim =
    { ...X, kind: "rawUnit", t: t, s: s, e: e, id1: id1, id2: id2, ep: ep, b: b }
  /// A caller-built unit on both streams: `id1` to `s` at `e`, `id3` to the other at its head.
  pure def Raw2(t: int, s: str, e: int, id1: int, id3: int): Stim =
    { ...X, kind: "rawUnit", t: t, s: s, e: e, id1: id1, two: true, id3: id3 }
  pure def Note(t: int, m: int): Stim = { ...X, kind: "noteMessage", t: t, m: m }
  /// A journal step of slot `t`: appendUnit, begin, lock, write, commit, rollback, timeout.
  pure def Tx(kind: str, t: int, want: str): Stim = { ...X, kind: kind, t: t, want: want }
  pure def Kick(n: int): Stim = { ...X, kind: "kick", n: n }
  pure val Crash: Stim = { ...X, kind: "crash" }
  /// A consumer step: register, bootstrap, claim, effect, advance, release, failHandle, expire,
  /// crashWorker.
  pure def Dl(kind: str, w: int, s: str, want: str): Stim =
    { ...X, kind: kind, w: w, s: s, want: want }
  /// A rebuild step: startRebuild, rebuildStream, cutover.
  pure def Rb(kind: str, s: str): Stim = { ...X, kind: kind, s: s }

  /// Takes the stimulus `x`. Deterministic, so `quint test` samples each scenario once;
  /// `event_store_traces::recorded` wraps it to record the arguments as the picks `step`
  /// records.
  action stim(x: Stim): bool = {
    val t = x.t
    val c = x.c
    val i = x.i
    val s = x.s
    val r = x.r
    val b = x.b
    val m = x.m
    val e = x.e
    val id1 = x.id1
    val id2 = x.id2
    val two = x.two
    val id3 = x.id3
    val ep = x.ep
    val n = x.n
    val w = x.w
    if (x.kind == "command") command(t, c, i, s, r, b, m)
    else if (x.kind == "rawUnit") rawUnit(t, s, e, id1, id2, two, id3, ep, b)
    else if (x.kind == "noteMessage") noteMessage(t, m)
    else if (x.kind == "appendUnit") appendUnit(t)
    else if (x.kind == "begin") begin(t)
    else if (x.kind == "lock") lock(t)
    else if (x.kind == "write") write(t)
    else if (x.kind == "commit") commit(t)
    else if (x.kind == "rollback") rollback(t)
    else if (x.kind == "timeout") timeout(t)
    else if (x.kind == "kick") kick(n)
    else if (x.kind == "crash") crash
    else if (x.kind == "register") register
    else if (x.kind == "bootstrap") bootstrap(s)
    else if (x.kind == "claim") claim(w, s)
    else if (x.kind == "effect") effect(w)
    else if (x.kind == "advance") advance(w)
    else if (x.kind == "release") release(w)
    else if (x.kind == "failHandle") failHandle(w)
    else if (x.kind == "expire") expire(s)
    else if (x.kind == "crashWorker") crashWorker(w)
    else if (x.kind == "startRebuild") startRebuild
    else if (x.kind == "rebuildStream") rebuildStream(s)
    else if (x.kind == "cutover") cutover
    else if (x.kind == "advanceToHead") advanceToHead(w)
    else if (x.kind == "effectUnfenced") effectUnfenced(w)
    else all { false, crash }
  }

  def wanted(x: Stim): bool = x.want == "" or last.outcome == x.want

  /// A scenario from `init`: every stimulus taken, with the outcome it wants.
  run play(S: List[Stim]): bool = init.then(S.length().reps(k => stim(S[k]).expect(wanted(S[k]))))

  /// A split transaction of slot `t` from begin to commit, for a unit naming `locks` streams.
  pure def splitCommit(t: int, locks: int): List[Stim] =
    [Tx("begin", t, "")]
      .concat(if (locks == 2) [Tx("lock", t, ""), Tx("lock", t, "")] else [Tx("lock", t, "")])
      .concat([Tx("write", t, ""), Tx("commit", t, COMMITTED)])

  // ES-I2: two units create the same stream; the second loses (§7.3 `lostRace`).
  pure val LOST_RACE: List[Stim] = [
    Raw(1, "a", NOSTREAM, 1, 0, 1, 1), Raw(2, "a", NOSTREAM, 2, 0, 1, 1),
    Tx("appendUnit", 1, COMMITTED), Tx("appendUnit", 2, WRONG_EXPECTED_REVISION),
  ]
  run lostRaceTest = play(LOST_RACE)
    .expect(j.heads.get("a") == 0 and j.log.get("a").get(0).id == 1 and allInvariants)

  // ES-I2: `Exactly(0)` on an absent stream is refused; a refusal creates no head; `NoStream`
  // then creates it.
  pure val NO_STREAM_VERSUS_EMPTY: List[Stim] = [
    Raw(1, "a", 0, 1, 0, 1, 1), Tx("appendUnit", 1, WRONG_EXPECTED_REVISION),
    Cmd(1, 1, 2, "a", "", 1, 0, ""), Tx("appendUnit", 1, COMMITTED),
    Raw(1, "a", NOSTREAM, 3, 0, 1, 1), Tx("appendUnit", 1, COMMITTED),
  ]
  run noStreamVersusEmptyTest = play(NO_STREAM_VERSUS_EMPTY)
    .expect(j.heads.get("a") == 0 and j.receipts.get(1).refused and allInvariants)

  // ES-I3: a unit on `b` commits in one step while a transaction on `a` holds its head.
  pure val INDEPENDENT_STREAMS: List[Stim] = [
    Raw(1, "a", NOSTREAM, 1, 0, 1, 1), Tx("begin", 1, ""), Tx("lock", 1, ""), Tx("write", 1, ""),
    Raw(2, "b", NOSTREAM, 2, 0, 1, 1), Tx("appendUnit", 2, COMMITTED), Tx("commit", 1, COMMITTED),
  ]
  run independentStreamsTest = play(INDEPENDENT_STREAMS)
    .expect(j.heads.get("a") == 0 and j.heads.get("b") == 0 and allInvariants)

  // ES-I3: two held writers on disjoint streams, both written before either commits.
  pure val INDEPENDENT_HELD_WRITERS: List[Stim] = [
    Raw(1, "a", NOSTREAM, 1, 0, 1, 1), Raw(2, "b", NOSTREAM, 2, 0, 1, 1),
    Tx("begin", 1, ""), Tx("begin", 2, ""), Tx("lock", 1, NONE), Tx("lock", 2, NONE),
    Tx("write", 1, NONE), Tx("write", 2, NONE), Tx("commit", 2, COMMITTED), Tx("commit", 1, COMMITTED),
  ]
  run independentHeldWritersTest = play(INDEPENDENT_HELD_WRITERS)
    .expect(j.commits == 2 and allInvariants)

  // ES-I1: a batch fails after its second event is written: nothing of it stays.
  pure val BATCH_FAILS_AFTER_SECOND_EVENT: List[Stim] = [
    Raw(1, "a", NOSTREAM, 1, 2, 1, 1), Tx("begin", 1, ""), Tx("lock", 1, ""), Tx("write", 1, ""),
    Tx("rollback", 1, STORAGE),
  ]
  run batchFailsAfterSecondEventTest = play(BATCH_FAILS_AFTER_SECOND_EVENT)
    .expect(j.heads.get("a") == NOSTREAM and IDS.forall(i => j.idLoc.get(i)._2 == NOREV))
    .expect(allInvariants)

  // ES-I1: a failure before commit leaves nothing; the same unit then commits.
  pure val ROLLBACK: List[Stim] = [
    Raw2(1, "a", NOSTREAM, 1, 2), Tx("begin", 1, ""), Tx("lock", 1, ""), Tx("lock", 1, ""),
    Tx("rollback", 1, STORAGE), Raw2(1, "a", NOSTREAM, 1, 2),
  ].concat(splitCommit(1, 2))
  run rollbackTest = play(ROLLBACK)
    .expect(j.heads.get("a") == 0 and j.heads.get("b") == 0 and j.commits == 1 and allInvariants)

  // ES-I4: an id already bound on another stream is refused.
  pure val DUPLICATE_ACROSS_STREAMS: List[Stim] = [
    Raw(1, "a", NOSTREAM, 1, 0, 1, 1), Tx("appendUnit", 1, COMMITTED),
    Raw(1, "b", NOSTREAM, 1, 0, 1, 1), Tx("appendUnit", 1, DUPLICATE_EVENT_ID),
  ]
  run duplicateAcrossStreamsTest = play(DUPLICATE_ACROSS_STREAMS)
    .expect(j.heads.get("b") == NOSTREAM and j.idLoc.get(1) == ("a", 0) and allInvariants)

  // ES-I4: an id twice in one batch is malformed, before any storage access.
  pure val REPEATED_WITHIN_BATCH: List[Stim] = [
    Raw(1, "a", NOSTREAM, 1, 1, 1, 1), Tx("appendUnit", 1, MALFORMED),
  ]
  run repeatedWithinBatchTest = play(REPEATED_WITHIN_BATCH)
    .expect(j.heads.get("a") == NOSTREAM and allInvariants)

  // ES-I12: a build supporting epoch 2 raises the stream; a build supporting 1 is refused by
  // the store before the receipt, and by the journal; it may still read the stream; an
  // epoch-1 event appended later leaves the stream at epoch 2.
  pure val EPOCH_RAISE: List[Stim] = [
    Cmd(1, 1, 1, "a", "", 2, 0, ""), Tx("appendUnit", 1, COMMITTED),
    Cmd(1, 1, 1, "a", "", 1, 0, STREAM_EPOCH_UNSUPPORTED),
    Raw(1, "a", 0, 3, 0, 1, 1), Tx("appendUnit", 1, STREAM_EPOCH_UNSUPPORTED),
    Cmd(1, 2, 1, "b", "a", 1, 0, ""), Tx("appendUnit", 1, COMMITTED),
    Raw(1, "a", 0, 4, 0, 1, 2), Tx("appendUnit", 1, COMMITTED),
  ]
  run epochRaiseTest = play(EPOCH_RAISE)
    .expect(j.epochs.get("a") == 2 and j.heads.get("a") == 1 and j.epochs.get("b") == 1)
    .expect(allInvariants)

  // ES-I12: a batch requiring more than the build supports is refused before any write.
  pure val REFUSED_ABOVE_EPOCH: List[Stim] = [
    Raw(1, "a", NOSTREAM, 1, 0, 2, 1), Tx("appendUnit", 1, EPOCH_ABOVE_BUILD),
  ]
  run refusedAboveEpochTest = play(REFUSED_ABOVE_EPOCH)
    .expect(j.heads.get("a") == NOSTREAM and j.epochs.get("a") == 0 and allInvariants)

  // ES-I5: an accepted command is answered from its receipt for ever, even after its stream
  // moved; another intent under its id is refused.
  pure val RECEIPT_REPLAY: List[Stim] = [
    Cmd(1, 1, 1, "a", "", 1, 0, ""), Tx("appendUnit", 1, COMMITTED),
    Cmd(1, 1, 1, "a", "", 1, 0, REPLAYED),
    Raw(1, "a", 0, 3, 0, 1, 1), Tx("appendUnit", 1, COMMITTED),
    Cmd(1, 1, 1, "a", "", 1, 0, REPLAYED),
    Cmd(1, 1, 2, "a", "", 1, 0, COMMAND_ID_REUSED),
  ]
  run receiptReplayTest = play(RECEIPT_REPLAY)
    .expect(g.accepts.get(1) == 1 and j.heads.get("a") == 1 and allInvariants)

  // ES-I5, ES-I16: the same command on two streams; the second writer waits on the receipt
  // key, then is answered from the first's receipt.
  pure val RACING_COMMANDS: List[Stim] = [
    Cmd(1, 1, 1, "a", "", 1, 0, ""), Cmd(2, 1, 1, "b", "", 1, 0, ""),
    Tx("begin", 1, ""), Tx("begin", 2, ""), Tx("lock", 1, NONE), Tx("lock", 2, NONE),
    Tx("write", 1, NONE), Tx("write", 2, WAITING), Tx("commit", 1, COMMITTED),
    Tx("write", 2, REPLAYED),
  ]
  run racingCommandsTest = play(RACING_COMMANDS)
    .expect(g.accepts.get(1) == 1 and j.heads.get("b") == NOSTREAM and allInvariants)

  // ES-I5 (E3): a refusal is replayed while its stream stays; once it moved the command is
  // decided afresh, accepted, and its acceptance replaces the refusal receipt.
  pure val REFUSED_THEN_ACCEPTED: List[Stim] = [
    Cmd(1, 1, 2, "a", "", 1, 0, ""), Tx("appendUnit", 1, COMMITTED),
    Cmd(1, 1, 2, "a", "", 1, 0, REPLAYED),
    Raw(1, "a", NOSTREAM, 3, 0, 1, 1), Tx("appendUnit", 1, COMMITTED),
    Cmd(1, 1, 2, "a", "", 1, 0, NONE), Tx("appendUnit", 1, COMMITTED),
    Cmd(1, 1, 2, "a", "", 1, 0, REPLAYED),
  ]
  run refusedThenAcceptedTest = play(REFUSED_THEN_ACCEPTED)
    .expect(not(j.receipts.get(1).refused) and j.heads.get("a") == 1 and allInvariants)

  // ES-I5 (ADR 0041): a refusal bound to both streams it read is replayed while both stay; the
  // other stream moving is enough to decide it afresh. The journal judges the same way: a stale
  // refusal prepared before the move is not answered from the receipt.
  pure val REFUSAL_REPLAYED_AT_SAME_REVISION: List[Stim] = [
    Cmd(1, 1, 2, "a", "b", 1, 0, ""), Tx("appendUnit", 1, COMMITTED),
    Cmd(1, 1, 2, "a", "b", 1, 0, REPLAYED),
    Raw(1, "b", NOSTREAM, 3, 0, 1, 1), Tx("appendUnit", 1, COMMITTED),
    Cmd(1, 1, 2, "a", "b", 1, 0, NONE), Tx("appendUnit", 1, COMMITTED),
  ]
  run refusalReplayedAtSameRevisionTest = play(REFUSAL_REPLAYED_AT_SAME_REVISION)
    .expect(not(j.receipts.get(1).refused) and j.heads.get("a") == 0 and refusalReplayedAtSameRevision)
    .expect(allInvariants)

  // ES-I6: a redelivered message is refused as a duplicate with no write, also for a unit
  // that only records its key.
  pure val INBOX_REDELIVERY: List[Stim] = [
    Cmd(1, 1, 1, "a", "", 1, 1, ""), Tx("appendUnit", 1, COMMITTED),
    Cmd(1, 2, 1, "a", "", 1, 1, ""), Tx("appendUnit", 1, DUPLICATE_MESSAGE),
    Note(1, 2), Tx("appendUnit", 1, COMMITTED), Note(1, 2), Tx("appendUnit", 1, DUPLICATE_MESSAGE),
  ]
  run inboxRedeliveryTest = play(INBOX_REDELIVERY)
    .expect(j.heads.get("a") == 0 and j.inbox.get(1) == 1 and j.inbox.get(2) == 2 and allInvariants)

  // ES-I1: a unit on two streams loses on one: neither is written.
  pure val MULTI_STREAM_CONFLICT: List[Stim] = [
    Raw2(1, "a", NOSTREAM, 1, 2), Raw(2, "b", NOSTREAM, 3, 0, 1, 1),
    Tx("appendUnit", 2, COMMITTED), Tx("appendUnit", 1, WRONG_EXPECTED_REVISION),
  ]
  run multiStreamConflictTest = play(MULTI_STREAM_CONFLICT)
    .expect(j.heads.get("a") == NOSTREAM and j.heads.get("b") == 0 and allInvariants)

  // §3.2: a `NoStream` read is invalidated by a concurrent create of the read stream.
  pure val READ_SET_INVALIDATED: List[Stim] = [
    Cmd(1, 1, 1, "a", "b", 1, 0, ""), Raw(2, "b", NOSTREAM, 3, 0, 1, 1),
    Tx("appendUnit", 2, COMMITTED), Tx("appendUnit", 1, WRONG_EXPECTED_REVISION),
  ]
  run readSetInvalidatedTest = play(READ_SET_INVALIDATED)
    .expect(j.heads.get("a") == NOSTREAM and not(j.receipts.get(1).present) and allInvariants)

  // ES-I2, ES-I16: a writer queues on a held head, then loses its expected revision.
  pure val HEAD_QUEUE: List[Stim] = [
    Raw(1, "a", NOSTREAM, 1, 0, 1, 1), Raw(2, "a", NOSTREAM, 2, 0, 1, 1),
    Tx("begin", 1, ""), Tx("lock", 1, NONE), Tx("begin", 2, ""), Tx("lock", 2, WAITING),
    Tx("write", 1, NONE), Tx("commit", 1, COMMITTED), Tx("lock", 2, NONE),
    Tx("write", 2, WRONG_EXPECTED_REVISION),
  ]
  run headQueueTest = play(HEAD_QUEUE)
    .expect(j.heads.get("a") == 0 and allInvariants)

  // ES-I4, ES-I16: a unit waits on an id another open unit wrote; the holder rolls back and
  // the waiter commits.
  pure val ID_WAIT: List[Stim] = [
    Raw(1, "a", NOSTREAM, 1, 0, 1, 1), Raw(2, "b", NOSTREAM, 1, 0, 1, 1),
    Tx("begin", 1, ""), Tx("lock", 1, NONE), Tx("write", 1, NONE),
    Tx("begin", 2, ""), Tx("lock", 2, NONE), Tx("write", 2, WAITING),
    Tx("rollback", 1, STORAGE), Tx("write", 2, NONE), Tx("commit", 2, COMMITTED),
  ]
  run idWaitTest = play(ID_WAIT)
    .expect(j.idLoc.get(1) == ("b", 0) and allInvariants)

  // ES-I16: units naming the same two streams "in opposite order" lock in `StreamId` order:
  // the second waits, never a cycle, and both finish.
  pure val OPPOSITE_ORDER_UNITS: List[Stim] = [
    Raw2(1, "a", NOSTREAM, 1, 2), Raw2(2, "b", NOSTREAM, 3, 4),
    Tx("begin", 1, ""), Tx("begin", 2, ""), Tx("lock", 1, NONE), Tx("lock", 2, WAITING),
    Tx("lock", 1, NONE), Tx("write", 1, NONE), Tx("commit", 1, COMMITTED),
    Tx("lock", 2, NONE), Tx("lock", 2, NONE), Tx("write", 2, WRONG_EXPECTED_REVISION),
  ]
  run oppositeOrderUnitsTest = play(OPPOSITE_ORDER_UNITS)
    .expect(j.commits == 1 and SLOTS.forall(t => slots.get(t).phase == FREE) and allInvariants)

  // §3.7 step 6, §5.5: a crash between the commit and its notification loses the kick, never
  // the delivery (the mark is durable); a replay kicks nothing; a kick runs once.
  pure val CRASH_BEFORE_REACTORS: List[Stim] = [
    Dl("register", 1, "a", ""),
    Cmd(1, 1, 1, "a", "", 1, 0, ""), Tx("appendUnit", 1, COMMITTED), Crash,
    Cmd(1, 1, 1, "a", "", 1, 0, REPLAYED),
    Dl("claim", 1, "a", NONE), Dl("effect", 1, "a", NONE), Dl("advance", 1, "a", NONE),
    Cmd(1, 2, 1, "a", "", 1, 0, ""), Tx("appendUnit", 1, COMMITTED), Kick(2),
  ]
  run crashBeforeReactorsTest = play(CRASH_BEFORE_REACTORS)
    .expect(g.kicked == NOKICKS.set(2, true) and kicks == NOKICKS)
    .expect(g.sent == NOREVS.set("a", NOREVS.get("a").set(0, true)))
    .expect(allInvariants)

  // ADR 0034: a `DirectiveIssued` event, its receipt and the observation's inbox key commit in
  // one transaction; a crash before the commit leaves none of them, and the retry commits all.
  pure val DIRECTIVE_IN_UNIT: List[Stim] = [
    Cmd(1, 1, 1, "a", "", 1, 1, ""), Tx("begin", 1, ""), Tx("lock", 1, NONE), Tx("write", 1, NONE),
    Crash, Cmd(1, 1, 1, "a", "", 1, 1, ""),
  ].concat(splitCommit(1, 1))
  run directiveInUnitTest = play(DIRECTIVE_IN_UNIT)
    .expect(j.log.get("a").get(0).dir and j.inbox.get(1) == 1 and j.receipts.get(1).present)
    .expect(allInvariants)

  // ES-I14: streams written before the consumer registered are marked by its bootstrap;
  // a stream written after registration is marked by its unit.
  pure val BOOTSTRAP_AFTER_WRITES: List[Stim] = [
    Cmd(1, 1, 1, "a", "", 1, 0, ""), Tx("appendUnit", 1, COMMITTED), Dl("register", 1, "a", ""),
    Cmd(1, 2, 1, "b", "", 1, 0, ""), Tx("appendUnit", 1, COMMITTED),
    Dl("bootstrap", 1, "a", ""), Dl("claim", 1, "a", ""), Dl("effect", 1, "a", ""),
    Dl("advance", 1, "a", ""), Dl("release", 1, "a", NONE),
    Dl("claim", 1, "b", ""), Dl("effect", 1, "b", ""), Dl("advance", 1, "b", ""),
  ]
  run bootstrapAfterWritesTest = play(BOOTSTRAP_AFTER_WRITES)
    .expect(g.sent.get("a").get(0) and g.sent.get("b").get(0) and d.ckpt.get("b") == 0)
    .expect(allInvariants)

  // ES-I14: a commit between the claim's fetch and its effect stays marked past the advance.
  pure val COMMIT_BETWEEN_FETCH_AND_APPLY: List[Stim] = [
    Dl("register", 1, "a", ""), Cmd(1, 1, 1, "a", "", 1, 0, ""), Tx("appendUnit", 1, COMMITTED),
    Dl("claim", 1, "a", ""), Cmd(1, 2, 1, "a", "", 1, 0, ""), Tx("appendUnit", 1, COMMITTED),
    Dl("effect", 1, "a", ""), Dl("advance", 1, "a", ""), Dl("release", 1, "a", NONE),
    Dl("claim", 1, "a", ""), Dl("effect", 1, "a", ""), Dl("advance", 1, "a", ""),
  ]
  run commitBetweenFetchAndApplyTest = play(COMMIT_BETWEEN_FETCH_AND_APPLY)
    .expect(d.ckpt.get("a") == 1 and d.marks.get("a") == NOREV and allInvariants)

  // ES-I14: a mark set while an event is processed survives the advance to that event.
  pure val MARK_DURING_PROCESSING: List[Stim] = [
    Dl("register", 1, "a", ""), Cmd(1, 1, 1, "a", "", 1, 0, ""), Tx("appendUnit", 1, COMMITTED),
    Dl("claim", 1, "a", ""), Dl("effect", 1, "a", ""),
    Cmd(1, 2, 1, "a", "", 1, 0, ""), Tx("appendUnit", 1, COMMITTED), Dl("advance", 1, "a", ""),
  ]
  run markDuringProcessingTest = play(MARK_DURING_PROCESSING)
    .expect(d.ckpt.get("a") == 0 and d.marks.get("a") == 1 and allInvariants)

  // ES-I14: a worker whose lease expired and was taken over can neither effect nor advance;
  // its release reports the lease lost; the new holder delivers with a higher fence.
  pure val STALE_WORKER_FENCED: List[Stim] = [
    Dl("register", 1, "a", ""), Cmd(1, 1, 1, "a", "", 1, 0, ""), Tx("appendUnit", 1, COMMITTED),
    Dl("claim", 1, "a", ""), Dl("expire", 1, "a", ""), Dl("claim", 2, "a", ""),
    Dl("release", 1, "a", LEASE_LOST), Dl("effect", 2, "a", ""), Dl("advance", 2, "a", ""),
  ]
  run staleWorkerFencedTest = play(STALE_WORKER_FENCED)
    .expect(d.lease.get("a").fence == 2 and d.ckpt.get("a") == 0 and allInvariants)

  // ES-I14: a worker dies after its effect and before its advance: the event is delivered again.
  pure val WORKER_CRASH_REDELIVERS: List[Stim] = [
    Dl("register", 1, "a", ""), Cmd(1, 1, 1, "a", "", 1, 0, ""), Tx("appendUnit", 1, COMMITTED),
    Dl("claim", 1, "a", ""), Dl("effect", 1, "a", ""), Dl("crashWorker", 1, "a", ""),
    Dl("expire", 1, "a", ""), Dl("claim", 2, "a", ""), Dl("effect", 2, "a", ""), Dl("advance", 2, "a", ""),
  ]
  run workerCrashRedeliversTest = play(WORKER_CRASH_REDELIVERS)
    .expect(d.ckpt.get("a") == 0 and allInvariants)

  // ES-I15: a consumer that keeps failing never moves past the event.
  pure val FAILING_CONSUMER_NO_SKIP: List[Stim] = [
    Dl("register", 1, "a", ""), Cmd(1, 1, 1, "a", "", 1, 0, ""), Tx("appendUnit", 1, COMMITTED),
    Dl("claim", 1, "a", ""), Dl("failHandle", 1, "a", NONE), Dl("claim", 1, "a", ""),
    Dl("failHandle", 1, "a", NONE),
  ]
  run failingConsumerNoSkipTest = play(FAILING_CONSUMER_NO_SKIP)
    .expect(d.ckpt.get("a") == NOREV and d.marks.get("a") == 0 and allInvariants)

  // ES-I11 (ES-12): units dual-write rebuilt streams; a stream rebuilt later folds what was
  // written meanwhile; the cutover switches to rows equal to the live ones.
  pure val SHADOW_REBUILD_DUAL_WRITE: List[Stim] = [
    Cmd(1, 1, 1, "a", "", 1, 0, ""), Tx("appendUnit", 1, COMMITTED), Rb("startRebuild", "a"),
    Rb("rebuildStream", "a"), Cmd(1, 2, 1, "a", "", 1, 0, ""), Tx("appendUnit", 1, COMMITTED),
    Raw(1, "b", NOSTREAM, 3, 0, 1, 1), Tx("appendUnit", 1, COMMITTED),
    Rb("rebuildStream", "b"), Rb("cutover", "a"),
  ]
  run shadowRebuildDualWriteTest = play(SHADOW_REBUILD_DUAL_WRITE)
    .expect(p.active == 2 and p.rows.get(2).get("a") == 2 and p.rows.get(2).get("b") == 1)
    .expect(allInvariants)

  // ES-I11 (ES-12): the cutover fence refuses a unit computed for the old generation set, in
  // one step and under a held transaction; decided again, it commits.
  pure val CUTOVER_FENCE: List[Stim] = [
    Rb("startRebuild", "a"), Rb("rebuildStream", "a"),
    Raw(2, "a", NOSTREAM, 3, 0, 1, 1), Tx("begin", 2, ""), Tx("lock", 2, ""), Tx("write", 2, NONE),
    Cmd(1, 1, 1, "b", "", 1, 0, ""), Rb("rebuildStream", "b"), Tx("appendUnit", 1, GENERATION_MOVED),
    Cmd(1, 1, 1, "b", "", 1, 0, ""), Rb("cutover", "a"), Tx("appendUnit", 1, GENERATION_MOVED),
    Tx("commit", 2, GENERATION_MOVED),
    Cmd(1, 1, 1, "b", "", 1, 0, ""), Tx("appendUnit", 1, COMMITTED),
  ]
  run cutoverFenceTest = play(CUTOVER_FENCE)
    .expect(p.active == 2 and p.rows.get(2).get("b") == 1 and j.heads.get("a") == NOSTREAM)
    .expect(allInvariants)

  // Negative world `headTrustingReader` (`HeadTrustingWorker`): advancing to the head skips a
  // revision committed after the claim: ES-I15 breaks.
  pure val HEAD_TRUSTING_READER: List[Stim] = [
    Dl("register", 1, "a", ""), Cmd(1, 1, 1, "a", "", 1, 0, ""), Tx("appendUnit", 1, COMMITTED),
    Dl("claim", 1, "a", ""), Cmd(1, 2, 1, "a", "", 1, 0, ""), Tx("appendUnit", 1, COMMITTED),
    Dl("effect", 1, "a", ""), Dl("advanceToHead", 1, "a", ""),
  ]
  run headTrustingReaderViolatesTest = play(HEAD_TRUSTING_READER)
    .expect(not(ES_I15_noSkip) and not(allInvariants))

  // Negative world `unfencedWorker` (`UnfencedLedger`): a worker whose lease was taken over
  // still starts its effect: ES-I14 breaks.
  pure val UNFENCED_WORKER: List[Stim] = [
    Dl("register", 1, "a", ""), Cmd(1, 1, 1, "a", "", 1, 0, ""), Tx("appendUnit", 1, COMMITTED),
    Dl("claim", 1, "a", ""), Dl("expire", 1, "a", ""), Dl("claim", 2, "a", ""),
    Dl("effectUnfenced", 1, "a", ""),
  ]
  run unfencedWorkerViolatesTest = play(UNFENCED_WORKER)
    .expect(not(ES_I14_durableDelivery) and not(allInvariants))
}

/**
 * The positive scenarios of `event_store` as scripted step relations, so `quint run --mbt`
 * exports each as an ITF regression trace carrying the action and picks of every step (the
 * pattern ver §6.2 names; the negative worlds are not exported, they break `allInvariants` by
 * design). A script takes exactly the stimuli of the `run` of the same name: both read the
 * same `List[Stim]`, so they cannot drift. `recorded` turns each argument into a pick named as
 * `step` names it.
 *
 * Each trace is regenerated into `formal/traces/regression/` (from the repository root; the
 * scripts are deterministic, the seed is recorded for byte-for-byte freshness, VER-01):
 *
 *   npx --prefix formal --no -- quint run --backend typescript --main event_store_traces \
 *     --init tinit --step <script> --max-steps <its length> --mbt --seed 0x5ce1 \
 *     --invariant allInvariants --out-itf formal/traces/regression/event_store-<script>.itf.json \
 *     formal/event_store.qnt
 *
 * `formal/traces/regression/manifest.toml` lists each with its length.
 */
module event_store_traces {
  import event_store as E

  /// The `allInvariants` of the model, checked on every exported state.
  val allInvariants = E::allInvariants

  /// The script's position: the stimulus it takes next.
  var pc: int

  action tinit = all { E::init, pc' = 0 }

  /// `E::stim(x)` with its arguments recorded as the picks of `step`, under the model's
  /// action names: an `any` makes the taken arm's action `mbt::actionTaken` (the model's own
  /// `stim` is an `if` chain, so `quint test` samples a scenario once). The arms call the same
  /// actions as `E::stim`, and each exported state's `last.stimulus` (set by the action itself)
  /// equals its `mbt::actionTaken`, which the export checks.
  action recorded(x: E::Stim): bool = {
    nondet t = Set(x.t).oneOf()
    nondet c = Set(x.c).oneOf()
    nondet i = Set(x.i).oneOf()
    nondet s = Set(x.s).oneOf()
    nondet r = Set(x.r).oneOf()
    nondet b = Set(x.b).oneOf()
    nondet m = Set(x.m).oneOf()
    nondet e = Set(x.e).oneOf()
    nondet id1 = Set(x.id1).oneOf()
    nondet id2 = Set(x.id2).oneOf()
    nondet two = Set(x.two).oneOf()
    nondet id3 = Set(x.id3).oneOf()
    nondet ep = Set(x.ep).oneOf()
    nondet n = Set(x.n).oneOf()
    nondet w = Set(x.w).oneOf()
    any {
      all { x.kind == "command", E::command(t, c, i, s, r, b, m) },
      all { x.kind == "rawUnit", E::rawUnit(t, s, e, id1, id2, two, id3, ep, b) },
      all { x.kind == "noteMessage", E::noteMessage(t, m) },
      all { x.kind == "appendUnit", E::appendUnit(t) },
      all { x.kind == "begin", E::begin(t) },
      all { x.kind == "lock", E::lock(t) },
      all { x.kind == "write", E::write(t) },
      all { x.kind == "commit", E::commit(t) },
      all { x.kind == "rollback", E::rollback(t) },
      all { x.kind == "timeout", E::timeout(t) },
      all { x.kind == "kick", E::kick(n) },
      all { x.kind == "crash", E::crash },
      all { x.kind == "register", E::register },
      all { x.kind == "bootstrap", E::bootstrap(s) },
      all { x.kind == "claim", E::claim(w, s) },
      all { x.kind == "effect", E::effect(w) },
      all { x.kind == "advance", E::advance(w) },
      all { x.kind == "release", E::release(w) },
      all { x.kind == "failHandle", E::failHandle(w) },
      all { x.kind == "expire", E::expire(s) },
      all { x.kind == "crashWorker", E::crashWorker(w) },
      all { x.kind == "startRebuild", E::startRebuild },
      all { x.kind == "rebuildStream", E::rebuildStream(s) },
      all { x.kind == "cutover", E::cutover },
      all { x.kind == "advanceToHead", E::advanceToHead(w) },
      all { x.kind == "effectUnfenced", E::effectUnfenced(w) },
    }
  }

  action script(S: List[E::Stim]): bool = all { pc < S.length(), recorded(S[pc]), pc' = pc + 1 }

  action lostRace = script(E::LOST_RACE)
  action noStreamVersusEmpty = script(E::NO_STREAM_VERSUS_EMPTY)
  action independentStreams = script(E::INDEPENDENT_STREAMS)
  action independentHeldWriters = script(E::INDEPENDENT_HELD_WRITERS)
  action batchFailsAfterSecondEvent = script(E::BATCH_FAILS_AFTER_SECOND_EVENT)
  action rollback = script(E::ROLLBACK)
  action duplicateAcrossStreams = script(E::DUPLICATE_ACROSS_STREAMS)
  action repeatedWithinBatch = script(E::REPEATED_WITHIN_BATCH)
  action epochRaise = script(E::EPOCH_RAISE)
  action refusedAboveEpoch = script(E::REFUSED_ABOVE_EPOCH)
  action receiptReplay = script(E::RECEIPT_REPLAY)
  action racingCommands = script(E::RACING_COMMANDS)
  action refusedThenAccepted = script(E::REFUSED_THEN_ACCEPTED)
  action refusalReplayedAtSameRevision = script(E::REFUSAL_REPLAYED_AT_SAME_REVISION)
  action inboxRedelivery = script(E::INBOX_REDELIVERY)
  action multiStreamConflict = script(E::MULTI_STREAM_CONFLICT)
  action readSetInvalidated = script(E::READ_SET_INVALIDATED)
  action headQueue = script(E::HEAD_QUEUE)
  action idWait = script(E::ID_WAIT)
  action oppositeOrderUnits = script(E::OPPOSITE_ORDER_UNITS)
  action crashBeforeReactors = script(E::CRASH_BEFORE_REACTORS)
  action directiveInUnit = script(E::DIRECTIVE_IN_UNIT)
  action bootstrapAfterWrites = script(E::BOOTSTRAP_AFTER_WRITES)
  action commitBetweenFetchAndApply = script(E::COMMIT_BETWEEN_FETCH_AND_APPLY)
  action markDuringProcessing = script(E::MARK_DURING_PROCESSING)
  action staleWorkerFenced = script(E::STALE_WORKER_FENCED)
  action workerCrashRedelivers = script(E::WORKER_CRASH_REDELIVERS)
  action failingConsumerNoSkip = script(E::FAILING_CONSUMER_NO_SKIP)
  action shadowRebuildDualWrite = script(E::SHADOW_REBUILD_DUAL_WRITE)
  action cutoverFence = script(E::CUTOVER_FENCE)
}
