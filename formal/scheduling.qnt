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
module scheduling {
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

  type Host = { report: Report, gateClosed: bool, drain: str }

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

  /// The free capacity the policy carves, on the four dimensions admission counts.
  type Free = { mem: int, disk: int, vms: int, mac: int }

  /// One pool as the carve sees it: its VM's memory, whether it takes a macOS-guest slot, its
  /// `max_running` headroom (live and quarantined leases count against it), the places it
  /// keeps ahead of the pools after it (`max_running` minus its live runners: its headroom and
  /// its quarantined places), and whether its admission is open.
  type Shape = { mem: int, macos: bool, head: int, room: int, open: bool }

  /// How many VMs of `s` fit `f`, on every dimension it uses.
  pure def fitShape(f: Free, s: Shape): int = {
    val n = min2(min2(max0(f.mem) / s.mem, max0(f.disk) / VM_DISK), max0(f.vms))
    if (s.macos) min2(n, max0(f.mac)) else n
  }

  /// `f` less `n` VMs of `s`.
  pure def minus(f: Free, s: Shape, n: int): Free = {
    mem: f.mem - n * s.mem, disk: f.disk - n * VM_DISK, vms: f.vms - n,
    mac: if (s.macos) f.mac - n else f.mac,
  }

  /// The places an open pool keeps ahead of the pools after it. KER-05H: its whole `room`,
  /// never what it was carved, so a later pool's share does not depend on how much of the
  /// free capacity the earlier pool could use; carving the leftover of a smaller free capacity
  /// let a later pool of smaller VMs take more of them (`carveMonotoneTest`).
  pure def keeps(s: Shape, carved: int): int = if (s.open) s.room else 0

  /// The carve of two pools in order: the first up to its headroom within `f`, the second up
  /// to its headroom within what the first keeps ahead of it.
  pure def carveTwo(f: Free, first: Shape, second: Shape): (int, int) = {
    val c1 = if (first.open) min2(first.head, fitShape(f, first)) else 0
    val c2 =
      if (second.open) min2(second.head, fitShape(minus(f, first, keeps(first, c1)), second))
      else 0
    (c1, c2)
  }

  type Plan = {
    freeMem: int, freeDisk: int, freeVms: int, freeMacos: int,
    carveLinux: int, carveMacos: int,
  }

  /// One pool's shape under the leases `ls`, open when the host's gate and its terms are.
  pure def shapeOf(h: Host, ls: int -> Lease, ts: str -> Terms, pool: str): Shape = {
    mem: vmMem(pool),
    macos: pool == MACOS,
    head: max0(MAX_RUNNING - countOf(ls, pool, live)),
    room: max0(MAX_RUNNING - countOf(ls, pool, runner)),
    open: gateOpen(h) and ts.get(pool).open,
  }

  /// The pure policy: the free capacity after every live lease (quarantined included), carved
  /// in pool-name order among pools whose admission is open, each up to its `max_running`
  /// headroom (live and quarantined leases count against it) within what the pools before it
  /// keep ahead of it (`keeps`).
  pure def planOf(h: Host, ls: int -> Lease, ts: str -> Terms): Plan = {
    val r = h.report
    val f = {
      mem: max0(limMem(r) - allocMemOf(ls)),
      disk: max0(limDisk(r) - allocVmsOf(ls) * VM_DISK),
      vms: max0(limVms(r) - allocVmsOf(ls)),
      mac: max0(limMacos(r) - allocMacosOf(ls)),
    }
    val c = carveTwo(f, shapeOf(h, ls, ts, LINUX), shapeOf(h, ls, ts, MACOS))
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

  action initWith(foreignMem: int, foreignMacos: int): bool = all {
    host' = { report: NO_REPORT, gateClosed: false, drain: NO_DRAIN },
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
    initWith(fm, fmac)
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

  /// carveMonotone (KER-05H): the carve's total never grows when the free capacity shrinks on
  /// any dimension, nor when an earlier pool's headroom grows within its room (a quarantine
  /// released), whichever of the two shapes comes first. The state space above carves the
  /// small Linux VM before the larger macOS one only, where carving the leftover is monotone;
  /// this checks the carve over every free capacity of a small host and both orders, so the
  /// larger-first order the Rust code meets with arbitrary pool names is covered too.
  pure def shape(pool: str, headroom: int, places: int): Shape =
    { mem: vmMem(pool), macos: pool == MACOS, head: headroom, room: places, open: true }
  pure def carveTotal(f: Free, first: Shape, second: Shape): int = {
    val c = carveTwo(f, first, second)
    c._1 + c._2
  }
  pure val FREES: Set[Free] = tuples(0.to(4), 0.to(3), 0.to(3), 0.to(2))
    .map(t => { mem: t._1, disk: t._2, vms: t._3, mac: t._4 })
  pure val ORDERS: Set[(str, str)] = Set((LINUX, MACOS), (MACOS, LINUX))
  pure def shrunk(f: Free): Set[Free] = Set(
    { ...f, mem: max0(f.mem - 1) }, { ...f, disk: max0(f.disk - 1) },
    { ...f, vms: max0(f.vms - 1) }, { ...f, mac: max0(f.mac - 1) },
  )
  pure val carveMonotone = FREES.forall(f => ORDERS.forall(o =>
    tuples(0.to(MAX_RUNNING), 0.to(MAX_RUNNING), 0.to(MAX_RUNNING)).forall(t => {
      val first = shape(o._1, t._1, max0(MAX_RUNNING - t._2))
      val second = shape(o._2, t._3, t._3)
      and {
        first.head > first.room or and {
          shrunk(f).forall(g2 => carveTotal(g2, first, second) <= carveTotal(f, first, second)),
          first.head == first.room
            or carveTotal(f, first, second) <= carveTotal(f, { ...first, head: first.head + 1 }, second),
        },
      }
    })))

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

  // ---- scenarios ----------------------------------------------------------------------------

  // KER-05H: the carve never gives more when the free capacity shrinks or a quarantine is
  // released, in either pool order (the larger-first order is the one that broke it).
  run carveMonotoneTest = initWith(0, 0).expect(carveMonotone)


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
