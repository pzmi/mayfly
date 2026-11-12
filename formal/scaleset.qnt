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
 * until deleted), the live session (`ghSession`) and whether its ids restarted (`idsRestarted`). The journal is the stream's state (`ss`) and
 * the listener's inbox receipts (`inbox`, keyed `(session, message)`). The listener holds at
 * most one message (`inHand`).
 *
 * Two flows, one per trace (GH-00R, `doc/github-scaleset.md` §6): `initAcquire` is the flow
 * GitHub Enterprise Server 3.18 and 3.19 keep, where the service offers a job (`JobAvailable`) and
 * mayfly acquires it; `initDirect` is github.com's, where the service assigns a job to the scale
 * set directly (`JobAssigned`, no offer, no acquire) while fewer than the `maxCapacity` of the
 * last poll are assigned (`ghAssigns`; at 0 nothing is assigned, the starvation of ADR 0069's
 * `demand` policy). Either way a runner mayfly starts takes any job assigned to the set (runners
 * of a set are interchangeable), so on github.com each assignment waits for its workflow run's
 * lookup (`pending`, `lookupRun`; a failed lookup refuses), a runner starts only for a job
 * classified trusted, and none while an assignment is pending or refused (ADR 0090, owner
 * 2026-11-11 and 2026-11-12): `startVm`, `refusedJobNeverGetsAVm`. A refused job holds its slot of
 * `maxCapacity` until GitHub releases it (`jobRequeued`, failed at the third release), its run is
 * cancelled (`cancelRun`, with `cancel_refused_runs`: `initDirectWith(true)`), the release window
 * lapses (`assignmentExpires`, A7) or the operator releases it (`operatorReleasesRefusal`).
 *
 * Assumptions on facts `doc/github-scaleset.md` leaves client-assumed or [open: HW-02] (§12), each
 * the safe side:
 *
 * - A1 (§12 item 4, client-assumed; refuted in part): within a numbering, message ids increase, and
 *   a message is redelivered until acked, also in a later session, oldest first. So the statistics
 *   cursor persists across sessions, and dedupe by `(session, message)` receipt alone may record a
 *   message twice in two sessions. A session keeps the cursor at its open as its floor: a message
 *   at or below the floor was recorded in an earlier session and applies nothing (no statistics, no
 *   plan, no release, no drift); only the stream's last record, redelivered as the session's first,
 *   is explained by A1. A numbering can restart (`idsRestart`): a session's first message strictly
 *   below the floor cannot be a redelivery (redelivery is oldest first), so it starts a new
 *   numbering: it is recorded as `MessageIdsRestarted` for the operator, applied as new, and the
 *   cursor and floor restart from it (`a1RestartIsRecoveredTest`). A restart is assumed to leave no
 *   message of the old numbering queued ([open: HW-02]). Any other message at or below the floor
 *   (later in a session) degrades the session for the operator, never silently continued
 *   (`a1RefutationIsFlagged`). One case cannot be told apart: a restarted id that equals the floor,
 *   as the session's first message, reads as the redelivery.
 * - A2 (§12 items 2, 3; open): a message's statistics may or may not count the jobs mayfly acquired
 *   since. The acquire budget counts every unbound reservation on top of `totalAssignedJobs`, so it
 *   may count a job twice (acquire less), never once too few (acquire more).
 * - A3 (§12 item 9): a create while a session lives answers 409 (server-observed); an abandoned
 *   session ends on its own (how soon is open). mayfly deletes only a session it journaled, never
 *   one it has no record of.
 * - A4 (§12 item 2; client-assumed, GHES only): acquire answers the subset of the requested ids it
 *   acquired; re-acquiring is harmless. An acquire whose reply is lost leaves its ids pending
 *   (counted) until an answer or a job fact names them.
 * - A5 (§12 items 1, 8; open): an offer or an assignment whose `eventName`, repository or labels
 *   are missing or unknown is unclassifiable and declined or refused like an untrusted one (`TRUST`
 *   gives each job its class).
 * - A6 (§12 items 1, 4; open): a message's statistics are GitHub's when it was emitted, and a
 *   session's create answers no message id, so a message emitted before an open but recorded after
 *   it (a crash before its record, or a backlog queued while no session was live) cannot be told
 *   from one emitted after it. Statistics are ordered by message id only, and such a message's
 *   older statistics replace the opening's until the backlog is consumed. That is safe for I23
 *   (every acquisition stays a reservation until a message at least as new releases it, A2) and
 *   converges: every change of GitHub's count emits a job event, so once every message is recorded
 *   the statistics are GitHub's own (`desiredFromLatestStatistics`). Keeping the opening's
 *   statistics instead (mutant `OPEN_WINS`) never converges.
 *
 * A scale set GitHub deletes as dormant (no session for 7 days, `dormantDeleted`) answers 404;
 * mayfly re-creates it (`recreateScaleSet`): a new scale-set id, a new numbering, and the old
 * set's reservations dropped on that observation (its assigned jobs went back to GitHub's queue).
 *
 * Every message here carries statistics. The Rust aggregate also takes an envelope without
 * them; it keeps the statistics' floor (`statistics_after`) apart from the cursor so that such a
 * message orders statistics exactly as this model's cursor does.
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
  /// Releases of an assigned job no runner takes before GitHub fails it (scaleset README, Job
  /// Reassignment: "up to 3 times").
  pure val MAX_RELEASES = 3
  /// Unknown message types per session before it is `Degraded{protocol_drift}`.
  pure val DRIFT_THRESHOLD = 2
  /// Consecutive 409s on create before the scale set escalates them to the operator (A3).
  pure val CONFLICT_LIMIT = 2
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
  /// assigned to the scale set, by an acquire or directly, no runner yet
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
  /// Model-local: a message from before the session is folded like a new one (KER-06's rule).
  pure val REAPPLY = "reapply"
  /// Model-local: a message's statistics never replace the session's opening ones.
  pure val OPEN_WINS = "openWins"
  /// ADR 0090 mutant: every assigned job is accepted without classification.
  pure val RUN_ASSIGNED = "runAssigned"
  /// ADR 0090 mutant: a runner starts while an assigned job is refused.
  pure val IGNORE_REFUSALS = "ignoreRefusals"
  /// ADR 0090 mutant: a runner starts while an assignment waits for its run's lookup.
  pure val IGNORE_PENDING = "ignorePending"
  /// ADR 0090 mutant: any assigned job's run is cancelled, refused or not, option or not.
  pure val CANCEL_ANY = "cancelAny"

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
    // the cursor when the live session opened: a message at or below it is from before (A1)
    floor: int,
    // a message was recorded in the live session
    sessionRecorded: bool,
    // 409s on create since the last session opened, and whether they were escalated (A3)
    conflicts: int,
    conflictAlert: bool,
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
    // assigned directly and classified trusted, no `JobStarted` or `JobCompleted` yet (ADR 0090)
    trusted: Set[int],
    // assigned directly and refused (`AssignedJobRefused`: untrusted, unclassifiable or its run
    // not looked up), until a job fact shows GitHub released it (ADR 0090)
    refused: Set[int],
    // assigned directly, its workflow run not looked up yet (ADR 0090: no runner starts first)
    pending: Set[int],
    // the pool's `cancel_refused_runs` (owner, 2026-11-12; default false)
    cancelRefused: bool,
    // refused jobs whose cancel request was answered (accepted or refused): no second request
    cancelTried: Set[int],
    // a `MessageIdsRestarted` was recorded for the operator (A1)
    renumbered: bool,
    // the journaled scale set's id (a generation here)
    setGen: int,
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
    recordedIds: Set[int],
    // the ids GitHub reused (A1 refuted) whose new message is not recorded yet
    reusedUnrecorded: Set[int],
    // a message GitHub emitted anew was dropped with no flag for the operator
    silentlyDropped: bool,
    // a message already recorded in an earlier session changed the stream again
    reappliedEarlier: bool,
    // 409s answered to creates since the last successful one (GitHub's side)
    consecutive409: int,
    applied: Set[(int, int)],
    // the last poll's maxCapacity, and what it was sent under
    sentMax: int,
    sentDeliverable: int,
    sentMode: str,
    // jobs whose `JobAssigned` was recorded and no job fact released yet
    assignedSeen: Set[int],
    // a runner started with no job classified trusted waiting (ADR 0090)
    vmUnbacked: bool,
    // a runner started while an untrusted job mayfly had seen assigned was still assigned
    vmWhileUntrustedKnown: bool,
    // an untrusted or unclassifiable assignment was accepted
    untrustedAccepted: bool,
    // a runner took an untrusted job: the residual race ADR 0090 names (not an invariant)
    untrustedRan: bool,
    // a reused id was applied as new with no `MessageIdsRestarted` and no degradation
    silentlyRenumbered: bool,
    // a reused id was recorded and applied nothing (degraded, or read as a redelivery)
    reusedIgnored: bool,
    // a run was cancelled whose job is trusted and was looked up, or without `cancel_refused_runs`
    cancelledUnrefused: bool,
    // jobs whose last run lookup failed (refused though their true class may be trusted)
    lookupFailed: Set[int],
  }

  var jobs: int -> str
  /// github.com's flow (direct assignment) or the acquire flow, fixed per trace
  var directFlow: bool
  /// the `maxCapacity` GitHub received on the last poll: it assigns no more than that (§5)
  var ghMax: int
  /// the scale set's id at GitHub (a generation; a dormant deletion and re-create moves it)
  var ghSetGen: int
  /// runners mayfly started that have no job yet
  var idle: int
  /// how often GitHub has released each job (it fails the job at the third)
  var released: int -> int
  var buffer: List[Item]
  var msgs: int -> Msg
  var nextMsg: int
  var queue: Set[int]
  var ghSession: int
  /// GitHub numbered a later session's messages from 1 again (A1 refuted; at most once here)
  var idsRestarted: bool
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
    trusted: Set[int],
    refused: Set[int],
    pending: Set[int],
    // ghost: an offer taken while already reserved, past the budget, untrusted, or while held
    dup: bool,
    untrusted: bool,
    held: bool,
    // ghost: assignments seen and not released; an untrusted assignment accepted
    seen: Set[int],
    accepted: bool,
  }

  /// Whether offers may be planned: the pool is open and the session active (a degraded session
  /// freezes minting, P21; `demand::Budget::frozen`).
  pure def planning(s: SS): bool = s.mode == OPEN and s.phase == ACTIVE

  /// `demand::decide_offer` (`PlanAcquire` for one offer): I32 first (only the pool's tier),
  /// then no second reservation of a request id, then nothing while held (pause, drain, safe
  /// mode, a degraded session), then I23: acquire only while assigned plus every unbound reservation (A2) is below
  /// deliverable.
  pure def offerStep(s: SS, assigned: int, how: str, a: Acc, r: int): Acc = {
    val reserved = a.acquiring.union(a.acquired).union(a.trusted)
    val take = { ...a, acquiring: a.acquiring.union(Set(r)), take: a.take.union(Set(r)),
      dup: a.dup or reserved.contains(r), untrusted: a.untrusted or TRUST.get(r) != TRUSTED,
      held: a.held or not(planning(s)) }
    val decline = { ...a, declined: a.declined.union(Set(r)) }
    if (how == ACQUIRE_EVERY) take
    else if (TRUST.get(r) != TRUSTED) decline
    else if (reserved.contains(r)) a
    else if (not(planning(s))) decline
    else if (assigned + reserved.size() < s.deliverable) take
    else decline
  }

  /// `AssignedJob` (ADR 0090, owner 2026-11-12): a job GitHub assigned to the scale set. One
  /// mayfly acquired is a fact only (its offer was classified, I32); any other waits for its
  /// workflow run to be looked up (`pending`, `lookupRun`) before it is classified, and no runner
  /// starts meanwhile; a new assignment of a job replaces an earlier one's classification. The
  /// mutant `RUN_ASSIGNED` accepts it at once, unlooked-up.
  pure def assignedStep(how: str, a: Acc, r: int): Acc = {
    val a1 = { ...a, assignedItems: a.assignedItems + 1, seen: a.seen.union(Set(r)) }
    if (a.acquired.contains(r)) a1
    else if (how == RUN_ASSIGNED)
      { ...a1, trusted: a1.trusted.union(Set(r)), accepted: a1.accepted or TRUST.get(r) != TRUSTED }
    // a new assignment of the job replaces whatever an earlier one of it was classified as
    else { ...a1, pending: a1.pending.union(Set(r)), refused: a1.refused.exclude(Set(r)),
      trusted: a1.trusted.exclude(Set(r)) }
  }

  /// One item of a message: an offer is planned; an assignment is classified (`assignedStep`);
  /// `JobStarted` and `JobCompleted` are observations that release the job's reservation (a
  /// pending one included: the job was acquired, or is gone), its classification and a refusal
  /// (GitHub released it); an unknown type is counted.
  pure def itemStep(s: SS, assigned: int, how: str, a: Acc, it: Item): Acc =
    if (it.kind == AVAILABLE) offerStep(s, assigned, how, a, it.req)
    else if (it.kind == ASSIGNED) assignedStep(how, a, it.req)
    else if (it.kind == STARTED or it.kind == COMPLETED) {
      val gone = Set(it.req)
      { ...a, acquiring: a.acquiring.exclude(gone), acquired: a.acquired.exclude(gone),
        trusted: a.trusted.exclude(gone), refused: a.refused.exclude(gone),
        pending: a.pending.exclude(gone), seen: a.seen.exclude(gone) }
    }
    else { ...a, unknown: a.unknown + 1 }

  pure def initAcc(s: SS, seen: Set[int]): Acc = {
    acquiring: s.acquiring, acquired: s.acquired, take: Set(), declined: Set(),
    assignedItems: 0, unknown: 0, trusted: s.trusted, refused: s.refused, pending: s.pending,
    dup: false, untrusted: false, held: false, seen: seen, accepted: false,
  }

  pure def ghostAfter(g0: Ghost, kind: str, before: SS, after: SS): Ghost = {
    ...g0,
    lastKind: kind,
    pendingLeftBy: if (before.acquiring.exists(r => not(after.acquiring.contains(r)))) kind else "",
  }

  // ---- frames -------------------------------------------------------------------------------

  /// GitHub's variables that most actions leave alone.
  action keepGh = all {
    directFlow' = directFlow, ghMax' = ghMax, ghSetGen' = ghSetGen, idle' = idle, released' = released,
  }

  action keepWorld = all {
    jobs' = jobs, buffer' = buffer, msgs' = msgs, nextMsg' = nextMsg, queue' = queue,
    ghSession' = ghSession, nextSession' = nextSession, journalWritable' = journalWritable,
    idsRestarted' = idsRestarted, keepGh,
  }

  action keepListener = all { inHand' = inHand, inHandRecorded' = inHandRecorded }

  action keepJournal = all { ss' = ss, inbox' = inbox }

  /// A change of the world (GitHub) only.
  action world(j: int -> str, b: List[Item], gs: int, ns: int): bool = all {
    jobs' = j, buffer' = b, msgs' = msgs, nextMsg' = nextMsg, queue' = queue,
    ghSession' = gs, nextSession' = ns, journalWritable' = journalWritable,
    idsRestarted' = idsRestarted, keepGh,
    keepListener, keepJournal,
    g' = ghostAfter(g, "world", ss, ss),
  }

  // ---- init ---------------------------------------------------------------------------------

  pure val STATS0 = { assigned: 0, running: 0 }

  /// The stream with an open session (session 1, initial statistics: nothing assigned) and a
  /// capacity input of 2 deliverable.
  pure val SS0: SS = {
    phase: ACTIVE, session: 1, stale: NONE, cursor: NONE, floor: NONE, sessionRecorded: false,
    conflicts: 0, conflictAlert: false, stats: STATS0, deliverable: 2, reportedMax: 2,
    mode: OPEN, desired: 0, acquiring: Set(), acquired: Set(), drift: 0,
    trusted: Set(), refused: Set(), renumbered: false, setGen: 1,
    pending: Set(), cancelRefused: false, cancelTried: Set(),
  }

  pure val G0: Ghost = {
    lastKind: "init", pendingLeftBy: "", ackedUnrecorded: false, doubleReserved: false,
    overAcquired: false, untrustedReserved: false, acquiredWhileHeld: false,
    stoleSession: false, appliedTwice: false, polledUnwritable: false, opened: true,
    recordedIds: Set(), reusedUnrecorded: Set(), silentlyDropped: false,
    reappliedEarlier: false, consecutive409: 0, applied: Set(), sentMax: 0,
    sentDeliverable: 0, sentMode: OPEN, assignedSeen: Set(), vmUnbacked: false,
    vmWhileUntrustedKnown: false, untrustedAccepted: false, untrustedRan: false,
    silentlyRenumbered: false, reusedIgnored: false, cancelledUnrefused: false, lookupFailed: Set(),
  }

  /// The acquire flow (GHES 3.18, 3.19): a backlog of three offers; the first batch holds two
  /// (the cap), the third waits in the buffer.
  action initAcquire = all {
    jobs' = REQS.mapBy(_ => J_AVAILABLE),
    directFlow' = false,
    released' = REQS.mapBy(_ => 0),
    ghMax' = 0,
    ghSetGen' = 1,
    idle' = 0,
    buffer' = [{ kind: AVAILABLE, req: 3 }],
    msgs' = MSG_IDS.mapBy(i =>
      if (i == 1)
        { items: [{ kind: AVAILABLE, req: 1 }, { kind: AVAILABLE, req: 2 }], stats: STATS0 }
      else { items: NO_ITEMS, stats: STATS0 }),
    nextMsg' = 2,
    queue' = Set(1),
    ghSession' = 1,
    idsRestarted' = false,
    nextSession' = 2,
    journalWritable' = true,
    inHand' = NONE,
    inHandRecorded' = false,
    ss' = SS0,
    inbox' = Set(),
    g' = G0,
  }

  /// github.com's flow: three queued jobs, no message yet; GitHub assigns them to the scale set
  /// directly once a poll advertises room (`ghAssigns`).
  action initDirect = initDirectWith(false)

  action initDirectWith(cancel: bool): bool = all {
    jobs' = REQS.mapBy(_ => J_AVAILABLE),
    directFlow' = true,
    released' = REQS.mapBy(_ => 0),
    ghMax' = 0,
    ghSetGen' = 1,
    idle' = 0,
    buffer' = NO_ITEMS,
    msgs' = MSG_IDS.mapBy(_ => { items: NO_ITEMS, stats: STATS0 }),
    nextMsg' = 1,
    queue' = Set(),
    ghSession' = 1,
    idsRestarted' = false,
    nextSession' = 2,
    journalWritable' = true,
    inHand' = NONE,
    inHandRecorded' = false,
    ss' = { ...SS0, cancelRefused: cancel },
    inbox' = Set(),
    g' = G0,
  }

  action init = any { initAcquire, initDirectWith(false), initDirectWith(true) }

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
      idsRestarted' = idsRestarted, keepGh,
      keepListener, keepJournal,
      g' = ghostAfter(g, "world", ss, ss),
    }
  }

  /// github.com assigns a queued job to the scale set directly, while fewer than the last poll's
  /// `maxCapacity` are assigned (ARC #3446, scaleset #133: at 0 it assigns nothing).
  val assignRoom = directFlow and assignedSet(jobs).size() < ghMax

  action ghAssigns(r: int): bool = all {
    assignRoom,
    jobs.get(r) == J_AVAILABLE,
    world(jobs.set(r, J_ACQUIRED), buffer.append({ kind: ASSIGNED, req: r }),
      ghSession, nextSession),
  }

  /// A runner mayfly started takes a job assigned to the set: any of them, since runners of a set
  /// are interchangeable (ADR 0027).
  action runnerStarts(r: int): bool = all {
    jobs.get(r) == J_ACQUIRED,
    idle > 0,
    jobs' = jobs.set(r, J_RUNNING), buffer' = buffer.append({ kind: STARTED, req: r }),
    msgs' = msgs, nextMsg' = nextMsg, queue' = queue,
    ghSession' = ghSession, nextSession' = nextSession, journalWritable' = journalWritable,
    idsRestarted' = idsRestarted,
    directFlow' = directFlow, ghMax' = ghMax, ghSetGen' = ghSetGen, idle' = idle - 1, released' = released,
    keepListener, keepJournal,
    g' = { ...ghostAfter(g, "world", ss, ss),
      untrustedRan: g.untrustedRan or TRUST.get(r) != TRUSTED },
  }

  /// The job finishes.
  action jobFinishes(r: int): bool = all {
    jobs.get(r) == J_RUNNING,
    world(jobs.set(r, J_DONE), buffer.append({ kind: COMPLETED, req: r }), ghSession, nextSession),
  }

  /// Release and requeue (doc/github-scaleset.md §4.3, §6): a job assigned to the set that no
  /// runner took in time (about 300 s, [open: HW-02]) is completed as canceled and queued again
  /// (here under the same request id, the harder case); at the `MAX_RELEASES`th release GitHub
  /// fails it instead. The acquire flow offers it again; github.com assigns it again
  /// (`ghAssigns`). On github.com whether the `JobCompleted` names the job is [open: HW-02]
  /// (`runnerRequestId` is 0 there): `named` false sends one that names none, so only the
  /// release window clears mayfly's classification (`assignmentExpires`).
  action jobRequeued(r: int, named: bool): bool = {
    val n = released.get(r) + 1
    val fails = n >= MAX_RELEASES
    val done = { kind: COMPLETED, req: if (named) r else NONE }
    all {
      jobs.get(r) == J_ACQUIRED,
      named or directFlow,
      jobs' = jobs.set(r, if (fails) J_DONE else J_AVAILABLE),
      buffer' = if (directFlow or fails) buffer.append(done)
        else buffer.append(done).append({ kind: AVAILABLE, req: r }),
      msgs' = msgs, nextMsg' = nextMsg, queue' = queue,
      ghSession' = ghSession, nextSession' = nextSession, journalWritable' = journalWritable,
      idsRestarted' = idsRestarted,
      directFlow' = directFlow, ghMax' = ghMax, ghSetGen' = ghSetGen, idle' = idle,
      released' = released.set(r, n),
      keepListener, keepJournal,
      g' = ghostAfter(g, "world", ss, ss),
    }
  }

  /// The service starts sending a message type this build does not know (R7).
  action unknownTypeQueued = all {
    buffer.length() < 3,
    world(jobs, buffer.append({ kind: UNKNOWN_TYPE, req: NONE }), ghSession, nextSession),
  }

  /// A1 refuted: the numbering restarts, so GitHub emits the next batch under an id the stream
  /// recorded already (once, to keep the state small). A restart is assumed to leave no message
  /// of the old numbering queued ([open: HW-02]).
  action idsRestart = {
    val n = minOf(CAP, buffer.length())
    all {
      buffer.length() > 0,
      ghSession >= 2,
      ss.cursor >= 1,
      not(idsRestarted),
      queue == Set(),
      jobs' = jobs,
      buffer' = buffer.slice(n, buffer.length()),
      msgs' = msgs.set(1, { items: buffer.slice(0, n), stats: statsOf(jobs) }),
      nextMsg' = nextMsg,
      queue' = queue.union(Set(1)),
      ghSession' = ghSession, nextSession' = nextSession, journalWritable' = journalWritable,
      idsRestarted' = true, keepGh,
      keepListener, keepJournal,
      g' = { ...ghostAfter(g, "world", ss, ss),
        reusedUnrecorded: g.reusedUnrecorded.union(Set(1)) },
    }
  }

  /// The live session ends at GitHub: its token expired, the session expired
  /// (`RunnerScaleSetSessionExpiredException` on refresh), or an abandoned session timed out (A3).
  action sessionEnds = all {
    ghSession != NONE,
    world(jobs, buffer, NONE, nextSession),
  }

  /// GitHub deletes a scale set no session has connected to for 7 days (the ARC lifetime ADR at
  /// `c6e4c94`; maintainer-stated): its queue goes, the jobs assigned to it and not to a runner go
  /// back to GitHub's queue (offered again in the acquire flow), its idle runners go. Once, with
  /// nothing running and nothing pending.
  action dormantDeleted = {
    val returned = REQS.filter(r => jobs.get(r) == J_ACQUIRED)
    all {
      ghSession == NONE,
      ghSetGen == 1,
      buffer == NO_ITEMS,
      REQS.forall(r => jobs.get(r) != J_RUNNING),
      jobs' = REQS.mapBy(r => if (returned.contains(r)) J_AVAILABLE else jobs.get(r)),
      buffer' = if (directFlow) NO_ITEMS
        else returned.fold(NO_ITEMS, (b, r) => b.append({ kind: AVAILABLE, req: r })),
      msgs' = msgs, nextMsg' = nextMsg, queue' = Set(),
      ghSession' = ghSession, nextSession' = nextSession, journalWritable' = journalWritable,
      idsRestarted' = idsRestarted,
      directFlow' = directFlow, ghMax' = 0, ghSetGen' = ghSetGen + 1, idle' = 0, released' = released,
      keepListener, keepJournal,
      g' = ghostAfter(g, "world", ss, ss),
    }
  }

  /// The journal becomes unwritable (safe mode with an unwritable journal), or writable again.
  action journalToggles = all {
    jobs' = jobs, buffer' = buffer, msgs' = msgs, nextMsg' = nextMsg, queue' = queue,
    ghSession' = ghSession, nextSession' = nextSession, idsRestarted' = idsRestarted, keepGh,
    journalWritable' = not(journalWritable),
    keepListener, keepJournal,
    g' = ghostAfter(g, "world", ss, ss),
  }

  // ---- the session (§3) ---------------------------------------------------------------------

  /// Create a session (`BeginOpen`, then the answer). With no live session GitHub answers one,
  /// and `RecordSessionOpened{statistics}` recomputes desired from its initial statistics (plan
  /// §7.2 step 5); the cursor becomes the session's floor. With one alive, 409 (A3):
  /// `RecordSessionLoss{conflicted}`, still in Backoff; the `CONFLICT_LIMIT`th in a row is
  /// escalated to the operator (`SessionConflictEscalated`) and the creates go on backing off.
  action openSession = {
    val stats0 = statsOf(jobs)
    val opened = { ...ss, phase: ACTIVE, session: nextSession, stats: stats0, drift: 0,
      desired: minOf(ss.deliverable, stats0.assigned), floor: ss.cursor, sessionRecorded: false,
      conflicts: 0, conflictAlert: false }
    val conflicted = { ...ss, phase: BACKOFF, conflicts: ss.conflicts + 1,
      conflictAlert: ss.conflictAlert or ss.conflicts + 1 >= CONFLICT_LIMIT }
    all {
      journalWritable,
      ss.phase == ENSURED or ss.phase == BACKOFF,
      ss.stale == NONE,
      // a create on a scale set GitHub deleted answers 404: `recreateScaleSet`
      ss.setGen == ghSetGen,
      nextSession <= MAX_SESSION,
      jobs' = jobs, buffer' = buffer, msgs' = msgs, nextMsg' = nextMsg, queue' = queue,
      journalWritable' = journalWritable, idsRestarted' = idsRestarted, keepGh,
      keepListener,
      inbox' = inbox,
      if (ghSession == NONE) all {
        ghSession' = nextSession,
        nextSession' = nextSession + 1,
        ss' = opened,
        g' = { ...ghostAfter(g, "observation", ss, opened), opened: true, consecutive409: 0 },
      } else all {
        ghSession' = ghSession,
        nextSession' = nextSession,
        ss' = conflicted,
        g' = { ...ghostAfter(g, "observation", ss, conflicted), consecutive409: g.consecutive409 + 1 },
      },
    }
  }

  /// A create whose answer was lost: GitHub holds a session mayfly has no record of (A3).
  action openLost = all {
    ss.phase == ENSURED or ss.phase == BACKOFF,
    ss.stale == NONE,
    ss.setGen == ghSetGen,
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
      nextSession' = nextSession, journalWritable' = journalWritable, idsRestarted' = idsRestarted,
      keepGh,
      ghSession' = ghAfter,
      keepListener,
      ss' = { ...ss, stale: NONE },
      inbox' = inbox,
      g' = { ...ghostAfter(g, "observation", ss, { ...ss, stale: NONE }),
        stoleSession: g.stoleSession or (ghSession != NONE and ghAfter == NONE and ghSession != ss.stale) },
    }
  }

  /// The listener's calls answer 401 after one refresh, a refresh answers
  /// `RunnerScaleSetSessionExpiredException`, or a call answers 404: the session is lost
  /// (`RecordSessionLoss{expired}`) and is created anew, never refreshed again. The in-hand
  /// message is dropped; it will be redelivered.
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

  /// A create answered 404 for the journaled scale set: GitHub deleted it as dormant. `ensure`
  /// looks it up by name (absent) and creates it again (ADR 0040: mayfly never deletes one, but
  /// re-creates its own); the new id is journaled, the numbering starts afresh, and the old set's
  /// reservations, classifications and refusals are dropped on that observation, since its
  /// assigned jobs went back to GitHub's queue (`RecordScaleSetRecreated`): for a refusal, the
  /// re-creation is one of the ways the job is observed gone from the set.
  action recreateScaleSet = {
    val fresh = { ...ss, phase: ENSURED, setGen: ghSetGen, cursor: NONE, floor: NONE,
      sessionRecorded: false, stats: STATS0, desired: 0, acquiring: Set(), acquired: Set(),
      trusted: Set(), refused: Set(), pending: Set(), cancelTried: Set() }
    all {
      journalWritable,
      ss.phase == ENSURED or ss.phase == BACKOFF,
      ss.stale == NONE,
      ss.setGen != ghSetGen,
      keepWorld, keepListener,
      ss' = fresh, inbox' = inbox,
      g' = { ...ghostAfter(g, "observation", ss, fresh), assignedSeen: Set() },
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

  /// Long poll with `maxCapacity` = the wire max, which GitHub keeps as the bound of what it
  /// assigns directly. GitHub answers the oldest unacknowledged message (a recorded one whose ack
  /// was lost included), or 202 with nothing. Polling stops while the journal is unwritable
  /// (ADR 0029).
  action poll = all {
    listening(ss),
    ghSession == ss.session,
    inHand == NONE,
    journalWritable,
    jobs' = jobs, buffer' = buffer, msgs' = msgs, nextMsg' = nextMsg, queue' = queue,
    ghSession' = ghSession, nextSession' = nextSession, journalWritable' = journalWritable,
    idsRestarted' = idsRestarted,
    directFlow' = directFlow, ghMax' = wireMax(ss), ghSetGen' = ghSetGen, idle' = idle, released' = released,
    inHand' = if (queue == Set()) NONE else lowest(queue),
    inHandRecorded' = false,
    keepJournal,
    g' = { ...ghostAfter(g, "listener", ss, ss),
      sentMax: wireMax(ss), sentDeliverable: ss.deliverable, sentMode: ss.mode,
      polledUnwritable: g.polledUnwritable or not(journalWritable) },
  }

  /// `RecordMessage{session, message, statistics, items}`, in one unit: the inbox receipt; a
  /// session's first message strictly below its floor starts a new numbering (A1 refuted:
  /// `MessageIdsRestarted`, applied as new, cursor and floor restart from it); any other message
  /// at or below the floor is from before the session (A1) and applies nothing, and unless it is
  /// the stream's last record redelivered first, it degrades the session; any other message
  /// applies its statistics if newer than the cursor (A1, A6), then job facts, the acquire plan
  /// (planned ids become `acquiring`, journaled before the call) and declined offers, assigned
  /// jobs classified (ADR 0090), unknown types, desired. A receipt that exists already is a
  /// `DuplicateMessage`: nothing more is written.
  action recordWith(how: str): bool = {
    val key = (ss.session, inHand)
    val m = msgs.get(inHand)
    // only the cursor can be redelivered within a session (oldest first): a receipt for any other
    // id is a reused id, never a duplicate
    val duplicate = inbox.contains(key) and inHand == ss.cursor and how != IGNORE_INBOX
    val restarts = inHand < ss.floor and not(ss.sessionRecorded) and how != REAPPLY
    val earlier = inHand <= ss.cursor and how != REAPPLY and not(restarts)
    val explained = inHand == ss.floor and not(ss.sessionRecorded)
    val regressed = earlier and not(explained)
    val newer = not(earlier) and (restarts or inHand > ss.cursor) and how != OPEN_WINS
    val stats = if (newer) m.stats else ss.stats
    val folded = m.items.foldl(initAcc(ss, g.assignedSeen),
      (a, it) => itemStep(ss, stats.assigned, how, a, it))
    val acc = if (earlier) initAcc(ss, g.assignedSeen) else folded
    val drift = ss.drift + acc.unknown
    val degrades = ss.phase == ACTIVE and (drift >= DRIFT_THRESHOLD or regressed)
    val after = {
      ...ss,
      cursor: if (restarts or inHand > ss.cursor) inHand else ss.cursor,
      floor: if (restarts) NONE else ss.floor,
      renumbered: ss.renumbered or restarts,
      stats: stats,
      acquiring: acc.acquiring,
      acquired: acc.acquired,
      trusted: acc.trusted,
      refused: acc.refused,
      pending: acc.pending,
      drift: drift,
      phase: if (degrades) DEGRADED else ss.phase,
      desired: if (earlier) ss.desired else desiredOf(how, ss.deliverable, stats, acc.assignedItems),
      sessionRecorded: true,
    }
    val ghAssigned = assignedSet(jobs)
    // ghost: GitHub's truth about the content, whatever its id says
    val recordedBefore = g.recordedIds.contains(inHand) and not(g.reusedUnrecorded.contains(inHand))
    val changed = newer or acc.take != Set() or acc.unknown > 0 or acc.acquiring != ss.acquiring
      or acc.acquired != ss.acquired or acc.trusted != ss.trusted or acc.refused != ss.refused
      or acc.pending != ss.pending
    val reused = g.reusedUnrecorded.contains(inHand)
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
          appliedTwice: g.appliedTwice or (g.applied.contains(key) and not(reused)),
          applied: g.applied.union(Set(key)),
          recordedIds: g.recordedIds.union(Set(inHand)),
          reusedUnrecorded: g.reusedUnrecorded.exclude(Set(inHand)),
          reappliedEarlier: g.reappliedEarlier or (recordedBefore and changed),
          silentlyDropped: g.silentlyDropped or
            (not(recordedBefore) and earlier and not(explained) and after.phase != DEGRADED),
          silentlyRenumbered: g.silentlyRenumbered or
            (reused and not(earlier) and not(after.renumbered) and after.phase != DEGRADED),
          assignedSeen: acc.seen,
          reusedIgnored: g.reusedIgnored or (reused and earlier),
          untrustedAccepted: g.untrustedAccepted or acc.accepted,
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
    idsRestarted' = idsRestarted, keepGh,
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
      idsRestarted' = idsRestarted, keepGh,
      keepListener,
      inbox' = inbox,
      ss' = if (recorded) after else ss,
      g' = { ...ghostAfter(g, "observation", ss, if (recorded) after else ss),
        acquiredWhileHeld: g.acquiredWhileHeld or ss.mode != OPEN },
    }
  }

  // ---- runners (ADR 0090) -------------------------------------------------------------------

  /// mayfly starts (activates) a runner for the scale set: only for a job it classified trusted
  /// that waits for a runner (acquired, or assigned and classified), never while an assigned job
  /// is refused (any runner of the set may take it), and only while planning (open, session
  /// active). The reconciler's shortfall and the host's admission (scheduling model) bound it
  /// further; here only the trust gate is modelled.
  val vmWaiting = ss.acquired.union(ss.trusted)

  /// Whether `startVm` may start a runner now (the trust gate, ADR 0090).
  val vmMayStart = and {
    journalWritable,
    planning(ss),
    ss.refused == Set(),
    ss.pending == Set(),
    idle < vmWaiting.size(),
  }

  action startVmWith(how: str): bool = {
    val waiting = vmWaiting
    all {
      if (how == IGNORE_REFUSALS) and {
        journalWritable, planning(ss), ss.pending == Set(), idle < waiting.size()
      } else if (how == IGNORE_PENDING) and {
        journalWritable, planning(ss), ss.refused == Set(), idle < waiting.size()
      } else vmMayStart,
      jobs' = jobs, buffer' = buffer, msgs' = msgs, nextMsg' = nextMsg, queue' = queue,
      ghSession' = ghSession, nextSession' = nextSession, journalWritable' = journalWritable,
      idsRestarted' = idsRestarted,
      directFlow' = directFlow, ghMax' = ghMax, ghSetGen' = ghSetGen, idle' = idle + 1, released' = released,
      keepListener, keepJournal,
      g' = { ...ghostAfter(g, "command", ss, ss),
        vmUnbacked: g.vmUnbacked or
          idle + 1 > waiting.filter(r => TRUST.get(r) == TRUSTED).size(),
        vmWhileUntrustedKnown: g.vmWhileUntrustedKnown or
          g.assignedSeen.exists(r => TRUST.get(r) != TRUSTED and jobs.get(r) == J_ACQUIRED) },
    }
  }

  action startVm = startVmWith(CORRECT)

  /// The workflow-run lookup of a pending assignment (ADR 0090, owner 2026-11-12): `GET
  /// /repos/{owner}/{repo}/actions/runs/{run_id}` ("Actions: Read") gives the head repository
  /// (a fork when it is not the base), the event and the actor; with the repository listed in
  /// config and a trusted event and no fork the job is trusted (`TRUST`), anything else is
  /// refused (`AssignedJobRefused`). A failed lookup (`ok` false: an error, a timeout, a missing
  /// field) refuses the job too: nothing runs unclassified. With `cancel_refused_runs` the refusal
  /// asks for the run to be cancelled (`cancelRun`).
  action lookupRun(r: int, ok: bool): bool = {
    val trusted = ok and TRUST.get(r) == TRUSTED
    val after = { ...ss,
      pending: ss.pending.exclude(Set(r)),
      trusted: if (trusted) ss.trusted.union(Set(r)) else ss.trusted,
      refused: if (trusted) ss.refused else ss.refused.union(Set(r)) }
    all {
      journalWritable,
      ss.pending.contains(r),
      keepWorld, keepListener,
      ss' = after, inbox' = inbox,
      g' = { ...ghostAfter(g, "observation", ss, after),
        lookupFailed: if (ok) g.lookupFailed.exclude(Set(r)) else g.lookupFailed.union(Set(r)) },
    }
  }

  /// `cancel_refused_runs` (owner, 2026-11-12; "Actions: Write"): `POST
  /// .../actions/runs/{run_id}/cancel` for a refused job, once. `effect`: GitHub cancels it (the
  /// job leaves the assignment and a `JobCompleted` follows, which releases the refusal like any
  /// release); otherwise the request failed or never reached it. `answered`: mayfly records the
  /// answer; a lost answer leaves the request to be retried. Either way the hold lasts until the
  /// job is observed gone, and the release window still bounds it.
  action cancelRunWith(how: str, r: int, effect: bool, answered: bool): bool = {
    val cancels = effect and jobs.get(r) == J_ACQUIRED
    val after = if (answered) { ...ss, cancelTried: ss.cancelTried.union(Set(r)) } else ss
    all {
      journalWritable,
      if (how == CANCEL_ANY) jobs.get(r) == J_ACQUIRED
      else and { ss.cancelRefused, ss.refused.contains(r) },
      not(ss.cancelTried.contains(r)),
      jobs' = if (cancels) jobs.set(r, J_DONE) else jobs,
      buffer' = if (cancels) buffer.append({ kind: COMPLETED, req: r }) else buffer,
      msgs' = msgs, nextMsg' = nextMsg, queue' = queue,
      ghSession' = ghSession, nextSession' = nextSession, journalWritable' = journalWritable,
      idsRestarted' = idsRestarted,
      directFlow' = directFlow, ghMax' = ghMax, ghSetGen' = ghSetGen, idle' = idle,
      released' = released,
      keepListener,
      ss' = after, inbox' = inbox,
      // the ghost reads the job's true class, not the guard: a cancel is legitimate only with
      // the option, for a job that is untrusted or whose lookup failed
      g' = { ...ghostAfter(g, "command", ss, after),
        cancelledUnrefused: g.cancelledUnrefused or not(ss.cancelRefused)
          or (TRUST.get(r) == TRUSTED and not(g.lookupFailed.contains(r))) },
    }
  }

  action cancelRun(r: int, effect: bool, answered: bool): bool =
    cancelRunWith(CORRECT, r, effect, answered)

  /// A classification lapses after the release window (ADR 0090): `refusal_release_window`, a
  /// config value at least `MAX_RELEASES` times GitHub's release time, conservative by default
  /// and measured by HW-02 (spec §12 item 2). A7 ([open: HW-02]): the window outlasts GitHub's
  /// releases, so when it lapses the job is no longer assigned at GitHub (the guard reads that
  /// assumption from the world; a window too short would let a runner start beside a job still
  /// assigned). It clears a refusal or a trusted classification whose `JobCompleted` named no job
  /// or never came, so a refusal never holds the scale set for ever.
  action assignmentExpires(r: int): bool = {
    val after = { ...ss, refused: ss.refused.exclude(Set(r)), trusted: ss.trusted.exclude(Set(r)) }
    all {
      journalWritable,
      ss.refused.union(ss.trusted).contains(r),
      jobs.get(r) != J_ACQUIRED,
      keepWorld, keepListener,
      ss' = after, inbox' = inbox,
      g' = { ...ghostAfter(g, "command", ss, after),
        assignedSeen: g.assignedSeen.exclude(Set(r)) },
    }
  }

  /// The operator releases a refusal (`mayfly pool release-refusal`, ADR 0090) after seeing the
  /// job failed or cancelled at GitHub: the command shows the job's state read from GitHub and
  /// is refused while GitHub still holds it assigned (the guard), so it lifts a hold and asserts
  /// no outcome.
  action operatorReleasesRefusal(r: int): bool = {
    val after = { ...ss, refused: ss.refused.exclude(Set(r)) }
    all {
      journalWritable,
      ss.refused.contains(r),
      jobs.get(r) != J_ACQUIRED,
      keepWorld, keepListener,
      ss' = after, inbox' = inbox,
      g' = { ...ghostAfter(g, "command", ss, after),
        assignedSeen: g.assignedSeen.exclude(Set(r)) },
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
    nondet flag2 = Set(true, false).oneOf()
    nondet d = DELIVERABLES.oneOf()
    nondet rm = DELIVERABLES.oneOf()
    nondet mode = MODES.oneOf()
    any {
      emit,
      ghAssigns(r),
      runnerStarts(r),
      jobFinishes(r),
      jobRequeued(r, flag),
      unknownTypeQueued,
      idsRestart,
      sessionEnds,
      dormantDeleted,
      journalToggles,
      openSession,
      openLost,
      deleteStale,
      recreateScaleSet,
      sessionLoss,
      restart,
      recoverDrift,
      poll,
      record,
      ack(flag),
      acquireCall(flag),
      startVm,
      lookupRun(r, flag),
      cancelRun(r, flag, flag2),
      assignmentExpires(r),
      operatorReleasesRefusal(r),
      applyCapacity(d, rm, mode),
    }
  }

  /// The message path, biased: no session churn, no capacity change and no outage, so a walk
  /// polls, records, acquires and acks through redelivery (lost acks and acquire answers), the
  /// truncated backlog, direct assignment and its classification, runners starting and
  /// finishing, release and requeue, and unknown types.
  action messageStep = {
    nondet r = REQS.oneOf()
    nondet flag = Set(true, false).oneOf()
    nondet flag2 = Set(true, false).oneOf()
    any {
      emit,
      ghAssigns(r),
      runnerStarts(r),
      jobFinishes(r),
      jobRequeued(r, flag),
      unknownTypeQueued,
      poll,
      record,
      ack(flag),
      acquireCall(flag),
      startVm,
      lookupRun(r, flag),
      cancelRun(r, flag, flag2),
      assignmentExpires(r),
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

  /// GitHub's side has nothing the stream has not seen: no job event waits for a message, every
  /// message emitted is recorded (with ids as A1 states them), and the scale set mayfly journaled
  /// is GitHub's (a dormant deletion changes GitHub's count with no message: the stream learns of
  /// it only when a create answers 404).
  val quiescent = and {
    buffer == NO_ITEMS,
    ss.setGen == ghSetGen,
    MSG_IDS.forall(i => i >= nextMsg or g.recordedIds.contains(i)),
    // a restarted numbering converges once its message is recorded and applied as new; one that
    // was read as a redelivery or degraded the session is the operator's, not a convergence
    g.reusedUnrecorded == Set(),
    not(g.reusedIgnored),
  }

  /// desiredFromLatestStatistics: desired = min(deliverable, totalAssignedJobs) of the statistics
  /// applied (never a count of messages, never plus running jobs), and those statistics are
  /// GitHub's own, read from its jobs, whenever the stream has seen everything GitHub emitted.
  /// Between, a message emitted before an open may apply older statistics than the opening's
  /// (A6), which I23 tolerates.
  val desiredFromLatestStatistics = and {
    g.opened implies ss.desired == minOf(ss.deliverable, ss.stats.assigned),
    quiescent implies ss.stats.assigned == statsOf(jobs).assigned,
  }

  /// A1: a message recorded in an earlier session and redelivered applies nothing again: no
  /// statistics, no plan, no release, no drift (only its receipt and the cursor's floor).
  val redeliveryAppliesNothing = not(g.reappliedEarlier)

  /// A1 refuted: a message GitHub emitted anew under an id at or below the session's floor is
  /// never dropped silently, and never applied silently: as a session's first message below
  /// the floor it starts a new numbering recorded for the operator (`MessageIdsRestarted`),
  /// otherwise the session is degraded. The one case it cannot tell apart, a restarted id equal
  /// to the floor as the session's first message, is excluded (A1).
  val a1RefutationIsFlagged = not(g.silentlyDropped) and not(g.silentlyRenumbered)

  /// ADR 0090: a runner starts only for a job mayfly classified trusted and that waits for one, and
  /// never more idle runners than such jobs.
  val vmOnlyForAJobClassifiedTrusted = not(g.vmUnbacked)

  /// ADR 0090: no runner starts while a job mayfly saw assigned to the set and did not classify
  /// trusted is still assigned (any runner of the set may take it): a refused job never gets one.
  val refusedJobNeverGetsAVm = not(g.vmWhileUntrustedKnown)

  /// ADR 0090 (owner, 2026-11-12): a run is cancelled only with `cancel_refused_runs`, and only
  /// for a job that is truly untrusted or whose run lookup failed (the ghost reads `TRUST`, not
  /// mayfly's own refusal).
  val cancelOnlyRefused = not(g.cancelledUnrefused)

  /// ADR 0090 and I32 for assignments: an assignment that is not trusted is never accepted, and
  /// every one mayfly has seen and no job fact released is refused, a recorded fact for the
  /// operator (`AssignedJobRefused`), never only a log line.
  val assignedJobsClassified = and {
    not(g.untrustedAccepted),
    ss.trusted.forall(r => TRUST.get(r) == TRUSTED),
    g.assignedSeen.forall(r =>
      TRUST.get(r) == TRUSTED or ss.refused.contains(r) or ss.pending.contains(r)),
  }

  /// A3: creates answered 409 `CONFLICT_LIMIT` times in a row are escalated to the operator.
  val persistentConflictsAreFlagged =
    g.consecutive409 >= CONFLICT_LIMIT implies ss.conflictAlert

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

  /// I13 for demand: a message applies once per receipt (a reused id carries another message).
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
    redeliveryAppliesNothing,
    a1RefutationIsFlagged,
    persistentConflictsAreFlagged,
    vmOnlyForAJobClassifiedTrusted,
    refusedJobNeverGetsAVm,
    assignedJobsClassified,
    cancelOnlyRefused,
  }

  // ---- scenarios ----------------------------------------------------------------------------

  /// Poll, record and ack the oldest message.
  action handle = poll.then(record).then(ack(false))

  // The happy path: the first batch's two trusted offers are planned within deliverable 2 and
  // acquired; the next batch's statistics count them, and desired follows the statistics.
  run acquireWithinDeliverableTest = {
    initAcquire.then(poll).then(record)
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
    initAcquire.then(applyCapacity(1, 1, OPEN)).then(handle)
      .expect(ss.acquiring == Set(1) and allInvariants)
      .then(applyCapacity(2, 2, OPEN)).then(emit).then(handle)
      .expect(ss.acquiring == Set(1) and not(ss.acquiring.union(ss.acquired).contains(3)))
      .expect(allInvariants)
  }

  // More assigned jobs than one batch shows (the 50-message cap): desired follows
  // `totalAssignedJobs`, not the batch.
  run truncatedBacklogDemandFromStatisticsTest = {
    initAcquire.then(handle).then(acquireCall(false)).then(emit)
      .expect(msgs.get(2).stats.assigned == 2 and msgs.get(2).items.length() == CAP)
      .then(handle).expect(ss.desired == 2 and desiredFromLatestStatistics)
  }

  // Redelivery after a lost ack is a duplicate: nothing is written, the ack is retried.
  run redeliveryAfterLostAckIsDuplicateTest = {
    initAcquire.then(poll).then(record).then(ack(true)).expect(queue == Set(1))
      .then(poll).expect(inHand == 1)
      .then(record).expect(ss.acquiring == Set(1, 2) and g.applied == Set((1, 1)))
      .then(ack(false)).expect(queue == Set() and allInvariants)
  }

  // A1: the message is redelivered in a new session, so its receipt key is new; it is the
  // stream's last record, at the session's floor, so it is recorded and applies nothing: its
  // offers are not planned again and its older statistics do not replace the opening's.
  run redeliveryInNewSessionReservesOnceTest = {
    initAcquire.then(poll).then(record).then(ack(true))
      .then(restart).expect(ss.phase == BACKOFF and ss.stale == 1 and ghSession == 1)
      .then(deleteStale).expect(ghSession == NONE)
      .then(openSession).expect(ss.phase == ACTIVE and ss.session == 2)
      .then(poll).expect(inHand == 1)
      .then(record)
      .expect(inbox.contains((2, 1)) and ss.acquiring == Set(1, 2) and ss.cursor == 1)
      .expect(ss.floor == 1 and ss.phase == ACTIVE)
      .expect(noDoubleReservationPerRequest and desiredFromLatestStatistics and allInvariants)
  }

  // A6, MED1 of the KER-06 review: the daemon crashes holding message 2, emitted before the
  // acquisition its predecessor planned; the new session opens with 2 assigned, then message 2
  // is recorded and its older statistics (0 assigned) replace the opening's, since no id tells
  // it from a message emitted after the open. I23 holds throughout (the acquisitions are still
  // reservations, A2); once the backlog is recorded the statistics are GitHub's own again.
  run crashBeforeRecordThenReopenConvergesTest = {
    initAcquire.then(handle).then(emit)
      .expect(msgs.get(2).stats.assigned == 0)
      .then(acquireCall(false)).expect(statsOf(jobs).assigned == 2)
      .then(poll).expect(inHand == 2)
      .then(restart).expect(inHand == NONE and ss.cursor == 1)
      .then(deleteStale).then(openSession)
      .expect(ss.stats.assigned == 2 and ss.desired == 2 and ss.floor == 1)
      .then(poll).expect(inHand == 2)
      .then(record)
      .expect(ss.stats.assigned == 0 and ss.desired == 0 and ss.acquired == Set(1, 2))
      .expect(allInvariants)
      .then(ack(false)).then(emit).then(handle)
      .expect(quiescent and ss.stats.assigned == 2 and ss.desired == 2 and allInvariants)
  }

  // A6: after an open, a change at GitHub reaches the stream only through messages; recording
  // them brings desired to GitHub's own count.
  run changeAfterOpenReachesDesiredTest = {
    initAcquire.then(handle).then(acquireCall(false))
      .then(restart).then(deleteStale).then(openSession).expect(ss.desired == 2)
      .then(startVm).then(runnerStarts(1)).then(jobFinishes(1))
      .then(emit).then(emit).then(emit)
      .then(handle).then(handle).then(handle)
      .expect(quiescent and statsOf(jobs).assigned == 1 and ss.desired == 1 and allInvariants)
  }

  // MED5 and LOW10: a message with an unknown type recorded in session 1, its ack lost, is
  // redelivered in session 2 after a restart: it counts no drift and plans nothing again.
  run redeliveryCountsNoDriftTwiceTest = {
    initAcquire.then(unknownTypeQueued).then(emit).then(handle)
      .then(poll).then(record).expect(ss.drift == 1)
      .then(ack(true)).then(restart).then(deleteStale).then(openSession)
      .expect(ss.drift == 0 and ss.floor == 2)
      .then(poll).expect(inHand == 2)
      .then(record).expect(ss.drift == 0 and ss.phase == ACTIVE and allInvariants)
  }

  // A1 refuted, recovered: session 2's numbering restarts at 1, below the floor (2), as its
  // first message; it cannot be a redelivery (oldest first), so it starts a new numbering:
  // recorded for the operator, applied as new, the cursor restarts from it, the session goes on.
  run a1RestartIsRecoveredTest = {
    initAcquire.then(handle).then(emit).then(handle).expect(ss.cursor == 2)
      .then(restart).then(deleteStale).then(openSession).expect(ss.floor == 2)
      .then(unknownTypeQueued).then(idsRestart).expect(queue == Set(1))
      .then(poll).then(record)
      .expect(ss.phase == ACTIVE and ss.renumbered and ss.drift == 1)
      .expect(ss.cursor == 1 and ss.floor == NONE and allInvariants)
      .then(ack(false)).then(acquireCall(false)).then(emit).then(handle)
      .expect(ss.cursor == 3 and ss.stats.assigned == 2 and ss.desired == 2)
      .expect(ss.phase == ACTIVE and allInvariants)
  }

  // A1 refuted later in a session: an id at or below the floor after the session recorded a
  // message cannot start a numbering; it applies nothing and degrades the session.
  run a1RefutedMidSessionDegradesTest = {
    initAcquire.then(handle).then(emit).then(handle).expect(ss.cursor == 2)
      .then(restart).then(deleteStale).then(openSession).expect(ss.floor == 2)
      .then(unknownTypeQueued).then(emit).then(handle).expect(ss.cursor == 3 and ss.drift == 1)
      .then(unknownTypeQueued).then(idsRestart).then(poll).then(record)
      .expect(ss.phase == DEGRADED and ss.drift == 1 and ss.cursor == 3)
      .expect(not(ss.renumbered) and allInvariants)
  }

  // A1's limit: a restarted id equal to the floor, as the session's first message, reads as the
  // redelivery of the stream's last record; it applies nothing, and the session goes on.
  run a1RefutedAtTheFloorReadsAsARedeliveryTest = {
    initAcquire.then(handle).then(restart).then(deleteStale).then(openSession).expect(ss.floor == 1)
      .then(idsRestart).then(poll).then(record)
      .expect(ss.phase == ACTIVE and ss.acquiring == Set(1, 2) and allInvariants)
  }

  // A3, MED3: a lost create leaves a session mayfly has no record of; creates answer 409; at
  // `CONFLICT_LIMIT` in a row the scale set escalates to the operator, keeps backing off and
  // never deletes it; once it ends, a create succeeds and the count starts again.
  run persistentConflictsEscalateTest = {
    initAcquire.then(restart).then(deleteStale).then(openLost)
      .then(openSession).expect(ss.conflicts == 1 and not(ss.conflictAlert))
      .then(openSession).expect(ss.conflicts == 2 and ss.conflictAlert and ss.phase == BACKOFF)
      .expect(allInvariants)
      .then(sessionEnds).then(openSession)
      .expect(ss.phase == ACTIVE and ss.conflicts == 0 and not(ss.conflictAlert) and allInvariants)
  }

  // LOW6 (P21): a degraded session records and releases but plans no offer: job 1, requeued and
  // offered again within budget, is declined as held.
  run degradedSessionPlansNothingTest = {
    initAcquire.then(applyCapacity(1, 1, OPEN)).then(handle).then(acquireCall(false))
      .then(unknownTypeQueued).then(emit).then(unknownTypeQueued).then(jobRequeued(1, true))
      .then(emit).then(emit)
      .then(handle).then(handle).expect(ss.phase == DEGRADED)
      .then(applyCapacity(2, 2, OPEN)).then(handle)
      .expect(ss.acquired == Set() and ss.acquiring == Set() and jobs.get(1) == J_AVAILABLE)
      .expect(allInvariants)
  }

  // A3: a create whose answer was lost leaves a session mayfly has no record of; the next
  // create answers 409 and the session backs off, never deleting it; once it ends, a create
  // succeeds and desired is recomputed from its initial statistics.
  run sessionConflictBacksOffTest = {
    initAcquire.then(handle).then(acquireCall(false))
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
    initAcquire.then(handle).then(acquireCall(false))
      .then(sessionEnds).then(sessionLoss).expect(ss.phase == BACKOFF and ss.stale == 1)
      .then(deleteStale).then(openSession)
      .expect(ss.session == 2 and ss.stats.assigned == 2 and ss.desired == 2 and allInvariants)
  }

  // R7: unknown message types are recorded, acked and counted; past the threshold the session
  // is Degraded and keeps recording; the operator clears it.
  run unknownTypesDegradeTheSessionTest = {
    initAcquire.then(unknownTypeQueued).then(unknownTypeQueued).then(emit).then(emit)
      .then(handle).then(handle).expect(ss.drift == 1 and ss.phase == ACTIVE)
      .then(handle).expect(ss.drift == 2 and ss.phase == DEGRADED and queue == Set())
      .then(recoverDrift).expect(ss.phase == ACTIVE and ss.drift == 0 and allInvariants)
  }

  // ADR 0029: draining sends 0 and acquires nothing; the message is still recorded and acked.
  run drainSendsZeroAndAcquiresNothingTest = {
    initAcquire.then(applyCapacity(2, 2, DRAINING)).then(poll).expect(g.sentMax == 0)
      .then(record).expect(ss.acquiring == Set() and ss.desired == 0)
      .then(ack(false)).expect(queue == Set() and allInvariants)
  }

  // ADR 0029: paused sends the live runners (the capacity policy's reported max) and acquires
  // nothing.
  run pauseSendsRunnersAndAcquiresNothingTest = {
    initAcquire.then(applyCapacity(2, 1, PAUSED)).then(poll).expect(g.sentMax == 1)
      .then(record).expect(ss.acquiring == Set())
      .then(ack(false)).expect(allInvariants)
  }

  // ADR 0029: safe mode with a writable journal records and acks, sends 0 and acquires nothing;
  // with an unwritable journal nothing is polled or recorded (GitHub keeps the message).
  run safeModeRecordsButNeverAcquiresTest = {
    initAcquire.then(applyCapacity(2, 2, SAFE)).then(handle)
      .expect(g.sentMax == 0 and ss.acquiring == Set() and queue == Set())
      .then(emit).then(journalToggles)
      .expect(not(journalWritable) and queue == Set(2) and allInvariants)
  }

  // Reassignment: an acquired job no runner took is completed as canceled and queued again;
  // its reservation is released by the completion and it may be planned again, once.
  run requeuedJobIsReservedOnceAgainTest = {
    initAcquire.then(applyCapacity(1, 1, OPEN)).then(handle).then(acquireCall(false))
      .expect(ss.acquired == Set(1))
      .then(jobRequeued(1, true)).expect(jobs.get(1) == J_AVAILABLE)
      .then(emit).then(handle).expect(ss.acquired == Set(1))
      .then(emit).then(handle)
      .expect(buffer == NO_ITEMS and ss.acquired == Set() and ss.acquiring == Set(1))
      .expect(noDoubleReservationPerRequest and allInvariants)
  }

  // A lost acquire answer leaves the ids pending (counted against deliverable); the retried
  // call answers them.
  run lostAcquireAnswerStaysPendingTest = {
    initAcquire.then(handle).then(acquireCall(true))
      .expect(ss.acquiring == Set(1, 2) and jobs.get(1) == J_ACQUIRED)
      .then(acquireCall(false))
      .expect(ss.acquiring == Set() and ss.acquired == Set() and allInvariants)
  }

  // github.com (ADR 0090): advertising 0 gets nothing assigned and the counts never move (ARC
  // #3446, scaleset #133); once a poll advertises room, a trusted job is assigned, classified
  // and gets a runner, which takes it.
  run directAssignmentNeedsAdvertisedRoomTest = {
    initDirect.then(applyCapacity(0, 0, OPEN)).then(poll).expect(ghMax == 0 and inHand == NONE)
      .expect(not(assignRoom))
      .then(applyCapacity(2, 2, OPEN)).then(poll).expect(ghMax == 2)
      .then(ghAssigns(1)).then(emit).then(handle)
      .expect(ss.pending == Set(1) and ss.desired == 1 and not(vmMayStart))
      .then(lookupRun(1, true))
      .expect(ss.trusted == Set(1) and ss.refused == Set() and ss.pending == Set())
      .then(startVm).expect(idle == 1 and not(vmMayStart))
      .then(runnerStarts(1)).then(emit).then(handle)
      .expect(ss.trusted == Set() and idle == 0 and not(g.untrustedRan) and allInvariants)
  }

  // ADR 0090: an assigned `pull_request_target` job is refused, after its run's lookup, before any
  // runner starts; it holds
  // its slot of `maxCapacity` (GitHub counts it and assigns no more), and while it is assigned no
  // runner starts, not even for a trusted job; GitHub releases it (canceled, requeued), the
  // refusal is released, and the trusted job gets its runner.
  run assignedUntrustedJobIsRefusedTest = {
    initDirect.then(poll).expect(ghMax == 2)
      .then(ghAssigns(3)).then(emit).then(handle).then(lookupRun(3, true))
      .expect(ss.refused == Set(3) and ss.trusted == Set() and ss.desired == 1)
      .expect(not(vmMayStart))
      .then(ghAssigns(1)).then(emit).then(handle).then(lookupRun(1, true))
      .expect(ss.refused == Set(3) and ss.trusted == Set(1) and ss.desired == 2)
      .expect(assignedSet(jobs).size() == 2 and not(assignRoom) and not(vmMayStart))
      .then(jobRequeued(3, true)).then(emit).then(handle)
      .expect(ss.refused == Set() and ss.trusted == Set(1))
      .then(startVm).expect(idle == 1 and allInvariants)
  }

  // The limit ADR 0090 names, not an invariant: a runner started for a trusted job may take an
  // untrusted job GitHub assigned after it started (runners of a set are interchangeable). The
  // per-job check stops every runner it can; `cancel_refused_runs` shortens what remains.
  run runnerMayTakeAJobAssignedAfterItStartedTest = {
    initDirect.then(poll).then(ghAssigns(1)).then(emit).then(handle).then(lookupRun(1, true))
      .then(startVm).then(ghAssigns(3)).then(runnerStarts(3))
      .expect(g.untrustedRan and allInvariants)
  }

  // GitHub deletes the scale set as dormant while no session lives; the create answers 404, and
  // mayfly re-creates it: a new id, the numbering afresh, the old set's reservations dropped,
  // and the job thrown back to GitHub's queue is offered to the new set.
  run dormantScaleSetIsRecreatedTest = {
    initAcquire.then(handle).then(acquireCall(false)).then(emit).then(handle)
      .expect(ss.acquired == Set(1, 2) and ss.cursor == 2)
      .then(restart).then(deleteStale).expect(ghSession == NONE)
      .expect(not(listening(ss)))
      .then(emit).then(dormantDeleted).expect(ghSetGen == 2 and queue == Set())
      .expect(ss.setGen != ghSetGen)
      .then(recreateScaleSet)
      .expect(ss.setGen == 2 and ss.cursor == NONE and ss.acquired == Set() and ss.desired == 0)
      .then(openSession).expect(ss.phase == ACTIVE and ss.floor == NONE)
      .then(emit).then(handle)
      .expect(ss.acquiring == Set(1, 2) and allInvariants)
  }

  // ADR 0090 progress: a refused job is released and assigned again until GitHub fails it at the
  // third release; then the trusted job gets its runner.
  run refusedJobFailsThenTrustedJobRunsTest = {
    initDirect.then(poll).then(ghAssigns(3)).then(ghAssigns(1)).then(emit).then(handle)
      .then(lookupRun(3, true)).then(lookupRun(1, true))
      .expect(ss.refused == Set(3) and ss.trusted == Set(1) and not(vmMayStart))
      .then(jobRequeued(3, true)).then(ghAssigns(3)).then(emit).then(handle)
      .then(lookupRun(3, true))
      .expect(ss.refused == Set(3) and released.get(3) == 1)
      .then(jobRequeued(3, true)).then(ghAssigns(3)).then(emit).then(handle)
      .then(lookupRun(3, true))
      .expect(ss.refused == Set(3) and released.get(3) == 2)
      .then(jobRequeued(3, true)).expect(jobs.get(3) == J_DONE)
      .then(emit).then(handle)
      .expect(ss.refused == Set() and vmMayStart)
      .then(startVm).then(runnerStarts(1))
      .expect(jobs.get(1) == J_RUNNING and not(g.untrustedRan) and allInvariants)
  }

  // ADR 0090 progress when the release names no job (`runnerRequestId` 0 on github.com): the
  // refusal lapses after the release window, or the operator releases it; then the trusted job
  // gets its runner.
  run refusalExpiresWhenTheReleaseNamesNoJobTest = {
    initDirect.then(poll).then(ghAssigns(3)).then(ghAssigns(1)).then(emit).then(handle)
      .then(lookupRun(3, true)).then(lookupRun(1, true))
      .then(jobRequeued(3, false)).then(emit).then(handle)
      .expect(ss.refused == Set(3) and jobs.get(3) == J_AVAILABLE and not(vmMayStart))
      .then(assignmentExpires(3))
      .expect(ss.refused == Set() and vmMayStart and allInvariants)
      .then(startVm).then(runnerStarts(1)).expect(jobs.get(1) == J_RUNNING and allInvariants)
  }

  run operatorReleasesARefusalTest = {
    initDirect.then(poll).then(ghAssigns(3)).then(ghAssigns(1)).then(emit).then(handle)
      .then(lookupRun(3, true)).then(lookupRun(1, true))
      .then(jobRequeued(3, false)).then(emit).then(handle)
      .then(operatorReleasesRefusal(3))
      .expect(ss.refused == Set() and ss.trusted == Set(1) and vmMayStart and allInvariants)
  }

  // ADR 0090 (owner, 2026-11-12): a failed run lookup refuses even a trusted job: nothing runs
  // unclassified, and no runner starts while its lookup is pending.
  run failedLookupRefusesTest = {
    initDirect.then(poll).then(ghAssigns(1)).then(emit).then(handle)
      .expect(ss.pending == Set(1) and not(vmMayStart))
      .then(lookupRun(1, false))
      .expect(ss.refused == Set(1) and ss.trusted == Set() and not(vmMayStart) and allInvariants)
  }

  // `cancel_refused_runs` (owner, 2026-11-12): the refused run is cancelled at once; the hold
  // lasts until the cancellation is observed, and then the trusted job gets its runner.
  run cancelledRefusedRunReleasesTheHoldTest = {
    initDirectWith(true).then(poll).then(ghAssigns(3)).then(ghAssigns(1)).then(emit)
      .then(handle).then(lookupRun(3, true)).then(lookupRun(1, true))
      .expect(ss.refused == Set(3) and not(vmMayStart))
      .then(cancelRun(3, true, true)).expect(jobs.get(3) == J_DONE and ss.refused == Set(3))
      .expect(not(vmMayStart))
      .then(emit).then(handle)
      .expect(ss.refused == Set() and vmMayStart and allInvariants)
      .then(startVm).then(runnerStarts(1)).expect(not(g.untrustedRan) and allInvariants)
  }

  // A cancel that fails (or whose answer is lost) changes nothing at GitHub: the hold stays and
  // the release window still bounds it.
  run failedCancelFallsBackToTheWindowTest = {
    initDirectWith(true).then(poll).then(ghAssigns(3)).then(ghAssigns(1)).then(emit)
      .then(handle).then(lookupRun(3, true)).then(lookupRun(1, true))
      .then(cancelRun(3, false, false)).expect(ss.cancelTried == Set() and jobs.get(3) == J_ACQUIRED)
      .then(cancelRun(3, false, true)).expect(ss.cancelTried == Set(3) and not(vmMayStart))
      .then(jobRequeued(3, false)).then(emit).then(handle).expect(not(vmMayStart))
      .then(assignmentExpires(3)).expect(vmMayStart and allInvariants)
  }

  // A numbering restarted within a session reuses an id the session recorded: its receipt is
  // not a duplicate (only the cursor can be redelivered), so it applies nothing and degrades.
  run restartWithinASessionDegradesTest = {
    initAcquire.then(poll).then(record).then(ack(true))
      .then(restart).then(deleteStale).then(openSession).then(handle)
      .expect(inbox.contains((2, 1)) and ss.cursor == 1)
      .then(unknownTypeQueued).then(emit).then(handle).expect(ss.cursor == 2)
      .then(unknownTypeQueued).then(idsRestart).then(poll).expect(inHand == 1)
      .then(record)
      .expect(ss.phase == DEGRADED and ss.cursor == 2 and allInvariants)
  }

  // A job released with a `JobCompleted` that names no job and assigned again is classified
  // afresh: its earlier refusal (a failed lookup) does not survive into the new assignment, so its
  // trusted run is never cancelled (simulation found the stale refusal, GH-00R's last review).
  run reassignedJobIsClassifiedAfreshTest = {
    initDirectWith(true).then(poll).then(ghAssigns(1)).then(emit).then(handle)
      .then(lookupRun(1, false)).expect(ss.refused == Set(1))
      .then(jobRequeued(1, false)).then(ghAssigns(1)).then(emit).then(handle)
      .expect(ss.pending == Set(1) and ss.refused == Set())
      .then(lookupRun(1, true))
      .expect(ss.trusted == Set(1) and ss.refused == Set() and allInvariants)
  }

  // The limit ADR 0090 names, second form: the untrusted job was assigned before the runner
  // started but after the last message mayfly recorded; mayfly could not know of it.
  run runnerMayTakeAJobAssignedBeforeItStartedTest = {
    initDirect.then(poll).then(ghAssigns(1)).then(emit).then(handle).then(lookupRun(1, true))
      .then(ghAssigns(3)).then(startVm).then(runnerStarts(3))
      .expect(g.untrustedRan and allInvariants)
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
    idsRestarted' = idsRestarted, keepGh,
    inHand' = NONE, inHandRecorded' = false,
    keepJournal,
    g' = { ...ghostAfter(g, "listener", ss, ss),
      ackedUnrecorded: g.ackedUnrecorded or not(g.recordedIds.contains(inHand)) },
  }

  // Mutant 4 violates i12RecordBeforeAck.
  run mutant4AckBeforeRecordViolatesTest = {
    initAcquire.then(poll).then(ackBeforeRecord)
      .expect(not(i12RecordBeforeAck) and not(allInvariants))
  }

  // Mutant 5 (count JobAssigned messages) violates desiredFromLatestStatistics: the batch holds
  // one JobAssigned, the statistics two.
  run mutant5CountMessagesViolatesTest = {
    initAcquire.then(handle).then(acquireCall(false)).then(emit)
      .then(poll).then(recordWith(COUNT_MESSAGES))
      .expect(not(desiredFromLatestStatistics) and not(allInvariants))
  }

  // Mutant 6 (add totalRunningJobs) violates desiredFromLatestStatistics once a job runs.
  run mutant6AddRunningViolatesTest = {
    initAcquire.then(applyCapacity(3, 3, OPEN)).then(handle).then(acquireCall(false))
      .then(startVm).then(runnerStarts(1)).then(emit)
      .expect(msgs.get(2).stats == { assigned: 2, running: 1 })
      .then(poll).then(recordWith(ADD_RUNNING))
      .expect(not(desiredFromLatestStatistics) and not(allInvariants))
  }

  // Mutant 7 (ignore the inbox receipt) applies a redelivered message twice.
  run mutant7IgnoreInboxViolatesTest = {
    initAcquire.then(poll).then(record).then(ack(true)).then(poll).then(recordWith(IGNORE_INBOX))
      .expect(not(messageAppliesOnce) and not(allInvariants))
  }

  // Model-local mutant OPEN_WINS (the opening's statistics are never replaced in a session): a
  // change after the open never reaches desired.
  action handleOpenWins = poll.then(recordWith(OPEN_WINS)).then(ack(false))

  run mutantOpenWinsNeverConvergesTest = {
    initAcquire.then(handle).then(acquireCall(false))
      .then(restart).then(deleteStale).then(openSession)
      .then(startVm).then(runnerStarts(1)).then(jobFinishes(1))
      .then(emit).then(emit).then(emit)
      .then(handleOpenWins).then(handleOpenWins).then(handleOpenWins)
      .expect(quiescent and ss.desired == 2)
      .expect(not(desiredFromLatestStatistics) and not(allInvariants))
  }

  // Model-local mutant REAPPLY (KER-06's rule: a message from before the session is folded like
  // a new one) counts the redelivered unknown type again.
  run mutantReapplyCountsDriftTwiceTest = {
    initAcquire.then(unknownTypeQueued).then(emit).then(handle)
      .then(poll).then(record).then(ack(true))
      .then(restart).then(deleteStale).then(openSession)
      .then(poll).then(recordWith(REAPPLY))
      .expect(ss.drift == 1 and not(redeliveryAppliesNothing) and not(allInvariants))
  }

  // ADR 0090 mutant RUN_ASSIGNED (assigned jobs accepted without classification): the
  // `pull_request_target` job is accepted and a runner starts for it.
  run mutantRunAssignedViolatesTest = {
    initDirect.then(poll).then(ghAssigns(3)).then(emit)
      .then(poll).then(recordWith(RUN_ASSIGNED)).then(ack(false))
      .expect(not(assignedJobsClassified) and not(allInvariants))
      .then(startVm)
      .expect(not(vmOnlyForAJobClassifiedTrusted) and not(refusedJobNeverGetsAVm))
  }

  // ADR 0090 mutant IGNORE_REFUSALS: a runner starts for the trusted job while the refused one is
  // assigned, so the refused job may take it.
  run mutantIgnoreRefusalsViolatesTest = {
    initDirect.then(poll).then(ghAssigns(3)).then(ghAssigns(1)).then(emit).then(handle)
      .then(lookupRun(3, true)).then(lookupRun(1, true))
      .expect(ss.refused == Set(3) and ss.trusted == Set(1))
      .then(startVmWith(IGNORE_REFUSALS))
      .expect(not(refusedJobNeverGetsAVm) and not(allInvariants))
  }

  // ADR 0090 mutant IGNORE_PENDING: a runner starts while the `pull_request_target` assignment
  // still waits for its run's lookup.
  run mutantIgnorePendingViolatesTest = {
    initDirect.then(poll).then(ghAssigns(1)).then(emit).then(handle).then(lookupRun(1, true))
      .then(ghAssigns(3)).then(emit).then(handle).expect(ss.pending == Set(3))
      .then(startVmWith(IGNORE_PENDING))
      .expect(not(refusedJobNeverGetsAVm) and not(allInvariants))
  }

  // ADR 0090 mutant CANCEL_ANY: a trusted job's run is cancelled, with the option off.
  run mutantCancelAnyViolatesTest = {
    initDirect.then(poll).then(ghAssigns(1)).then(emit).then(handle).then(lookupRun(1, true))
      .then(cancelRunWith(CANCEL_ANY, 1, true, true))
      .expect(not(cancelOnlyRefused) and not(allInvariants))
  }

  // Mutant 19 (acquire every offer) violates I23 past deliverable and I32 on an untrusted offer.
  run mutant19AcquireEveryViolatesTest = {
    initAcquire.then(applyCapacity(1, 1, OPEN)).then(poll).then(recordWith(ACQUIRE_EVERY))
      .expect(not(i23AcquireBelowDeliverable) and not(allInvariants))
      .then(ack(false)).then(emit).then(poll).then(recordWith(ACQUIRE_EVERY))
      .expect(not(i32TrustedOnly))
  }
}
