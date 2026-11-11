// -*- mode: Bluespec; -*-
/**
 * The scheduling model (KER-05): one host's `Host` aggregate (doc/domain-model.md §6.3), the
 * `Pool` aggregates whose terms it applies (§6.2), the pure capacity policy that carves the
 * host's free capacity into per-pool deliverables and `reported_max` (plan §9), and the data
 * plane's resource ledger that enforces the same limits again (ADR 0024, I33).
 *
 * The Rust code is `mayfly_control_domain::host`, `mayfly_control_domain::pool` and
 * `mayfly_control_domain::capacity`; the conformance driver (VER-01) maps each action below onto
 * one of their stimuli, and the plan functions onto `capacity::plan`.
 *
 * The data plane (`dp`, `vms`, `sent`) is the truth: what the provider runs, what foreign VMs
 * consume, and the capacity reports it has sent. The host (`host`, `leases`, `slotGen`, `terms`)
 * is what the journal records; it learns of the truth only through reports, which may arrive
 * late and out of order. Each action is one stimulus: `command` (a `Host` or `Pool` command),
 * `report` (a `CapacityReported` applied by `RecordCapacityReport`), `dp` (the data plane
 * executes or reports) or `world` (something we do not control changes).
 *
 * Ghost state (`g`) records history for the invariants and never feeds a guard.
 */
module scheduling_machine {
  // ---- constants ----------------------------------------------------------------------------

  /// Attempt ids the reconciler may mint: each reserves at most once.
  pure val ATTEMPTS = Set(1, 2, 3)
  /// Slot numbers a report may offer.
  pure val SLOTS = Set(0, 1, 2)
  /// Two pools, one per guest-OS class; name order is the carve-out order (`linux` first).
  pure val LINUX = "linux"
  pure val MACOS = "macos"
  pure val POOLS = Set(LINUX, MACOS)
  /// What the host offers VMs, in units (the data plane's totals).
  pure val TOTAL_MEM = 5
  pure val TOTAL_DISK = 3
  /// The operator's reserve, held back from admission on both planes.
  pure val RESERVE_MEM = 1
  /// G3: at most two running macOS guests per host, foreign ones included.
  pure val MACOS_GUESTS = 2
  /// Each pool's `max_running` (its terms).
  pure val MAX_RUNNING = 2
  /// Stage failures (per window) that trip a pool's breaker.
  pure val BREAKER_N = 2
  /// The most foreign memory the world may consume.
  pure val FOREIGN_MAX = 2
  /// Reports the data plane may send (the model is finite).
  pure val MAX_REPORTS = 3
  pure val NO_SLOT = 99

  // Lease phases; FREE is "this attempt id never reserved".
  pure val FREE = "free"
  pure val RESERVED = "reserved"
  pure val ACTIVATED = "activated"
  pure val QUARANTINED = "quarantined"
  pure val RELEASED = "released"

  // The VM of a lease, as the provider has it.
  pure val ABSENT = "absent"
  pure val RUNNING = "running"
  pure val STOPPED = "stopped"

  // Drain of the host.
  pure val NO_DRAIN = "none"
  pure val DRAINING = "draining"
  pure val DRAINED = "drained"

  pure def vmMem(pool: str): int = if (pool == MACOS) 2 else 1
  pure val VM_DISK = 1

  // ---- state --------------------------------------------------------------------------------

  /// `CapacityReported`: what the data plane's ledger says, at the time it was sent. `id` 0 is
  /// "no report applied yet".
  type Report = {
    id: int, mem: int, disk: int, slots: int, macos: int,
    foreignMem: int, foreignVms: int, foreignMacos: int,
  }

  type Lease = { phase: str, slot: int, gen: int, pool: str, running: bool }

  /// `policy` and `waiting` are the capacity policy's inputs per pool (ADR 0069, DEC-06): the
  /// pool's configured `capacity_policy` (`EXACT` or `DEMAND`) and the jobs it is told are
  /// waiting for new VMs. Both are fixed at `init`: the properties are stated for a fixed demand.
  type Host = {
    report: Report, gateClosed: bool, drain: str, policy: str -> str, waiting: str -> int,
  }

  type Terms = { rev: int, open: bool }

  type Pool = { rev: int, active: bool, failures: int, tripped: bool }

  /// The data plane's truth outside our VMs. `memUnknown`: a listed VM's memory is not known
  /// (an adopted or malformed VM the provider did not describe, or a foreign VM the probe could
  /// not size), so the free memory is indeterminate (ADR 0052).
  type Dp = { foreignMem: int, foreignMacos: int, nextId: int, memUnknown: bool }

  type Ghost = {
    lastKind: str,
    // (slot, generation) pairs ever reserved
    issued: Set[(int, int)],
    // a (slot, generation) reserved twice
    reissue: bool,
    // a reservation whose generation is not above its slot's previous one
    genNotIncreasing: bool,
    // a reservation that did not fit the last applied report
    overAllocated: bool,
    // a reservation that took a third macOS guest slot
    macosOver: bool,
    // a reservation while the gate, the drain or the pool's terms were closed
    reservedClosed: bool,
    // a grant while the gate, the drain or the pool's terms were closed
    grantedClosed: bool,
    // a reservation or grant after drain start
    afterDrain: bool,
    // the drain completed with a lease in scope
    drainedWithLeases: bool,
    // the data plane started a VM that did not fit its own ledger
    dpOvercommit: bool,
    // the data plane started a VM while the memory it needs was not known (ADR 0052)
    dpStartedUnknown: bool,
    // the data plane created a second VM for one lease
    doubleCreate: bool,
    // a quarantined lease released while its VM exists
    quarantineReleasedEarly: bool,
    // a quarantined VM deleted without an operator's resolution
    autoDeleteQuarantined: bool,
    // an older report replaced a newer one
    staleApplied: bool,
    // a reservation for a pool whose breaker was tripped at the terms the host holds
    trippedAdmitted: bool,
  }

  var host: Host
  var leases: int -> Lease
  var slotGen: int -> int
  var terms: str -> Terms
  var pools: str -> Pool
  var dp: Dp
  var vms: int -> str
  var created: int -> bool
  var sent: Set[Report]
  var g: Ghost

  // ---- helpers ------------------------------------------------------------------------------

  pure def max0(x: int): int = if (x < 0) 0 else x
  pure def min2(x: int, y: int): int = if (x < y) x else y

  pure def stoppedQuarantined(l: Lease): bool = l.phase == QUARANTINED and not(l.running)
  pure def live(l: Lease): bool = Set(RESERVED, ACTIVATED, QUARANTINED).contains(l.phase)
  /// A lease that can still produce a runner: live and not quarantined.
  pure def runner(l: Lease): bool = Set(RESERVED, ACTIVATED).contains(l.phase)

  /// What a lease holds: everything, except a stopped quarantined lease, which shrinks to disk
  /// and slot (plan §9).
  pure def holdsMem(l: Lease): int =
    if (not(live(l))) 0
    else if (l.phase == QUARANTINED and not(l.running)) 0
    else vmMem(l.pool)

  pure def allocMemOf(ls: int -> Lease): int = ATTEMPTS.fold(0, (s, a) => s + holdsMem(ls.get(a)))
  pure def allocVmsOf(ls: int -> Lease): int =
    ATTEMPTS.fold(0, (s, a) => if (live(ls.get(a))) s + 1 else s)
  pure def allocMacosOf(ls: int -> Lease): int =
    ATTEMPTS.fold(0, (s, a) => if (live(ls.get(a)) and ls.get(a).pool == MACOS) s + 1 else s)
  pure def countOf(ls: int -> Lease, pool: str, pred: Lease => bool): int =
    ATTEMPTS.fold(0, (s, a) => if (pred(ls.get(a)) and ls.get(a).pool == pool) s + 1 else s)

  // What a report admits before our leases: totals minus foreign consumption and the reserve.
  pure def limMem(r: Report): int = max0(r.mem - r.foreignMem - RESERVE_MEM)
  pure def limDisk(r: Report): int = r.disk
  pure def limVms(r: Report): int = max0(r.slots - r.foreignVms)
  pure def limMacos(r: Report): int = max0(min2(r.macos, MACOS_GUESTS) - r.foreignMacos)

  /// Whether one more VM of `pool` fits a report on top of the leases `ls`, on every dimension
  /// it uses: memory, disk and a VM slot always, a macOS-guest slot for a macOS pool only (a
  /// Linux VM is admitted while a newer report has shrunk the macOS slots below what we hold).
  pure def fitsReport(r: Report, ls: int -> Lease, pool: str): bool = and {
    allocMemOf(ls) + vmMem(pool) <= limMem(r),
    (allocVmsOf(ls) + 1) * VM_DISK <= limDisk(r),
    allocVmsOf(ls) + 1 <= limVms(r),
    pool != MACOS or allocMacosOf(ls) + 1 <= limMacos(r),
  }

  pure def freeSlotsOf(r: Report, ls: int -> Lease): Set[int] =
    SLOTS.filter(s => s < r.slots and not(ATTEMPTS.exists(a => live(ls.get(a)) and ls.get(a).slot == s)))

  /// The host reserves the lowest free slot.
  pure def lowest(xs: Set[int]): int = xs.fold(NO_SLOT, (m, x) => if (x < m) x else m)

  pure def gateOpen(h: Host): bool = h.report.id > 0 and not(h.gateClosed) and h.drain == NO_DRAIN

  // ---- the capacity policy (`capacity::plan`) ------------------------------------------------

  /// The free capacity the policy carves, on the four dimensions admission counts. Also one
  /// VM's footprint (`vms` 1, `mac` 1 for a macOS guest) and what a pool keeps.
  type Free = { mem: int, disk: int, vms: int, mac: int }

  pure val NOTHING: Free = { mem: 0, disk: 0, vms: 0, mac: 0 }
  pure def plus(f: Free, g: Free): Free =
    { mem: f.mem + g.mem, disk: f.disk + g.disk, vms: f.vms + g.vms, mac: f.mac + g.mac }
  /// `f` less `g`, floored at zero on every dimension.
  pure def less(f: Free, g: Free): Free = {
    mem: max0(f.mem - g.mem), disk: max0(f.disk - g.disk), vms: max0(f.vms - g.vms),
    mac: max0(f.mac - g.mac),
  }
  pure def times(v: Free, n: int): Free = { mem: v.mem * n, disk: v.disk * n, vms: v.vms * n, mac: v.mac * n }
  pure def lower(f: Free, g: Free): Free = {
    mem: min2(f.mem, g.mem), disk: min2(f.disk, g.disk), vms: min2(f.vms, g.vms),
    mac: min2(f.mac, g.mac),
  }
  pure def within(f: Free, g: Free): bool =
    f.mem <= g.mem and f.disk <= g.disk and f.vms <= g.vms and f.mac <= g.mac

  /// One pool as the carve sees it: its VM's footprint, its `max_running` headroom (live and
  /// quarantined leases count against it), its `room` (`max_running` minus its live runners:
  /// its headroom and its quarantined places), what its quarantined leases fall short of one
  /// of its VMs (`owed`: the sum of `vm − holds` over them, so a stopped one owes its memory),
  /// and whether its admission is open.
  type Shape = { vm: Free, head: int, room: int, owed: Free, open: bool }

  /// How many VMs of `v` fit `f`, on every dimension it uses (at most `f.vms`: a VM takes a slot).
  pure def fitIn(f: Free, v: Free): int = {
    val bySlots = max0(f.vms)
    val byMem = if (v.mem > 0) min2(bySlots, max0(f.mem) / v.mem) else bySlots
    val byDisk = if (v.disk > 0) min2(byMem, max0(f.disk) / v.disk) else byMem
    if (v.mac > 0) min2(byDisk, max0(f.mac) / v.mac) else byDisk
  }

  pure def fitShape(f: Free, s: Shape): int = fitIn(f, s.vm)

  /// What an open pool keeps ahead of the pools after it (KER-05D, ADR 0069): room for its
  /// headroom of its VMs, and for each quarantined place (`room − head` of them) only what its
  /// quarantined leases do not hold already (`owed`, at most one VM per place). Releasing a
  /// quarantined lease then frees `holds` and grows the keep by `vm − owed` of it, which is at
  /// most `holds`, so the pools after it never lose (`carveMonotone`). KER-05H kept a whole VM
  /// per quarantined place (`keepsRoom`): the quarantined lease's own holds, already out of the
  /// free capacity, were kept a second time. The keep never depends on the free capacity, so a
  /// later pool's share does not depend on how much of it the earlier pool could use: handing a
  /// later pool the leftover of a smaller free capacity let it take more (`carveLeftover`).
  pure def keeps(s: Shape): Free =
    if (s.open) plus(times(s.vm, s.head), lower(times(s.vm, max0(s.room - s.head)), s.owed))
    else NOTHING

  /// The carve of two pools in order: the first up to its headroom within `f`, the second up
  /// to its headroom within what the first keeps ahead of it.
  pure def carveTwo(f: Free, first: Shape, second: Shape): (int, int) = {
    val c1 = if (first.open) min2(first.head, fitShape(f, first)) else 0
    val c2 = if (second.open) min2(second.head, fitShape(less(f, keeps(first)), second)) else 0
    (c1, c2)
  }

  /// Whether `n` VMs of `v` (`n` at least 1) fit `f` less `kept`, as `fitIn(less(f, kept), v)
  /// >= n` reads it: a slot each, and `n·v + kept ≤ f` on every other dimension the VM uses.
  pure def fitsAfter(f: Free, kept: Free, v: Free, n: int): bool = and {
    n + kept.vms <= f.vms,
    v.mem == 0 or n * v.mem + kept.mem <= f.mem,
    v.disk == 0 or n * v.disk + kept.disk <= f.disk,
    v.mac == 0 or n * v.mac + kept.mac <= f.mac,
  }

  /// `min2(head, fitIn(less(f, kept), v))` for a headroom of at most `MAX_RUNNING`: the number
  /// of `n` in `1..MAX_RUNNING` within the headroom whose `n` VMs fit, since fitting is monotone
  /// in `n`; a sum of indicators, so no argument is copied.
  pure def carveCount(f: Free, kept: Free, v: Free, headroom: int): int =
    1.to(MAX_RUNNING).fold(0, (c, n) => c + (if (n <= headroom and fitsAfter(f, kept, v, n)) 1 else 0))

  /// One pool as the state machine's plan reads it: counts, not a `Shape`. `cap` is how many
  /// VMs its policy lets it be carved and keep for: `MAX_RUNNING` under `EXACT`, the jobs it is
  /// told wait for under `DEMAND`.
  type Counts = { pool: str, open: bool, live: int, runners: int, stopped: int, cap: int }

  /// `carveTwo(f, shapeOfCounts(a), shapeOfCounts(b))` for the state machine's pools (headroom
  /// at most `MAX_RUNNING`), written from the counts so that Apalache's inliner, which copies
  /// an operator's argument at every use, keeps the plan small: a `Shape` read field by field,
  /// the floors of `less` and the nested minima of `fitIn` copied the plan's terms until KER-05D's
  /// carve ran it out of memory before it checked anything (DEC-06). Equal to `carveTwo` on
  /// every such input (`carveCountsTest` in `scheduling`).
  pure def carveCounts(f: Free, a: Counts, b: Counts): (int, int) = {
    val headA = min2(max0(MAX_RUNNING - a.live), a.cap)
    val headB = min2(max0(MAX_RUNNING - b.live), b.cap)
    // `keeps(shapeOfCounts(a))`: its headroom of VMs, and for its quarantined places what its
    // stopped quarantined leases owe (their memory), at most a VM per place
    val places = max0(MAX_RUNNING - a.runners) - max0(MAX_RUNNING - a.live)
    val vmA = vmOf(a.pool)
    val kept = if (a.open) {
      mem: vmA.mem * headA + min2(vmA.mem * max0(places), vmA.mem * a.stopped),
      disk: vmA.disk * headA,
      vms: headA,
      mac: vmA.mac * headA,
    } else NOTHING
    val c1 = if (a.open) carveCount(f, NOTHING, vmA, headA) else 0
    val c2 = if (b.open) carveCount(f, kept, vmOf(b.pool), headB) else 0
    (c1, c2)
  }

  type Plan = {
    freeMem: int, freeDisk: int, freeVms: int, freeMacos: int,
    carveLinux: int, carveMacos: int,
  }

  /// One VM of `pool`: memory, disk, a slot, and a macOS-guest slot for macOS.
  pure def vmOf(pool: str): Free =
    { mem: vmMem(pool), disk: VM_DISK, vms: 1, mac: if (pool == MACOS) 1 else 0 }

  /// What a live lease holds (`holdsMem` and the disk, slot and macOS-guest slot it keeps).
  pure def holdsOf(l: Lease): Free =
    { mem: holdsMem(l), disk: VM_DISK, vms: 1, mac: if (l.pool == MACOS) 1 else 0 }

  /// One pool's counts under the leases `ls`, open when the host's gate and its terms are, its
  /// `cap` its waiting jobs when its policy is `DEMAND`.
  pure def countsOf(h: Host, ls: int -> Lease, ts: str -> Terms, pool: str): Counts = {
    pool: pool,
    open: gateOpen(h) and ts.get(pool).open,
    live: countOf(ls, pool, live),
    runners: countOf(ls, pool, runner),
    stopped: countOf(ls, pool, stoppedQuarantined),
    cap: if (h.policy.get(pool) == DEMAND) h.waiting.get(pool) else MAX_RUNNING,
  }

  /// The capacity policies `capacity::plan` implements (ADR 0069): a pool's `capacity_policy`.
  pure val EXACT = "exact"         // KER-05D: keep the headroom, and what quarantines owe
  pure val DEMAND = "demand"       // DEC-06: EXACT, keeping only for the waiting jobs it is given

  /// `DEMAND` (ADR 0069, DEC-06): the pool's headroom cut to the `waiting` jobs it is told of, so
  /// it is carved, and keeps ahead of the pools after it, only for them; what it keeps for its
  /// quarantined places is unchanged. With `waiting` fixed this is `EXACT` over a smaller
  /// headroom, so every property of `EXACT` holds (`carveMonotoneDemand`); what `waiting` can be
  /// is the open question (`sustainedLoad`, GH-00).
  pure def demandShape(s: Shape, waiting: int): Shape = {
    val told = min2(s.head, waiting)
    { ...s, head: told, room: s.room - (s.head - told) }
  }

  /// The pure policy: the free capacity after every live lease (quarantined included), carved
  /// in pool-name order among pools whose admission is open, each up to its `max_running`
  /// headroom (live and quarantined leases count against it, cut to its waiting jobs under
  /// `DEMAND`) within what the pools before it keep ahead of it (`keeps`, computed from counts
  /// by `carveCounts`).
  pure def planOf(h: Host, ls: int -> Lease, ts: str -> Terms): Plan = {
    val r = h.report
    val f = {
      mem: max0(limMem(r) - allocMemOf(ls)),
      disk: max0(limDisk(r) - allocVmsOf(ls) * VM_DISK),
      vms: max0(limVms(r) - allocVmsOf(ls)),
      mac: max0(limMacos(r) - allocMacosOf(ls)),
    }
    val c = carveCounts(f, countsOf(h, ls, ts, LINUX), countsOf(h, ls, ts, MACOS))
    { freeMem: f.mem, freeDisk: f.disk, freeVms: f.vms, freeMacos: f.mac, carveLinux: c._1, carveMacos: c._2 }
  }

  pure def carveOf(p: Plan, pool: str): int = if (pool == MACOS) p.carveMacos else p.carveLinux

  /// Runners a pool can deliver: its live, unquarantined leases plus its carve-out.
  pure def deliverableOf(p: Plan, ls: int -> Lease, pool: str): int =
    countOf(ls, pool, runner) + carveOf(p, pool)

  /// The `maxCapacity` sent for a pool's scale set: its deliverable while admission is open,
  /// only its live runners while it is closed (OD15).
  pure def reportedMaxOf(h: Host, p: Plan, ls: int -> Lease, ts: str -> Terms, pool: str): int =
    if (gateOpen(h) and ts.get(pool).open) deliverableOf(p, ls, pool) else countOf(ls, pool, runner)

  // ---- guards -------------------------------------------------------------------------------

  pure def canReserve(h: Host, ls: int -> Lease, ts: str -> Terms, a: int, pool: str): bool = {
    val r = h.report
    and {
      ls.get(a).phase == FREE,
      gateOpen(h),
      ts.get(pool).open,
      countOf(ls, pool, live) < MAX_RUNNING,
      freeSlotsOf(r, ls).size() > 0,
      fitsReport(r, ls, pool),
    }
  }

  /// The data plane's own ledger: our running and existing VMs plus foreign consumption.
  def oursRunningMem: int =
    ATTEMPTS.fold(0, (s, a) => if (vms.get(a) == RUNNING) s + vmMem(leases.get(a).pool) else s)
  def oursExisting: int = ATTEMPTS.fold(0, (s, a) => if (vms.get(a) != ABSENT) s + 1 else s)
  def oursRunningMacos: int =
    ATTEMPTS.fold(0, (s, a) =>
      if (vms.get(a) == RUNNING and leases.get(a).pool == MACOS) s + 1 else s)

  def dpFits(a: int): bool = {
    val pool = leases.get(a).pool
    and {
      // every VM needs memory: none starts while the free memory is not known (ADR 0052)
      not(dp.memUnknown),
      oursRunningMem + dp.foreignMem + vmMem(pool) <= TOTAL_MEM - RESERVE_MEM,
      (oursExisting + 1) * VM_DISK <= TOTAL_DISK,
      oursExisting + dp.foreignMacos + 1 <= SLOTS.size(),
      pool != MACOS or oursRunningMacos + dp.foreignMacos + 1 <= MACOS_GUESTS,
    }
  }

  action commit(kind: str, h: Host, ls: int -> Lease, ghost: Ghost): bool = all {
    host' = h,
    leases' = ls,
    slotGen' = slotGen,
    terms' = terms,
    pools' = pools,
    dp' = dp,
    vms' = vms,
    created' = created,
    sent' = sent,
    g' = { ...ghost, lastKind: kind },
  }

  // ---- init ---------------------------------------------------------------------------------

  pure val NO_REPORT: Report = {
    id: 0, mem: 0, disk: 0, slots: 0, macos: 0, foreignMem: 0, foreignVms: 0, foreignMacos: 0,
  }

  /// Every pool `EXACT`, the default (ADR 0069).
  pure val ALL_EXACT: str -> str = POOLS.mapBy(_ => EXACT)
  pure val NO_WAITING: str -> int = POOLS.mapBy(_ => 0)

  action initWith(foreignMem: int, foreignMacos: int): bool =
    initFull(foreignMem, foreignMacos, ALL_EXACT, NO_WAITING)

  /// `init` with each pool's capacity policy and the jobs it is told are waiting (DEC-06).
  action initFull(
    foreignMem: int, foreignMacos: int, policy: str -> str, waiting: str -> int,
  ): bool = all {
    host' = {
      report: NO_REPORT, gateClosed: false, drain: NO_DRAIN, policy: policy, waiting: waiting,
    },
    leases' = ATTEMPTS.mapBy(_ => { phase: FREE, slot: NO_SLOT, gen: 0, pool: LINUX, running: false }),
    slotGen' = SLOTS.mapBy(_ => 0),
    terms' = POOLS.mapBy(_ => { rev: 0, open: false }),
    pools' = POOLS.mapBy(_ => { rev: 1, active: true, failures: 0, tripped: false }),
    dp' = { foreignMem: foreignMem, foreignMacos: foreignMacos, nextId: 1, memUnknown: false },
    vms' = ATTEMPTS.mapBy(_ => ABSENT),
    created' = ATTEMPTS.mapBy(_ => false),
    sent' = Set(),
    g' = {
      lastKind: "init", issued: Set(), reissue: false, genNotIncreasing: false,
      overAllocated: false, macosOver: false, reservedClosed: false, grantedClosed: false,
      afterDrain: false, drainedWithLeases: false, dpOvercommit: false, dpStartedUnknown: false,
      doubleCreate: false,
      quarantineReleasedEarly: false, autoDeleteQuarantined: false, staleApplied: false,
      trippedAdmitted: false,
    },
  }

  action init = {
    nondet fm = Set(0, 1).oneOf()
    nondet fmac = Set(0, 1).oneOf()
    nondet linuxPolicy = Set(EXACT, DEMAND).oneOf()
    nondet macosPolicy = Set(EXACT, DEMAND).oneOf()
    nondet linuxWaiting = 0.to(MAX_RUNNING).oneOf()
    nondet macosWaiting = 0.to(MAX_RUNNING).oneOf()
    initFull(fm, fmac,
      Map(LINUX -> linuxPolicy, MACOS -> macosPolicy),
      Map(LINUX -> linuxWaiting, MACOS -> macosWaiting))
  }

  // ---- the data plane ----------------------------------------------------------------------

  /// `CapacityReported`: the ledger's truth now, sent to the control plane. Memory that is not
  /// known is reported with nothing deliverable (ADR 0052: free zero in that dimension).
  action dpReport = all {
    dp.nextId <= MAX_REPORTS,
    sent' = sent.union(Set({
      id: dp.nextId, mem: if (dp.memUnknown) 0 else TOTAL_MEM, disk: TOTAL_DISK,
      slots: SLOTS.size(), macos: MACOS_GUESTS,
      foreignMem: dp.foreignMem, foreignVms: dp.foreignMacos, foreignMacos: dp.foreignMacos,
    })),
    dp' = { ...dp, nextId: dp.nextId + 1 },
    host' = host, leases' = leases, slotGen' = slotGen, terms' = terms, pools' = pools,
    vms' = vms, created' = created,
    g' = { ...g, lastKind: "dp" },
  }

  /// The data plane executes the clone and start of a lease's VM, if it fits its own ledger
  /// (I33); a create that does not fit is refused (`insufficient_resources`) and changes nothing.
  action dpCreate(a: int): bool = all {
    runner(leases.get(a)),
    vms.get(a) == ABSENT,
    not(created.get(a)),
    dpFits(a),
    vms' = vms.set(a, RUNNING),
    created' = created.set(a, true),
    host' = host, leases' = leases, slotGen' = slotGen, terms' = terms, pools' = pools, dp' = dp,
    sent' = sent,
    g' = { ...g, lastKind: "dp", dpOvercommit: g.dpOvercommit or not(dpFits(a)),
      dpStartedUnknown: g.dpStartedUnknown or dp.memUnknown,
      doubleCreate: g.doubleCreate or created.get(a) },
  }

  /// Automation retires and deletes an unquarantined lease's VM (the attempt's `DeleteVm`).
  action dpDelete(a: int): bool = all {
    vms.get(a) != ABSENT,
    leases.get(a).phase != QUARANTINED,
    vms' = vms.set(a, ABSENT),
    host' = host, leases' = leases, slotGen' = slotGen, terms' = terms, pools' = pools, dp' = dp,
    created' = created, sent' = sent,
    g' = { ...g, lastKind: "dp",
      autoDeleteQuarantined: g.autoDeleteQuarantined or leases.get(a).phase == QUARANTINED },
  }

  // ---- the world ---------------------------------------------------------------------------

  action worldWith(fm: int, fmac: int): bool = all {
    fm >= 0, fm <= FOREIGN_MAX, fmac >= 0,
    // macOS itself refuses a third running macOS guest, ours or foreign (G3)
    oursRunningMacos + fmac <= MACOS_GUESTS,
    dp' = { ...dp, foreignMem: fm, foreignMacos: fmac },
    host' = host, leases' = leases, slotGen' = slotGen, terms' = terms, pools' = pools,
    vms' = vms, created' = created, sent' = sent,
    g' = { ...g, lastKind: "world" },
  }

  /// External consumption changes: foreign memory, foreign macOS guests.
  action foreignChanges = {
    nondet fm = Set(dp.foreignMem - 1, dp.foreignMem + 1).oneOf()
    nondet fmac = Set(dp.foreignMacos - 1, dp.foreignMacos, dp.foreignMacos + 1).oneOf()
    worldWith(fm, fmac)
  }

  /// A listed VM's memory stops or starts being known: a VM the provider does not describe is
  /// listed, or a complete listing describes every VM again (ADR 0052).
  action vmFactsChangeWith(unknown: bool): bool = all {
    dp' = { ...dp, memUnknown: unknown },
    host' = host, leases' = leases, slotGen' = slotGen, terms' = terms, pools' = pools,
    vms' = vms, created' = created, sent' = sent,
    g' = { ...g, lastKind: "world" },
  }

  action vmFactsChange = {
    nondet unknown = Set(true, false).oneOf()
    vmFactsChangeWith(unknown)
  }

  /// Our VM disappears without our delete (a complete inventory will observe it absent).
  action vmVanishes(a: int): bool = all {
    vms.get(a) != ABSENT,
    vms' = vms.set(a, ABSENT),
    host' = host, leases' = leases, slotGen' = slotGen, terms' = terms, pools' = pools, dp' = dp,
    created' = created, sent' = sent,
    g' = { ...g, lastKind: "world" },
  }

  // ---- Host commands ----------------------------------------------------------------------

  /// `RecordCapacityReport`: a newer report is applied; an older one is ignored.
  action applyReport(r: Report): bool = {
    val newer = r.id > host.report.id
    all {
      sent.contains(r),
      host' = if (newer) { ...host, report: r } else host,
      sent' = sent.exclude(Set(r)),
      leases' = leases, slotGen' = slotGen, terms' = terms, pools' = pools, dp' = dp, vms' = vms,
      created' = created,
      g' = { ...g, lastKind: "report", staleApplied: g.staleApplied or (newer and r.id <= host.report.id) },
    }
  }

  action recordReport = {
    nondet r = sent.oneOf()
    applyReport(r)
  }

  /// `ReserveSlot`: the lowest free slot, its next generation, within the last report.
  action reserve(a: int, pool: str): bool = {
    val s = lowest(freeSlotsOf(host.report, leases))
    val gen = slotGen.get(s) + 1
    val after = leases.set(a, { phase: RESERVED, slot: s, gen: gen, pool: pool, running: false })
    val r = host.report
    all {
      canReserve(host, leases, terms, a, pool),
      leases' = after,
      slotGen' = slotGen.set(s, gen),
      host' = host, terms' = terms, pools' = pools, dp' = dp, vms' = vms, created' = created,
      sent' = sent,
      g' = {
        ...g,
        lastKind: "command",
        issued: g.issued.union(Set((s, gen))),
        reissue: g.reissue or g.issued.contains((s, gen)),
        genNotIncreasing: g.genNotIncreasing or gen <= slotGen.get(s),
        overAllocated: g.overAllocated or not(and {
          allocMemOf(after) <= limMem(r),
          allocVmsOf(after) * VM_DISK <= limDisk(r),
          allocVmsOf(after) <= limVms(r),
          pool != MACOS or allocMacosOf(after) <= limMacos(r),
        }),
        macosOver: g.macosOver
          or (pool == MACOS and allocMacosOf(after) + r.foreignMacos > MACOS_GUESTS),
        reservedClosed: g.reservedClosed or not(gateOpen(host)) or not(terms.get(pool).open),
        afterDrain: g.afterDrain or host.drain != NO_DRAIN,
        trippedAdmitted: g.trippedAdmitted
          or (pools.get(pool).tripped and terms.get(pool).rev == pools.get(pool).rev),
      },
    }
  }

  /// `GrantActivation`: only with the gate and the pool's terms open (I26), never after drain
  /// start (I08).
  action grant(a: int): bool = {
    val l = leases.get(a)
    all {
      l.phase == RESERVED,
      gateOpen(host),
      terms.get(l.pool).open,
      commit("command", host, leases.set(a, { ...l, phase: ACTIVATED }),
        { ...g,
          grantedClosed: g.grantedClosed or not(gateOpen(host)) or not(terms.get(l.pool).open),
          afterDrain: g.afterDrain or host.drain != NO_DRAIN }),
    }
  }

  /// `QuarantineLease{vm_running}`: the lease keeps its slot and resources; a stopped VM's lease
  /// shrinks to disk and slot. Also how a stop of a quarantined VM reaches the host.
  action quarantine(a: int): bool = {
    val l = leases.get(a)
    all {
      live(l),
      commit("command", host, leases.set(a, { ...l, phase: QUARANTINED, running: vms.get(a) == RUNNING }), g),
    }
  }

  /// The attempt stops its quarantined VM (with proof, never a delete) and the host is told.
  action stopQuarantined(a: int): bool = {
    val l = leases.get(a)
    all {
      l.phase == QUARANTINED,
      vms.get(a) == RUNNING,
      vms' = vms.set(a, STOPPED),
      leases' = leases.set(a, { ...l, running: false }),
      host' = host, slotGen' = slotGen, terms' = terms, pools' = pools, dp' = dp,
      created' = created, sent' = sent,
      g' = { ...g, lastKind: "command" },
    }
  }

  /// An operator resolves the quarantine case with a delete with proof (ADR 0025).
  action operatorResolve(a: int): bool = all {
    leases.get(a).phase == QUARANTINED,
    vms.get(a) != ABSENT,
    vms' = vms.set(a, ABSENT),
    host' = host, leases' = leases, slotGen' = slotGen, terms' = terms, pools' = pools, dp' = dp,
    created' = created, sent' = sent,
    g' = { ...g, lastKind: "command" },
  }

  /// `ReleaseSlot{evidence}`: only on the observed absence of the VM (deleted, lost, never
  /// created). No timer and no cap releases a lease (I28).
  action release(a: int): bool = {
    val l = leases.get(a)
    all {
      live(l),
      vms.get(a) == ABSENT,
      commit("command", host, leases.set(a, { ...l, phase: RELEASED, running: false }),
        { ...g, quarantineReleasedEarly: g.quarantineReleasedEarly
          or (l.phase == QUARANTINED and vms.get(a) != ABSENT) }),
    }
  }

  action closeGate = all { not(host.gateClosed), commit("command", { ...host, gateClosed: true }, leases, g) }
  action openGate = all { host.gateClosed, commit("command", { ...host, gateClosed: false }, leases, g) }
  action startDrain = all { host.drain == NO_DRAIN, commit("command", { ...host, drain: DRAINING }, leases, g) }
  action cancelDrain = all { host.drain != NO_DRAIN, commit("command", { ...host, drain: NO_DRAIN }, leases, g) }

  /// `CompleteHostDrain`: only once no lease is left in scope.
  action completeDrain = all {
    host.drain == DRAINING,
    ATTEMPTS.forall(a => not(live(leases.get(a)))),
    commit("command", { ...host, drain: DRAINED }, leases,
      { ...g, drainedWithLeases: g.drainedWithLeases or ATTEMPTS.exists(a => live(leases.get(a))) }),
  }

  /// `ApplyPoolTerms` (policy P4): the pool's current terms, refused unless newer.
  action applyTerms(pool: str): bool = {
    val p = pools.get(pool)
    all {
      p.rev > terms.get(pool).rev,
      terms' = terms.set(pool, { rev: p.rev, open: p.active and not(p.tripped) }),
      host' = host, leases' = leases, slotGen' = slotGen, pools' = pools, dp' = dp, vms' = vms,
      created' = created, sent' = sent,
      g' = { ...g, lastKind: "command" },
    }
  }

  // ---- Pool commands ----------------------------------------------------------------------

  action setPool(pool: str, p: Pool): bool = all {
    pools' = pools.set(pool, p),
    host' = host, leases' = leases, slotGen' = slotGen, terms' = terms, dp' = dp, vms' = vms,
    created' = created, sent' = sent,
    g' = { ...g, lastKind: "command" },
  }

  action pausePool(pool: str): bool = {
    val p = pools.get(pool)
    all { p.active, setPool(pool, { ...p, active: false, rev: p.rev + 1 }) }
  }

  action resumePool(pool: str): bool = {
    val p = pools.get(pool)
    all { not(p.active), setPool(pool, { ...p, active: true, rev: p.rev + 1 }) }
  }

  /// `CountAttemptFailure`: counted while the breaker is closed; at `BREAKER_N` it trips and
  /// the terms revision moves (I27).
  action countFailure(pool: str): bool = {
    val p = pools.get(pool)
    val n = p.failures + 1
    all {
      not(p.tripped),
      setPool(pool, { ...p, failures: n, tripped: n >= BREAKER_N,
        rev: if (n >= BREAKER_N) p.rev + 1 else p.rev }),
    }
  }

  /// `ResetBreaker`: an operator closes a tripped breaker.
  action resetBreaker(pool: str): bool = {
    val p = pools.get(pool)
    all { p.tripped, setPool(pool, { ...p, failures: 0, tripped: false, rev: p.rev + 1 }) }
  }

  // ---- the relation -------------------------------------------------------------------------

  action step = {
    nondet a = ATTEMPTS.oneOf()
    nondet pool = POOLS.oneOf()
    any {
      dpReport,
      dpCreate(a),
      dpDelete(a),
      foreignChanges,
      vmFactsChange,
      vmVanishes(a),
      recordReport,
      reserve(a, pool),
      grant(a),
      quarantine(a),
      stopQuarantined(a),
      operatorResolve(a),
      release(a),
      closeGate,
      openGate,
      startDrain,
      cancelDrain,
      completeDrain,
      applyTerms(pool),
      pausePool(pool),
      resumePool(pool),
      countFailure(pool),
      resetBreaker(pool),
    }
  }

  /// Capacity pressure, biased towards admission: reports, terms, reservations, grants,
  /// creates, quarantine and releases under changing foreign consumption, without the gate,
  /// drain and pool modes that close admission. A uniform walk of `step` closes admission
  /// often enough that it rarely fills a host; this relation fills it and holds it full.
  action admitStep = {
    nondet a = ATTEMPTS.oneOf()
    nondet pool = POOLS.oneOf()
    any {
      dpReport,
      recordReport,
      applyTerms(pool),
      reserve(a, pool),
      grant(a),
      dpCreate(a),
      foreignChanges,
      quarantine(a),
      stopQuarantined(a),
      operatorResolve(a),
      dpDelete(a),
      release(a),
      vmFactsChange,
    }
  }

  // ---- invariants ---------------------------------------------------------------------------

  def plan: Plan = planOf(host, leases, terms)

  /// I01: each live lease holds exactly one (slot, generation) and no slot holds two leases.
  val i01OneSlotPerAttempt = ATTEMPTS.forall(a => ATTEMPTS.forall(b =>
    (a != b and live(leases.get(a)) and live(leases.get(b))) implies leases.get(a).slot != leases.get(b).slot))

  /// I02: no (slot, generation) is ever reserved twice.
  val i02NoSharedSlotGeneration = not(g.reissue)

  /// I03: every reservation fit the capacity the host knew of (live, activated and quarantined
  /// leases all counted), and every start fit the data plane's ledger.
  val i03StartsWithinCapacity = not(g.overAllocated) and not(g.dpOvercommit)

  /// I07: a pool's `reported_max` never exceeds what it can deliver.
  val i07NeverAdvertiseMore = {
    val p = plan
    POOLS.forall(pool => reportedMaxOf(host, p, leases, terms, pool) <= deliverableOf(p, leases, pool))
  }

  /// I08: no reservation and no grant after drain start; the drain completes only with no
  /// lease in scope.
  val i08NothingAfterDrain = not(g.afterDrain) and not(g.drainedWithLeases)

  /// I13: at most one VM is ever created per lease, so a reservation cannot multiply clones.
  val i13NoDuplicateClones = not(g.doubleCreate)

  /// I24: the carve-outs (what `reported_max` advertises beyond live runners) fit the host's
  /// free capacity together, on every dimension.
  val i24CarveOutsFitTheHost = {
    val p = plan
    and {
      p.carveLinux * vmMem(LINUX) + p.carveMacos * vmMem(MACOS) <= p.freeMem,
      (p.carveLinux + p.carveMacos) * VM_DISK <= p.freeDisk,
      p.carveLinux + p.carveMacos <= p.freeVms,
      p.carveMacos <= p.freeMacos,
    }
  }

  /// I26: no reservation and no grant while the host's gate or the pool's terms are closed.
  val i26NoAdmissionWhileClosed = not(g.reservedClosed)

  /// noGrantWhileClosed: no grant while the gate or the pool's terms are closed.
  val noGrantWhileClosed = not(g.grantedClosed)

  /// I27: failures are bounded by the threshold, a tripped breaker is published closed, and no
  /// reservation is made under terms that show it tripped.
  val i27BreakerBounded = POOLS.forall(pool => and {
    pools.get(pool).failures <= BREAKER_N,
    pools.get(pool).tripped iff pools.get(pool).failures >= BREAKER_N,
    (pools.get(pool).tripped and terms.get(pool).rev == pools.get(pool).rev)
      implies not(terms.get(pool).open),
  }) and not(g.trippedAdmitted)

  /// I28: a quarantined VM is never deleted by automation, its lease is released only on the
  /// VM's observed absence, and it counts against its pool's `max_running`.
  val i28QuarantineNeverAutoDeleted = and {
    not(g.autoDeleteQuarantined),
    not(g.quarantineReleasedEarly),
    POOLS.forall(pool => countOf(leases, pool, live) <= MAX_RUNNING),
  }

  /// I33: the data plane never starts a VM that does not fit its own ledger.
  val i33DataPlaneNeverOvercommits = not(g.dpOvercommit)

  /// noStartWhileCapacityUnknown: the data plane never starts a VM while the memory it needs
  /// is not known; nothing unknown is assumed (ADR 0052).
  val noStartWhileCapacityUnknown = not(g.dpStartedUnknown)

  /// generationsMonotonic: a slot's generation only grows, and every lease's generation is at
  /// most its slot's.
  val generationsMonotonic = not(g.genNotIncreasing) and ATTEMPTS.forall(a =>
    leases.get(a).phase == FREE or leases.get(a).gen <= slotGen.get(leases.get(a).slot))

  /// macosGuestsAtMostTwo: never more than two running macOS guests, ours and foreign; and no
  /// reservation counted a third against the report.
  val macosGuestsAtMostTwo = oursRunningMacos + dp.foreignMacos <= MACOS_GUESTS and not(g.macosOver)

  /// externalConsumptionShrinksAdmissible: more foreign consumption in the applied report never
  /// gives more free capacity or more carve-out.
  val externalConsumptionShrinksAdmissible = {
    val r = host.report
    val more = { ...host, report: { ...r, foreignMem: r.foreignMem + 1, foreignVms: r.foreignVms + 1 } }
    val p = plan
    val p2 = planOf(more, leases, terms)
    and {
      p2.freeMem <= p.freeMem,
      p2.freeVms <= p.freeVms,
      p2.carveLinux + p2.carveMacos <= p.carveLinux + p.carveMacos,
    }
  }

  /// quarantineHoldsCapacity: a quarantined lease keeps its slot (no free slot is one of
  /// theirs) and its disk, and its memory while its VM runs.
  val quarantineHoldsCapacity = ATTEMPTS.forall(a => leases.get(a).phase == QUARANTINED implies and {
    not(freeSlotsOf(host.report, leases).contains(leases.get(a).slot)),
    vms.get(a) != RUNNING or holdsMem(leases.get(a)) == vmMem(leases.get(a).pool),
  })

  /// allocationWithinReport: every reservation fit the newest report applied, and an older
  /// report never replaced a newer one.
  val allocationWithinReport = not(g.overAllocated) and not(g.staleApplied)

  /// carveOutAdmissible: whatever the policy carves out for a pool, the host admits.
  val carveOutAdmissible = {
    val p = plan
    val unused = ATTEMPTS.filter(a => leases.get(a).phase == FREE)
    val a = lowest(unused)
    POOLS.forall(pool =>
      (carveOf(p, pool) > 0 and unused.size() > 0) implies canReserve(host, leases, terms, a, pool))
  }

  // Every invariant of the model, so a new one cannot be left out of a stage.
  val allInvariants = and {
    i01OneSlotPerAttempt,
    i02NoSharedSlotGeneration,
    i03StartsWithinCapacity,
    i07NeverAdvertiseMore,
    i08NothingAfterDrain,
    i13NoDuplicateClones,
    i24CarveOutsFitTheHost,
    i26NoAdmissionWhileClosed,
    noGrantWhileClosed,
    i27BreakerBounded,
    i28QuarantineNeverAutoDeleted,
    i33DataPlaneNeverOvercommits,
    noStartWhileCapacityUnknown,
    generationsMonotonic,
    macosGuestsAtMostTwo,
    externalConsumptionShrinksAdmissible,
    quarantineHoldsCapacity,
    allocationWithinReport,
    carveOutAdmissible,
  }
}

/**
 * The capacity policy's properties and candidates, and every scenario of the model, over the
 * state machine `scheduling_machine` above: the main module that `quint test` and `quint run`
 * use. `formal-verify` checks `scheduling_machine` alone (`VERIFY_MAIN` in
 * `tools/xtask/src/formal_verify.rs`): Apalache's inliner expands every definition of the
 * module it is given, and the candidate policies below (a carve that depends on another carve,
 * folded over every free capacity) expand past any heap, which ran it out of memory before it
 * checked anything once KER-05D added them (DEC-06).
 */
module scheduling {
  import scheduling_machine.*

  /// carveMonotone (KER-05H, KER-05D): neither pool's carve grows when the free capacity shrinks
  /// on any dimension, and neither shrinks when a quarantined lease of the first pool is
  /// released (a running one frees a whole VM, a stopped one its disk and slots), whichever of
  /// the two shapes comes first; the carve-outs fit the free capacity together (I24); and each
  /// pool is carved at least what KER-05H's keep carved it (`keepsRoom`). The state space above
  /// carves the small Linux VM before the larger macOS one only, where carving the leftover is
  /// monotone; this checks the carve over every free capacity of a small host, both orders, and
  /// every count of runners and of running and stopped quarantined leases of the first pool.
  pure def firstShape(pool: str, runners: int, held: int, stopped: int): Shape = {
    vm: vmOf(pool),
    head: max0(MAX_RUNNING - runners - held),
    room: max0(MAX_RUNNING - runners),
    owed: times({ ...NOTHING, mem: vmMem(pool) }, stopped),
    open: true,
  }
  pure def secondShape(pool: str, headroom: int): Shape =
    { vm: vmOf(pool), head: headroom, room: headroom, owed: NOTHING, open: true }
  pure val FREES: Set[Free] = tuples(0.to(4), 0.to(3), 0.to(3), 0.to(2))
    .map(t => { mem: t._1, disk: t._2, vms: t._3, mac: t._4 })
  pure val ORDERS: Set[(str, str)] = Set((LINUX, MACOS), (MACOS, LINUX))
  pure def shrunk(f: Free): Set[Free] = Set(
    { ...f, mem: max0(f.mem - 1) }, { ...f, disk: max0(f.disk - 1) },
    { ...f, vms: max0(f.vms - 1) }, { ...f, mac: max0(f.mac - 1) },
  )
  /// Pool by pool, `c` is at most `d`.
  pure def atMost(c: (int, int), d: (int, int)): bool = c._1 <= d._1 and c._2 <= d._2
  /// The carve-outs fit `f` together and each is within its pool's headroom (I24).
  pure def fitsTogether(f: Free, first: Shape, second: Shape, c: (int, int)): bool = and {
    within(plus(times(first.vm, c._1), times(second.vm, c._2)), f),
    c._1 <= first.head,
    c._2 <= second.head,
  }
  pure val carveMonotone = FREES.forall(f => ORDERS.forall(o =>
    tuples(0.to(MAX_RUNNING), 0.to(MAX_RUNNING), 0.to(MAX_RUNNING), 0.to(MAX_RUNNING)).forall(t => {
      val runners = t._1
      val held = t._2
      val stopped = t._3
      val first = firstShape(o._1, runners, held, stopped)
      val second = secondShape(o._2, t._4)
      val c = carveTwo(f, first, second)
      // releasing one quarantined lease: a running one gives back its VM, a stopped one its
      // disk and slots (its memory it gave back when it stopped)
      val runningReleased = carveTwo(plus(f, vmOf(o._1)), firstShape(o._1, runners, held - 1, stopped), second)
      val stoppedReleased =
        carveTwo(plus(f, { ...vmOf(o._1), mem: 0 }), firstShape(o._1, runners, held - 1, stopped - 1), second)
      stopped > held or and {
        fitsTogether(f, first, second, c),
        shrunk(f).forall(g2 => atMost(carveTwo(g2, first, second), c)),
        held == stopped or atMost(c, runningReleased),
        stopped == 0 or atMost(c, stoppedReleased),
        atMost(carveKeeping(f, first, second, keepsRoom(first)), c),
      }
    })))

  /// carveMonotoneDemand (DEC-06): `carveMonotone`'s properties for `DEMAND` with every fixed
  /// number of waiting jobs of the first pool: the carve-outs fit together, neither pool's carve
  /// grows when the free capacity shrinks or shrinks when a quarantined lease of the first pool
  /// is released, and the later pool is carved at least what `EXACT` carves it (the first keeps
  /// no more than under `EXACT`).
  pure val carveMonotoneDemand = FREES.forall(f => ORDERS.forall(o =>
    tuples(0.to(MAX_RUNNING), 0.to(MAX_RUNNING), 0.to(MAX_RUNNING), 0.to(MAX_RUNNING),
      0.to(MAX_RUNNING)).forall(t => {
      val runners = t._1
      val held = t._2
      val stopped = t._3
      val waiting = t._5
      val first = firstShape(o._1, runners, held, stopped)
      val second = secondShape(o._2, t._4)
      val c = carveDemand(f, first, second, waiting)
      val runningReleased =
        carveDemand(plus(f, vmOf(o._1)), firstShape(o._1, runners, held - 1, stopped), second, waiting)
      val stoppedReleased = carveDemand(plus(f, { ...vmOf(o._1), mem: 0 }),
        firstShape(o._1, runners, held - 1, stopped - 1), second, waiting)
      stopped > held or and {
        fitsTogether(f, demandShape(first, waiting), second, c),
        shrunk(f).forall(g2 => atMost(carveDemand(g2, first, second, waiting), c)),
        held == stopped or atMost(c, runningReleased),
        stopped == 0 or atMost(c, stoppedReleased),
        carveTwo(f, first, second)._2 <= c._2,
      }
    })))

  /// One pool's `Shape` from its counts, as `capacity::plan` builds it: its headroom, room and
  /// what its quarantined leases owe (only a stopped one owes, its memory: `holdsOf`), cut by
  /// `demandShape` when its cap is below its headroom.
  pure def shapeOfCounts(c: Counts): Shape = {
    val exact = {
      vm: vmOf(c.pool),
      head: max0(MAX_RUNNING - c.live),
      room: max0(MAX_RUNNING - c.runners),
      owed: times({ ...NOTHING, mem: vmMem(c.pool) }, c.stopped),
      open: c.open,
    }
    demandShape(exact, c.cap)
  }

  /// carveCounts (DEC-06): the state machine's `carveCounts` is `carveTwo` over the shapes the
  /// counts give, on every free capacity of a small host, both pool orders, every count of
  /// runners and of running and stopped quarantined leases of the first pool, every live count
  /// of the second, every cap (`MAX_RUNNING` is `EXACT`), open or closed.
  pure val carveCountsAgree = FREES.forall(f => ORDERS.forall(o =>
    tuples(0.to(MAX_RUNNING), 0.to(MAX_RUNNING), 0.to(MAX_RUNNING), 0.to(MAX_RUNNING),
      0.to(MAX_RUNNING), Set(true, false)).forall(t => {
      val a = { pool: o._1, open: true, live: t._1 + t._2, runners: t._1, stopped: t._3, cap: t._5 }
      val b = { pool: o._2, open: t._6, live: t._4, runners: t._4, stopped: 0, cap: MAX_RUNNING }
      (t._3 > t._2 or t._1 + t._2 > MAX_RUNNING) or and {
        carveCounts(f, a, b) == carveTwo(f, shapeOfCounts(a), shapeOfCounts(b)),
        carveCounts(f, { ...a, open: false }, b)
          == carveTwo(f, shapeOfCounts({ ...a, open: false }), shapeOfCounts(b)),
      }
    })))

  // ---- KER-05D: candidate carve policies (ADR 0069, doc/domain-model.md §6.3.1) --------------
  //
  // Each candidate carves two pools in priority order (the first is the earlier pool name).
  // `carveTwo` above is `EXACT`, the policy `capacity::plan` implements; the others are kept
  // here, with the checks and scenarios that measured them, so the choice can be re-examined.

  pure val ROOM = "room"           // KER-05H: keep a whole VM per place in `room`
  pure val LEFTOVER = "leftover"   // greedy: hand the next pool what the earlier one left
  pure val ENVELOPE = "envelope"   // the leftover, clipped to stay monotone in total
  pure val SMALL_FIRST = "small"   // LEFTOVER with the smaller VM first when one is smaller
  pure val POLICIES = Set(ROOM, EXACT, LEFTOVER, ENVELOPE, DEMAND, SMALL_FIRST)

  /// KER-05H's keep: a whole VM per place in `room`, quarantined places included.
  pure def keepsRoom(s: Shape): Free = if (s.open) times(s.vm, s.room) else NOTHING

  /// The first pool up to its headroom within `f`; the second within `f` less `kept`.
  pure def carveKeeping(f: Free, first: Shape, second: Shape, kept: Free): (int, int) = {
    val c1 = if (first.open) min2(first.head, fitShape(f, first)) else 0
    val c2 = if (second.open) min2(second.head, fitShape(less(f, kept), second)) else 0
    (c1, c2)
  }

  /// Greedy: the second pool is carved what the first one's carve leaves (before KER-05H).
  pure def carveLeftover(f: Free, first: Shape, second: Shape): (int, int) = {
    val c1 = if (first.open) min2(first.head, fitShape(f, first)) else 0
    carveKeeping(f, first, second, times(first.vm, c1))
  }

  /// The greedy leftover, clipped to its monotone envelope: the total is the least greedy total
  /// over every free capacity at least `f`. That least total is reached where the first pool
  /// just fits `m` VMs (`f` raised to `m` of its VMs), for `m` from its carve to its headroom.
  pure def carveEnvelope(f: Free, first: Shape, second: Shape): (int, int) = {
    val greedy = carveLeftover(f, first, second)
    val floor = if (not(first.open)) greedy._1 + greedy._2 else
      greedy._1.to(max2(greedy._1, first.head)).fold(greedy._1 + greedy._2, (least, m) => {
        val at = carveLeftover(raised(f, times(first.vm, m)), first, second)
        min2(least, at._1 + at._2)
      })
    (greedy._1, min2(greedy._2, max0(floor - greedy._1)))
  }
  pure def max2(x: int, y: int): int = if (x > y) x else y
  pure def raised(f: Free, g: Free): Free =
    { mem: max2(f.mem, g.mem), disk: max2(f.disk, g.disk), vms: max2(f.vms, g.vms), mac: max2(f.mac, g.mac) }

  /// EXACT with the first pool's headroom cut to the `waiting` jobs it is told of
  /// (`demandShape`): it is carved and keeps only for them. Monotone for a fixed `waiting`; what
  /// `waiting` can be is the question (`sustainedLoad`).
  pure def carveDemand(f: Free, first: Shape, second: Shape, waiting: int): (int, int) =
    carveTwo(f, demandShape(first, waiting), second)

  /// LEFTOVER with the smaller VM first when one is within the other on every dimension, the
  /// given order otherwise (VMs that differ in kind have no size order).
  pure def carveSmallFirst(f: Free, first: Shape, second: Shape): (int, int) =
    if (within(second.vm, first.vm) and second.vm != first.vm) {
      val c = carveLeftover(f, second, first)
      (c._2, c._1)
    } else carveLeftover(f, first, second)

  pure def carveBy(p: str, f: Free, first: Shape, second: Shape, waiting: int): (int, int) =
    if (p == ROOM) carveKeeping(f, first, second, keepsRoom(first))
    else if (p == EXACT) carveTwo(f, first, second)
    else if (p == LEFTOVER) carveLeftover(f, first, second)
    else if (p == ENVELOPE) carveEnvelope(f, first, second)
    else if (p == DEMAND) carveDemand(f, first, second, waiting)
    else carveSmallFirst(f, first, second)

  // The properties, over a small host, three VM kinds (a Linux VM, a larger macOS one, and a
  // Linux VM with more disk than the macOS one, so two kinds have no size order), the first
  // pool with no quarantine, a running or a stopped one; `DEMAND` with 0 and 1 waiting jobs.
  pure val FAT: Free = { mem: 1, disk: 2, vms: 1, mac: 0 }
  pure val KINDS: Set[Free] = Set(vmOf(LINUX), vmOf(MACOS), FAT)
  pure val SMALL_FREES: Set[Free] = tuples(0.to(4), 0.to(2), 0.to(2), 0.to(1))
    .map(t => { mem: t._1, disk: t._2, vms: t._3, mac: t._4 })
  pure val CASES: Set[(Free, Shape, Shape)] =
    tuples(SMALL_FREES, KINDS, KINDS, Set((0, 0), (1, 0), (1, 1)), 1.to(MAX_RUNNING))
      .filter(t => t._2 != t._3)
      .map(t => (
        t._1,
        { vm: t._2, head: MAX_RUNNING - t._4._1, room: MAX_RUNNING,
          owed: times({ ...NOTHING, mem: t._2.mem }, t._4._2), open: true },
        { vm: t._3, head: t._5, room: t._5, owed: NOTHING, open: true },
      ))
  pure def waitingFor(p: str): Set[int] = if (p == DEMAND) Set(0, 1) else Set(0)
  /// Releasing the first pool's quarantined lease: its holds come back.
  pure def released(c: (Free, Shape, Shape)): (Free, Shape) = {
    val first = c._2
    val holds = if (first.owed.mem > 0) { ...first.vm, mem: 0 } else first.vm
    (plus(c._1, holds), { ...first, head: first.head + 1, owed: NOTHING })
  }
  pure def total(c: (int, int)): int = c._1 + c._2

  pure def safeFor(p: str): bool = CASES.forall(c => waitingFor(p).forall(w =>
    fitsTogether(c._1, c._2, c._3, carveBy(p, c._1, c._2, c._3, w))))
  pure def eachMonotoneFor(p: str): bool = CASES.forall(c => waitingFor(p).forall(w =>
    shrunk(c._1).forall(g2 => atMost(carveBy(p, g2, c._2, c._3, w), carveBy(p, c._1, c._2, c._3, w)))))
  pure def totalMonotoneFor(p: str): bool = CASES.forall(c => waitingFor(p).forall(w =>
    shrunk(c._1).forall(g2 => total(carveBy(p, g2, c._2, c._3, w)) <= total(carveBy(p, c._1, c._2, c._3, w)))))
  pure def releaseMonotoneFor(p: str): bool = CASES.forall(c => waitingFor(p).forall(w =>
    c._2.head == c._2.room or {
      val r = released(c)
      atMost(carveBy(p, c._1, c._2, c._3, w), carveBy(p, r._1, r._2, c._3, w))
    }))
  /// Per policy, in `POLICY_ORDER`: (safe, each pool monotone, total monotone, release monotone).
  pure def propertiesOf(p: str): (bool, bool, bool, bool) =
    (safeFor(p), eachMonotoneFor(p), totalMonotoneFor(p), releaseMonotoneFor(p))

  // Usefulness, part 1: representative hosts in GiB (macOS VM 15 GiB and 60 GiB of disk, Linux
  // VM 4 GiB and 20 GiB), the earlier pool first. Each scenario lists its carve per policy in
  // the order of `POLICY_ORDER` (doc/domain-model.md §6.3.1 has the table).
  pure val POLICY_ORDER = [ROOM, EXACT, LEFTOVER, ENVELOPE, DEMAND, SMALL_FIRST]
  pure val MAC_VM: Free = { mem: 15, disk: 60, vms: 1, mac: 1 }
  pure val LIN_VM: Free = { mem: 4, disk: 20, vms: 1, mac: 0 }
  pure def pool2(vm: Free, headroom: int, places: int, owed: Free): Shape =
    { vm: vm, head: headroom, room: places, owed: owed, open: true }
  type Scenario = { free: Free, first: Shape, second: Shape, waiting: int }
  pure def carvesOf(s: Scenario): List[(int, int)] =
    POLICY_ORDER.foldl([], (l, p) => l.append(carveBy(p, s.free, s.first, s.second, s.waiting)))
  /// S1: 13 GiB free, too little for a macOS VM; no macOS job waits; Linux jobs wait.
  pure val S1: Scenario = {
    free: { mem: 13, disk: 400, vms: 8, mac: 2 },
    first: pool2(MAC_VM, 2, 2, NOTHING), second: pool2(LIN_VM, 8, 8, NOTHING), waiting: 0,
  }
  /// S2: the same with one macOS job waiting.
  pure val S2: Scenario = { ...S1, waiting: 1 }
  /// S3: KER-05H's counterexample after foreign use grew: 14 GiB free, a 15 GiB VM first.
  pure val S3: Scenario = {
    free: { mem: 14, disk: 400, vms: 4, mac: 2 },
    first: pool2({ ...MAC_VM, mac: 0 }, 1, 1, NOTHING), second: pool2({ ...LIN_VM, mem: 2 }, 3, 3, NOTHING),
    waiting: 1,
  }
  /// S4: one running quarantined macOS VM (ADR 0025: kept until an operator resolves it); 49
  /// GiB free beside it, one macOS job waiting.
  pure val S4: Scenario = {
    free: { mem: 49, disk: 400, vms: 9, mac: 1 },
    first: pool2(MAC_VM, 1, 2, NOTHING), second: pool2(LIN_VM, 12, 12, NOTHING), waiting: 1,
  }
  /// S5: one stopped quarantined macOS VM (it holds its disk and slots); slots bind: 3 free.
  pure val S5: Scenario = {
    free: { mem: 64, disk: 400, vms: 3, mac: 1 },
    first: pool2(MAC_VM, 1, 2, { ...NOTHING, mem: 15 }), second: pool2(LIN_VM, 12, 12, NOTHING),
    waiting: 1,
  }
  /// S6: `max_running` 2 macOS VMs on a host that can hold one (24 GiB free, nothing running).
  pure val S6: Scenario = {
    free: { mem: 24, disk: 400, vms: 8, mac: 2 },
    first: pool2(MAC_VM, 2, 2, NOTHING), second: pool2(LIN_VM, 8, 8, NOTHING), waiting: 2,
  }

  // Usefulness, part 2 (`sustainedLoad`): a 32 GiB host full of eight Linux VMs whose jobs keep
  // coming; one macOS job waits. Each round one Linux VM finishes, then both pools are carved and
  // reserve what they are carved (the macOS pool up to its one job). The result is the round in
  // which the macOS VM is reserved, or -1 within `ROUNDS`. `DEMAND` is run twice: told every
  // waiting job (`visible`), or only those GitHub assigned under the last `maxCapacity` it was
  // sent (`seen`: whether `X-ScaleSetMaxCapacity` caps assignment is unverified, GH-00 item 6).
  pure val ROUNDS = 8
  type Load = { free: Free, linux: int, macos: int, seen: int, round: int, started: int }
  pure val LOADED: Load = {
    free: { mem: 0, disk: 240, vms: 2, mac: 2 }, linux: 8, macos: 0, seen: 0, round: 0, started: -1,
  }
  pure def roundOf(p: str, visible: bool, s: Load): Load = {
    val done = if (s.linux > 0) 1 else 0
    val f = plus(s.free, times(LIN_VM, done))
    val first = pool2(MAC_VM, MAX_RUNNING - s.macos, MAX_RUNNING - s.macos, NOTHING)
    val second = pool2(LIN_VM, 8 - (s.linux - done), 8 - (s.linux - done), NOTHING)
    val waiting = if (visible) 1 - s.macos else s.seen
    val c = carveBy(p, f, first, second, waiting)
    val mac = min2(c._1, 1 - s.macos)
    {
      free: less(f, plus(times(MAC_VM, mac), times(LIN_VM, c._2))),
      linux: s.linux - done + c._2,
      macos: s.macos + mac,
      // GitHub assigns the waiting job only while the reported max leaves room for it
      seen: min2(1 - s.macos - mac, c._1 - mac),
      round: s.round + 1,
      started: if (s.started < 0 and mac > 0) s.round + 1 else s.started,
    }
  }
  pure def sustainedLoad(p: str, visible: bool): int =
    1.to(ROUNDS).fold(LOADED, (s, _) => roundOf(p, visible, s)).started

  // ---- scenarios ----------------------------------------------------------------------------

  // KER-05H, KER-05D: no pool's carve grows when the free capacity shrinks or shrinks when a
  // quarantine is released, in either pool order (the larger-first order is the one that broke
  // KER-05H's greedy carve); the carve-outs fit together and carve at least KER-05H's keep did.
  run carveMonotoneTest = initWith(0, 0).expect(carveMonotone)

  // KER-05D: the candidate policies against the properties (safe, each pool monotone, total
  // monotone, release monotone), in `POLICY_ORDER`: only the keeping policies are monotone pool
  // by pool, and handing a later pool the leftover is not even monotone in total.
  run carvePoliciesTest = initWith(0, 0).expect(
    POLICY_ORDER.foldl([], (l, p) => l.append(propertiesOf(p))) == [
      (true, true, true, true),     // ROOM
      (true, true, true, true),     // EXACT
      (true, false, false, false),  // LEFTOVER
      (true, false, true, false),   // ENVELOPE
      (true, true, true, true),     // DEMAND, for a fixed number of waiting jobs
      (true, false, true, false),   // SMALL_FIRST
    ])

  // KER-05D: what each candidate carves (earlier pool, later pool) on the representative hosts,
  // in `POLICY_ORDER` (the table of doc/domain-model.md §6.3.1).
  run carveScenariosTest = initWith(0, 0).expect(and {
    //                  ROOM    EXACT   LEFTOVER ENVELOPE DEMAND  SMALL_FIRST
    carvesOf(S1) == [(0, 0), (0, 0), (0, 3), (0, 1), (0, 3), (0, 3)],
    carvesOf(S2) == [(0, 0), (0, 0), (0, 3), (0, 1), (0, 0), (0, 3)],
    carvesOf(S3) == [(0, 0), (0, 0), (0, 3), (0, 1), (0, 0), (0, 3)],
    carvesOf(S4) == [(1, 4), (1, 8), (1, 8), (1, 8), (1, 8), (0, 9)],
    carvesOf(S5) == [(1, 1), (1, 2), (1, 2), (1, 2), (1, 2), (0, 3)],
    carvesOf(S6) == [(1, 0), (1, 0), (1, 2), (1, 1), (1, 0), (0, 6)],
  })

  // KER-05D: under sustained Linux load the waiting macOS job starts once four Linux VMs have
  // finished in the room its pool keeps; a policy that lets the Linux pool into that room
  // starves it, and so does `DEMAND` when it only sees the jobs GitHub assigned under the
  // reported max (the job is never assigned, so the pool never keeps room for it).
  run sustainedLoadTest = initWith(0, 0).expect(and {
    POLICY_ORDER.foldl([], (l, p) => l.append(sustainedLoad(p, true))) == [4, 4, -1, -1, 4, -1],
    sustainedLoad(DEMAND, false) == -1,
  })

  // DEC-06: the state machine's carve, from counts, is `carveTwo` over the pools' shapes.
  run carveCountsTest = initWith(0, 0).expect(carveCountsAgree)

  // DEC-06: `DEMAND` keeps `carveMonotone`'s properties for every fixed number of waiting jobs,
  // and never carves the later pool less than `EXACT`.
  run carveMonotoneDemandTest = initWith(0, 0).expect(carveMonotoneDemand)

  // DEC-06: the 13 GiB case (S1): 13 GiB free beside an idle macOS pool whose 15 GiB VM does not
  // fit. `EXACT` keeps the macOS pool's headroom and carves the Linux pool nothing; `DEMAND`,
  // told no macOS job waits, keeps nothing and carves three Linux VMs. Told one macOS job waits
  // (S2), it keeps the room for it as `EXACT` does.
  run demandFixesThe13GiBCaseTest = initWith(0, 0).expect(and {
    carveBy(EXACT, S1.free, S1.first, S1.second, S1.waiting) == (0, 0),
    carveBy(DEMAND, S1.free, S1.first, S1.second, S1.waiting) == (0, 3),
    carveBy(DEMAND, S2.free, S2.first, S2.second, S2.waiting) == (0, 0),
  })

  // DEC-06: the risk `DEMAND` carries until GH-00 answers `doc/github-scaleset.md` §12 item 6.
  // If GitHub assigns jobs only under the `maxCapacity` mayfly sends, a pool carved nothing is
  // never told of its job: under sustained Linux load the macOS job starves (-1), where
  // `EXACT` starts it in round 4; told of the job it starts in round 4 too.
  run demandStarvesWhenDemandIsInvisibleTest = initWith(0, 0).expect(and {
    sustainedLoad(EXACT, false) == 4,
    sustainedLoad(DEMAND, true) == 4,
    sustainedLoad(DEMAND, false) == -1,
  })


  /// The report the data plane sends as its `id`-th, with this foreign consumption.
  pure def reportOf(id: int, fm: int, fmac: int): Report = {
    id: id, mem: TOTAL_MEM, disk: TOTAL_DISK, slots: SLOTS.size(), macos: MACOS_GUESTS,
    foreignMem: fm, foreignVms: fmac, foreignMacos: fmac,
  }

  /// The first report applied and both pools' terms open: the host admits. Deterministic, so
  /// a scenario runs once (`quint test` samples a scenario with a nondeterministic choice).
  action readyWith(fm: int, fmac: int): bool =
    dpReport.then(applyReport(reportOf(1, fm, fmac))).then(applyTerms(LINUX)).then(applyTerms(MACOS))

  action ready = readyWith(0, 0)

  // DEC-06, in the state machine: with one unit of foreign memory the idle Linux pool (first in
  // name order) keeps its two VMs' memory ahead of the macOS pool under `EXACT`, so the macOS
  // pool is carved nothing; under `DEMAND`, told no Linux job waits, the Linux pool keeps and is
  // carved nothing and the macOS pool is carved one VM, which the host admits.
  run demandLetsTheLaterPoolInTest = {
    initFull(1, 0, Map(LINUX -> DEMAND, MACOS -> EXACT), NO_WAITING).then(readyWith(1, 0))
      .expect(plan.carveLinux == 0 and plan.carveMacos == 1 and allInvariants)
      .expect(planOf({ ...host, policy: ALL_EXACT }, leases, terms).carveMacos == 0)
      .then(reserve(1, MACOS)).expect(leases.get(1).phase == RESERVED and allInvariants)
  }

  // Admission fills the host on the tightest dimension, then refuses; the plan carves nothing.
  run fillsThenRefusesTest = {
    initWith(0, 0).then(ready)
      .expect(plan.carveLinux == 2 and plan.carveMacos == 1)
      .then(reserve(1, MACOS)).expect(leases.get(1).slot == 0 and leases.get(1).gen == 1)
      .then(reserve(2, LINUX))
      // memory: 3 of 4 units allocated, a macOS guest needs 2
      .expect(not(canReserve(host, leases, terms, 3, MACOS)) and canReserve(host, leases, terms, 3, LINUX))
      .then(reserve(3, LINUX))
      .expect(plan.carveLinux == 0 and plan.carveMacos == 0 and plan.freeVms == 0 and allInvariants)
  }

  // A released slot is reserved again under its next generation, never the same one.
  run generationsIncreaseTest = {
    initWith(0, 0).then(ready)
      .then(reserve(1, LINUX)).then(release(1))
      .then(reserve(2, LINUX))
      .expect(leases.get(2).slot == 0 and leases.get(2).gen == 2 and allInvariants)
  }

  // Two macOS guests at most, foreign ones included: a foreign guest leaves room for one.
  run macosAtMostTwoTest = {
    initWith(0, 1).then(readyWith(0, 1))
      .expect(canReserve(host, leases, terms, 1, MACOS))
      .then(reserve(1, MACOS))
      .expect(not(canReserve(host, leases, terms, 2, MACOS)))
      .then(dpCreate(1))
      .expect(oursRunningMacos + dp.foreignMacos == 2 and allInvariants)
  }

  // A stale report never allocates beyond the newer one already applied.
  run staleReportIgnoredTest = {
    initWith(0, 0).then(dpReport)
      .then(worldWith(2, 0)).then(dpReport)
      // apply the newer (tighter) report first, then the older (roomier) one
      .then(applyReport(reportOf(2, 2, 0)))
      .then(applyReport(reportOf(1, 0, 0)))
      .expect(host.report.id == 2)
      .then(applyTerms(LINUX)).then(applyTerms(MACOS))
      .then(reserve(1, LINUX)).then(reserve(2, LINUX))
      .expect(not(canReserve(host, leases, terms, 3, MACOS)) and allocationWithinReport)
  }

  // Drain admits nothing and completes only once the leases are gone.
  run drainAdmitsNothingTest = {
    initWith(0, 0).then(ready).then(reserve(1, LINUX)).then(startDrain)
      .expect(not(canReserve(host, leases, terms, 2, LINUX)))
      .expect(plan.carveLinux == 0 and reportedMaxOf(host, plan, leases, terms, LINUX) == 1)
      .then(dpCreate(1)).then(dpDelete(1)).then(release(1))
      .then(completeDrain).expect(host.drain == DRAINED and allInvariants)
  }

  // A quarantined lease holds its capacity, shrinks to disk and slot when stopped, and is never
  // released while its VM exists; only the operator's resolution frees it.
  run quarantineHoldsCapacityTest = {
    initWith(0, 0).then(ready)
      .then(reserve(1, MACOS)).then(dpCreate(1)).then(quarantine(1))
      .expect(plan.freeMem == 2 and plan.freeVms == 2 and plan.freeMacos == 1)
      .then(stopQuarantined(1))
      .expect(plan.freeMem == 4 and plan.freeVms == 2 and plan.freeMacos == 1)
      // its VM exists: no release
      .expect(vms.get(1) == STOPPED and allInvariants)
      .then(operatorResolve(1)).then(release(1))
      .expect(leases.get(1).phase == RELEASED and plan.freeVms == 3 and plan.freeMacos == 2)
  }

  // A pool whose capacity quarantined leases consume admits nothing (I28, ADR 0025).
  run quarantineConsumesPoolTest = {
    initWith(0, 0).then(ready)
      .then(reserve(1, MACOS)).then(dpCreate(1)).then(quarantine(1)).then(stopQuarantined(1))
      .then(reserve(2, MACOS)).then(dpCreate(2)).then(quarantine(2)).then(stopQuarantined(2))
      .expect(not(canReserve(host, leases, terms, 3, MACOS)) and plan.carveMacos == 0)
      .expect(reportedMaxOf(host, plan, leases, terms, MACOS) == 0 and allInvariants)
  }

  // The breaker trips at N failures; the published terms close and nothing is admitted until
  // an operator resets it.
  run breakerTripsTest = {
    initWith(0, 0).then(ready)
      .then(countFailure(LINUX)).then(countFailure(LINUX))
      .expect(pools.get(LINUX).tripped)
      .then(applyTerms(LINUX))
      .expect(not(terms.get(LINUX).open) and not(canReserve(host, leases, terms, 1, LINUX)))
      .then(resetBreaker(LINUX)).then(applyTerms(LINUX))
      .expect(canReserve(host, leases, terms, 1, LINUX) and allInvariants)
  }

  // A paused pool's scale set is told only its live runners; the gate closed admits nothing.
  run pauseReportsLiveOnlyTest = {
    initWith(0, 0).then(ready).then(reserve(1, LINUX))
      .then(pausePool(LINUX)).then(applyTerms(LINUX))
      .expect(reportedMaxOf(host, plan, leases, terms, LINUX) == 1 and plan.carveLinux == 0)
      .then(closeGate)
      .expect(not(canReserve(host, leases, terms, 2, MACOS)) and plan.carveMacos == 0)
  }

  // ADR 0052: while a VM's memory is not known the data plane starts nothing and reports no
  // deliverable memory, so the host reserves nothing; a complete description brings it back.
  run unknownMemoryStartsNothingTest = {
    initWith(0, 0).then(ready)
      .then(reserve(1, LINUX))
      .then(vmFactsChangeWith(true))
      .expect(not(dpFits(1)))
      .then(dpReport).then(applyReport({ ...reportOf(2, 0, 0), mem: 0 }))
      .expect(not(canReserve(host, leases, terms, 2, LINUX)) and plan.carveLinux == 0)
      .then(vmFactsChangeWith(false))
      .expect(dpFits(1))
      .then(dpCreate(1))
      .expect(vms.get(1) == RUNNING and allInvariants)
  }

  // The data plane refuses a start that does not fit its ledger, whatever the host allocated:
  // foreign memory grew after the report the host allocated within.
  run dataPlaneRefusesWhatDoesNotFitTest = {
    initWith(0, 0).then(ready)
      .then(reserve(1, MACOS)).then(reserve(2, LINUX))
      .then(worldWith(2, 0))
      .then(dpCreate(2))
      .expect(vms.get(2) == RUNNING and not(dpFits(1)) and allInvariants)
  }
}
