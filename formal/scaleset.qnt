// -*- mode: Bluespec; -*-
/**
 * The scale-set model (KER-06): one pool's `ScaleSet` stream (doc/domain-model.md §6.1), the
 * listener that polls, records, acquires and acks for it (plan §7.2, doc/github-scaleset.md
 * §3–§5), and GitHub's side of the protocol: jobs, the message queue with its 50-message cap,
 * redelivery until delete, and sessions. The Rust aggregate is `mayfly_control_domain::scaleset`
 * and the pure demand functions are `mayfly_control_domain::demand`; `offerStep` is
 * `demand::decide_offer`, `desiredOf` is `demand::desired`, `wireMax` is `demand::wire_max`.
 *
 * The world is GitHub: each job's status (`jobs`), job events not yet in a message (`buffer`),
 * the messages it emitted (`msgs`, ids increasing), the unacknowledged ones (`queue`, redelivered
 * until deleted) and the live session (`ghSession`). The journal is the stream's state (`ss`) and
 * the listener's inbox receipts (`inbox`, keyed `(session, message)`). The listener holds at
 * most one message (`inHand`).
 *
 * Assumptions where `doc/github-scaleset.md` marks a fact [unverified: GH-00] (§12), each the
 * safe side:
 *
 * - A1 (§12 item 1): message ids increase per scale set, across sessions; a message is
 *   redelivered until acked, also in a later session. So the statistics cursor persists across
 *   sessions, and dedupe by `(session, message)` receipt alone may record a message twice in
 *   two sessions: no reservation may depend on a message being recorded once.
 * - A2 (§12 items 3, 6): a message's statistics may or may not count the jobs mayfly acquired
 *   since. The acquire budget counts every unbound reservation on top of `totalAssignedJobs`,
 *   so it may count a job twice (acquire less), never once too few (acquire more).
 * - A3 (§12 item 2): a create while a session lives answers 409; an abandoned session ends on
 *   its own. mayfly deletes only a session it journaled, never one it has no record of.
 * - A4 (§12 item 3): acquire answers the subset of the requested ids it acquired; re-acquiring
 *   is harmless. An acquire whose reply is lost leaves its ids pending (counted) until an
 *   answer or a job fact names them.
 * - A5 (§12 item 5): an offer whose `eventName` is missing or unknown is unclassifiable and
 *   declined like an untrusted one (`TRUST` gives each job its class).
 *
 * Every message here carries statistics. The Rust aggregate also takes an envelope without
 * them; it keeps the statistics' floor (`statistics_after`) apart from the cursor so that such a
 * message orders statistics exactly as this model's keys do.
 *
 * `CAP = 2` stands for the service's 50 messages per batch: a backlog larger than a batch is
 * truncated, which is why demand never counts job messages. Ghost state (`g`) records history
 * for the invariants and never feeds a guard.
 */
module scaleset {
  // ---- constants ----------------------------------------------------------------------------

  /// Jobs, by `runnerRequestId`.
  pure val REQS = Set(1, 2, 3)
  pure val TRUSTED = "trusted"
  pure val UNTRUSTED = "untrusted"
  /// The trust class of each job's offer (ADR 0027): 3 is a `pull_request_target`.
  pure val TRUST: int -> str = Map(1 -> TRUSTED, 2 -> TRUSTED, 3 -> UNTRUSTED)
  pure val MAX_MSG = 5
  pure val MSG_IDS = 1.to(MAX_MSG)
  pure val MAX_SESSION = 3
  /// Job messages per batch (the service's 50).
  pure val CAP = 2
  /// Unknown message types per session before it is `Degraded{protocol_drift}`.
  pure val DRIFT_THRESHOLD = 2
  pure val DELIVERABLES = 0.to(2)
  pure val NONE = 0

  // Job message types (doc/github-scaleset.md §4.3), and one the build does not know.
  pure val AVAILABLE = "JobAvailable"
  pure val ASSIGNED = "JobAssigned"
  pure val STARTED = "JobStarted"
  pure val COMPLETED = "JobCompleted"
  pure val UNKNOWN_TYPE = "unknown"

  // A job's status at GitHub.
  pure val J_NONE = "none"
  pure val J_AVAILABLE = "available"
  pure val J_ACQUIRED = "acquired"
  pure val J_RUNNING = "running"
  pure val J_DONE = "done"

  // Session phases (plan §7.2; `Opening` is the instant between `BeginOpen` and the answer,
  // and its lost answer is `openLost`).
  pure val ENSURED = "Ensured"
  pure val ACTIVE = "Active"
  pure val BACKOFF = "Backoff"
  pure val DEGRADED = "Degraded"

  // The pool's mode as the capacity input carries it (ADR 0029).
  pure val OPEN = "open"
  pure val PAUSED = "paused"
  pure val DRAINING = "draining"
  pure val SAFE = "safe"
  pure val MODES = Set(OPEN, PAUSED, DRAINING, SAFE)

  // How `record` decides: the aggregate, and the curated mutants of verification §9 kept out of
  // every step relation.
  pure val CORRECT = "correct"
  /// Mutant 5: demand counts `JobAssigned` messages.
  pure val COUNT_MESSAGES = "countMessages"
  /// Mutant 6: demand adds `totalRunningJobs`.
  pure val ADD_RUNNING = "addRunning"
  /// Mutant 19: every offer is acquired.
  pure val ACQUIRE_EVERY = "acquireEvery"
  /// Mutant 7: the inbox receipt is ignored, a redelivery applies again.
  pure val IGNORE_INBOX = "ignoreInbox"

  // ---- state --------------------------------------------------------------------------------

  type Item = { kind: str, req: int }
  type Stats = { assigned: int, running: int }
  type Msg = { items: List[Item], stats: Stats }

  /// The `ScaleSet` stream's state.
  type SS = {
    phase: str,
    // the journaled live session; NONE when there is none
    session: int,
    // a journaled session to delete before the next create (§3.2); NONE if none
    stale: int,
    // the highest message id recorded, in any session (A1): the statistics' cursor
    cursor: int,
    // the latest statistics applied
    stats: Stats,
    deliverable: int,
    reportedMax: int,
    mode: str,
    desired: int,
    // planned by a recorded message, acquire outcome not observed yet (A4)
    acquiring: Set[int],
    // acquired, no `JobStarted` or `JobCompleted` yet
    acquired: Set[int],
    // unknown message types since the session opened
    drift: int,
  }

  type Ghost = {
    // what the last action was: "init" | "command" | "observation" | "world" | "listener"
    lastKind: str,
    // the kind of the last action, if it took an id out of `acquiring`; "" otherwise
    pendingLeftBy: str,
    ackedUnrecorded: bool,
    doubleReserved: bool,
    overAcquired: bool,
    untrustedReserved: bool,
    acquiredWhileHeld: bool,
    stoleSession: bool,
    appliedTwice: bool,
    polledUnwritable: bool,
    // a session was ever opened (desired is defined from then on)
    opened: bool,
    // the newest statistics recorded: a message's key is 2 * id, a session open's is
    // 2 * (highest id recorded before it) + 1 (A1)
    latest: Stats,
    latestKey: int,
    recordedIds: Set[int],
    applied: Set[(int, int)],
    // the last poll's maxCapacity, and what it was sent under
    sentMax: int,
    sentDeliverable: int,
    sentMode: str,
  }

  var jobs: int -> str
  var buffer: List[Item]
  var msgs: int -> Msg
  var nextMsg: int
  var queue: Set[int]
  var ghSession: int
  var nextSession: int
  var journalWritable: bool
  var inHand: int
  var inHandRecorded: bool
  var ss: SS
  var inbox: Set[(int, int)]
  var g: Ghost

  // ---- helpers ------------------------------------------------------------------------------

  pure val NO_ITEMS: List[Item] = []

  pure def minOf(a: int, b: int): int = if (a < b) a else b
  pure def maxOf(xs: Set[int]): int = xs.fold(NONE, (m, x) => if (x > m) x else m)
  pure def lowest(xs: Set[int]): int = xs.fold(MAX_MSG + 1, (m, x) => if (x < m) x else m)

  /// GitHub's statistics: `totalAssignedJobs` counts jobs waiting for a runner and running ones.
  pure def assignedSet(j: int -> str): Set[int] =
    REQS.filter(r => j.get(r) == J_ACQUIRED or j.get(r) == J_RUNNING)
  pure def statsOf(j: int -> str): Stats = {
    assigned: assignedSet(j).size(),
    running: REQS.filter(r => j.get(r) == J_RUNNING).size(),
  }

  /// `demand::wire_max`: the `maxCapacity` sent on every poll (ADR 0029): the capacity policy's
  /// reported max while open or paused (paused, it reports only live runners), 0 while draining
  /// or in safe mode.
  pure def wireMax(s: SS): int = if (s.mode == OPEN or s.mode == PAUSED) s.reportedMax else 0

  /// `demand::desired`: min(deliverable, totalAssignedJobs); no standby (ADR 0028), never a count
  /// of job messages, never `totalRunningJobs` (plan §7.2).
  pure def desiredOf(how: str, deliverable: int, stats: Stats, assignedItems: int): int =
    if (how == COUNT_MESSAGES) minOf(deliverable, assignedItems)
    else if (how == ADD_RUNNING) minOf(deliverable, stats.assigned + stats.running)
    else minOf(deliverable, stats.assigned)

  /// What a message's items do to the stream, folded in arrival order.
  type Acc = {
    acquiring: Set[int],
    acquired: Set[int],
    take: Set[int],
    declined: Set[int],
    assignedItems: int,
    unknown: int,
    // ghost: an offer taken while already reserved, past the budget, untrusted, or while held
    dup: bool,
    untrusted: bool,
    held: bool,
  }

  /// `demand::decide_offer` (`PlanAcquire` for one offer): I32 first (only the pool's tier),
  /// then no second reservation of a request id, then nothing while held (pause, drain, safe
  /// mode), then I23: acquire only while assigned plus every unbound reservation (A2) is below
  /// deliverable.
  pure def offerStep(s: SS, assigned: int, how: str, a: Acc, r: int): Acc = {
    val reserved = a.acquiring.union(a.acquired)
    val take = { ...a, acquiring: a.acquiring.union(Set(r)), take: a.take.union(Set(r)),
      dup: a.dup or reserved.contains(r), untrusted: a.untrusted or TRUST.get(r) != TRUSTED,
      held: a.held or s.mode != OPEN }
    val decline = { ...a, declined: a.declined.union(Set(r)) }
    if (how == ACQUIRE_EVERY) take
    else if (TRUST.get(r) != TRUSTED) decline
    else if (reserved.contains(r)) a
    else if (s.mode != OPEN) decline
    else if (assigned + reserved.size() < s.deliverable) take
    else decline
  }

  /// One item of a message: an offer is planned; `JobStarted` and `JobCompleted` are
  /// observations that release the job's reservation (a pending one included: the job was
  /// acquired, or is gone); `JobAssigned` is a fact only; an unknown type is counted.
  pure def itemStep(s: SS, assigned: int, how: str, a: Acc, it: Item): Acc =
    if (it.kind == AVAILABLE) offerStep(s, assigned, how, a, it.req)
    else if (it.kind == ASSIGNED) { ...a, assignedItems: a.assignedItems + 1 }
    else if (it.kind == STARTED or it.kind == COMPLETED)
      { ...a, acquiring: a.acquiring.exclude(Set(it.req)), acquired: a.acquired.exclude(Set(it.req)) }
    else { ...a, unknown: a.unknown + 1 }

  pure def initAcc(s: SS): Acc = {
    acquiring: s.acquiring, acquired: s.acquired, take: Set(), declined: Set(),
    assignedItems: 0, unknown: 0, dup: false, untrusted: false, held: false,
  }

  pure def ghostAfter(g0: Ghost, kind: str, before: SS, after: SS): Ghost = {
    ...g0,
    lastKind: kind,
    pendingLeftBy: if (before.acquiring.exists(r => not(after.acquiring.contains(r)))) kind else "",
  }

  // ---- frames -------------------------------------------------------------------------------

  action keepWorld = all {
    jobs' = jobs, buffer' = buffer, msgs' = msgs, nextMsg' = nextMsg, queue' = queue,
    ghSession' = ghSession, nextSession' = nextSession, journalWritable' = journalWritable,
  }

  action keepListener = all { inHand' = inHand, inHandRecorded' = inHandRecorded }

  action keepJournal = all { ss' = ss, inbox' = inbox }

  /// A change of the world (GitHub) only.
  action world(j: int -> str, b: List[Item], gs: int, ns: int): bool = all {
    jobs' = j, buffer' = b, msgs' = msgs, nextMsg' = nextMsg, queue' = queue,
    ghSession' = gs, nextSession' = ns, journalWritable' = journalWritable,
    keepListener, keepJournal,
    g' = ghostAfter(g, "world", ss, ss),
  }

  // ---- init ---------------------------------------------------------------------------------

  /// A scale set with an open session (session 1, initial statistics: nothing assigned), a
  /// capacity input of 2 deliverable, and a backlog of three offers: the first batch holds two
  /// (the cap), the third waits in the buffer.
  action init = {
    val stats0 = { assigned: 0, running: 0 }
    all {
      jobs' = REQS.mapBy(_ => J_AVAILABLE),
      buffer' = [{ kind: AVAILABLE, req: 3 }],
      msgs' = MSG_IDS.mapBy(i =>
        if (i == 1) { items: [{ kind: AVAILABLE, req: 1 }, { kind: AVAILABLE, req: 2 }], stats: stats0 }
        else { items: NO_ITEMS, stats: stats0 }),
      nextMsg' = 2,
      queue' = Set(1),
      ghSession' = 1,
      nextSession' = 2,
      journalWritable' = true,
      inHand' = NONE,
      inHandRecorded' = false,
      ss' = {
        phase: ACTIVE, session: 1, stale: NONE, cursor: NONE, stats: stats0, deliverable: 2,
        reportedMax: 2, mode: OPEN, desired: 0, acquiring: Set(), acquired: Set(), drift: 0,
      },
      inbox' = Set(),
      g' = {
        lastKind: "init", pendingLeftBy: "", ackedUnrecorded: false, doubleReserved: false,
        overAcquired: false, untrustedReserved: false, acquiredWhileHeld: false,
        stoleSession: false, appliedTwice: false, polledUnwritable: false, opened: true,
        latest: stats0, latestKey: 1, recordedIds: Set(), applied: Set(), sentMax: 0,
        sentDeliverable: 0, sentMode: OPEN,
      },
    }
  }

  // ---- GitHub -------------------------------------------------------------------------------

  /// The service puts at most `CAP` pending job events in a new message, with its statistics now.
  action emit = {
    val n = minOf(CAP, buffer.length())
    all {
      buffer.length() > 0,
      nextMsg <= MAX_MSG,
      jobs' = jobs,
      buffer' = buffer.slice(n, buffer.length()),
      msgs' = msgs.set(nextMsg, { items: buffer.slice(0, n), stats: statsOf(jobs) }),
      nextMsg' = nextMsg + 1,
      queue' = queue.union(Set(nextMsg)),
      ghSession' = ghSession, nextSession' = nextSession, journalWritable' = journalWritable,
      keepListener, keepJournal,
      g' = ghostAfter(g, "world", ss, ss),
    }
  }

  /// A runner takes an acquired job.
  action runnerStarts(r: int): bool = all {
    jobs.get(r) == J_ACQUIRED,
    world(jobs.set(r, J_RUNNING), buffer.append({ kind: STARTED, req: r }), ghSession, nextSession),
  }

  /// The job finishes.
  action jobFinishes(r: int): bool = all {
    jobs.get(r) == J_RUNNING,
    world(jobs.set(r, J_DONE), buffer.append({ kind: COMPLETED, req: r }), ghSession, nextSession),
  }

  /// Reassignment (doc/github-scaleset.md §4.3): an acquired job no runner took in time is
  /// completed as canceled and queued again (here under the same request id, the harder case).
  action jobRequeued(r: int): bool = all {
    jobs.get(r) == J_ACQUIRED,
    world(jobs.set(r, J_AVAILABLE),
      buffer.append({ kind: COMPLETED, req: r }).append({ kind: AVAILABLE, req: r }),
      ghSession, nextSession),
  }

  /// The service starts sending a message type this build does not know (R7).
  action unknownTypeQueued = all {
    buffer.length() < 3,
    world(jobs, buffer.append({ kind: UNKNOWN_TYPE, req: NONE }), ghSession, nextSession),
  }

  /// The live session ends at GitHub: its token expired, or an abandoned session timed out (A3).
  action sessionEnds = all {
    ghSession != NONE,
    world(jobs, buffer, NONE, nextSession),
  }

  /// The journal becomes unwritable (safe mode with an unwritable journal), or writable again.
  action journalToggles = all {
    jobs' = jobs, buffer' = buffer, msgs' = msgs, nextMsg' = nextMsg, queue' = queue,
    ghSession' = ghSession, nextSession' = nextSession,
    journalWritable' = not(journalWritable),
    keepListener, keepJournal,
    g' = ghostAfter(g, "world", ss, ss),
  }

  // ---- the session (§3) ---------------------------------------------------------------------

  /// Create a session (`BeginOpen`, then the answer). With no live session GitHub answers one,
  /// and `RecordSessionOpened{statistics}` recomputes desired from its initial statistics (plan
  /// §7.2 step 5). With one alive, 409 (A3): `RecordSessionLoss{conflicted}`, still in Backoff.
  action openSession = {
    val stats0 = statsOf(jobs)
    val key = 2 * maxOf(g.recordedIds) + 1
    val opened = { ...ss, phase: ACTIVE, session: nextSession, stats: stats0, drift: 0,
      desired: minOf(ss.deliverable, stats0.assigned) }
    all {
      journalWritable,
      ss.phase == ENSURED or ss.phase == BACKOFF,
      ss.stale == NONE,
      nextSession <= MAX_SESSION,
      jobs' = jobs, buffer' = buffer, msgs' = msgs, nextMsg' = nextMsg, queue' = queue,
      journalWritable' = journalWritable,
      keepListener,
      inbox' = inbox,
      if (ghSession == NONE) all {
        ghSession' = nextSession,
        nextSession' = nextSession + 1,
        ss' = opened,
        g' = { ...ghostAfter(g, "observation", ss, opened), opened: true, latest: stats0, latestKey: key },
      } else all {
        ghSession' = ghSession,
        nextSession' = nextSession,
        ss' = { ...ss, phase: BACKOFF },
        g' = ghostAfter(g, "observation", ss, { ...ss, phase: BACKOFF }),
      },
    }
  }

  /// A create whose answer was lost: GitHub holds a session mayfly has no record of (A3).
  action openLost = all {
    ss.phase == ENSURED or ss.phase == BACKOFF,
    ss.stale == NONE,
    ghSession == NONE,
    nextSession <= MAX_SESSION,
    world(jobs, buffer, nextSession, nextSession + 1),
  }

  /// Delete the journaled stale session before a new create (§3.2); a 404 (it already ended)
  /// completes the delete too. Never a session mayfly has no record of.
  action deleteStale = deleteSession(ss.stale)

  /// `DELETE` of session `s`, then `RecordSessionDeleted{s}`.
  action deleteSession(s: int): bool = {
    val ghAfter = if (ghSession == s) NONE else ghSession
    all {
      journalWritable,
      s != NONE,
      jobs' = jobs, buffer' = buffer, msgs' = msgs, nextMsg' = nextMsg, queue' = queue,
      nextSession' = nextSession, journalWritable' = journalWritable,
      ghSession' = ghAfter,
      keepListener,
      ss' = { ...ss, stale: NONE },
      inbox' = inbox,
      g' = { ...ghostAfter(g, "observation", ss, { ...ss, stale: NONE }),
        stoleSession: g.stoleSession or (ghSession != NONE and ghAfter == NONE and ghSession != ss.stale) },
    }
  }

  /// The listener's calls answer 401 after one refresh, or 404: the session is lost
  /// (`RecordSessionLoss{expired}`). The in-hand message is dropped; it will be redelivered.
  action sessionLoss = {
    val lost = { ...ss, phase: BACKOFF, stale: ss.session, session: NONE }
    all {
      journalWritable,
      ss.phase == ACTIVE or ss.phase == DEGRADED,
      ghSession != ss.session,
      keepWorld,
      inHand' = NONE, inHandRecorded' = false,
      ss' = lost, inbox' = inbox,
      g' = ghostAfter(g, "observation", ss, lost),
    }
  }

  /// The daemon restarts: the in-hand message is lost; the journaled session is stale and is
  /// deleted before a new one is created (`RecordSessionLoss{restarted}`).
  action restart = {
    val lost = if (ss.phase == ACTIVE or ss.phase == DEGRADED)
      { ...ss, phase: BACKOFF, stale: ss.session, session: NONE } else ss
    all {
      journalWritable,
      keepWorld,
      inHand' = NONE, inHandRecorded' = false,
      ss' = lost, inbox' = inbox,
      g' = ghostAfter(g, "command", ss, lost),
    }
  }

  /// The operator clears protocol drift (`RecordSessionRecovered`).
  action recoverDrift = {
    val back = { ...ss, phase: ACTIVE, drift: 0 }
    all {
      journalWritable,
      ss.phase == DEGRADED,
      keepWorld, keepListener,
      ss' = back, inbox' = inbox,
      g' = ghostAfter(g, "command", ss, back),
    }
  }

  // ---- messages (§4) ------------------------------------------------------------------------

  pure def listening(s: SS): bool = s.phase == ACTIVE or s.phase == DEGRADED

  /// Long poll with `maxCapacity` = the wire max. GitHub answers the oldest unacknowledged
  /// message (a recorded one whose ack was lost included), or 202 with nothing. Polling stops
  /// while the journal is unwritable (ADR 0029).
  action poll = all {
    listening(ss),
    ghSession == ss.session,
    inHand == NONE,
    journalWritable,
    keepWorld,
    inHand' = if (queue == Set()) NONE else lowest(queue),
    inHandRecorded' = false,
    keepJournal,
    g' = { ...ghostAfter(g, "listener", ss, ss),
      sentMax: wireMax(ss), sentDeliverable: ss.deliverable, sentMode: ss.mode,
      polledUnwritable: g.polledUnwritable or not(journalWritable) },
  }

  /// `RecordMessage{session, message, statistics, items}`, in one unit: the inbox receipt, the
  /// statistics if newer than the cursor (A1), job facts, the acquire plan (planned ids become
  /// `acquiring`, journaled before the call) and declined offers, unknown types, desired. A
  /// receipt that exists already is a `DuplicateMessage`: nothing more is written.
  action recordWith(how: str): bool = {
    val key = (ss.session, inHand)
    val m = msgs.get(inHand)
    val duplicate = inbox.contains(key) and how != IGNORE_INBOX
    val newer = inHand > ss.cursor
    val stats = if (newer) m.stats else ss.stats
    val acc = m.items.foldl(initAcc(ss), (a, it) => itemStep(ss, stats.assigned, how, a, it))
    val drift = ss.drift + acc.unknown
    val after = {
      ...ss,
      cursor: if (newer) inHand else ss.cursor,
      stats: stats,
      acquiring: acc.acquiring,
      acquired: acc.acquired,
      drift: drift,
      phase: if (ss.phase == ACTIVE and drift >= DRIFT_THRESHOLD) DEGRADED else ss.phase,
      desired: desiredOf(how, ss.deliverable, stats, acc.assignedItems),
    }
    val ghAssigned = assignedSet(jobs)
    val newestKey = 2 * inHand
    all {
      journalWritable,
      listening(ss),
      inHand != NONE,
      not(inHandRecorded),
      keepWorld,
      inHand' = inHand,
      inHandRecorded' = true,
      if (duplicate) all {
        ss' = ss,
        inbox' = inbox,
        g' = ghostAfter(g, "observation", ss, ss),
      } else all {
        ss' = after,
        inbox' = inbox.union(Set(key)),
        g' = {
          ...ghostAfter(g, "observation", ss, after),
          doubleReserved: g.doubleReserved or acc.dup,
          untrustedReserved: g.untrustedReserved or acc.untrusted,
          acquiredWhileHeld: g.acquiredWhileHeld or acc.held,
          overAcquired: g.overAcquired or (acc.take != Set() and
            ghAssigned.size() + acc.acquiring.union(acc.acquired).exclude(ghAssigned).size() > ss.deliverable),
          appliedTwice: g.appliedTwice or g.applied.contains(key),
          applied: g.applied.union(Set(key)),
          recordedIds: g.recordedIds.union(Set(inHand)),
          latest: if (newestKey > g.latestKey) m.stats else g.latest,
          latestKey: if (newestKey > g.latestKey) newestKey else g.latestKey,
        },
      },
    }
  }

  action record = recordWith(CORRECT)

  /// `DeleteMessage` once the record (or its receipt) is durable (I12). A lost ack leaves the
  /// message queued: it is redelivered.
  action ack(lost: bool): bool = all {
    inHand != NONE,
    inHandRecorded,
    ghSession == ss.session,
    jobs' = jobs, buffer' = buffer, msgs' = msgs, nextMsg' = nextMsg,
    queue' = if (lost) queue else queue.exclude(Set(inHand)),
    ghSession' = ghSession, nextSession' = nextSession, journalWritable' = journalWritable,
    inHand' = NONE, inHandRecorded' = false,
    keepJournal,
    g' = { ...ghostAfter(g, "listener", ss, ss),
      ackedUnrecorded: g.ackedUnrecorded or not(g.recordedIds.contains(inHand)) },
  }

  /// `AcquireJobs` for the pending ids, only while open (nothing is acquired while paused,
  /// draining or in safe mode: ADR 0029). GitHub acquires those still available and answers
  /// them (A4); `RecordAcquireResult` moves the answered ones to `acquired` and releases the
  /// rest. A lost answer changes GitHub but not the journal: the ids stay pending.
  action acquireCall(lost: bool): bool = {
    val got = ss.acquiring.filter(r => jobs.get(r) == J_AVAILABLE)
    val after = { ...ss, acquiring: Set(), acquired: ss.acquired.union(got) }
    val recorded = not(lost) and journalWritable
    all {
      ss.mode == OPEN,
      ss.acquiring != Set(),
      listening(ss),
      ghSession == ss.session,
      jobs' = REQS.mapBy(r => if (got.contains(r)) J_ACQUIRED else jobs.get(r)),
      buffer' = got.fold(buffer, (b, r) => b.append({ kind: ASSIGNED, req: r })),
      msgs' = msgs, nextMsg' = nextMsg, queue' = queue,
      ghSession' = ghSession, nextSession' = nextSession, journalWritable' = journalWritable,
      keepListener,
      inbox' = inbox,
      ss' = if (recorded) after else ss,
      g' = { ...ghostAfter(g, "observation", ss, if (recorded) after else ss),
        acquiredWhileHeld: g.acquiredWhileHeld or ss.mode != OPEN },
    }
  }

  // ---- capacity (ADR 0024, ADR 0029) --------------------------------------------------------

  /// `ApplyCapacity{revision, deliverable, reported_max, mode}` from the capacity policy
  /// (`capacity::plan`'s carve-out, ≤ deliverable): desired is recomputed from the latest
  /// statistics.
  action applyCapacity(d: int, rm: int, mode: str): bool = {
    val after = { ...ss, deliverable: d, reportedMax: rm, mode: mode,
      desired: minOf(d, ss.stats.assigned) }
    all {
      journalWritable,
      rm <= d,
      keepWorld, keepListener,
      ss' = after, inbox' = inbox,
      g' = ghostAfter(g, "command", ss, after),
    }
  }

  // ---- step relations -----------------------------------------------------------------------

  /// Every stimulus.
  action step = {
    nondet r = REQS.oneOf()
    nondet flag = Set(true, false).oneOf()
    nondet d = DELIVERABLES.oneOf()
    nondet rm = DELIVERABLES.oneOf()
    nondet mode = MODES.oneOf()
    any {
      emit,
      runnerStarts(r),
      jobFinishes(r),
      jobRequeued(r),
      unknownTypeQueued,
      sessionEnds,
      journalToggles,
      openSession,
      openLost,
      deleteStale,
      sessionLoss,
      restart,
      recoverDrift,
      poll,
      record,
      ack(flag),
      acquireCall(flag),
      applyCapacity(d, rm, mode),
    }
  }

  /// The message path, biased: no session churn, no capacity change and no outage, so a walk
  /// polls, records, acquires and acks through redelivery (lost acks and acquire answers), the
  /// truncated backlog, runners starting and finishing, reassignment and unknown types.
  action messageStep = {
    nondet r = REQS.oneOf()
    nondet flag = Set(true, false).oneOf()
    any {
      emit,
      runnerStarts(r),
      jobFinishes(r),
      jobRequeued(r),
      unknownTypeQueued,
      poll,
      record,
      ack(flag),
      acquireCall(flag),
    }
  }

  // ---- invariants ---------------------------------------------------------------------------

  /// I12: a message is acked only once its record (or its receipt) is durable.
  val i12RecordBeforeAck = not(g.ackedUnrecorded)

  /// noDoubleReservationPerRequest: a request id is planned only when no reservation of it is
  /// open, whatever redelivery, a second session or reassignment shows (A1).
  val noDoubleReservationPerRequest = and {
    not(g.doubleReserved),
    ss.acquiring.intersect(ss.acquired) == Set(),
  }

  /// I23: acquisition only while assigned < deliverable; checked against GitHub's own count at
  /// the moment of the plan: what is assigned there plus every reservation it does not hold yet
  /// fits the deliverable.
  val i23AcquireBelowDeliverable = not(g.overAcquired)

  /// I32: an offer outside the pool's tier (v0.x: trusted only) is never reserved.
  val i32TrustedOnly = and {
    not(g.untrustedReserved),
    ss.acquiring.union(ss.acquired).forall(r => TRUST.get(r) == TRUSTED),
  }

  /// desiredFromLatestStatistics: desired = min(deliverable, totalAssignedJobs) of the newest
  /// statistics recorded (A1): never a count of messages, never plus running jobs, never an older
  /// message's.
  val desiredFromLatestStatistics =
    g.opened implies ss.desired == minOf(ss.deliverable, g.latest.assigned)

  /// I07 and ADR 0029: the maxCapacity sent never exceeds deliverable, and is 0 while draining
  /// or in safe mode.
  val i07WireWithinDeliverable = and {
    g.sentMax <= g.sentDeliverable,
    (g.sentMode == DRAINING or g.sentMode == SAFE) implies g.sentMax == 0,
  }

  /// ADR 0029: nothing is planned or acquired while paused, draining or in safe mode; and no
  /// poll while the journal is unwritable.
  val heldAcquiresNothing = and {
    not(g.acquiredWhileHeld),
    not(g.polledUnwritable),
  }

  /// I13 for demand: a message applies once per receipt.
  val messageAppliesOnce = not(g.appliedTwice)

  /// A pending acquisition leaves `acquiring` only on an observation (an acquire answer or a
  /// job fact), never on a command or a timeout (I10's rule for the acquire effect).
  val acquireUnknownLeavesOnlyOnObservation =
    g.pendingLeftBy == "" or g.pendingLeftBy == "observation"

  /// A3: mayfly deletes only a session it journaled.
  val neverStealsASession = not(g.stoleSession)

  // Every invariant of the model, so a new one cannot be left out of a stage.
  val allInvariants = and {
    i12RecordBeforeAck,
    noDoubleReservationPerRequest,
    i23AcquireBelowDeliverable,
    i32TrustedOnly,
    desiredFromLatestStatistics,
    i07WireWithinDeliverable,
    heldAcquiresNothing,
    messageAppliesOnce,
    acquireUnknownLeavesOnlyOnObservation,
    neverStealsASession,
  }

  // ---- scenarios ----------------------------------------------------------------------------

  /// Poll, record and ack the oldest message.
  action handle = poll.then(record).then(ack(false))

  // The happy path: the first batch's two trusted offers are planned within deliverable 2 and
  // acquired; the next batch's statistics count them, and desired follows the statistics.
  run acquireWithinDeliverableTest = {
    init.then(poll).then(record)
      .expect(ss.acquiring == Set(1, 2) and ss.desired == 0 and inbox.contains((1, 1)))
      .then(ack(false)).expect(queue == Set())
      .then(acquireCall(false))
      .expect(ss.acquiring == Set() and ss.acquired == Set(1, 2) and jobs.get(1) == J_ACQUIRED)
      .then(emit).then(handle)
      .expect(ss.stats.assigned == 2 and ss.desired == 2 and ss.cursor == 2 and allInvariants)
  }

  // I23: with deliverable 1 the second offer is declined for capacity; the untrusted offer is
  // declined for its tier (I32) whatever the capacity.
  run declinedPastDeliverableAndTierTest = {
    init.then(applyCapacity(1, 1, OPEN)).then(handle)
      .expect(ss.acquiring == Set(1) and allInvariants)
      .then(applyCapacity(2, 2, OPEN)).then(emit).then(handle)
      .expect(ss.acquiring == Set(1) and not(ss.acquiring.union(ss.acquired).contains(3)))
      .expect(allInvariants)
  }

  // More assigned jobs than one batch shows (the 50-message cap): desired follows
  // `totalAssignedJobs`, not the batch.
  run truncatedBacklogDemandFromStatisticsTest = {
    init.then(handle).then(acquireCall(false)).then(emit)
      .expect(msgs.get(2).stats.assigned == 2 and msgs.get(2).items.length() == CAP)
      .then(handle).expect(ss.desired == 2 and desiredFromLatestStatistics)
  }

  // Redelivery after a lost ack is a duplicate: nothing is written, the ack is retried.
  run redeliveryAfterLostAckIsDuplicateTest = {
    init.then(poll).then(record).then(ack(true)).expect(queue == Set(1))
      .then(poll).expect(inHand == 1)
      .then(record).expect(ss.acquiring == Set(1, 2) and g.applied == Set((1, 1)))
      .then(ack(false)).expect(queue == Set() and allInvariants)
  }

  // A1: the message is redelivered in a new session, so its receipt key is new; its offers are
  // already reserved and are not reserved again, and its older statistics do not replace the
  // new session's initial ones.
  run redeliveryInNewSessionReservesOnceTest = {
    init.then(poll).then(record).then(ack(true))
      .then(restart).expect(ss.phase == BACKOFF and ss.stale == 1 and ghSession == 1)
      .then(deleteStale).expect(ghSession == NONE)
      .then(openSession).expect(ss.phase == ACTIVE and ss.session == 2)
      .then(poll).expect(inHand == 1)
      .then(record)
      .expect(inbox.contains((2, 1)) and ss.acquiring == Set(1, 2) and ss.cursor == 1)
      .expect(noDoubleReservationPerRequest and desiredFromLatestStatistics and allInvariants)
  }

  // A3: a create whose answer was lost leaves a session mayfly has no record of; the next
  // create answers 409 and the session backs off, never deleting it; once it ends, a create
  // succeeds and desired is recomputed from its initial statistics.
  run sessionConflictBacksOffTest = {
    init.then(handle).then(acquireCall(false))
      .then(restart).then(deleteStale)
      .then(openLost).expect(ghSession == 2 and ss.phase == BACKOFF and ss.stale == NONE)
      .then(openSession).expect(ss.phase == BACKOFF and ghSession == 2)
      .then(sessionEnds).then(openSession)
      .expect(ss.phase == ACTIVE and ss.session == 3 and ss.desired == 2)
      .expect(neverStealsASession and allInvariants)
  }

  // An expired session is lost, deleted (404: it already ended) and re-opened; desired comes
  // from the new session's initial statistics.
  run sessionExpiryReopensWithInitialStatisticsTest = {
    init.then(handle).then(acquireCall(false))
      .then(sessionEnds).then(sessionLoss).expect(ss.phase == BACKOFF and ss.stale == 1)
      .then(deleteStale).then(openSession)
      .expect(ss.session == 2 and ss.stats.assigned == 2 and ss.desired == 2 and allInvariants)
  }

  // R7: unknown message types are recorded, acked and counted; past the threshold the session
  // is Degraded and keeps recording; the operator clears it.
  run unknownTypesDegradeTheSessionTest = {
    init.then(unknownTypeQueued).then(unknownTypeQueued).then(emit).then(emit)
      .then(handle).then(handle).expect(ss.drift == 1 and ss.phase == ACTIVE)
      .then(handle).expect(ss.drift == 2 and ss.phase == DEGRADED and queue == Set())
      .then(recoverDrift).expect(ss.phase == ACTIVE and ss.drift == 0 and allInvariants)
  }

  // ADR 0029: draining sends 0 and acquires nothing; the message is still recorded and acked.
  run drainSendsZeroAndAcquiresNothingTest = {
    init.then(applyCapacity(2, 2, DRAINING)).then(poll).expect(g.sentMax == 0)
      .then(record).expect(ss.acquiring == Set() and ss.desired == 0)
      .then(ack(false)).expect(queue == Set() and allInvariants)
  }

  // ADR 0029: paused sends the live runners (the capacity policy's reported max) and acquires
  // nothing.
  run pauseSendsRunnersAndAcquiresNothingTest = {
    init.then(applyCapacity(2, 1, PAUSED)).then(poll).expect(g.sentMax == 1)
      .then(record).expect(ss.acquiring == Set())
      .then(ack(false)).expect(allInvariants)
  }

  // ADR 0029: safe mode with a writable journal records and acks, sends 0 and acquires nothing;
  // with an unwritable journal nothing is polled or recorded (GitHub keeps the message).
  run safeModeRecordsButNeverAcquiresTest = {
    init.then(applyCapacity(2, 2, SAFE)).then(handle)
      .expect(g.sentMax == 0 and ss.acquiring == Set() and queue == Set())
      .then(emit).then(journalToggles)
      .expect(not(journalWritable) and queue == Set(2) and allInvariants)
  }

  // Reassignment: an acquired job no runner took is completed as canceled and queued again;
  // its reservation is released by the completion and it may be planned again, once.
  run requeuedJobIsReservedOnceAgainTest = {
    init.then(applyCapacity(1, 1, OPEN)).then(handle).then(acquireCall(false))
      .expect(ss.acquired == Set(1))
      .then(jobRequeued(1)).expect(jobs.get(1) == J_AVAILABLE)
      .then(emit).then(handle).expect(ss.acquired == Set(1))
      .then(emit).then(handle)
      .expect(buffer == NO_ITEMS and ss.acquired == Set() and ss.acquiring == Set(1))
      .expect(noDoubleReservationPerRequest and allInvariants)
  }

  // A lost acquire answer leaves the ids pending (counted against deliverable); the retried
  // call answers them.
  run lostAcquireAnswerStaysPendingTest = {
    init.then(handle).then(acquireCall(true))
      .expect(ss.acquiring == Set(1, 2) and jobs.get(1) == J_ACQUIRED)
      .then(acquireCall(false))
      .expect(ss.acquiring == Set() and ss.acquired == Set() and allInvariants)
  }

  // ---- curated mutants (verification §9), kept out of every step relation -------------------

  /// Mutant 4: the listener acks a message before its record commits.
  action ackBeforeRecord = all {
    inHand != NONE,
    not(inHandRecorded),
    ghSession == ss.session,
    jobs' = jobs, buffer' = buffer, msgs' = msgs, nextMsg' = nextMsg,
    queue' = queue.exclude(Set(inHand)),
    ghSession' = ghSession, nextSession' = nextSession, journalWritable' = journalWritable,
    inHand' = NONE, inHandRecorded' = false,
    keepJournal,
    g' = { ...ghostAfter(g, "listener", ss, ss),
      ackedUnrecorded: g.ackedUnrecorded or not(g.recordedIds.contains(inHand)) },
  }

  // Mutant 4 violates i12RecordBeforeAck.
  run mutant4AckBeforeRecordViolatesTest = {
    init.then(poll).then(ackBeforeRecord)
      .expect(not(i12RecordBeforeAck) and not(allInvariants))
  }

  // Mutant 5 (count JobAssigned messages) violates desiredFromLatestStatistics: the batch holds
  // one JobAssigned, the statistics two.
  run mutant5CountMessagesViolatesTest = {
    init.then(handle).then(acquireCall(false)).then(emit)
      .then(poll).then(recordWith(COUNT_MESSAGES))
      .expect(not(desiredFromLatestStatistics) and not(allInvariants))
  }

  // Mutant 6 (add totalRunningJobs) violates desiredFromLatestStatistics once a job runs.
  run mutant6AddRunningViolatesTest = {
    init.then(applyCapacity(3, 3, OPEN)).then(handle).then(acquireCall(false))
      .then(runnerStarts(1)).then(emit)
      .expect(msgs.get(2).stats == { assigned: 2, running: 1 })
      .then(poll).then(recordWith(ADD_RUNNING))
      .expect(not(desiredFromLatestStatistics) and not(allInvariants))
  }

  // Mutant 7 (ignore the inbox receipt) applies a redelivered message twice.
  run mutant7IgnoreInboxViolatesTest = {
    init.then(poll).then(record).then(ack(true)).then(poll).then(recordWith(IGNORE_INBOX))
      .expect(not(messageAppliesOnce) and not(allInvariants))
  }

  // Mutant 19 (acquire every offer) violates I23 past deliverable and I32 on an untrusted offer.
  run mutant19AcquireEveryViolatesTest = {
    init.then(applyCapacity(1, 1, OPEN)).then(poll).then(recordWith(ACQUIRE_EVERY))
      .expect(not(i23AcquireBelowDeliverable) and not(allInvariants))
      .then(ack(false)).then(emit).then(poll).then(recordWith(ACQUIRE_EVERY))
      .expect(not(i32TrustedOnly))
  }
}
