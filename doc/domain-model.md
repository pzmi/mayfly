# mayfly: domain model

**Status:** owner-reviewed, 2026-11-09 (task DDD-01; decisions applied by DEC-01). Owner: Patryk
Żmigrodzki. Every OWNER hot spot is decided and recorded as an ADR (§10.1 names it); the remaining sub-items of OD20 stay planner defaults.
**What:** the bounded contexts, ubiquitous language, aggregates, policies and read models of
mayfly, converged from an event-storming session. It sits *upstream* of [`plan.md`](plan.md),
the Quint models in `formal/` and the code: plan.md says what the product does and how it is
structured; this document says what the words mean and where each rule is protected. A citation
"dm §6.4" points here. Hot spots marked **OWNER** (§10) are open until the owner decides; until
then the recommended option is the working assumption of every later section; today that holds
only for the remaining sub-items of OD20.

---

## 1. How this model was made, and how to change it

### 1.1 Method

Big Picture and Process Level event storming, run asynchronously on 2026-11-09. Four participants
wrote stickies independently against plan.md (draft), verification.md §6.1/§8.4 and the blueprint:

| Perspective | Focus | Main contributions |
|:--|:--|:--|
| GitHub demand | Scale Set protocol, JIT, statistics | four credentials, registration as its own resource, `maxCapacity`/acquire gap, warm vs standby |
| VM fleet | Tart, macOS virtualization, images | `tart run` lifetime, pull vs clone, multi-dimensional capacity, image context, instance identity |
| SRE / on-call | operating the daemon at 03:00 | modes (safe/drain/pause/degraded), recovery plans, quarantine cases, breakers, upgrade/rollback |
| Security | trust boundaries, secrets, ownership | ownership proof, per-attempt guest identity, trust tiers, epoch floor, secret handle semantics |

The facilitator merged the four walls: de-duplicated events under one name each (§2), grouped
them into consistency boundaries (§6) and contexts (§3), turned every "whenever X then Y" into an
owned policy (§7), and collected every contradiction into hot spots (§10). Persistence follows
the owner's decision that the control plane uses a **stream-scoped, storage-neutral event store**
as `doc/event-store.md` designs it: one command appends to one
stream; streams have independent versions; there is no global writer and no global order;
cross-stream rules are eventual and run through durable per-stream consumers with idempotency
keys. Every aggregate below was cut to fit that store.

Legend used throughout: **[X]** external fact (GitHub, Tart, host OS, guest, operator's hand
outside mayfly; we only observe it), **[D]** our decision (journaled with its inputs), **[O]**
observed outcome of one of our own effects (an external fact we caused; its reply can be lost),
**★** pivotal event (the world is different afterwards; a boundary in the timeline).

### 1.2 Changing the model (model-first)

1. A change to a term, an aggregate's boundary, an invariant, a policy or a context relationship
   is made **here first**, in the same commit as (or before) the plan.md text, the Quint model,
   its scenarios, the conformance driver and the Rust code that follow from it (D10 extended).
2. A new event type gets its name here (§6) before it gets a golden fixture. Names are final once
   released: the journal is backward-compatible forever (§10, OD2).
3. A new invariant takes the next `I<n>` id here and in verification.md §6.1 together; an invariant
   is never removed to get green.
4. A new hot spot is appended to §10 with its participants and severity; an OWNER item is closed
   only by the owner, with the decision and date written next to it.
5. A section of this document that disagrees with the code is a bug in one of them: decide which,
   fix that one, and say so in the commit body.

---

## 2. Big picture timeline

Swimlanes are bounded contexts (§3): **OPS** Operations, **DEM** Demand, **FLT** Fleet,
**IMG** Image, **ATT** Attempt, **REG** Registration, **SEC** Secrets (no journal of its own; its
events land on ATT/REG), **EXE** VM Execution (data plane, not journaled; its facts arrive as
observations and are recorded by ATT/IMG/FLT), **EXT** external actors. Names are the final event
names of §6 (DP observations keep the same name when recorded).

### 2.1 Happy path: one job on a cold host

| # | Lane | Event | Kind | |
|--:|:--|:--|:--|:--|
| 1 | OPS | `RunStarted{run_no, boot_id, version, schema}` | D | |
| 2 | OPS | `EpochMinted{epoch}` (≥ external floor, floor written first) | D | ★ |
| 3 | EXE | `Hello{host_id, boot_id, capabilities}` → `Welcome{epoch}` | O | |
| 4 | EXE | `InventoryObserved{vms, complete: true}` | X | |
| 5 | OPS | `RecoveryCompleted{adopted, cleaned, reported, unknown}` | D | |
| 6 | FLT | `AdmissionOpened{cause: recovery_done}` | D | ★ |
| 7 | IMG | `ImageRegistered{digest, manifest}` → `ImageVerified{digest}` | D | |
| 8 | FLT | `PoolConfigured{revision, tier, image_pin, limits, prepared_policy}` | D | |
| 9 | FLT | `PoolTermsApplied{pool, revision, open: true}` (on Host) | D | |
| 10 | IMG | `PlacementRequested{host, digest}` → `ImagePulled` → `ImageMaterialised{base_name}` | D/O | |
| 11 | DEM | `ScaleSetEnsured{github_id, runner_group, labels}` | O | |
| 12 | DEM | `SessionOpened{session_id, statistics}` (initial statistics are authoritative demand) | O | ★ |
| 13 | DEM | `ReportedMaxChanged{value, reason}` | D | |
| 14 | EXT | workflow author pushes; GitHub queues a job for the scale set | X | |
| 15 | DEM | `MessageRecorded{message_id, statistics, offers, acquired, declined, desired}` | X+D | ★ |
| 16 | EXT | GitHub `DeleteMessage` accepted (ack, not journaled) | O | |
| 17 | FLT | `SlotReserved{slot, generation, attempt_id, pool, os, resources}` | D | ★ |
| 18 | ATT | `AttemptStarted{vm_name, slot, generation, image_digest, pool, tier}` | D | |
| 19 | ATT | `DirectiveIssued{CloneVm}` → `VmCloned{instance_id}` | D/O | |
| 20 | ATT | `DirectiveIssued{StartVm}` → `VmBooted` | D/O | |
| 21 | ATT | `DirectiveIssued{VerifyGuest{nonce}}` → `GuestVerified{facts}` | D/O | |
| 22 | ATT | `AttemptPrepared` (booted, verified, unregistered, no secret) | D | ★ |
| 23 | FLT | `LeaseActivated{attempt_id}` (activation grant) | D | ★ |
| 24 | ATT | `ActivationStarted` → `DirectiveIssued{VerifyGuest{fresh nonce}}` → `GuestVerified` | D/O | |
| 25 | REG | `RegistrationRequested{runner_name}` → `RegistrationIssued{runner_id}` | D/O | ★ |
| 26 | SEC/ATT | `JitHandleIssued{handle_id, scope}` + `DirectiveIssued{LaunchRunner{handle}}` | D | |
| 27 | ATT | `RunnerLaunched` (DP saw the launcher take stdin and the runner start) | O | |
| 28 | REG | `RegistrationDelivered` | D | |
| 29 | EXT | runner connects to the Actions service; GitHub assigns the job | X | |
| 30 | DEM | `MessageRecorded{… JobStarted{request_id, runner_name}}` | X | |
| 31 | ATT | `JobBound{request_id}` (Listening → Busy) | D | ★ |
| 32 | REG | `RegistrationConsumed{request_id}` | D | |
| 33 | EXT | job runs (untrusted workflow code inside the guest) | X | |
| 34 | ATT | `RunnerExited{code}` | X | |
| 35 | DEM | `MessageRecorded{… JobCompleted{result}}` → REG `RegistrationCompleted{result}` | X | ★ |
| 36 | ATT | `AttemptRetired{reason: job_done}` → `DiagnosticsExported` | D/O | |
| 37 | ATT | `DirectiveIssued{StopVm}` → `VmStopped` | D/O | |
| 38 | ATT | `DirectiveIssued{DeleteVm{proof}}` → `VmDeleted` (inventory shows absence) | D/O | ★ |
| 39 | FLT | `SlotReleased{slot, generation, attempt_id}` | D | ★ |

The pivotal events cut the timeline into phases: **run** (2–6), **demand** (12, 15), **admission**
(17), **preparation** (22), **activation** (23–25), **binding** (31), **completion** (35),
**reclamation** (38–39). A secret exists only between 25 and 33; a slot is held from 17 to 39.

### 2.2 Unhappy paths (main ones)

**U1 Lost effect reply (clone, start, delete).** `DirectiveIssued` → `DirectiveOutcomeUnknown` →
(no new directive for that target) → `InventoryObserved` → `VmCloned` | `VmDeleted` | observed
absence → state advances. A stalled `Unknown` raises `UnknownEffectAged` (read model, alert);
only an observation clears it; when the provider cannot answer, it stays `Unknown` and is
reported as a provider limitation (OD10, ADR 0025).

**U2 Guest never ready.** `VerifyGuest` deadline → `AttemptBackedOff{stage, n}` (same name and
generation) → budget spent → `AttemptRetired{reason: guest_unready}` → delete → FLT
`AttemptFailureCounted{pool, digest, stage}` → N failures in window → `PoolBreakerTripped{digest}`
→ `PoolTermsApplied{open: false}`. Bad images cost a bounded number of attempts (I27).

**U3 Guest identity mismatch.** `GuestRejected{reason: challenge_failed}` → no mint →
`AttemptQuarantined{reason: guest_identity_refused, vm: stopped}` → OPS `QuarantineCaseOpened`.

**U4 JIT minted, reply lost / crash before delivery.** REG `RegistrationIssueUnknown` → reaper
lists runners by our generated name → `RegistrationIssued{runner_id}` (found) or
`RegistrationAbsent` → `DeregistrationRequested` → `RegistrationDeregistered`; ATT
`AttemptRetired{reason: jit_lost}`; a new attempt gets a new name. A crash after `RegistrationIssued`
and before `JitHandleIssued` produces `JitHandleOrphaned` at the next `RunStarted` (the broker
is empty by construction) with the same retirement.

**U5 Delivery outcome unknown.** `RunnerLaunchUnknown` → REG `RegistrationPresumedLeaked` →
deregister if proven ours → ATT `AttemptRetired{reason: jit_presumed_leaked}`; never relaunched.

**U6 Cancel before start (poisoned idle).** DEM `MessageRecorded{statistics: assigned↓}` →
reconciler: surplus Listening runner (no standby exists: OD8) → ATT `RetirementRequested` → REG `DeregistrationRequested` →
`RegistrationDeregistered` → ATT `AttemptRetired` → stop/delete. If GitHub answers 422,
`DeregistrationRefusedBusy` → retirement withdrawn, the attempt stays and serves the job (I21).

**U7 Guest dies mid-job.** `RunnerExited{code}` or `VmStopped` while Busy → `AttemptRetired
{reason: guest_died}` → diagnostics (likely `DiagnosticsLost`) → delete; the job result comes later
from GitHub as `JobCompleted{abandoned}` and closes the registration.

**U8 Daemon crash while Busy.** next `RunStarted` → `PreviousRunEndedUnclean` → `EpochMinted{e+1}`
→ `InventoryObserved` (VM still running under its per-VM shim, OD1, ADR 0037) →
`AttemptAdopted{epoch}` → job completes → normal reclamation. Never deleted while the registration
is not terminal and the VM runs.

**U9 Host reboot.** `RunStarted{boot_id changed}` → every journaled-running VM is stopped by fact →
`VmObservedStopped` → Busy attempts become `AttemptRetired{reason: host_rebooted}` and their
registrations are reaped once GitHub reports them offline or abandoned.

**U10 Disk low / foreign macOS guest.** EXE `CapacityReported{free shrinks: disk | macOS slots
held by a foreign guest}` → FLT `CapacityReportApplied` (and `DiskLowWaterCrossed` below the
reserve) | EXE refusal `DirectiveRefused{insufficient_resources}` or `HypervisorLimitHit` →
`AdmissionClosed{cause}` / admissible shrinks → DEM
`ReportedMaxChanged{reason}` at the next poll (≤ one long-poll of lag).

**U11 GitHub degraded.** DEM `SessionDegraded{cause: 401|403|429|5xx|network}` → JIT minting frozen
(REG refuses new `RegistrationRequested`), running jobs continue, prepared VMs stay prepared,
nothing is deleted on the basis of missing GitHub data (`unreachable` never means `absent`).
`SessionExpired` → `SessionOpened` with fresh initial statistics; `SessionConflicted` (409 after a
restart) → backoff under the session TTL.

**U12 Journal doubt.** `RunStarted` → verification fails (unknown event type, epoch below floor,
structural check) → `SafeModeEntered{cause}` (journal if writable, sidecar always) → effects
contained (§5 "safe mode") → `RecoveryPlanProposed` → `RecoveryPlanApproved{actor}` →
`RecoveryActionApplied…` → `RecoveryPlanCompleted` → re-verification → `SafeModeLeft{plan_id}`.

**U13 Drain.** `HostDrainStarted` | `PoolDrainStarted` → `PoolTermsApplied{open: false}` →
`ReportedMaxChanged{0}` → prepared and standby attempts retired → Busy finish →
`HostDrainCompleted` when no non-terminal lease remains. `DrainForced{actor}` is separate and
stops Busy VMs.

**U14 Quarantine and its resolution.** `AttemptQuarantined{reason, vm: stopped|running}` →
`LeaseQuarantined` (Host counts it against the cap) → `QuarantineCaseOpened` →
`QuarantineCaseEscalated{age}` → `QuarantineCaseResolved{disposition, actor}` → ATT continues
cleanup (`Deleting`) or, on observed absence, `VmVanished` (a VM the attempt recorded, DEC-07)
or `AttemptLost` (one it never recorded) → `SlotReleased` only on observed absence.

**U15 A recorded VM vanished (DEC-07, ADR 0066 as amended 2026-11-11).** A complete listing
lacks a VM the attempt recorded (`VmCloned`) and mayfly did not delete: the stop's or start's
re-observation (`DirectiveRefused{target_absent}`) or the inventory → `VmVanished{discovered_by,
instance_id, vm}` (Lost) → `AttemptClosed`, `SlotReleased{attempt_lost}`; the reconciler mints a
fresh attempt for the remaining demand; P15 counts a pool failure; the `attempts` view, `mayfly
status` and the bundle show the desync. An incomplete listing concludes nothing (I25).

---

## 3. Bounded contexts

Single host, one process in v0.x: contexts are **modules with their own language and
aggregates**, not services. The seams are the module boundaries, the events between them and the
CP↔DP protocol; they are cheap now and make the multi-host edition a transport change.

| Context | Plane | Purpose | Owned language | Aggregates | Upstream of / downstream of |
|:--|:--|:--|:--|:--|:--|
| **Demand** | CP | Turn GitHub's scale-set queue into a number of runners we owe; decide which offers to acquire; tell GitHub what we can deliver | scale set, session, message, ack, offer, acquire, statistics, assigned, desired, reported max, trust tier (of an offer) | `ScaleSet` | downstream of GitHub (ACL); upstream of Fleet (desired) |
| **Fleet** | CP | Allocate the capacity each host's data plane reports and make every admission decision; pools as policy (OD4, ADR 0024) | host, slot, slot generation, lease, admissible, deliverable, pool, pool terms, pause, drain, breaker, prepared policy | `Host`, `Pool` | downstream of Demand, Image, Operations; upstream of Attempt |
| **Attempt** | CP | One disposable VM's life from admission to observed deletion, serving at most one job | attempt, VM name, clone, boot, verify, prepared, activation, listening, busy, retire, backoff, adopt, quarantine (as a state), unknown outcome | `Attempt` | downstream of Fleet (lease), Image (digest); customer of Registration and Secrets; supplier of directives to Execution |
| **Registration** | CP | The GitHub-side runner registration as a resource with its own lifecycle; reaping leaks | runner registration, runner name, runner id, JIT config, issue, deliver, consume, deregister, presumed leaked, reaper | `RunnerRegistration` | supplier to Attempt; downstream of GitHub (ACL) |
| **Image** | CP | Catalog of immutable images and their placement on hosts | image, digest, manifest, image revision, runner version, verify, withdraw, placement, image base, pull, materialise, evict | `Image`, `ImagePlacement` | upstream of Fleet (pool pin) and Attempt (digest); supplier of image directives to Execution |
| **Operations** | CP | The daemon's run, its epoch and its containment; human-in-the-loop work | run, epoch, epoch floor, safe mode, recovery (automatic), repair (plan), recovery plan, quarantine case, operator, actor, assertion | `HostRun`, `RecoveryPlan`, `QuarantineCase` | upstream of every CP context (epoch fence, mode); downstream of all (reads their state for plans) |
| **Secrets** | CP (supporting) | Hold dangerous values; hand out single-take handles | secret, credential, secret handle, take, sweep, seal | none (service; handle *events* land on `Attempt`) | supplier to Attempt, Demand, Registration; conformist link adapter |
| **VM Execution** | DP | Execute directives safely against the provider and guest; observe what exists; account for and enforce the host's resources and report capacity (ADR 0024) | directive, observation, fence, executor, supervisor, shim, process lost, inventory (complete/incomplete), image cache, guest session, resource ledger, capacity report | none durable but host facts: the epoch floor, the shims' files and the registry of image bases (ADR 0087); pure FSMs: executor, supervisor, image cache, guest session | downstream of CP via published language; ACL over Tart, host OS, guest |

**Shared kernel** (`mayfly-domain`): ids, `OwnedVmName`, `SlotGeneration`, `Epoch`, `Fence`,
`ImageDigest`, `Resources`, `Deadline`, `OwnershipProof`, `TrustTier`, `GuestOs`.
**Published language** (`mayfly-protocol`): `Hello`/`Welcome`, `Directive`, `Observation`,
`SecretHandle` (§4).

### 3.1 Decisions on the candidates raised

1. **Demand (GitHub scale set): its own context.** It is the only place that speaks GitHub's
   queue language and it runs at poll cadence; it must never wait on VM work. One aggregate,
   kept small.
2. **Runner Registration: its own context, split from Attempt and from Demand.** Its authority
   is GitHub, it outlives the VM (a registration can exist with no VM and vice versa), it has its
   own failure mode (leak) and its own reaper. It sits on the *opposite* side of the flow from
   Demand (Attempt is its customer; Demand is Fleet's supplier), which is why the two are not
   merged although they share the GitHub ACL.
3. **Fleet/Capacity: one context, two aggregates.** `Host` owns the *allocation* of one
   machine's capacity, as its data plane reports it, and every admission and activation decision
   (the data plane owns accounting and hard enforcement: OD4, ADR 0024); `Pool` owns policy (tier, image pin,
   limits, prepared policy, pause/drain, breaker). Capacity is **not** the pool's: limits cross
   every pool on a host. Host enforces pool limits through **pool terms** it has accepted (§6.3).
4. **Attempt vs VM lease: not split.** An attempt and its VM have the same identity, name,
   slot generation and lifetime (1:1, decided at reservation, ended at observed deletion). Two
   streams would need a process manager for every transition and double the `Unknown` space.
   What *was* worth splitting out is split: the job (not ours: a `JobLedger` projection plus a
   `job_binding` field), the registration (`RunnerRegistration`), the slot accounting (`Host`
   leases) and the human workflow (`QuarantineCase`). Inside `Attempt`, VM state and the
   `pending` outcome overlay are orthogonal fields (§6.4).
5. **Image: one context, CP catalog + CP placement; the DP image cache is Execution's model.**
   Image has its own lifecycle, actors (maintainer), external system (OCI registry) and clock
   (GitHub's runner deprecation). Pull/materialise are effects with `Unknown` outcomes, so they
   need a journaled intent: `ImagePlacement` per (host, digest). The DP keeps a pure cache model
   rebuilt from observation, like the VM supervisor.
6. **VM Execution: the data plane, one context.** No journal and no durable decisions (D6);
   it keeps only host facts: the epoch floor, the shims' files and the registry of image bases
   (ADR 0087). Its language
   (process lost, inventory incomplete, guest session) is not the CP's; observations translate.
7. **Secrets broker: a supporting context without an aggregate.** Values are memory-only and not
   replayable; the *rules* (scope, single take, epoch binding) are pure functions in the domain,
   and the facts (`JitHandleIssued`, `JitHandleTaken`, `JitHandleOrphaned`) are events on the
   attempt they are scoped to. This keeps "replay reproduces state" (I16) and "secrets never in
   the journal" (D13) from colliding.
8. **Operations: one context, three aggregates.** Run/epoch, recovery plans and quarantine cases
   have operator cadence, audit needs and long lifetimes unlike attempts. Safe mode lives here
   (run state), but is *enforced* at the effect boundary (§5, §6.8), because a journal in doubt
   cannot be the thing that enforces it. Drain is **not** here: it is an admission term on `Host`
   and `Pool` (Fleet), so I08 is protected by one aggregate.
9. **Rejected:** a `Job` context (we never own jobs), a `Credential` aggregate (token lifecycle
   is infrastructure in the GitHub ACL; only its *failures* are domain facts on `ScaleSet`), a
   `Slot` aggregate per slot (a sum invariant across slots needs one boundary: `Host`), a separate
   `OperatingMode` aggregate (mode is derived: §8 `OperatingMode` read model).

---

## 4. Context map

```text
                          ┌──────────── GitHub ────────────┐          ┌── OCI registry ──┐
                          │ REST · Actions svc · msg queue │          └────────┬─────────┘
                          └───────┬────────────────┬───────┘                   │
                         ACL (adapter-github)  ACL (adapter-github)            │ (DP pulls)
                                  │ conformist     │ conformist                │
   ┌──────────────┐   desired,    ▼                ▼                            │
   │  Operations  │◀─ ─ ─ ─ ┌──────────┐     ┌──────────────┐                  │
   │ HostRun      │ epoch,  │  Demand  │     │ Registration │                  │
   │ RecoveryPlan │ mode    │ ScaleSet │     │ RunnerReg.   │                  │
   │ Quarantine   │────────▶└────┬─────┘     └──────▲───────┘                  │
   └──────┬───────┘   (U)        │ desired (U→D,      │ customer/supplier       │
          │ epoch fence (OHS)    │ customer/supplier) │ (Attempt asks for JIT)  │
          ▼                      ▼                    │                         │
   ┌──────────────┐ pin,   ┌──────────┐ lease   ┌─────┴────┐   handle   ┌──────────┐
   │    Image     │──────▶ │  Fleet   │───────▶ │ Attempt  │◀──────────│ Secrets  │
   │ Image        │ (U→D)  │ Host     │ (U→D)   │ Attempt  │ (supplier) │ (broker) │
   │ ImagePlacem. │        │ Pool     │◀ ─ ─ ─ ─│          │            └──────────┘
   └──────┬───────┘        └──────────┘ outcome └────┬─────┘
          │ image directives                         │ directives
          ▼                                          ▼
  ═══════════════════ published language: mayfly-protocol (Hello/Welcome, Directive, Observation, SecretHandle) ═══════════════════
                                          │
                                ┌─────────┴─────────┐
                                │   VM Execution    │  DP, no journal (host facts only)
                                └──┬──────┬──────┬──┘
                         ACL (tart)│ ACL(guest)  │ ACL (host)
                                   ▼      ▼      ▼
                                Tart   guest   host OS          shared kernel: mayfly-domain (all)
```

| Upstream | Downstream | Relationship | What crosses | Mechanism |
|:--|:--|:--|:--|:--|
| GitHub | Demand, Registration | **Conformist behind an ACL** (`mayfly-adapter-github`) | messages, statistics, JIT configs, runner records, HTTP status | the ACL translates wire types into domain facts (`Offer`, `Statistics`, `RegistrationIssued`); no wire type reaches a domain crate |
| Demand | Fleet | Customer/supplier (Fleet supplies deliverable capacity, Demand supplies desired) | `desired`, `assigned`; back: `deliverable` per scale set | Reconciler (policy) reads both; no cross-stream transaction |
| Image | Fleet, Attempt | Customer/supplier | verified digest, guest OS, runner version, base name per host | `Pool.image_pin` holds a digest value; `Attempt` copies it at reservation |
| Fleet | Attempt | Customer/supplier (lease) | slot, generation, resources, activation grant | `SlotReserved` → policy starts the attempt; `LeaseActivated` gates the mint |
| Attempt | Fleet | Feedback (events consumed by policy) | outcome per stage and digest; release | `AttemptFailureCounted`, `SlotReleased` via policies |
| Registration | Attempt | Supplier (Attempt is the customer) | registration issued/consumed/deregistered | policy; Attempt never calls GitHub |
| Secrets | Attempt, Demand, Registration | Supplier; link adapter is conformist to handle semantics | `SecretHandle`, `Secret<T>` in memory | broker port `issue/take/revoke/sweep`; no `get` |
| Operations | all CP contexts | **Open host service** for the run epoch and mode | `run_epoch` on every automation command; safe-mode containment | command envelope field; aggregates refuse older epochs (I18) |
| CP | DP | **Published language** (`mayfly-protocol`), versioned | directives, observations, handles, epochs | link port; at-least-once both ways |
| DP | Tart, guest, host OS | **ACL** per adapter | argv, JSON inventory, stdin, probes | `adapter-tart` (with the guest transport, ADR 0082), `adapter-host` |
| all | all | **Shared kernel** `mayfly-domain` | value objects of §3 | crate dependency; changed only with every consumer |

### 4.1 Module layout (no new crates)

```text
mayfly-control-domain/src/
  demand/        scale_set.rs (aggregate) · demand.rs (desired, shortfall: pure) · acquire.rs
                 (AcquirePolicy) · trust.rs (classify offer → tier) · events.rs
  fleet/         host.rs · pool.rs · capacity.rs (pure plan → CapacityReport) · breaker.rs · events.rs
  attempt/       attempt.rs · lifecycle.rs (state × stimulus matrix) · events.rs
  registration/  registration.rs · naming.rs · events.rs
  image/         image.rs · placement.rs · events.rs
  operations/    host_run.rs · recovery_plan.rs · quarantine.rs · recovery.rs (decision table) ·
                 mode.rs (effective mode, admission explain: pure) · events.rs
  secrets/       scope.rs (handle scope and take rules: pure)
  kernel/        Aggregate, DomainEvent, registry, envelopes (KER-03)
mayfly-control/src/
  demand/listener.rs · registration/{manager,reaper}.rs · fleet/reconciler.rs ·
  attempt/saga.rs · image/placement_manager.rs · operations/{startup,repair,watchdog}.rs ·
  secrets/broker.rs · relay.rs (directive delivery) · inbox.rs (observations) ·
  projections/ · queries/ · commands/ · ports/
mayfly-data-domain/src/   executor.rs · supervisor.rs · image_cache.rs · guest_session.rs · fence.rs
mayfly-data/src/          executor · inventory observer · resource monitor · image cache manager
```

New crates are **not** justified in v0.x: every context fits a module of the existing pure and
application crates, the GitHub ACL already has `mayfly-adapter-github`, and the architecture
checker can pin module edges (e.g. `demand` never names `attempt`) with module rules rather than
crates. Revisit when a context gets a second plane or its own release cadence (multi-host).

---

## 5. Ubiquitous language

One meaning per term, used identically in code, config keys, metrics, logs and docs. *Rejected*
words are not used in code; a rejected word in config or a metric is renamed before release.
Context in brackets.

### 5.1 GitHub side

| Term | Definition | Rejected / not to be confused with |
|:--|:--|:--|
| **Job** [Demand] | A GitHub workflow job, identified for us by its `runnerRequestId`. Mayfly never creates, cancels or executes one. | "attempt", "task", "build" |
| **Offer** [Demand] | A `JobAvailable` for our scale set: a job we may acquire; nobody holds it yet. | "queued job" (GitHub's UI "queued" also counts jobs never offered to us) |
| **Acquire** [Demand] | The `AcquireJobs` call: we commit to serving the job. Distinct from **ack**. | "claim", "reserve" (reserve is a slot word) |
| **Ack** [Demand] | `DeleteMessage` for a recorded message. Only after the record commits (I12). | "acknowledge the job" |
| **Message** [Demand] | One queue message (a batch of ≤ 50 job messages plus statistics). | "event" (an event is ours, in the journal), "webhook" |
| **Statistics** [Demand] | The level snapshot carried by a message or a session open. The only demand input. | — |
| **Assigned** [Demand] | `TotalAssignedJobs`: acquired plus running. **Demand = assigned.** | "queued", "pending" |
| **Desired** [Demand] | Runners we owe: `min(deliverable, assigned + min_standby)`; v0.x `min_standby = 0`. Never a running sum of messages. | "target capacity" |
| **Shortfall** [Fleet] | `max(0, desired − (Activating + Listening + Busy))`: runners still to produce. Prepared VMs serve it first; they do not count in it. | — |
| **Session** [Demand] | The GitHub message-queue session of one scale set. | "epoch", "operator session", "run" |
| **Scale set** [Demand] | The GitHub-side resource whose *name* workflows put in `runs-on:`. One pool = one scale set (OD14). | "runner group" (a GitHub access group the scale set lives in) |
| **Refusal sink** [Demand] | A disposable Linux VM running GitHub's official runner whose job-started hook fails every job, started for a job the run lookup refused so that GitHub fails it at once (ADR 0090). | "decoy", "fake runner" (mayfly never speaks the runner protocol itself) |
| **Trust tier** [Demand, Fleet] | A classification of a **job offer** (never of a person): `trusted`, `untrusted`, `refused`. A pool serves exactly one tier. | `allow_untrusted_prs` (a degenerate toggle, replaced) |
| **Runner registration** [Registration] | The GitHub-side runner record (`runner_id`, runner name), existing from `RegistrationIssued` until deregistration or GitHub's own removal. | bare "runner" |
| **Runner process** [Execution] | The `actions/runner` OS process inside a guest. | bare "runner" |
| **Runner name** [Registration] | Generated from the attempt (`= VmName`), never from workflow data, never reused (I19). | — |
| **JIT config** [Registration, Secrets] | GitHub's single-use runner configuration for one runner name. A secret. States: issued, delivered, consumed, deregistered, presumed leaked. "Single-use" bounds its use; it does not make it harmless. | "token", "registration token" (mayfly has no classic registration token: ADR 0028) |
| **Delivered / consumed / presumed leaked** [Registration] | Delivered: the launcher took the bytes. Consumed: GitHub bound a job to the runner. Presumed leaked: delivery outcome unknown, or the handle orphaned, or identity doubt after delivery. Never say "used". | "used" |
| **Deregister** [Registration] | REST delete of a runner registration by its journaled id. | "unregister", "remove runner" |
| **Cancel / abandon / reassign** | Cancel: a human cancelled the job. Abandon: GitHub gave up because the runner vanished mid-job. Reassign: GitHub offers the job again. Three paths, three recoveries. | — |
| **Credential** [Secrets] | One of four GitHub credentials (§9.1): App JWT, installation token, Actions-service token, message-queue token. | "the token" |
| **Standby** [Fleet, Demand] | Policy of holding *registered-idle* runners ahead of demand (`min_standby`). Not in v0.x (OD8). | "warm registered" |

### 5.2 VM side

| Term | Definition | Rejected / not to be confused with |
|:--|:--|:--|
| **Host** [Fleet] | One Apple Silicon machine with one dedicated `TART_HOME` and one data-plane session at a time, named by `host_id`. | "node", "worker" |
| **Host agent** [Execution] | The data-plane process (or role) on a host. | "host" |
| **VM** [Execution] | A machine registered in the provider, identified by its provider name. Exists independently of any attempt (image bases are VMs too). | "runner", "instance" |
| **Provider instance id** [Execution, shared] | The provider's identity of one VM *incarnation*, recorded at clone observation; distinguishes "our VM" from "another VM with the same name". For Tart: the random MAC set on every clone (`tart set --random-mac`; OD6, ADR 0026). | "VM name" (reusable) |
| **Image** [Image] | An immutable, content-addressed artefact (digest) plus its manifest (guest OS, runner version, image revision, launcher hash). Never run directly. | "golden", "template" |
| **Image base** [Image, Execution] | The materialised local VM on a host from which clones are made (`mfb-<digest12>`); never started, never deleted except by an explicit eviction of an unreferenced digest (I05). | "base image", "template", "golden" |
| **Image revision** [Image] | The maintainer's version label of an image (I14's "image generation"). | "image generation" (collides with slot generation) |
| **Clone** (verb) [Execution] | Create a VM from an image base (APFS copy-on-write: fast). Never from an OCI reference. | "pull" (a pull is an image placement effect) |
| **Placement** [Image] | The presence of an image on a host: pulled, verified and materialised as an image base. | "cache entry" (that is the DP's observation) |
| **Attempt** [Attempt] | One try to serve at most one job: one VM name, one `(host, slot, generation)`, at most one registration. A retry of an effect stays inside the attempt; a new try is a new attempt. | "runner", "lease", "job", "instance" |
| **Slot** [Fleet] | A numbered admission position on a host for one guest-OS class, held by at most one lease. | "capacity" |
| **Slot generation** [Fleet] | Counter per slot, bumped at each reservation; part of the VM name and of the ownership proof. Retries keep it. | "generation" alone, "epoch" |
| **Lease** [Fleet] | Host's record that an attempt holds a slot and a resource vector, from `SlotReserved` to `SlotReleased`. States: reserved, activated, quarantined, released. | "VM lease aggregate" (not an aggregate) |
| **Physical / admissible / deliverable / reported max** [Fleet, Demand] | Physical: what the data plane reports the host can run (ADR 0024). Admissible: physical minus reserves, external consumption, closed terms. Deliverable: runners we can produce for a scale set (admissible carve-out incl. live leases). Reported max: the `maxCapacity` we send, ≤ deliverable (I07, I24). | bare "capacity" |
| **External consumption** [Execution, Fleet] | Resources used by things we did not admit (a foreign macOS guest holding one of the two tokens, foreign memory). Counted by the data plane's resource ledger and carried in its capacity report; shrinks admissible. | — |
| **Resource ledger / capacity report** [Execution] | The data plane's in-memory accounting of a host's resources per dimension (total, used by ours and foreign, free), rebuilt from observation; it refuses directives that do not fit and reports capacity (`CapacityReported`) to the control plane (ADR 0024). | "capacity" alone |
| **Prepared** [Attempt] | Booted, guest verified, unregistered, no secret, no workflow code ever ran. | "warm" (as a state), "ready" |
| **Warm** [Fleet] | The pool policy of holding Prepared attempts ahead of demand (`min_prepared`). Never a state name. | "warm registered" |
| **Activation** [Attempt, Fleet] | The step that gives a prepared VM a secret: Host grant, fresh guest verification, registration, launch. | "start" |
| **Listening** [Attempt] | Runner launched with a registration, no job bound yet (GitHub's *idle*). | "warm", "ready" |
| **Busy** [Attempt] | A job is bound to the attempt's runner (observed from GitHub or the launcher). | — |
| **Idle** | Only GitHub's `TotalIdleRunners` (connected, no job). Never used for an unregistered VM. | — |
| **Guest identity** [Attempt, Execution] | The proof, by a per-attempt challenge through a channel addressed by VM name, that the answering guest is *this* clone. Belongs to an attempt, never to an image or IP. | "host key matched" |
| **Running / guest ready** | Running: the provider says the VM runs. Guest ready: we verified the guest's facts. Never "ready" alone. | "booted" for both |
| **Shim** [Execution] | The per-VM `mayfly vm-shim` process that owns one `tart run` as its child, holds its lock and writes `exit.json` (OD1, ADR 0037). | "wrapper" |
| **Process lost** [Execution] | A running VM whose shim is gone (killed, host trouble); controlled only via provider commands by name with proof. | — |
| **Inventory (complete / incomplete)** [Execution] | A full provider listing at time T, or a failed/unparseable one. Incomplete is never read as absence (I25). | "empty inventory" |

### 5.3 Ownership, fencing and modes

| Term | Definition | Rejected / not to be confused with |
|:--|:--|:--|
| **Owned** | A VM is owned iff the journal records `VmCloned` for an attempt AND the observed name equals the attempt's `OwnedVmName` AND the generation matches AND the observed provider instance id equals the recorded one. | "managed", "ours by prefix" |
| **Managed-looking** | An `mf-*` VM without ownership. Reported, never touched (I04). | "orphan" |
| **Foreign** | A VM whose name is not `mf-*`/`mfb-*`. Never touched. | — |
| **Orphan** | Always qualified: **leaked VM** (owned VM whose attempt is terminal but the VM persists: delete with proof), **leaked registration** (journaled registration with no live attempt: deregister), **unrecognised registration** (our name shape, no journal record: report). | bare "orphan" |
| **Ownership proof** | The claim the CP attaches to a destructive directive: `{attempt_id, host_id, run_epoch, slot, generation, vm_name, provider_instance_id, role: clone}`. The DP re-observes and refuses on any mismatch. Defence in depth, not a signature. | "token" |
| **Epoch** (run epoch) [Operations] | Monotonic integer minted for each CP↔DP session (every run and every DP reconnect), persisted and written to the external floor before use. Fences directives (I09) and automation commands (I18). | session, generation, version |
| **Epoch floor** [Operations] | The highest epoch ever minted, kept outside the journal (OD7). A journal whose epoch is below it was rolled back. | — |
| **Fence** | The check "carrier's epoch ≥ the highest seen" at an aggregate or at the DP. | "lock" |
| **Run** [Operations] | One life of the daemon on a host, from `RunStarted` to `RunEnded`; numbered. | "session", "epoch" |
| **Mode** | A host- or pool-wide condition derived from aggregate states (safe, draining, paused, tripped, admission closed, degraded). Shown by the `OperatingMode` read model with its cause chain. **State** belongs to one aggregate instance. | — |
| **Safe mode** [Operations] | Containment when the system cannot trust its own record of ownership or cannot write durably: no create/start/stop/delete executed, no secret minted, `reported_max = 0`, no acquire; observation, status and plan proposal continue. Left only through a completed recovery plan or a verified restart. | "degraded", "frozen" |
| **Degraded** | An external dependency is impaired while the journal is trusted (GitHub, provider, disk, keychain). Derived, per cause; self-heals. Nothing destructive is decided from *missing* external data. | "safe mode" |
| **Pause** [Fleet] | Pool terms closed for new admission; existing attempts continue, prepared stay prepared; `reported_max` for the pool's scale set reports only live runners (OD15). Reversible, no deadline. | "drain" |
| **Drain** [Fleet] | Host or pool terms closed *and* prepared/standby attempts retired; Busy finish. Completes when the scope has no non-terminal lease. Never kills a Busy VM. | attempt state "Draining" (removed), session "Draining" (now `Closing`) |
| **Force** | Operator-only, journaled, stops Busy VMs (jobs fail). Never automatic. | — |
| **Tripped** [Fleet] | Pool breaker open for an image digest after N stage failures in a window; reset only by an operator. | "paused" |
| **Quarantine** [Attempt, Operations] | An attempt held because automation cannot prove ownership or an outcome: VM kept (stopped unless evidence of a live job), slot leased, never reused, **never deleted automatically** (OD9, ADR 0025); resolved only through a **quarantine case** by an operator. Reasons are a closed enum (§6.10). | "retained", safe mode, pause |
| **Retire** [Attempt] | The decision that an attempt will serve no (further) job and cleanup begins. A decision, not an effect. A Busy attempt is never retired, only finished or forced. | "delete", "destroy" |
| **Withdraw** [Image] | Stop new use of an image; eviction follows when unreferenced. | "retire image" |
| **Delete / deleted** | Delete: the provider effect. `VmDeleted`: the observed absence. "Destroy" only in prose. | — |
| **Adopt** [Attempt] | After a restart, take responsibility for an owned VM under the new epoch without launching anything (`AttemptAdopted`). Never applied without ownership. | "import", "claim" |
| **Recovery** [Operations] | The automatic startup reconcile (classify, adopt, resolve). | — |
| **Repair** [Operations] | Operator-approved application of a recovery plan (safe mode, quarantine). The CLI verb `mayfly recover` keeps its name (never rename) and means repair; docs say so. | — |
| **Unknown (outcome)** | A directive was sent and whether it took effect is not known (the daemon was killed mid-call, a hung command was killed at its deadline, a reply was lost, the host slept). An overlay on the attempt's state, not a state; resolved only by re-observation, never by assertion (OD10, ADR 0025). | "unknown VM" (that is *managed-looking* or *foreign*) |
| **Provider limitation** [Operations] | The report that the provider cannot answer a re-observation (command, last error, age): the effect stays `Unknown` and its slot held. There is **no operator assertion** of an outcome (OD10, ADR 0025). | "assertion", "manual fix" |
| **Actor** | Who issued a command: `operator(uid)`, `automation(policy)`, `startup`. Recorded in every event's envelope. | — |
| **Secret / secret handle** [Secrets] | Secret: a value granting authority, never serialised except to its one destination. Handle: an opaque, random, single-take reference scoped to `(attempt_id, directive_id, epoch)` with an expiry, valid only in the issuing process; journaled by id. | — |

---

## 6. Aggregates

Aggregates here are **consistency boundaries and their rules**, not classes (ADR 0022): each is
implemented as plain data (state, commands, events) plus pure `decide`/`evolve` functions and
tables. "Anemic" data is the intended shape.

### 6.0 Rules common to every aggregate (fit for a stream-scoped, multi-writer store)

- **One command, one stream.** No command writes two streams; every cross-aggregate rule is a
  policy (§7) with eventual consistency. Commands never read projections; a policy may, and
  passes what it read *as a versioned input* the aggregate checks (e.g. pool terms revision).
- **Envelope.** Every command carries `command_id`, `actor`, `issued_at` (from the `Clock` port)
  and `run_epoch`. Receipts are per `(stream, command_id)`; an exact replay returns the recorded
  outcome. Observations use `observation_id` as `command_id`, so the inbox *is* the receipt.
- **Run-epoch fence (I18).** Each aggregate stores the highest `run_epoch` it has accepted and
  refuses older ones (`StaleRunRefused`, a rejection, not an event). A zombie daemon, a second
  instance on a copied state dir or a delayed reactor of the previous run cannot write. Operator
  commands carry the current epoch too.
- **No global order.** No aggregate, policy or projection depends on the relative order of two
  streams. Policies are durable per-stream consumers with checkpoints; external effects carry the
  idempotency key named in §7.
- **Size.** Long-lived streams (`Host`, `Pool`, `ScaleSet`, `HostRun`) grow with use; they rely on
  the store's snapshots and keep state small. Per-attempt streams are short (≈ 15–30 events).
- **Directives are events.** A directive's intent is a `DirectiveIssued{directive_id, kind,
  deadline}` event on the attempt or placement stream; the relay is a durable consumer of those
  events and stamps the current run epoch at send time. Whether the store materialises an outbox
  table is `doc/event-store.md`'s choice.

### 6.0.1 New invariants

I01–I16 keep their verification.md meaning, with one refinement: **I14** reads "image revision"
for "image generation". **I10 stays strict**: an ambiguous effect leaves `Unknown` only on an
observation; the proposed operator assertion was declined (OD10, ADR 0025).

| Id | Invariant | Protected by |
|:--|:--|:--|
| I17 | Run epochs strictly increase across restarts, DP reconnects and journal restores; the external floor is written before an epoch is used; a journal below the floor puts the host in safe mode. | `HostRun` + startup |
| I18 | An aggregate refuses a command whose `run_epoch` is lower than the highest it has accepted. | every aggregate |
| I19 | A registration is deregistered only by a journaled `runner_id`; a runner name is issued at most once, ever. | `RunnerRegistration` |
| I20 | Every issued registration reaches a terminal state or is on the reaper's list (bounded liveness). | `RunnerRegistration` + reaper |
| I21 | A registered-idle runner is deregistered before its VM is stopped; a busy refusal (409 on the Actions-service path, 422 on REST: `doc/github-scaleset.md` §7) cancels that retirement. | retirement policy P11 + `Attempt` |
| I22 | A JIT config is minted only after an activation grant and a guest verification of *this* attempt in the current epoch, and is delivered at most once; a delivery `Unknown` or an orphaned handle means presumed leaked and the attempt never serves a job. | `Attempt`, `RunnerRegistration`, broker |
| I23 | Acquisition stops when assigned ≥ deliverable: mayfly never holds jobs it cannot serve. | `ScaleSet` (AcquirePolicy) |
| I24 | Σ `reported_max` over the scale sets a host serves ≤ that host's deliverable (refines I07). | capacity policy + `ScaleSet` |
| I25 | An incomplete inventory is never read as the absence of any VM. | DP observer + inbox policy |
| I26 | No activation grant while host or pool terms are closed (drain, pause, trip, admission closed) (refines I08). | `Host` |
| I27 | Stage failures per (pool, digest) are bounded per window; at N the breaker trips and nothing is admitted for that digest until an operator resets it. | `Pool` + `Host` terms |
| I28 | A quarantined VM is never deleted automatically (no timer, no cap); its lease keeps counting against capacity until an operator resolves the case, and a pool whose capacity is consumed admits nothing (OD9, ADR 0025). | `Host` + `QuarantineCase` + DP refusal of unproven deletes |
| I29 | In safe mode no clone, start, stop, delete, pull or evict executes, and no secret is minted or taken. | relay, DP executor, broker (effect boundary) |
| I30 | Guest-originated data never decides ownership or a job's result and never reaches a VM name, path, argv or shell. | `Attempt` + guest ACL |
| I31 | A recovery plan applies only against the journal heads, inventory digest and epoch it was drawn from; each action is intent-then-outcome. | `RecoveryPlan` |
| I32 | A pool serves one trust tier; an offer is acquired only by a pool whose tier admits it (v0.x: `trusted` only, ADR 0027). | `ScaleSet` (classification) + `Pool` |
| I33 | The data plane never executes a create, start, pull or materialise directive that does not fit its own resource ledger (memory, disk, macOS-guest slots including foreign guests), whatever the control plane allocated (OD4, ADR 0024). | DP executor + resource ledger |

### 6.1 `ScaleSet` [Demand]

- **Stream:** `scaleset-<pool>` (one scale set per pool, OD14). Lifetime: the pool's; small state.
- **State:** `phase ∈ {Unconfigured, Ensured, Opening, Active, Backoff{cause}, Degraded{cause},
  Closing, Closed}`; `github_id`; `session_id` and a journaled `stale_session`; `cursor` (highest
  recorded message id) and the session's `floor` (the cursor at its open); `statistics` + the id
  they are ordered by; the capacity input (`deliverable`, `reported_max`, wire mode, revision);
  `desired`; reservations (`acquiring`: planned, answer not observed; `acquired`: no `JobStarted`
  or `JobCompleted` yet); `drift`; 409s since the last open; `tier`; highest `run_epoch`.
- **Commands:** `EnsureScaleSet`, `BeginOpen`, `RecordSessionOpened{session_id, statistics}`,
  `RecordSessionLoss{cause: expired|conflicted|failed|restarted}`, `RecordSessionDeleted
  {session_id}`, `RecordMessage{session_id, message_id, statistics, items}` (command id =
  `(session_id, message_id)`), `RecordAcquireResult{requested, acquired}`, `ApplyCapacity
  {revision, deliverable, reported_max, mode}`, `RecordSessionRecovered`, `StartClosing`. `PlanAcquire{offers}` is a pure
  function (AcquirePolicy + trust classification) the listener calls *before* `AcquireJobs`.
  *As built (KER-06):* `RecordMessage` runs `PlanAcquire` itself and journals the plan before
  the call (`doc/github-scaleset.md` §6, §9), so a redelivered offer is never planned twice; the
  answer is the observation `RecordAcquireResult{requested, acquired}`; `BeginOpen`,
  `RecordSessionDeleted{session_id}` (only a journaled session) and `RecordSessionRecovered`
  complete the session FSM; `ApplyCapacity` carries the reported max and the pool's wire mode
  (ADR 0029). Unknown types degrade the session at `PROTOCOL_DRIFT_THRESHOLD`. The assumptions on
  facts not yet server-observed (A1–A6) are listed in `formal/scaleset.qnt`,
  `doc/github-scaleset.md` §12 and `mayfly_control_domain::demand`.
  *As built (KER-06G, GH-00R's model):* on github.com, where jobs are assigned with no offer,
  `RecordMessage` keeps each `JobAssigned` that names `(jobId, workflowRunId)` pending (a new
  assignment replaces an earlier classification), and `RecordRunLookup{job, lookup}` classifies
  it through `demand::classify_assigned` against the pool's policy (`ApplyTrustPolicy`):
  `AssignedJobTrusted` or `AssignedJobRefused{reason}` (a failed lookup refuses); a runner starts
  only for a job classified trusted (`demand::runner_may_start`); the in-guest hook of every
  runner fails an untrusted job it receives (ADR 0090, the hard boundary); a refused job holds
  nothing and goes to a refusal sink (`StartSink`, the wire max pinned at the assigned count
  until `RecordSinkEnded`) or, with `cancel_refused_runs`, its run is cancelled
  (`RecordCancelAnswer`, once); `ScaleSetState::refusal_action` names the next. A session's
  first message below its floor records `MessageIdsRestarted` and restarts the cursor; a
  receipt counts as a duplicate only for the cursor's id (`ScaleSetState::is_duplicate`), any
  other id at or below it degrades; a 404 for the journaled scale set is
  `RecordScaleSetVanished{id}` (back to `Unconfigured`), then `RecordScaleSetRecreated{id}`.
- **Events** (as built, KER-06 and its review fix KER-06F): `ScaleSetEpochAdvanced`,
  `ScaleSetEnsured`, `SessionOpenRequested`, `SessionOpened{session_id, statistics, desired}`,
  `SessionLost{cause: expired|conflicted|failed|restarted, stale_session}`,
  `SessionConflictEscalated{conflicts}`, `SessionDeleted`, `MessageRecorded{session_id,
  message_id, origin: fresh|redelivered|id_regressed, statistics, statistics_applied: bool,
  released, acquire, declined{request_id, reason: no_capacity|tier|unclassifiable|held|
  already_reserved}, job_facts, unknown_types, desired}`, `AcquireObserved{acquired, released}`,
  `CapacityApplied{capacity, wire_max, desired}`, `SessionDegraded{cause: protocol_drift|
  message_ids_regressed}`, `SessionRecovered`, `ClosingStarted`, `SessionClosed`. The design's
  `SessionExpired`, `SessionConflicted` and `ScaleSetConflicted` are `SessionLost{cause}`, and
  `ReportedMaxChanged` is `CapacityApplied`. Job facts inside `MessageRecorded` are
  `JobAssigned{request_id, job?}`, `JobStarted{request_id, runner_id, runner_name, job?}` and
  `JobCompleted{request_id, result, runner_id, job?}` (`job`, KER-06G: the assignment's key and
  classifying fields, or the key a fact releases); they are facts for policies, and release a
  reservation or a classification, nothing more. KER-06G adds `MessageIdsRestarted{message_id,
  floor}`, `TrustPolicyApplied{policy}`, `AssignedJobTrusted{job}`, `AssignedJobRefused{job,
  reason: repository_not_allowed|event_not_allowed|fork|unclassifiable|lookup_failed}`,
  `SinkStarted{job}`, `SinkEnded`, `CancelAnswered{job, answer: accepted|refused}`,
  `ScaleSetVanished{github_id}` and `ScaleSetRecreated{github_id}`.
- **Protects alone:** I12 (record before ack: ack is issued by the listener only after this
  command's receipt), I13 for demand (a message applies once; statistics from an older message
  never replace newer: `statistics_applied = false`), I23 (`PlanAcquire` budget =
  `deliverable − assigned`, using the latest statistics plus `acquired_unbound`), I32 (offers whose
  tier ≠ pool tier are declined, never acquired), `desiredFromLatestStatistics`; a message from
  an earlier session applies nothing, and one A1 does not explain degrades the session; a
  degraded or closing session plans no offer (P21).
- **Must not protect:** that a slot exists for each desired runner (Fleet), that runners exist
  (Attempt), cross-scale-set sums (I24: capacity policy computes per-scale-set deliverable carve-
  outs and sends them with a revision), token validity (ACL).
- **Session open recomputes desired from the initial statistics without any message** (closes the
  acquire-before-record gap: a crash between `AcquireJobs` and `RecordMessage` is healed by the next
  session's statistics).

### 6.2 `Pool` [Fleet]

- **Stream:** `pool-<name>`. Lifetime: config lifetime; a removed pool is `Decommissioned`, never
  deleted (attempts reference it).
- **State:** `revision`; `tier`; `image_pin: ImageDigest` (+ `draining_digests`); limits
  (`max_running`, `min_prepared`); `mode ∈ {Active, Paused, Draining, Drained,
  Decommissioned}`; `breaker: map digest → {failures in window, Tripped{at}|Closed}`;
  `terms_revision` (bumped by every change that affects admission).
- **Commands:** `ConfigurePool{revision, …}`, `PinImage{digest}` (only an `Image` the policy saw
  `ImageVerified`, passed as input), `PausePool`, `ResumePool`, `StartPoolDrain`,
  `CompletePoolDrain`, `CountAttemptFailure{attempt_id, digest, stage, at}` (command id =
  attempt's event id: idempotent), `ResetBreaker{digest, actor}`, `Decommission`.
- **Events:** `PoolConfigured`, `ImagePinned{digest, previous}`, `PoolPaused`, `PoolResumed`,
  `PoolDrainStarted`, `PoolDrainCompleted`, `AttemptFailureCounted`, `PoolBreakerTripped{digest,
  failures}`, `PoolBreakerReset{digest, actor}`, `PoolDecommissioned`.
- **Protects alone:** I27 (counting and tripping), "the pin is a verified digest", "breaker reset
  needs an operator", tier immutability per revision.
- **Must not protect:** running counts and resource limits (Host), demand (Demand). A pool change
  reaches admission only through `PoolTermsApplied` on Host (P4).
- **As built (KER-05):** `ConfigurePool` also carries the pool's guest OS and per-VM resources
  (the capacity policy's shape of a pool's VM); `min_prepared` absent means 0 (ADR 0028);
  `PinImage` carries the image's `ImageVerified` event as `verified_ref` and is refused without
  it; a breaker counts one failure per attempt, only while closed, inside `breaker_window`;
  `Decommission` needs `Drained`. The pool's terms (open = `Active`, a pin, and that pin's
  breaker closed) are `pool::terms`, which P4 sends as `ApplyPoolTerms`. An accepted command
  under a newer run epoch records `PoolEpochAdvanced` first, which is how `evolve` learns the
  fence (I18). `draining_digests` is not kept: the reconciler derives it from the attempts.

### 6.3 `Host` [Fleet]

- **Stream:** `host-<host_id>`. Lifetime: the host's; snapshot-backed.
- **Authority (OD4, ADR 0024):** the data plane accounts for and enforces the host's resources
  and reports them; `Host` *allocates* within the last report. Its leases are allocation
  bookkeeping, not the source of truth: when they disagree the data plane's refusal wins.
- **State:** slot table per guest-OS class with `generation` per slot; leases
  `attempt_id → {slot, generation, pool, os, resources, phase ∈ {Reserved, Activated, Quarantined
  {holds}}}`; `capacity: {report_id, totals, used, free, foreign}` (the last applied
  `CapacityReported`, including external consumption); operator reserves;
  `pool_terms: pool → {revision, open, max_running, tier}`;
  `gate: {drain: Option<Drain>, closed_causes: set{disk_low, tart_unsupported, woke, recovering,
  inventory_incomplete}}`; highest `run_epoch`.
- **Commands:** `RegisterHost{reserves}`, `RecordCapacityReport{report_id, capacity}` (an older
  `report_id` is ignored), `ApplyPoolTerms{pool, revision, …}` (refused if
  `revision ≤` stored), `ReserveSlot{attempt_id, pool, os, resources, terms_revision}`,
  `GrantActivation{attempt_id}`, `QuarantineLease{attempt_id, vm_running: bool}`,
  `ReleaseSlot{attempt_id, evidence: VmDeleted|AttemptLost}`, `RecordResourceThreshold{disk_low|
  disk_ok|…}`, `CloseAdmission{cause}`,
  `OpenAdmission{cause}`, `StartHostDrain{deadline}`, `CancelHostDrain`, `CompleteHostDrain`,
  `ForceDrain{scope, actor}` (operator only).
- **Events:** `HostRegistered`, `CapacityReportApplied{report_id}`, `PoolTermsApplied`, `SlotReserved{slot, generation, attempt_id,
  pool, os, resources}`, `LeaseActivated`, `LeaseQuarantined{holds}`, `SlotReleased`,
  `DiskLowWaterCrossed`, `DiskRecovered`, `AdmissionClosed{cause}`,
  `AdmissionOpened{cause}`, `HostDrainStarted`, `HostDrainCancelled`, `HostDrainCompleted`, `DrainForced{scope, actor}`,
  `ForeignVmReported{name_escaped, class}` (first sighting per run).
- **Protects alone:** I01, I02, I03 at allocation (every lease counts on every dimension from
  `SlotReserved` to `SlotReleased` against the reported free capacity; a stopped quarantined lease
  shrinks to disk + slot), G3's two macOS tokens minus the foreign guests the report shows (the
  data plane enforces the same limits again: I33), per-pool `max_running` against accepted terms, generations strictly
  monotonic per slot, I26 (no grant with the gate closed or the pool's terms closed), I28, I08 (no
  reservation and no grant after drain start; drain completes only with zero leases in scope).
- **Must not protect:** that an attempt exists for a lease (P2 starts it; a lease whose attempt
  never started is released by the reconciler after `AttemptStarted` fails to appear: the attempt id
  is minted by the reconciler and used in both streams, so the check is a lookup), job demand,
  image validity, epoch minting (HostRun), safe mode (effect boundary).
- **Why the gate is here and not on HostRun:** I08/I26 must be decided in the same stream as the
  grant; mode changes are rare, so sharing the stream costs nothing.
- **As built (KER-05):** a capacity report carries `total`, `foreign`, `base` (ours outside any
  VM: image bases, pulls), `vm_slots`, `foreign_vms`, `macos_guests` and
  `foreign_macos_guests`; `Host` admits within `total − foreign − base − reserves` minus every
  live lease, on each dimension the new VM uses, with G3's two macOS guests applied whatever
  the report says (`capacity::admissible`, `capacity::fit`). Slots are one table per host (a
  slot number is unique across guest-OS classes; macOS is a limit over it). `ReserveSlot`
  carries `terms_revision` and is refused unless it equals the applied one; a pool at
  `max_running` refuses with `quarantine_held` when quarantined leases are among them.
  `ReleaseSlot{evidence}` takes `vm_deleted`, `attempt_lost` or `attempt_never_started`, the
  last only for a merely reserved lease. `QuarantineLease{vm_running: false}` shrinks the
  lease to disk and slots. `ForceDrain` needs the host draining (scope host) or the pool's
  terms closed (scope pool). `HostEpochAdvanced` records the run-epoch fence as for `Pool`.
  `ForeignVmReported` is left to the data-plane observer (KER-08, CP-03): the report already
  carries foreign consumption, which is all admission reads.

### 6.3.1 The capacity carve: properties and policy (KER-05D, ADR 0069)

`capacity::plan` carves a host's free capacity `F` (memory, disk, VM slots, macOS-guest slots)
into per-pool carve-outs `c_i`, which become each scale set's `reported_max` (`maxCapacity`).
Pools are served in name order, the **priority** order: pool `i` has a VM footprint `v_i`, a
headroom `h_i` (`max_running` minus live and quarantined leases), a room `ρ_i` (`max_running`
minus live runners: its headroom and its quarantined places) and `o_i`, what its quarantined
leases fall short of one VM each (a stopped one owes its memory, a running one nothing). `⊖` is
subtraction floored at zero on every dimension; `∨` the larger on every dimension.

**Properties.**

- **S (safety, I24, I07):** `Σ c_i·v_i ≤ F` on every dimension and `c_i ≤ h_i`; a reservation
  never exceeds its pool's carve-out and the `Host` admits it (`carveOutAdmissible`), within the
  data plane's report (I03), which the data plane enforces again (I33). Pinned by
  `tests/capacity.rs::should_fit_every_carve_out_together_when_inputs_are_random`,
  `should_admit_within_every_reported_limit_when_inputs_are_random`.
- **M1 (monotone in free capacity, pool by pool):** less free capacity on any dimension (more
  foreign use, base, reserves) never carves **any pool** more, so never more in total. Pinned
  by `should_carve_no_pool_more_when_foreign_consumption_grows_and_inputs_are_random`,
  `should_never_admit_more_when_foreign_consumption_grows`,
  `should_carve_no_more_when_foreign_use_leaves_a_large_vm_unfit`; model `carveMonotone`.
- **M2 (release monotone):** releasing a quarantined lease, or its VM stopping, never carves any
  pool less. Pinned by
  `should_carve_no_pool_less_when_a_quarantined_lease_is_released_and_inputs_are_random`,
  `should_carve_no_less_when_a_quarantine_of_an_earlier_pool_is_released`; model
  `carveMonotone`.
- **U (usefulness):** (U1) *no starvation*: a pool with a waiting job, headroom and a VM the
  host can hold is admitted once the later pools' VMs in its room finish, however busy the
  later pools stay (model `sustainedLoadTest`); (U2) *nothing kept that M1 and M2 do not
  need*: within S, M1, M2 and the priority order, every later pool is carved the most any such
  policy can carve it (the bound below).

**Why pool by pool, not only in total.** Monotone in total lets a later pool gain exactly when
an earlier one loses, that is, take room the earlier pool cannot use right now. Each plan round
turns that into leases, so the earlier pool's room is eaten one small VM at a time and its large
VM never starts: `ENVELOPE` below is monotone in total and starves the macOS job for ever under
sustained Linux load. Pool by pool, with the priority order, the later pool stays out of the
earlier pool's room, which is what U1 rests on; and a scale set's `maxCapacity` never rises
because the host got busier.

**What no policy can do.** (1) No policy is safe, monotone even in total, serves the earlier
pool first and hands a later pool every VM that fits what the earlier one left (work
conserving): KER-05H's counterexample (18 GiB free, a 15 GiB VM first, 2 GiB VMs second: one
plus one; 14 GiB: work conservation carves three) breaks any of them. (2) Any policy with S, M1
and the priority order carves a later pool `B` at most `fit_B(F ⊖ v_A·h_A)`: at `F' = F ∨
v_A·h_A ≥ F` the earlier pool `A` is carved `h_A`, so S leaves `B` at most `F' ⊖ v_A·h_A = F ⊖
v_A·h_A`, and M1 gives `c_B(F) ≤ c_B(F')`. M2 adds, per quarantined place, the part of a VM its lease
does not hold (its release gives `A` a whole VM of headroom but frees only what it held): `A`
keeps `v_A·h_A + min(v_A·(ρ_A − h_A), o_A)`. That is the policy
below (`EXACT`), so it carves every later pool the most S, M1, M2 and the order allow. The
owner's example (13 GiB free beside an idle macOS pool, Linux jobs waiting) is therefore out of
reach of every policy with these properties unless the macOS pool's `h_A` shrinks to its real
demand (`DEMAND`).

**Candidates** (`formal/scheduling.qnt`, section "KER-05D"; `carvePoliciesTest` checks each
over every free capacity of a small host, three VM kinds in both orders, with and without a
running or stopped quarantined lease):

| Policy | What the earlier pool keeps ahead of the later | S | M1 pool | M1 total | M2 |
|:--|:--|:--|:--|:--|:--|
| `ROOM` (KER-05H) | a whole VM per place in `ρ_A`, quarantined places included | yes | yes | yes | yes |
| `EXACT` (KER-05D) | `v_A·h_A` + what its quarantined leases owe, at most a VM per place | yes | yes | yes | yes |
| `LEFTOVER` (before KER-05H) | only what it was carved | yes | no | no | no |
| `ENVELOPE` | the leftover, clipped to the least total over every larger free capacity | yes | no | yes | no |
| `DEMAND` | `EXACT` with `h_A` cut to the waiting jobs it is told of | yes | yes¹ | yes¹ | yes¹ |
| `SMALL_FIRST` | `LEFTOVER`, the smaller VM served first when one is smaller on every dimension | yes | no | yes² | no |

¹ For a fixed number of waiting jobs; what that number can be is the question (below).
² On the checked host; with VMs of no size order it falls back to `LEFTOVER`.

**Usefulness** (`carveScenariosTest`: macOS VM 15 GiB / 60 GiB, Linux VM 4 GiB / 20 GiB, macOS
pool first; carve-outs as (macOS, Linux)):

| Scenario | `ROOM` | `EXACT` | `LEFTOVER` | `ENVELOPE` | `DEMAND` | `SMALL_FIRST` |
|:--|:--|:--|:--|:--|:--|:--|
| S1: 13 GiB free, no macOS job | (0, 0) | (0, 0) | (0, 3) | (0, 1) | (0, 3) | (0, 3) |
| S2: 13 GiB free, a macOS job waits | (0, 0) | (0, 0) | (0, 3) | (0, 1) | (0, 0) | (0, 3) |
| S3: KER-05H after +4 GiB foreign | (0, 0) | (0, 0) | (0, 3) | (0, 1) | (0, 0) | (0, 3) |
| S4: a running quarantined macOS VM, 49 GiB free | (1, 4) | (1, 8) | (1, 8) | (1, 8) | (1, 8) | (0, 9) |
| S5: a stopped quarantined macOS VM, 3 slots free | (1, 1) | (1, 2) | (1, 2) | (1, 2) | (1, 2) | (0, 3) |
| S6: `max_running` 2 macOS on 24 GiB | (1, 0) | (1, 0) | (1, 2) | (1, 1) | (1, 0) | (0, 6) |
| Sustained Linux load: round the macOS job starts | 4 | 4 | never | never | 4³ | never |

³ When it is told every waiting job; `never` when it sees only the jobs GitHub assigned under
the `maxCapacity` it was sent (`sustainedLoadTest`).

**Recommendation and what is built.** `EXACT` is built (KER-05D): it carves every pool at
least what `ROOM` did, in every state (pinned by
`should_carve_every_pool_at_least_the_whole_place_keep_when_inputs_are_random`), more whenever
an earlier pool has a quarantined lease (S4, S5: pinned by
`should_keep_only_what_a_quarantined_lease_does_not_hold_when_an_earlier_pool_has_one`,
`should_keep_the_memory_a_stopped_quarantined_lease_owes_when_slots_bind`), and keeps S, M1,
M2 and U1. A quarantined VM, kept until an operator resolves it (ADR 0025), now costs its pool
one VM's capacity, not two. Beyond it, two options were put to the owner in ADR 0069:

- **`DEMAND`** fixes S1 (Linux runs beside an idle macOS pool) and keeps every property, **if**
  the demand it reads does not depend on the `maxCapacity` it sends. Our demand is `desired =
  min(deliverable, totalAssignedJobs)` (plan §7.2), and whether `X-ScaleSetMaxCapacity` caps
  `totalAssignedJobs` was unverified (`doc/github-scaleset.md` §12 item 6 then; §5 now: public
  evidence says it caps it at 0, GH-00R). If it caps it, a pool
  carved nothing is never assigned its job, never keeps room for it and starves (the `never`
  of footnote 3). **Decided (owner, 2026-11-10, DEC-06): "implement both now".** Each pool's
  `capacity_policy` is `exact` (the default) or `demand`; a `demand` pool is carved, and keeps
  ahead of later pools, only for the VMs it waits for (`CapacityPolicy::Demand { waiting }`:
  its scale set's latest `totalAssignedJobs` beyond its live runners plus its `min_prepared`
  deficit, never capped by its own carve-out, which would feed back). The default stays `exact`
  until GH-00 answers item 6; since GH-00R (ADR 0069 amended 2026-11-11) `demand` is an
  explicit, experimental opt-in, because the evidence says such a pool starves. Per pool, not per host: the keep is what each earlier pool holds
  back, so the operator relaxes exactly the pool whose idle headroom starves the others, and
  each pool's keep still depends only on its own inputs, so the properties hold pool by pool
  for a fixed demand (model `carveMonotoneDemand`, `demandFixesThe13GiBCaseTest`,
  `demandStarvesWhenDemandIsInvisibleTest`, `demandLetsTheLaterPoolInTest`; Rust
  `tests/capacity.rs::should_fit_and_stay_monotone_when_pools_mix_policies_and_inputs_are_random`,
  `should_carve_no_pool_less_when_a_quarantine_is_released_and_pools_mix_policies`,
  `should_carve_later_pools_at_least_exact_when_earlier_pools_keep_only_for_demand`,
  `should_carve_as_exact_when_every_demand_covers_its_headroom`,
  `should_carve_a_later_pool_beside_an_idle_one_when_the_earlier_keeps_only_for_demand`).
- **Cap the keep at what the host can hold** (S6: a macOS `max_running` the host cannot run
  keeps room it can never use) would carve S6's Linux pool two VMs, but makes the keep depend
  on the report's totals: a host whose totals shrink would carve a later pool more. Refusing or
  explaining such a `max_running` at configuration is the safer remedy.

### 6.4 `Attempt` [Attempt]

- **Stream:** `attempt-<attempt_id>`. Lifetime: from `AttemptStarted` to `AttemptClosed`
  (after `SlotReleased` is observed by policy); ≈ 15–30 events.
- **State:** `vm_state ∈ {Reserved, Cloning, Booting, Verifying, Prepared, Activating, Listening,
  Busy, Finished, Retiring, Deleting, Deleted, Lost, Backoff{stage, n}, Quarantined{reason, vm}}`;
  `pending: Option<{directive_id, kind, outcome: InFlight|Unknown{since}}>` (the overlay);
  `vm_name`, `slot`, `generation`, `image_digest`, `pool`, `tier`, `instance_id`, `retry_budget`,
  `jit: {handle_id, phase ∈ None|Issued|Taken|Orphaned}`, `job_binding: Option<request_id>`,
  `retire_reason`, highest `run_epoch`.
- **Commands:** `StartAttempt` (from `SlotReserved`), `IssueNext` (saga), `RecordObservation{obs}`,
  `BeginActivation{grant_ref}`, `RecordJitHandle{handle_id, registration_ref}`, `BindJob`,
  `RequestRetirement{reason}`, `ConfirmRetirement` (after deregistration or when no registration;
  inside quarantine, the answer to the quarantine stop's deregistration, ADR 0076),
  `WithdrawRetirement` (busy), `Quarantine{reason}`, `Adopt{epoch, inventory_ref}`,
  `ResolveQuarantine{disposition}`, `Close`, and from CP-02 `RecordDirectiveSent{directive_id,
  deadline}` (the relay) and `RecordDirectiveTimeout{directive_id}` (the saga's timer). (No
  `AssertOutcome`: OD10, ADR 0025.)
- **Events:** `AttemptStarted`, `DirectiveIssued`, `DirectiveSent{deadline}` (CP-02: the relay
  put it on the link; only a sent directive's deadline runs), `DirectiveRefused{reason}`,
  `DirectiveOutcomeUnknown`, `DirectiveObservedNotApplied` (a complete inventory shows an
  `Unknown` effect did not happen: U1), `VmCloned{instance_id}`, `VmBooted`, `GuestVerified{facts, nonce_ref}`,
  `GuestRejected{reason}`, `AttemptPrepared`, `ActivationStarted`, `JitHandleIssued`,
  `JitHandleTaken`, `JitHandleOrphaned`, `RunnerLaunched`, `RunnerLaunchUnknown`, `JobBound`,
  `RunnerExited{code}`, `VmObservedStopped`, `AttemptBackedOff`, `RetirementRequested`,
  `RetirementWithdrawn`, `AttemptRetired{reason}`, `DiagnosticsExported{bytes, hash}`,
  `DiagnosticsLost{reason}`, `VmStopped`, `VmDeleted`, `AttemptLost{evidence}` (a quarantined
  VM the attempt never recorded, released by its absence),
  `VmVanished{discovered_by: inventory|stop_vm|start_vm, instance_id, vm}` (DEC-07),
  `AttemptQuarantined{reason, vm}`, `StopDeregistrationRequested`, `StopDeregistrationConfirmed`,
  `StopDeregistrationRefusedBusy` (ADR 0076), `AttemptAdopted{epoch}`,
  `StaleObservationIgnored{generation|epoch|directive}` (sampled), `AttemptClosed`. An operator's
  resolution of the quarantine case is `AttemptRetired{quarantine_resolved}`: cleanup then stops
  and deletes with proof (KER-04).
- **Behaviour decided by KER-04F (accepted as stated, owner may override):**
  - *Adoption with a launch in flight.* The broker is empty after a restart, so an issued handle
    is orphaned and the JIT presumed leaked even when its `LaunchRunner` is pending (the
    launcher may have taken it). The launch the data plane re-observes is still recorded
    (`RunnerLaunched`: the attempt is Listening, its runner real and registered), but that
    runner never gets a job (`BindJob` refused `jit_presumed_leaked`); P13 retires it after
    deregistration.
  - *Bounded cleanup.* Stops and deletes count against the attempt's retry budget like the
    stage directives. A stop or delete the data plane keeps refusing ends in
    `AttemptQuarantined{delete_refused_repeatedly}`; a refused proof quarantines
    (`provider_identity_mismatch`, also for `LaunchRunner`, ADR 0038). Inside quarantine,
    automation stops a running VM with proof unless a job is live, a bounded number of times
    (a refused proof counts as one more refusal), and never deletes it. A VM quarantined for
    `ownership_doubt` has no recorded instance id, so no proof: it is neither stopped nor
    deleted, and a complete inventory without it releases it (`AttemptLost`).
  - *A vanished VM* (ADR 0066, amended 2026-11-11 by DEC-07). A VM the attempt recorded
    cloning, which mayfly did not delete, found absent from a **complete** listing, whether a
    `StopVm` or `StartVm` is refused `target_absent` or the inventory does not list it, is a
    desync: the attempt records `VmVanished` and is lost and closed, quarantined or not, and
    never resurrected; the reconciler provisions a fresh attempt for the remaining demand, and
    the desync is surfaced (the `attempts` view's `desync`, `mayfly status`, the bundle) and
    counted by the breaker (P15). Quarantine is for a VM that is present and does not match
    the record; an incomplete listing concludes nothing (I25). A start refused `proof_mismatch`
    quarantines `provider_identity_mismatch`. (Until DEC-07 such an attempt was quarantined
    `vm_vanished_externally` and kept for an operator, and the inventory's absence was a silent
    `AttemptLost`.)
  - *Actor.* Who issued each command (operator uid, policy, startup) is recorded in the event
    envelope, not the payload; `ResolveQuarantine` is refused unless the actor is an operator,
    so an `AttemptRetired{quarantine_resolved}` event's envelope names that operator.
- **Behaviour decided by VER-01F** (the conformance replay's divergences; `doc/formal-models.md`,
  lifecycle):
  - *A retirement request outlives its phase.* Requested while activating or listening, it stays
    set when a fresh verification fails (`Backoff`) or the runner exits (`Finished`), and
    `ConfirmRetirement` (or `WithdrawRetirement`) is decided there too: the deregistration has
    happened, so retiring is safe (I21). A lost attempt is terminal and confirms nothing.
  - *An Unknown stop that a listing shows stopped* is `VmStopped`, the stop's success: the
    count of tries restarts for the delete.
  - *A vanished case ends with its resolution*: a later quarantine has its own reason. (Since
    DEC-07 a vanish closes the attempt, so no vanished case is resolved.)
- **Behaviour decided by KER-04G** (deregister before stop in Backoff, I21; `doc/formal-models.md`,
  lifecycle, `i21DeregisterBeforeStop`):
  - *Backoff holds the registration too.* A fresh verification refused after the JIT was issued
    (typically after a restart) leaves the attempt in `Backoff(activate)` with its registration
    held and a runner possibly registered in the VM (a launch answered or of unknown outcome).
    `RequestRetirement` there records `RetirementRequested` and waits for P11's deregistration,
    as in Activating and Listening; it no longer retires at once (which skipped the
    deregistration and issued the stop). A request while one waits is refused
    `already_retiring` in every phase, Finished and Backoff included.
  - *A spent activation budget requests the retirement.* With the registration held, the last
    refused fresh verification records `AttemptBackedOff` and `RetirementRequested{stage_failed}`
    instead of `AttemptRetired{stage_failed}`; the confirmation retires it with that reason.
  - *No retry while the retirement waits.* The saga issues nothing in a Backoff whose retirement
    waits; a busy answer (`WithdrawRetirement`) lets it retry the stage.
  - *Outside it:* an operator's resolution is not gated on the deregistration; a definitely
    refused launch and an exited runner leave no idle runner in the VM, so their retirement stops
    at once. A quarantine's automatic stop was outside it too until ADR 0076 (below).
- **Behaviour decided by the owner on 2026-11-10** (ADR 0076, DEC-06: "deregister"; I21 for the
  quarantine stop, `doc/formal-models.md`, lifecycle, `i21DeregisterBeforeStop`):
  - *The quarantine stop deregisters first.* Entering quarantine with the VM running, its
    instance id recorded, no job live and a runner of the registration that may be registered
    in it (a JIT was issued, the runner was not seen exiting, and no retirement passed P11
    before) records `StopDeregistrationRequested` after `AttemptQuarantined`. The automatic stop
    waits for P11, which serves it as a retirement (`retirement_step`, keyed by that event):
    `ConfirmRetirement` records `StopDeregistrationConfirmed` (removed or not found) and the stop
    follows; `WithdrawRetirement` records `StopDeregistrationRefusedBusy` (a job runs there) and
    the VM is not stopped until the runner is seen exiting. While GitHub cannot be reached
    nothing answers, so the VM keeps running; its quarantine holds its capacity anyway (I28).
    Neither confirmation retires the attempt: it stays quarantined, and only an operator
    resolves it.
- **Protects alone:** I06 and `busyNeverWarm` (no path from Listening/Busy/Finished back to
  Prepared; Retired never activates), I10 (`pending.Unknown` leaves only on an observation or an
  nothing else: no timeout, no assertion), I13 (retries reuse name and generation; at most one in-flight directive per
  target), I14 (one job binding, one digest), I22's attempt half (activation needs a grant ref and
  a fresh `GuestVerified` in the current epoch; a launch `Unknown` or orphaned handle forbids any
  further launch), I30 (guest facts are typed claims; ownership fields are set only from
  CP-generated values and DP inventory), `retryKeepsNameAndGeneration`, "a delete directive carries
  a proof built from this stream's `VmCloned`" (I04/G4 at the CP).
- **Must not protect:** capacity (Host), demand or which job it gets (GitHub binds the job;
  `JobBound` records it), the registration's existence at GitHub (Registration), the case
  workflow (QuarantineCase), image validity (checked at reservation by policy).
- **Lifecycle (replaces plan §6):**

```text
Reserved → Cloning → Booting → Verifying → Prepared
Prepared → Activating → Listening → Busy → Finished
Prepared|Activating|Listening|Backoff --RetirementRequested--> (registration held: wait deregistration) → Retiring
Finished|stage failure (budget spent)|Retiring → (diagnostics) → Deleting → Deleted
Cloning|Booting|Verifying|Activating → Backoff{n} → same stage (same name, generation)
any state ↔ pending.Unknown overlay (no transition until observed)
Verifying|Activating --GuestRejected--> Quarantined;  Deleting --proof refused--> Quarantined
any non-terminal --recorded VM absent without our delete (VmVanished, a desync)--> Lost → Closed
Quarantined --case resolved--> Deleting;  Quarantined --observed absence--> Lost
```

### 6.5 `RunnerRegistration` [Registration]

- **Stream:** `registration-<attempt_id>` (at most one registration per attempt, I22; a lost JIT
  means a new attempt). Lifetime: may outlive the attempt until terminal.
- **State:** `phase ∈ {Requested, IssueUnknown, Issued, Delivered, Connected, Consumed, Completed
  {result}, DeregistrationRequested, Deregistered, Absent, PresumedLeaked}`; `runner_name`,
  `runner_id`, `scale_set`, `request_id`.
- **Commands:** `RequestRegistration{attempt_id, runner_name}` (refused while the scale set is
  Degraded: passed as input), `RecordIssued{runner_id}`, `RecordIssueUnknown`,
  `RecordDelivered`, `RecordConsumed{request_id}`, `RecordCompleted{result}`,
  `PresumeLeaked{reason}`, `RequestDeregistration{reason}`, `RecordDeregistration{204|404|busy}`,
  `RecordFoundByName{runner_id}|RecordAbsent` (reaper).
- **Events:** `RegistrationRequested`, `RegistrationIssued`, `RegistrationIssueUnknown`,
  `RegistrationDelivered`, `RegistrationConnected`, `RegistrationConsumed`,
  `RegistrationCompleted{result}`, `RegistrationPresumedLeaked{reason}`,
  `DeregistrationRequested`, `RegistrationDeregistered`, `DeregistrationRefusedBusy`,
  `RegistrationAbsent`.
- **Protects alone:** I19, I20 (with the reaper), "a JIT config is issued once per registration",
  "busy refusal is final for that retirement" (I21's registration half).
- **Must not protect:** stop/delete ordering of the VM (P11 sequences Attempt after this stream),
  unrecognised registrations at GitHub (no stream; `UnrecognisedRegistrations` read model, report
  only, never deleted).
- **As built (KER-09, `formal/registration.qnt`):** `PresumedLeaked` and
  `DeregistrationRequested` are overlays on the phase (`leaked`, `deregistration`), not phases: a
  leaked runner may still connect and take a job, and a removal may be pending in any phase that
  holds a runner id. `RecordConnected{runner_id}` is the online confirmation of ADR 0038 (only
  the issued id confirms; not seen by the deadline → `PresumeLeaked{not_seen_online}`).
  `RecordFoundByName` records `RegistrationFoundByName` (the id journaled, presumed leaked:
  mayfly never held that config); `RecordAbsent` also takes a definite refusal of the mint.
  `RequestDeregistration` names its reason (`retirement{event}`, `presumed_leaked`, `not_live`);
  a busy answer (`DeregistrationRefusedBusy`) is final for the retirement it served, even after
  the job ends (a counterexample of the model's first draft), and a later retirement asks again.
  P11's choice is the pure `registration::retirement_step` (deregister, confirm, withdraw, wait):
  confirm only when no runner of the registration can be registered with its config in the VM
  (nothing issued, an unknown issue, absent, deregistered). The run epoch is fenced by
  `RegistrationEpochAdvanced` (I18). A busy removal is named busy, never by its code. A busy
  answer cancels only the retirement its request served (a stale request of retirement A answered
  busy cancels A, not a later B). `RecordDelivered` is refused once the registration is presumed
  leaked (`presumed_leaked`, I22). Limitation: `RecordAbsent` after an unknown issue is terminal
  and trusts one listing by name, so a runner GitHub lists only later is on no reaper list through
  this stream (P14 still reports it as unjournaled, never deleting it); CP-05 takes a second
  listing after a grace period before recording it.
- **As built (CP-05, `mayfly_control::registration`):** the reaper records `RecordAbsent` after
  an unknown issue only on a second empty listing by name at least `listing_grace` (10 min)
  after the first, which the model states as `absentOnlyWhenNoRunner` with the grace no shorter
  than GitHub's listing lag; the first listing is kept in memory, so a restart only lists again.
  A failed mint is `RecordAbsent` only for a definite refusal (throttled past the retries,
  refused, unusable credentials, the scale set gone) and `RecordIssueUnknown` otherwise, a taken
  name included. A minted config the attempt never got a handle for (a crash between the two) is
  `PresumeLeaked{handle_orphaned}`. The unrecognised runners P14 reports are its last sweep's
  report (`RegistrationManager::last_sweep`), not a projection; P12 is left to GH-04.

### 6.6 `Image` [Image]

- **Stream:** `image-<digest>`. Lifetime: until withdrawn and evicted everywhere.
- **State:** manifest (guest OS, runner version, image revision, launcher hash, required host),
  `status ∈ {Registered, Verified, Rejected, Withdrawn}`, `runner_version_expired_at`.
- **Commands:** `RegisterImage{manifest}`, `RecordVerification{ok|reason}`,
  `RecordRunnerVersionExpired{minimum}`, `WithdrawImage{actor}`.
- **Events:** `ImageRegistered`, `ImageVerified`, `ImageRejected{reason}`,
  `ImageRunnerVersionExpired{minimum}`, `ImageWithdrawn`. (Canary events are deferred to OPS:
  `ImageCanaryPassed/Failed` when the canary lane exists.)
- **Protects alone:** digest immutability, status monotonic (no Verified after Rejected/Withdrawn),
  manifest schema. **Must not protect:** "not withdrawn while pinned" (policy P17 refuses
  `WithdrawImage` via a pinned-by projection check and unpins first), placement.
- **As built (KER-11):** the manifest is `{os, runner_version, revision}` (OPS-02 adds the launcher
  hash and the required host as optional fields); a runner version is `major.minor.patch` as three
  numbers, so it orders by number and names a base with no free text. The status order is
  `Registered` → `Verified` → `Rejected` → `Withdrawn`: a failure may reject a verified image (a
  drift), nothing moves back, and a rejected or withdrawn image refuses a passed verification
  (`already_rejected`, `withdrawn`). `WithdrawImage` is an operator's (`operator_only`) and records
  the uid; the expiry is recorded once, only below the minimum (`runner_not_expired`), with the
  command's instant. A newer run epoch is recorded as `ImageEpochAdvanced` (I18).
- **As built (OPS-02, ADR 0091):** the manifest recorded stays `{os, runner_version, revision}`:
  the runner's checksum (which joins the data plane's pin table, checked at the
  materialisation), the architecture (checked by `mayflyd` at start) and the launcher hash
  (recorded and format-checked; comparing it with the image's launcher is planned, no task yet)
  stay in mayfly's manifest file, since nothing in the control plane reads them.
  `RecordVerification{passed}` means the manifest passed `mayflyd`'s checks against its
  deployment; a mismatch refuses the start, never a permanent rejection. The content is
  verified at the pull: a drifted pull fails at the data plane (the placement `Failed`, the pool
  not admissible on that host); recording `ImageRejected` on a drift is P17's pending branch
  (planned, no task yet). The pin (`mayfly_control::images::pin_image`)
  is refused unless the image is `Verified`, names its `ImageVerified` event and is saved with a
  read of the image stream, so a withdrawal that commits first refuses it.

### 6.7 `ImagePlacement` [Image]

- **Stream:** `placement-<host_id>-<digest12>`. Lifetime: pull to eviction.
- **State:** `phase ∈ {Wanted, Pulling, Pulled, Materialised{base_name}, Evicting, Evicted, Failed
  {reason}}` + `pending` overlay like Attempt; `bytes`.
- **Events:** `PlacementRequested`, `DirectiveIssued{PullImage|MaterialiseImage|EvictImage}`,
  `DirectiveOutcomeUnknown`, `ImagePulled{bytes}`, `ImageMaterialised{base_name}`,
  `ImageDriftDetected{expected, observed}`, `PlacementFailed{reason}`, `EvictionRequested`,
  `ImageEvicted`.
- **Protects alone:** single in-flight pull per (host, digest), eviction only from `Materialised`
  with zero referencing leases (the reference count is passed in by policy and re-checked by the DP
  against its base set), I05 at the CP (the base is never the target of a VM directive).
- **As built (KER-11):** event types are unique in the registry, so the directive bookkeeping is
  `PlacementDirectiveIssued{PullImage|MaterialiseImage{runner}|EvictImage{runner}}`,
  `PlacementDirectiveSent{deadline}` (the relay put it on the link), `PlacementOutcomeUnknown`
  (only once sent and past the deadline: a directive held unsent did nothing, ADR 0025), `PlacementDirectiveNotApplied` (only from
  unknown, on a complete listing) and `PlacementDirectiveRefused{reason}`; a refused pull or first
  materialisation, or a drift, adds `PlacementFailed{}`, and a new `PlacementRequested` starts
  again (as after `Evicted`). `ImagePulled` carries no byte count: disk is the data plane's ledger
  (ADR 0024). A base is named `mfb-<digest12>-r<major>-<minor>-<patch>` from the digest and the
  runner version (`image::base_name`). The state keeps the **base pointer** (`base`) and the
  **previous base** for rollback (ADR 0030 decision 3, ADR 0039 decision 2):
  `RequestRefresh{runner}` (`BaseRefreshRequested`) materialises another base of the same digest,
  `ImageMaterialised` moves the pointer and keeps the base it left, `RollBackBase`
  (`BaseRolledBack{to, from}`) swaps them; at most two bases, so a refresh is refused while the
  previous is kept (`previous_base_held`). `RequestEviction{scope: previous|placement,
  references}` (`EvictionRequested`) needs `Materialised`, nothing pending and zero references
  (`base_referenced`), and evicts the previous base first. The digest's pin and its previous
  digest stay the `Pool`'s (`ImagePinned{digest, previous}`).
- **As built (CP-07):** P17's manager places every pool's pin on every host, issues each
  placement's due directive, records a sent directive past its deadline as unknown, and evicts a
  materialised base no pool pins and no live lease references; a failed placement is not
  requested again. A pool is admissible on a host only once its pin is materialised there: the
  reconciler's check (`reconcile::HostView::placed`), not `Host`'s. P18 trips the pin's breaker
  through `Pool`'s `TripBreaker{digest, reason: runner_version, at}` (`PoolBreakerTripped` v2
  names its reason). Withdrawal is refused while a pool pins the image, saved with a read of
  every pool stream.

### 6.8 `HostRun` [Operations]

- **Stream:** `hostrun-<host_id>`. Lifetime: the host's; one cycle per run.
- **State:** `run_no`, `boot_id`, `version`, `schema`, `phase ∈ {Starting, Recovering, Operating,
  ShuttingDown, Ended}`, `epoch`, `safe_mode: Option<{cause, since, plan_id?}>`, `last_clean_end`.
- **Commands:** `StartRun{boot_id, version}`, `MintEpoch{floor_read}`, `CompleteRecovery{counts}`,
  `EnterSafeMode{cause, actor}`, `LeaveSafeMode{plan_id, verification}`, `BeginShutdown`, `EndRun`.
- **Events:** `RunStarted`, `PreviousRunEndedUnclean`, `EpochMinted{epoch}`, `RecoveryCompleted`,
  `SafeModeEntered{cause, actor}`, `SafeModeLeft{plan_id}`, `ShutdownStarted`, `RunEnded{clean}`.
- **Protects alone:** I17 (epoch > max(stored, floor); the floor write precedes the event's use),
  "safe mode is left only by a completed plan or a verified restart", one run Operating at a time.
- **Must not protect:** enforcement of safe mode (the relay, the DP executor and the broker refuse
  effects when told the mode at `Welcome`/by the relay: I29), admission (Host gate, mirrored by P3).
- **As built (KER-10):** `MintEpoch{floor, verification}` mints the epoch and, in the same unit,
  records `SafeModeEntered` for a floor above the journal (`epoch_below_floor`) or a failed
  verification; `EpochMinted` carries `floor` and `verified`. `LeaveSafeMode{exit}` takes
  `plan_completed{plan_id, completed_ref}` (this run verified) or `verified_restart` (a later run
  that verified; never for `epoch_below_floor`, which needs the plan). `RunEnded{run_no}` is
  always clean; an unclean end is the next run's `PreviousRunEndedUnclean`. `StartRun` and
  `MintEpoch` carry no epoch of their own; every later command is fenced by the minted one.
  The plan binding of `safe_mode` (P20) is left to CP-04.
- **As built (KER-10F):** the plan exit is `plan_completed{plan_id, completed_ref, plan_epoch,
  verification}`: the plan of this run, a verification taken after it, and then a restart before
  `CompleteRecovery` (`restart_required`), as the model's `completePlan` takes the daemon down.
  KER-10G: the plan exit is accepted only while the run is `Recovering`, so safe mode entered
  while operating is left by a restart that recovers, never straight back to operating.
  `MintEpoch` carries `crash_loop` and records it as safe mode in the same unit, so a crash-looping
  run is never a verified restart. Once the run minted, a command under a newer epoch is refused
  (`epoch_not_minted`).

### 6.9 `RecoveryPlan` [Operations]

- **Stream:** `recovery-<plan_id>`, `plan_id` = hash of content. Lifetime: until completed,
  refused, expired or superseded.
- **State:** bindings `{stream heads digest, inventory digest, github snapshot digest | unreachable,
  epoch}`, `actions[]` (target name + instance id, attempt, kind ∈ {adopt, stop, delete_with_proof,
  release_slot, mark_lost (only on observed absence), rebuild_projection, ignore}; there is no
  `assert_outcome` action (OD10), evidence, irreversible
  flag), `do_not_touch[]`, `phase ∈ {Proposed, Approved{actor}, Applying{next}, Completed,
  Failed{partial}, Refused{reason}, Expired}`.
- **Events:** `RecoveryPlanProposed`, `RecoveryPlanApproved`, `RecoveryActionApplied{index,
  outcome}`, `RecoveryPlanCompleted`, `RecoveryPlanRefused{stale}`, `RecoveryPlanExpired`.
- **Protects alone:** I31, immutability once proposed, "no delete action without ownership evidence"
  (checked at propose and re-checked at apply), single approval, resumable application.
  "At most one plan Applying per host" is a policy rule (P20) using `HostRun.safe_mode.plan_id`.
- **Applies through ordinary commands** (`Attempt.Adopt`, `Attempt.ResolveQuarantine`, …), never by
  writing other streams' events.
- **As built (KER-10):** actions are the recovery table's safe-mode proposals only
  (`adopt`, `mark_lost`, `quarantine{reason}`, `keep`, `report`, `ignore`); `stop`,
  `delete_with_proof`, `release_slot` and `rebuild_projection` are not offered until a task
  needs them, so no plan action deletes. Each action is `RecoveryActionStarted{index}` (intent,
  only while bound) then `RecoveryActionApplied{index, outcome, heads_after}`, in order; a moved
  binding (journal heads, inventory, GitHub, epoch) records `RecoveryPlanRefused{stale}`; a
  failed action stops the plan resumably (`Failed`); only an unstarted plan expires. Fenced by
  `RecoveryPlanEpochAdvanced`.
- **As built (KER-10F):** the binding is per stream: `RecoveryPlanDrawn{bindings{heads: stream →
  revision, inventory, github, epoch}}` replaces `RecoveryPlanProposed` (still decoded; such a
  plan is refused when applied, its digest cannot separate the plan's own writes). Every action's
  attempt stream is a bound head; `ProposePlan` is refused unless `bindings.epoch` is the
  command's run epoch, and an intent or completion under another run epoch records
  `RecoveryPlanRefused{epoch}`. `RecoveryActionRecorded{index, outcome, write}` replaces
  `RecoveryActionApplied`: the write is the attempt unit's own (stream, revision before, after),
  the bound head advances only by it, and a write that started past the bound revision records
  `RecoveryPlanRefused{journal_heads}` with the outcome. KER-10G: the plan's own units take
  `Observed{heads: HeadsRead}`, built only by `Bindings::read_now` (the bound streams read now,
  appended with those current revisions as read conditions); only an action's attempt command
  is conditioned on the bound revisions (`Bindings::read_conditions`); `ApprovePlan` is refused
  under another run epoch (CP-04 card).

### 6.10 `QuarantineCase` [Operations]

- **Stream:** `quarantine-<attempt_id>`. Lifetime: until an operator resolves it; there is no
  retention deadline and no automatic deletion (OD9, ADR 0025).
- **State:** `reason` (closed enum: `ownership_doubt`, `guest_identity_refused`,
  `jit_presumed_leaked`, `provider_identity_mismatch`, `delete_refused_repeatedly`,
  `redaction_violation`, `forensic_retention`, `outcome_unprovable`, `vm_vanished_externally`:
  ADR 0066, no longer opened since DEC-07, which closes a vanished VM's attempt instead), `holds {disk, memory?, macos_token?}`, `evidence`
  refs, `opened_at`, `escalation_level`, `phase ∈ {Open, Inspected, Resolving, Resolved{disposition,
  actor}}`; dispositions (operator only) `delete_with_proof` (the data plane re-checks the proof),
  `released_observed_absent` (only on a complete inventory without the VM), `keep`.
- **Events:** `QuarantineCaseOpened`, `QuarantineCaseInspected{actor}`,
  `QuarantineCaseEscalated{level}`, `QuarantineCaseResolved{disposition, actor}`.
- **Protects alone:** "resolution needs an operator actor", "no disposition is applied by a timer
  or a cap" (I28), closed reason set. Escalation (`QuarantineCaseEscalated`) is loud and never acts
  on the VM. **Must not protect:** capacity accounting (Host), freeing the slot (Attempt → Host only on observed absence).
- **As built (KER-10):** no `Resolving` phase (no event marks it); `ResolveCase{disposition,
  absence}` is operator-only, `delete_with_proof` needs the case's recorded instance id and
  `released_observed_absent` the observation id of the complete inventory without the VM. No
  case opens for `vm_vanished_externally` (ADR 0066 as amended); a resolved case may be followed
  by a new one on the same stream. Fenced by `QuarantineEpochAdvanced`.
- **As built (KER-10F):** `QuarantineCaseResolved` v2 records `instance_id` for
  `delete_with_proof` (v1 upcasts with none); the `DeleteVm` must carry it. A case refuses another
  attempt (`wrong_attempt`) and, while open, another reason (`already_open`). The aggregate
  trusts the named observation of a release; CP-06 verifies it against the stored observation.

---

## 7. Policies and process managers

Each policy is a durable consumer of the streams named in "Whenever", restartable from its
per-stream checkpoints, never replayed against external systems without its idempotency key.
"Backoff" is exponential with full jitter through the `Rng` port unless stated. GitHub calls go
through one budget per endpoint family (REST, Actions service, queue) honouring `Retry-After`
literally (F25).

| Id | Whenever | Then | Owner | Idempotency key | Retry / backoff | Timeout | Failure path |
|:--|:--|:--|:--|:--|:--|:--|:--|
| P1 | daemon start | lock → open store → verify (floor, event registry, structure) → `StartRun` → `MintEpoch` → handshake → inventory → resolve `Unknown` by observation → classify (recovery table) → `CompleteRecovery` | OPS startup | `run_no` | none (one pass; restart reruns) | recovery budget T_rec | journal doubt → `SafeModeEntered`; budget exceeded → stays `Recovering`, admission closed, alert |
| P2 | `SlotReserved` | `StartAttempt{attempt_id}`, saved with a read of the host stream at the revision showing the lease (a release committed first refuses it; CP-03F) | FLT→ATT | `attempt_id` (minted by the reconciler, used in both streams) | backoff | T_start | no `AttemptStarted` in T_start and no directive ever issued → `ReleaseSlot{evidence: never_started}`, saved by the reconciler with a read of the attempt stream expecting no stream, so an attempt that started after the pass's read keeps its lease (CP-03F) |
| P3 | `RecoveryCompleted`, `SafeModeEntered/Left`, unsupported Tart, `HostWoke`, incomplete inventory streak | `CloseAdmission{cause}` / `OpenAdmission{cause}` on Host | OPS→FLT | source `event_id` | backoff | — | retried until applied; admission stays closed meanwhile (safe direction) |
| P4 | any `Pool` event that bumps `terms_revision`, a host registered | `ApplyPoolTerms{revision}` on every host serving the pool | FLT terms policy | `(pool, revision)`, per host | level-triggered; re-run next pass (kicks, ≤ 1/min) | — | older revision refused by Host, newer wins; saved with a read of the pool stream at the revision read, so superseded terms are never applied: no gap can open admission. As built (CP-08): a pass like P5, not a durable consumer, so a host registered later and a lost kick are covered; applies nothing in safe mode |
| P5 | `MessageRecorded`, `SessionOpened`, capacity input change, `SlotReleased`, timer (≤ 1/min) | compute `CapacityReport` (pure); `ApplyCapacity` per scale set (I24 carve-out); `ReserveSlot` for shortfall and `min_prepared`; `RequestRetirement` for surplus prepared/standby, drained scope, old digests | FLT reconciler | `command_id = hash(input stream versions, ordinal)` | level-triggered; re-run next pass | — | Host refusal = nothing admitted (reason to `AdmissionExplain`); a racing pass over-reserves at most within capacity and the surplus is retired next pass |
| P6 | any `Attempt` event that completes a stage | `IssueNext`: the stage's directive (`CloneVm`, `StartVm`, `VerifyGuest{nonce}`, `ExportDiagnostics`, `StopVm`, `DeleteVm{proof}`) | ATT saga | `directive_id = (attempt_id, stage, try)` | stage retry budget, same name and generation | per-stage deadline per guest OS | budget spent → retire (stage failure, P15) or quarantine (identity, proof) |
| P7 | `DirectiveIssued` (attempt, placement) | send over the link, stamped with the current epoch; in safe mode hold every create/destroy/pull/evict kind (I29) | relay | `directive_id` | at-least-once, backoff | directive deadline | deadline → `DirectiveOutcomeUnknown` + `ReportInventory`; never a new directive for the same target while `Unknown` |
| P8 | `Observation` from the DP | route to the attempt/placement stream: `RecordObservation`; inventory: per owned VM, delta only; unrecognised `mf-*`/foreign → `ForeignVmReported` (first sighting per run) | CP inbox | `observation_id` | — | — | stale epoch/generation → `StaleObservationIgnored` (sampled), still audited when it reports a consumed JIT; incomplete inventory → no change (I25) |
| P9 | `AttemptPrepared` ∧ shortfall > 0 for its pool | `GrantActivation` (Host) → `BeginActivation` (fresh `VerifyGuest` through `tart exec`) → on `GuestVerified` in Activating: `RequestRegistration` → mint JIT → `RecordIssued` → broker `issue` → `RecordJitHandle` + `LaunchRunner{handle}` (DP checks lock PID = shim child, then `tart exec -i` on stdin: ADR 0038) → GitHub shows the runner id online, else P13 | ATT activation manager | `attempt_id`; runner name = `vm_name` (re-mint impossible by I19) | grant: next pass; mint: once, never retried with the same name | JIT delivery deadline T_jit (≈ 2 min) | grant refused → stays Prepared; mint `Unknown` → P10; delivery `Unknown` / deadline → P13 |
| P10 | `RegistrationIssueUnknown` | list runners by `runner_name`; `RecordFoundByName{runner_id}` → `RequestDeregistration`, or `RecordAbsent`; attempt `RequestRetirement{jit_lost}` | REG reaper | `runner_name` | GitHub budget | — | GitHub unreachable → stays on reaper list (I20), alert after T_reap |
| P11 | `RetirementRequested` on an attempt holding a registration | `RequestDeregistration` → 204/404 → `ConfirmRetirement`; busy → `WithdrawRetirement` | ATT↔REG | `(attempt_id, retirement event_id)` | GitHub budget | T_dereg | GitHub down → retirement waits; never stop before the answer (I21); hysteresis H_idle before choosing a victim |
| P12 | `MessageRecorded.JobStarted` / `.JobCompleted` | find registration by runner name (projection) → `BindJob` + `RecordConsumed`; on completion `RecordCompleted{result}`, then `RequestRetirement{job_done}` once `RunnerExited` or grace G expires (then probe) | DEM→ATT/REG | `request_id` | — | grace G | unknown runner name → `UnrecognisedRegistrations`; a JobStarted for a registration already completed → audit, ignore |
| P13 | `RunnerLaunchUnknown`, `JitHandleOrphaned`, identity doubt after delivery, redaction violation | `PresumeLeaked` → `RequestDeregistration` (if journaled id); attempt `RequestRetirement{jit_presumed_leaked}` or `Quarantine` | REG/SEC | `attempt_id` | GitHub budget | — | deregistration impossible → stays on reaper list; attempt never relaunched (I22) |
| P14 | `RunStarted`, daily timer, `mayfly doctor`, `SessionOpened` after a long outage | list our scale set's runners; journaled + non-live → `RequestDeregistration`; our name shape, unjournaled → report only | REG reaper | `(run_no, day)` | GitHub budget, ≤ 1 listing per hour | — | report, never delete by name pattern |
| P15 | final `AttemptBackedOff`, stage-failure `AttemptRetired`, `GuestRejected`, `VmVanished` (DEC-07) | `CountAttemptFailure{digest, stage}` on Pool; trip at N in window W → P4 closes terms | FLT breaker | source `event_id` | backoff | — | — |
| P16 | `AttemptQuarantined` | `QuarantineLease` (Host), `QuarantineCaseOpened` (ERROR event, metric, status), `StopVm` unless a job is live; the lease keeps counting, so a pool whose capacity is consumed admits nothing (I28) | OPS | `attempt_id` | backoff | escalation at T_q1 (ticket), T_q2 (page) | every reason → escalate only; never delete, never on a timer or a cap (ADR 0025) |
| P17 | `ImagePinned` | `PlacementRequested` per host; on `ImageMaterialised` the pool becomes admissible there; prepared attempts on the old digest retired; old digest unreferenced → `EvictionRequested`; `WithdrawImage` refused while pinned | IMG placement manager | `(host, digest)` | backoff; single pull per host | pull deadline (large) | `PlacementFailed` → pool not admissible on that host, reason shown; drift → `ImageRejected` |
| P18 | `ImageRunnerVersionExpired` | trip the breaker for that digest with reason `runner_version`; alert the maintainer | IMG→FLT | `digest` | — | — | admission for the pool stops with a named reason, never silent queueing |
| P19 | `pending.Unknown` older than T_u1 / T_u2 | re-observe (`ReportInventory`, provider queries are idempotent reads); at T_u2 alert; a provider that cannot answer is reported as a provider limitation | ATT | `(attempt_id, directive_id, level)` | re-observation with backoff, for ever | — | never resolved except by an observation; no assertion (OD10, ADR 0025); the slot stays held |
| P20 | `SafeModeEntered` | propose a plan draft automatically (dry run); on approval apply actions as commands, intent then outcome; `RecoveryPlanCompleted` → re-verify → `LeaveSafeMode` | OPS repair | `plan_id`, action index | per action | plan expiry T_plan | stale bindings → `RecoveryPlanRefused`; partial → `Failed{partial}`, resumable; one plan Applying per host |
| P21 | queue 401, REST 401/403, 409, 429, 5xx streak, host wake | queue 401 → refresh session; REST 401/403 → refresh token once, then `SessionDegraded`; 409 → backoff under TTL; 429 → `Retry-After`; degraded → minting frozen, jobs continue | DEM session supervisor | `(session_id, cause)` | per endpoint family | session TTL | degraded is self-healing; never delete on missing GitHub data |
| P22 | DP host facts: `CapacityReported`, disk threshold, wake (clock gap), Tart version change, hypervisor limit | Host `RecordCapacityReport` / `RecordResourceThreshold` / `CloseAdmission`; on wake: treat deadlines as expired by jump, re-observe before acting, retire prepared VMs older than the gap, reopen sessions | FLT/OPS | observation id | — | — | — |
| P23 | `HostDrainStarted` / `PoolDrainStarted` | retire prepared/standby in scope; `ReportedMaxChanged{0}`; zero leases in scope → `CompleteHostDrain`/`CompletePoolDrain`; timeout → alert, never force | FLT | drain event id | — | drain deadline | `DrainForced` only by an operator command |
| P24 | `ShutdownStarted` (SIGTERM) | close admission, stop acquiring and acking, finish durable writes, leave VMs running (adoptable), exit before the service manager's deadline | OPS | `run_no` | — | `ExitTimeOut` − margin | missed deadline = crash path, recovered by P1 |
| P25 | stall of any supervised loop (listener, relay, reconciler, store writer) > T_wd | exit with the distinct watchdog code; the service manager restarts; next run records `PreviousRunEndedUnclean` | OPS watchdog | — | service manager throttle | T_wd | crash loop (K starts in W before `RecoveryCompleted`) → start straight into safe mode |

---

## 8. Read models (projections)

All are rebuildable per-stream projections (ES-I10 style: a projection row is keyed by its source
stream) or live queries over them; none is an input to a command (policies may read them).

| Read model | Answers | Fed by | Notes |
|:--|:--|:--|:--|
| `OperatingMode` | What mode is the host in, since when, why? | `HostRun`, `Host` gate, `Pool` mode/breaker, `ScaleSet` phase | effective mode = pure `operations::mode` over the flags; never stored as one enum |
| `AdmissionExplain` | **Why is nothing starting?** | `OperatingMode`, `CapacityReport` (last reconciler pass), `ScaleSet` statistics, `Pool` | one ordered cause chain: safe mode > recovering > drain > pause > breaker/runner version > quarantine cap > disk > memory > macOS tokens (incl. external) > image not placed > GitHub degraded > no demand |
| `CapacityBoard` | reported (by the data plane) / allocated / admissible / deliverable / reported max per host and scale set, per dimension | `Host` (applied capacity reports, leases), reconciler output | carries the min() winner of `desired` |
| `Attempts` + `AttemptTimeline` | state, stage durations, pending directive, evidence per attempt | `Attempt` | `mayfly vm inspect`; feeds `ProvisionTimeline` percentiles |
| `PendingEffects` | every in-flight and `Unknown` directive with age and the observation that would resolve it | `Attempt`, `ImagePlacement` | alert source for P19 |
| `Inventory` | each VM: owned (attempt, generation, instance id) / leaked / managed-looking / foreign / image base; size; age | `Attempt`, `ImagePlacement`, `ForeignVmReported`, latest DP inventory (live) | `mayfly vm list`; labels orphan classes |
| `Registrations` | registration ↔ attempt binding, phase, age; reaper list; unrecognised registrations | `RunnerRegistration`, P14 reports | `mayfly doctor` |
| `JobLedger` | per request id: offered, acquired, declined (reason), bound attempt, result, seconds queued | `ScaleSet.MessageRecorded` | a log for debugging and latency; never a demand input |
| `SessionStatus` | session phase, cursor, statistics + age, last poll age, credential health (expiries and last failures, never values), budgets per endpoint family | `ScaleSet`, ACL metrics | |
| `QuarantineBoard` | case, reason, holds, age, evidence, allowed resolutions, retention deadline | `QuarantineCase`, `Host` leases | `mayfly quarantine list` |
| `RecoveryPlanView` | plan actions, bindings, irreversible flags, do-not-touch list, progress | `RecoveryPlan` | `mayfly recover --dry-run` prints it |
| `ImageCatalog` | digest, manifest, status, pinned by, placed on, bytes, live clones | `Image`, `ImagePlacement`, `Pool`, `Attempt` | |
| `SecurityTimeline` | per attempt: tier, identity checks, JIT phases, handle events, ownership evidence, refusals | `Attempt`, `RunnerRegistration` | incident review |
| `AuditTrail` | every mutating operator command: actor, command id, args digest, result | event envelopes (`actor`) | operator socket also logs refusals |
| `FenceView` | current epoch, floor, refused stale directives/commands | `HostRun`, DP refusals (observations) | |

Metrics derive from the same projections (bounded labels): add `mayfly_mode{mode}`,
`mayfly_epoch`, `mayfly_unknown_effects{age_bucket}`, `mayfly_quarantined{pool,reason}`,
`mayfly_breaker_open{pool}`, `mayfly_registrations{phase}`, `mayfly_job_result_total{pool,result}`,
`mayfly_heartbeat_timestamp_seconds`, `mayfly_secret_handles_outstanding`.

---

## 9. External systems and anti-corruption layers

### 9.1 GitHub (`mayfly-adapter-github`, CP)

Three endpoints, four credentials, one budget per endpoint family. The protocol is specified, with
each fact's evidence strength, in `doc/github-scaleset.md` (GH-00R, 2026-11-11); what no public
source settles is marked **[open: HW-02]** there and recorded in HW-02's session (GH-00).

| Credential | Obtained from | Used for | Lifetime | On failure |
|:--|:--|:--|:--|:--|
| 1. **App JWT** (RS256, from the private key in the secret store) | signed locally | exchanging for an installation token | ≤ 10 min, backdated `iat` | key unreadable/locked → `SessionDegraded{app_auth}` |
| 2. **Installation access token** | `POST /app/installations/{id}/access_tokens` | REST: runner list/get/delete, permission probe | ≈ 1 h | 401/403 → refresh once, then degraded (permission drift is an audit fact) |
| 3. **Actions-service token** (+ service URL, on github.com a host under `.actions.githubusercontent.com`) | the scale-set registration handshake | scale set CRUD, JIT generation, session create/delete | its `exp` claim (lifetime [open: HW-02]) | refresh by re-handshake; degraded on repeat |
| 4. **Message-queue access token** | `SessionOpened` | poll, ack, acquire (GHES) | about 1 h (server-observed); 401 → refresh session | `SessionExpired` (a refresh answering `RunnerScaleSetSessionExpiredException`) → new session, initial statistics |

Secret *artefacts* that are not mayfly's credentials but flow through the ACL: the **JIT config**
(per runner) and registry pull credentials
(Image). All are `Secret<T>`; none is journaled; fixtures are scrubbed with a canary test.

What crosses in: messages (`JobAvailable/Assigned/Started/Completed`, statistics), runner
records, workflow runs (the per-job lookup of ADR 0090), status codes,
`Retry-After`/rate headers. Errors are told apart by `typeName`, never by status. Translation:
wire messages → `Offer` (GHES), `AssignedJob` (github.com: repository, owner, event, labels,
keyed by job and run, never by `runnerRequestId`, which is 0 there), `Statistics`,
`JobStarted/JobCompleted` facts with sanitised, length-bounded strings (repo, ref, labels are
untrusted data, never interpolated; a missing classifying field makes a job unclassifiable); unknown message types → counted, acked after the
record, `SessionDegraded{protocol_drift}` past a threshold (never safe mode: R7). The ACL is the
only code that knows wire shapes; the domain sees neither JSON nor HTTP.

### 9.2 VM provider: Tart (`mayfly-adapter-tart`, DP)

- **What crosses:** argv (`clone`, `set`, `run --no-graphics`, `ip`, `stop`, `delete`, `list
  --format json`, `pull`, `exec`, `--version`, `get`); bounded stdout/stderr; JSON inventory.
- **Translation:** `tart list` → `InventoryObserved{complete}` or `InventoryIncomplete` (any
  parse or exit failure; I25); exit codes → `Definite`/`Ambiguous`; "2 macOS guests running"
  refusal → `HypervisorLimitHit`; version string → capability set checked against fixture
  versions (unsupported → admission closed, R12).
- **Process model when the daemon dies (OD1, decided: ADR 0037):** each VM runs under a per-VM
  shim, `mayfly vm-shim`, started with `setsid`; the shim runs `tart run` as its child with stdio
  to bounded files (never a pipe: SIGPIPE would kill the VM), holds its own lock and writes
  `exit.json` atomically when the child exits. A daemon crash or upgrade leaves VMs and shims
  running. On start the daemon reconciles the journal, `tart list`, each shim's lock and
  `exit.json`: a live shim's VM is adopted, a stopped one finalised from `exit.json`. VMs are
  stopped only by `tart stop` by name with proof, never by SIGTERM or SIGKILL.
- **Resource ledger (OD4, ADR 0024):** the host agent accounts for memory, disk, VM slots and the
  two macOS-guest slots (foreign guests included) from inventory and probes, refuses directives
  that do not fit (I33) and reports `CapacityReported`.
- **Never:** read TART_HOME internals to decide ownership; delete a VM whose source is OCI or whose
  name is an image base; trust an empty inventory; clone from an OCI reference (only from a local
  base, ADR 0039); send a signal to `tart run`. `TART_HOME` carries a deployment marker
  (`deployment_id`, epoch floor: OD7, ADR 0026), fsynced before an epoch is used; a mismatch fails
  `doctor` and closes admission. Every clone gets `tart set --random-mac`, read back as the
  provider instance id (OD6).

### 9.3 OCI registry (Image, via DP)

Pull by digest only, never by tag; content re-hashed after pull (`ImageDriftDetected` on mismatch);
registry credentials are a broker secret class; single-flight pull per host; `TART_NO_AUTO_PRUNE`
set, eviction explicit (P17).

### 9.4 Host OS (`mayfly-adapter-host`, DP)

Free disk (volume "important usage" capacity), memory pressure, boot id, OS version, clock gap
(monotonic vs wall: wake detection), power assertions. Translation: thresholds only become
facts (`DiskLowWaterCrossed`, `HostWoke`); raw readings are metrics with a freshness bound.

### 9.5 Guest (`mayfly-adapter-tart`'s guest transport, DP; trust T4; ADR 0082)

- **Out:** a per-attempt challenge nonce, then the JIT config over the launcher's stdin (once).
- **In:** typed, bounded claims (runner version, image revision, clock skew, free disk, launcher
  status, runner exit code), diagnostics as a 0600 artefact (hash and size journaled, never the
  content). Control sequences stripped before any terminal output.
- **Identity (OD5, decided: ADR 0038):** transport `tart exec` only (addressed by VM name through
  the hypervisor, no IP, no SSH); the nonce answer proves *this* clone; before the JIT is sent with
  `tart exec -i <vm> <fixed launcher>` on stdin, the DP checks that the lock PID is the shim's
  child and the instance id matches; afterwards GitHub must show the issued runner id online.
  `TART_HOME` is `0700` under a dedicated user; images without the guest agent are refused.

### 9.6 Service manager and operator

launchd (KeepAlive, ThrottleInterval, `ExitTimeOut`), distinct exit codes (lock refused, watchdog,
config invalid) so a refused second instance is not restarted forever. Operator: peer-credential
check on the socket (uid allowlist, default the service uid), command classes (read, pool control,
destructive, dangerous), mutating commands carry an actor and are journaled before they act.

---

## 10. Hot spots

Severity: **B** blocker before real-world use (phase 5), **H** high, **M** medium. Participants:
GD (GitHub demand), VF (VM fleet), SR (SRE/ops), SC (security).

### 10.1 OWNER decisions (ordered by impact)

Each has a recommended option (the working assumption until decided) and the alternatives. The
owner's answers of 2026-11-09 follow each item; the original text is kept as the record of what
was asked.

**OD1 (B) `tart run` process model on daemon death.** [VF HS6, SR H4, H19] Whether VMs survive a
daemon crash/upgrade decides adoption, upgrades and "never delete Busy".
*Recommend:* detached `tart run` (own session), control after restart via provider commands,
runner outcome via guest launcher status + GitHub (§9.2); HW-01 must confirm survival and `tart
stop` semantics. *Alternatives:* (a) a per-VM launchd job (heavy, needs plist management);
(c) VMs die with the daemon, upgrades require drain, F13 becomes "job lost, cleaned up".
**Decided (owner, 2026-11-09), ADR 0037:** a per-VM shim (`mayfly vm-shim`, containerd-style) owns each `tart run`; the daemon re-adopts on start from the journal, `tart list`, shim locks and `exit.json`; stop only through `tart stop`.

**OD2 (B) Upgrade and rollback rule; recovery from an untrusted journal.** [SR H1, H4, SC HS-8]
A forward-only journal plus "unknown event → safe mode" makes every binary rollback a safe-mode
incident, and safe mode has no repair path when the journal itself is the doubt.
*Recommend:* adopt the compatibility question for every change — *is an older build that ignores this
still correct?* — events evolve by optional fields or new types only; a new event type ships one
release *before* it is written (N knows and tolerates it, N+1 writes it); the store's
per-stream epoch/compatibility marker refuses a too-old build per stream instead of the whole
journal; a backup is taken before every migration; rollback beyond N−1 = restore backup, which the
epoch floor detects (I17) → safe mode → **salvage plan**: trust the verified prefix, classify VMs
created after it as *managed-looking with evidence*, and let the operator assert ownership per VM
in the plan (journaled `OutcomeAsserted`, never automatic). *Alternatives:* drain-before-upgrade
mandatory and no rollback; or inventory+GitHub-only rebuild (nothing deletable: hosts fill up).
**Decided (owner, 2026-11-09), ADR 0043 (the recommendation, with the owner's question that users may skip versions answered):** any forward jump of any number of versions is supported (every event readable for ever, every upcaster and migration kept; a CI upgrade matrix opens the journal of every released version and upgrades it straight to the current build); rollback one version is supported (prepare/activate, event-store §6); deeper rollback restores the automatic pre-migration backup (`mayfly restore <backup>`), which the epoch floor detects (I17); an older build meeting newer data refuses to write the affected streams and says why; a deep-rollback recovery quarantines the VMs it cannot prove (`ownership_doubt`), never asserted (OD10, ADR 0025), never deleted. The salvage plan therefore no longer asserts ownership.

**OD3 (B) Trust tiers for v0.x.** [SC HS-2, Q10; GD H5] Scale-set runners are interchangeable, so
trust can only be enforced per scale set, and offer data may not carry a fork flag [VERIFY].
*Recommend:* v0.x is **trusted-only**: every pool has `tier = trusted`; offers classified
untrusted or `pull_request_target`/`workflow_run`/`issue_comment`-triggered are declined
(`declined{reason: tier}`), and the docs require GitHub's own fork-approval settings; replace
`allow_untrusted_prs` by `tier` (the old key is refused with a message, never silently ignored).
*Alternatives:* untrusted tier with its own scale set, per-repo caps, no cross-tier co-residency
on a host (I32's second half) and the reachability tests of SEC-01 as a gate.
**Decided (owner, 2026-11-09), ADR 0027:** trusted-only, as recommended.

**OD4 (B) `maxCapacity` semantics and AcquirePolicy; capture before fake.** [GD H1, H11, H15]
*Recommend:* acquire only while `assigned < deliverable` (I23); send `reported_max =
deliverable` per scale set as the *total* concurrent runners, adjusted once the capture shows
whether the service reads it as total or increment; **GH-00 (owner) records a live session in a
disposable org before GH-01**, and the fake is built from that capture.
*Alternatives:* acquire every offer and rely on `maxCapacity` alone (parks jobs; F11 hole).
**Decided (owner, 2026-11-09), ADR 0024:** the acquire policy as recommended (GH-00 capture first), and in addition **the data plane owns host resource accounting and hard enforcement** (memory, disk, macOS-VM slots including foreign VMs; refuses directives that do not fit; reports capacity); the control plane allocates only within reported capacity (pools, admission order, `maxCapacity` to GitHub). §3, §5 and §6.3 follow.

**OD5 (B) Guest identity per attempt.** [SC HS-4, VF HS8] Per-image host keys identify the image,
not the clone; IP reuse can deliver a JIT to the wrong guest.
*Recommend:* `tart exec` + per-attempt nonce challenge, re-run immediately before the mint (I22);
images without `tart exec` are refused for v0.x. *Alternatives:* SSH with per-clone keys learned
at first boot; SSH with per-image key (rejected: confused deputy).
**Decided (owner, 2026-11-09), ADR 0038:** JIT delivery by `tart exec -i <exact VM> <fixed in-image launcher>` on stdin only; lock-PID = shim-child check before the exec; random MAC per clone; `TART_HOME` `0700` under a dedicated user; GitHub confirms the issued runner id came online; no SSH; images without the guest agent refused.

**OD6 (B) Ownership proof shape and provider instance id.** [SC HS-1, VF HS7, HS18]
*Recommend:* proof = §5.3; Tart's instance id = the VM's MAC address (Tart assigns a fresh one on
clone [VERIFY]) plus the observed creation time, recorded at `VmCloned`; the DP re-observes right
before `DeleteVm` and also refuses any name in its image-base set. *Alternatives:* name +
generation only (plan today; a re-created VM with our name is deletable); a marker file written
into the VM directory (touches provider internals).
**Decided (owner, 2026-11-09), ADR 0026:** instance id = random MAC per clone (`tart set --random-mac`, Tart keeps a clone's MAC otherwise), recorded at `VmCloned`, re-observed right before `DeleteVm`; image-base names refused.

**OD7 (H) External epoch floor.** [SC HS-8, SR H4] *Recommend:* adopt; the floor lives in the
`TART_HOME` deployment marker (not restored with the state dir, and it also makes "dedicated
TART_HOME" checkable), written with fsync before `EpochMinted` is used. *Alternatives:* Keychain
item (headless access issues), sidecar in the state dir (restored with it: useless against
restores), none (accept ghost-actor risk).
**Decided (owner, 2026-11-09), ADR 0026:** the floor file in `TART_HOME`, fsynced before the epoch is used; a journal epoch below the floor means safe mode.

**OD8 (H) Warm vs standby.** [GD H2, H12, Q2; VF HS17; SC Q9] *Recommend:* only **Prepared**
(unregistered) VMs ahead of demand (`min_prepared`, default 0, and 0 enforced for macOS unless
explicitly overridden: one prepared macOS VM halves the host); **standby** (registered-idle) is out
of v0.x (`min_standby = 0`, desired = `min(deliverable, assigned)`), so poisoned-idle exists only
for acquired-then-cancelled jobs, handled by P11. Prepared VMs expire after `max_prepared_age`
and on wake. *Alternatives:* standby for latency (needs P11 at scale and more GitHub budget).
**Decided (owner, 2026-11-09), ADR 0028:** as recommended: prepared, unregistered VMs only (`min_prepared`, default 0, 0 for macOS); no registered standby.

**OD9 (H) Quarantine cap, reasons and expiry.** [SR H6, SC HS-12, VF HS13] *Recommend:* closed
reason enum (§6.10); safe failures are *not* quarantined (delete refused once → retry with
proof; guest wedge with proof → force stop then delete); cap = 1 per pool for macOS, `max(1,
max_running/2)` for Linux, at the cap the pool trips (I28); ticket at 4 h, page at 24 h;
`forensic_retention` cases auto-delete with proof after 7 days, every other reason escalates only.
*Alternatives:* no cap (silent capacity erosion), auto-delete on expiry (unsafe for ownership doubt).
**Decided (owner, 2026-11-09), ADR 0025 (differs from the recommendation):** quarantine whenever ownership or an outcome cannot be proven; **never** delete a quarantined VM automatically (evidence preserved): no cap-driven deletion, no `forensic_retention` expiry; the capacity loss is accepted; escalation is loud; a pool stops admitting when its capacity is consumed. I28 is rewritten accordingly.

**OD10 (H) Unknown escalation by operator assertion (I10 amendment).** [SR H7] *Recommend:*
`mayfly attempt assert <id> --outcome absent|stopped|… --evidence <text>`, journaled as
`OutcomeAsserted{actor, evidence}`, flagged in every read model, allowed only after T_u2.
*Alternatives:* keep I10 strict (slots can be held forever with no exit but a recovery plan).
**Decided (owner, 2026-11-09), ADR 0025 (the recommendation is declined):** never assume. `Unknown` resolves only by re-observation (provider queries are idempotent reads); there is no operator assertion of outcomes; when a provider cannot answer, the effect stays `Unknown` and is reported as a provider limitation. I10 stays strict.

**OD11 (H) Registration lifecycle and reaper cadence.** [GD H3, SC HS-5] *Recommend:* §6.5 and
P10/P14: the reaper runs at run start, after long outages and daily, deletes only journaled ids,
reports unrecognised ones. Live capture answers how long an unclaimed JIT registration lingers
[VERIFY]. *Alternatives:* rely on GitHub's own cleanup (leaks inflate `TotalRegisteredRunners`).
**Decided (owner, 2026-11-09), ADR 0029:** as recommended.

**OD12 (H) Runner version shelf life.** [GD H6, VF HS9, SC HS-6] *Recommend:* keep the runner
baked and self-update off; record the runner version in the manifest; detect expiry
(`ImageRunnerVersionExpired` from the minimum GitHub publishes or a runner exit/diag signature
[VERIFY]) → breaker trips with reason `runner_version`; the image maintainer (owner) owns a
refresh SLA (recommend: rebuild monthly and on every runner release flagged as required).
*Alternatives:* allow runner self-update in the guest (a VM does work that is not the job; slower,
non-reproducible).
**Decided (owner, 2026-11-09), ADR 0030:** as recommended, plus convenient refresh tooling (`mayfly image refresh <pool>`, ADR 0039).

**OD13 (H) Image placement in v0.x.** [VF HS1, HS2, SC HS-6] *Recommend:* split pull from clone:
`ImagePlacement` with `PullImage/MaterialiseImage/EvictImage` directives; `CloneVm` clones only
from a local base. *Alternatives:* operator materialises bases by hand and CP only verifies
presence (smaller v0.x, keeps a manual step; acceptable as a first cut if DP-07 slips).
**Decided (owner, 2026-11-09), ADR 0039:** never pull at job time; clones only from local service-owned bases; `mayfly image refresh <pool>` (pull and record the digest, a base named by digest and runner version, boot once and install the pinned SHA-checked runner via `tart exec`, move the pool's base pointer in the journal keeping the previous one, GC unreferenced bases); `TART_NO_AUTO_PRUNE`; runner-age warning; optional daily runner-only refresh.

**OD14 (M) Scale set cardinality and scope.** [GD H5, H16, Q1] *Recommend:* one pool = one scale
set, strictly; org scope only in v0.x, repo scope second, enterprise later; scale sets are left in
place on decommission (deleting one fails queued workflows). *Alternatives:* several pools per scale
set (needs label routing GitHub does not do for JIT).
**Decided (owner, 2026-11-09), ADR 0040:** one scale set per pool; organisation scope (repository scope allowed for testing); never deleted automatically.

**OD15 (M) Wire behaviour of pause and safe mode.** [SR H2, Q7–Q9] *Recommend:* pause reports
`reported_max` = live runners of the pool (no new offers) and acquires nothing; safe mode keeps the
session open if the journal is writable, reports 0, acquires nothing and acks only recorded
messages; if the journal is not writable it stops polling (redelivery is safe). Drain reports 0.
*Alternatives:* close the session on pause/safe mode (expiry churn on resume).
**Decided (owner, 2026-11-09), ADR 0029:** as recommended.

**OD16 (M) launchd mode, sleep and unattended recovery.** [VF HS11, SR H9, H13] *Recommend:*
LaunchAgent in an auto-login session for v0.x (Keychain and Virtualization.framework work),
`pmset`/power assertion "never sleep" checked by `doctor` as FAIL; `HostWoke` handled anyway (P22);
ADR before OPS-01. *Alternatives:* LaunchDaemon (prototype first; Keychain and GUI-session risks).
**Decided (owner, 2026-11-09), ADR 0031:** both: `mayflyd` in the foreground by hand locally, launchd auto-start (LaunchAgent, auto-login session) on servers.

**OD17 (M) Wedged-daemon detection.** [SR H8, H17] *Recommend:* internal watchdog (P25) +
`mayfly_heartbeat_timestamp_seconds` and an external dead-man alert documented in the runbook; the
metrics endpoint is loopback HTTP (scrapable), not the CLI socket. *Alternatives:* KeepAlive only
(misses hangs).
**Decided (owner, 2026-11-09), ADR 0031:** internal watchdog plus launchd `KeepAlive`.

**OD18 (M) Classic mode scope.** [GD H14] *Recommend:* keep as a smoke-test adapter only (`min_ready
≤ 1`, trusted repos), behind the same `RunnerRegistration` aggregate; not a supported production
mode in v0.x. *Alternatives:* drop it (GH-05 removed); full support (doubles the state space).
**Decided (owner, 2026-11-09), ADR 0028:** no classic mode at all; scale sets only; GH-05 removed.

**OD19 (M) Softnet and the non-root service.** [VF HS16, SC HS-9] *Recommend:* no Softnet in
v0.x (trusted-only, OD3), NAT with the reachability matrix of SEC-01 recorded; revisit with the
untrusted tier. *Alternatives:* Softnet with a privileged helper (ADR, breaks "non-root").
**Decided (owner, 2026-11-09), ADR 0032 (differs from the recommendation):** support Softnet; its privilege model (Tart's softnet behind a narrow sudoers rule, or a privileged helper) is designed by NET-01 with its own ADR; the daemon stays non-root.

**OD20 (M) Operator authorization, key at rest, audit.** [SC HS-3, HS-10, Q3, Q7, Q8; SR H14]
*Recommend:* App key as a 0600 file in a 0700 dir (recorded risk acceptance), `RLIMIT_CORE=0`,
zeroize; socket peer-uid allowlist = service uid, CLI via `sudo -u` so `SUDO_USER` is the actor;
audit in the journal envelopes for v0.x, no sidecar/off-host shipping until phase 7; repo names and
logins in job facts are pseudonymisable fields (hash + display copy in a projection only).
*Alternatives:* Keychain under launchd; separate admin uid; hash-chained sidecar now.
**Decided in part (owner, 2026-11-09), ADR 0033:** secrets come from a `SecretProvider` port with backends chosen per secret by URI (`keychain:`, `file:` 0600, `env:`, `exec:`, extensible), Envoy-style; Keychain on a laptop, file or exec on a server; never persisted by mayfly. The other sub-items (peer-uid allowlist, actor via `sudo -u`, audit in envelopes, pseudonymisable fields) stay planner defaults.

### 10.2 Resolved by the facilitator (no owner decision needed; owner may override)

| Id | Hot spot (participants, severity) | Resolution |
|:--|:--|:--|
| R1 | Attempt conflates job and VM lifecycles (VF HS5, GD §6, SR §6) H | Not split (§3.1 item 4); job → `JobLedger` + `job_binding`; registration and quarantine split out; `pending` overlay |
| R2 | "Warm" has two meanings; "drain" four; "unknown" three; "orphan" three; "generation" two (all) H | §5: Prepared/Warm/Standby/Listening; drain as a mode, session `Closing`; Unknown = outcome overlay; qualified orphans; slot generation vs image revision |
| R3 | Demand formula mixes units (GD H12) H | desired = `min(deliverable, assigned + min_standby)`; shortfall subtracts Activating+Listening+Busy; C < A: wait, never acquire more (I23) |
| R4 | Empty poll carries no statistics (GD H7) M | reconcile on local state only; bound staleness to one long-poll; a session re-open refreshes statistics; reported in `SessionStatus` |
| R5 | Multiple scale sets promise the same slots (GD H5) H | per-scale-set deliverable carve-out by the capacity policy (I24) |
| R6 | Capacity is multi-dimensional with different lifetimes; foreign VMs steal macOS tokens; flat disk reserve (VF HS3, HS4, HS10) H | Host resource vector; external consumption facts; disk reserve = Σ ceilings of live leases + reserve; quarantined stopped lease shrinks to disk + slot |
| R7 | Unknown GitHub message type: safe mode is wrong (GD H10) M | ack after record, count, degrade past a threshold; never safe mode |
| R8 | Safe mode undefined on what keeps running (SR H2) H | §5.3 definition + I29 + OD15; observation, status, plan proposal, and completion handling of Busy jobs continue; no create/destroy |
| R9 | Pool circuit breaker missing; G2 bounded per attempt only (SR P7, SC) H | `Pool.breaker` (I27) |
| R10 | No host drain; shutdown vs drain conflated (SR H3) M | `HostDrainStarted` on Host; P23 vs P24 |
| R11 | Session conflict and acquire-before-record gap on restart (GD H9, H15) H | initial statistics authoritative (§6.1); 409 backoff; new fault F42 |
| R12 | Tart upgrade mid-run; empty inventory read as "all gone" (VF HS14, SR H16) H | I25; capability re-probe on version change; admission closed when unsupported |
| R13 | Recovery table gaps (VF HS19, SR H16) H | new rows in §11.3 (KER-07) |
| R14 | Secret on the wire form leaks into recorders (SC HS-7) H | two types: `LaunchRunner{handle}` (stored, recorded) vs a non-`Serialize` wire frame built by the link adapter; canary-secret test over every recorder |
| R15 | Diagnostics vs delete ordering (VF HS15, SR H18) M | bounded export, `DiagnosticsLost` and proceed; quarantine keeps the VM |
| R16 | Generation per attempt vs per slot (SC HS-13) M | slot generation (per slot, bumped per reservation); consistent with "new attempt, new generation" |
| R17 | Single-writer assumptions (owner decision b) H | §6.0: one command one stream, run-epoch fence on every aggregate (I18), durable per-stream consumers, no global order |
| R18 | Untrusted strings in names, argv, terminals (SC HS-11) M | names generated from ids only; argv `--` before names; escaped, bounded, control-stripped output |
| R19 | Name format redundancy (VF HS12) M | keep `mf-<pool>-<host>-s<slot>-g<gen>-<short>` for operators; `short` ≥ 64 random bits; codec is a hint, never ownership; image bases `mfb-<digest12>` |
| R20 | Config reload at runtime undefined (SR H12) M | pool settings reload as a new `PoolConfigured` revision; host/provider/scale-set scope need restart; a removed pool is `Decommissioned`, its attempts finish |

---

## 11. Consequences (for the orchestrator, after owner review)

**Applied by DEC-01 (2026-11-09)** to plan.md, tasks.md and verification.md together with the
owner's decisions of §10.1; where a decision differs from a row below (OD4, OD9, OD10, OD18,
OD19, OD1, OD5), the decision and the target document win, and the row is annotated.

### 11.1 plan.md

| Section | Change |
|:--|:--|
| §1.1 G3, G4 | G3: "…guest-OS limits (at most two running macOS guests per host, foreign ones included)…"; G4: add "and the recorded provider instance id; the data plane re-observes before deleting". |
| §1.3 | Replace `allow_untrusted_prs` by `tier` (OD3); add "domain model: doc/domain-model.md is upstream of this plan". |
| §2 | Add the authority "GitHub is the authority on runner registrations; the data plane is the last line of refusal". |
| §3.1 | Secrets row: single-take handle scoped to `(attempt, directive, epoch)`; DP failure stance: refuses on proof mismatch after re-observation and in safe mode. |
| §3.2 | Epoch = run epoch per CP↔DP session, ≥ external floor (I17); directive kinds add `VerifyGuest{nonce}`, `ExportDiagnostics`, `PullImage`, `MaterialiseImage`, `EvictImage`, `ReportHostFacts`; `CloneVm{base_name, vm_name}` (not a digest); `DeleteVm{proof}` with the §5.3 proof; observations add `GuestVerified`, `GuestRejected`, `RunnerLaunchUnknown`, `InventoryIncomplete`, `HypervisorLimitHit`, `HostFacts{disk, wake, tart_version, external_guests}`, `ImagePulled`, `ImageMaterialised`; `Welcome` carries the mode (safe/normal); stored vs wire form of `LaunchRunner` (R14). |
| §4 | No new crate; module layout of dm §4.1 noted under `mayfly-control-domain`, `mayfly-control`, `mayfly-data-domain`. |
| §5.1 | Stream table replaced by dm §6 (ten aggregates, stream ids); add the run-epoch fence and "one command, one stream"; persistence per `doc/event-store.md` (§5.2 shrinks to a pointer once that doc lands). |
| §5.3 | Compatibility rule of OD2 (prepare-then-write for new event types; backup before migration). |
| §5.4 | Commands and reactors renamed per dm §6/§7; reactors are durable per-stream consumers. |
| §6 | Replace the FSM and rules by dm §6.4 (Prepared, Verifying, Listening, Lost; `pending` overlay; drain is not a state; quarantine reasons). Add rule: activation requires a Host grant and a fresh guest verification (I22, I26). |
| §7.1 | Four credentials table (dm §9.1); memory hygiene. |
| §7.2 | Session states (`Closing` for draining); AcquirePolicy (I23); trust classification before acquire; desired/shortfall formulas (R3); initial statistics authoritative; unknown message types (R7); capture first (OD4). |
| §7.3 | Classic mode as smoke-test only (OD18). |
| new §7.5 | Runner registrations: lifecycle, deregister-before-stop, 422, reaper (dm §6.5, P10–P14). |
| §8.1 | `VmProvider` gains `instance_id` in `VmObservation`, `inventory()` returns `Complete|Incomplete`; `start` goes through the per-VM shim (OD1, ADR 0037); image operations on a separate `ImageStore` port (pull, materialise, evict, list bases). |
| §8.3 | Guest identity per attempt (OD5); runner shelf life (OD12); launcher status for exit codes after restart. |
| §8.4 | Ownership proof and instance id (OD6); image base names `mfb-*` and the DP base set; TART_HOME deployment marker. |
| §9 | Resource vector, external consumption, per-scale-set deliverable carve-out (I24), quarantine shrink, macOS `min_prepared = 0` default. |
| §10 | Startup sequence = P1 with the floor check; table rows of §11.3; safe mode = dm §5.3 + OD15; drain = P23, shutdown = P24, force separate; salvage plan (OD2). |
| §11 | CLI: `mayfly host drain|resume`, `mayfly pool reset-breaker`, `mayfly quarantine list|inspect|resolve`, ~~`mayfly attempt assert`~~ (dropped: OD10), `mayfly image pull|withdraw`, `mayfly safe-mode enter`; config: `tier`, `min_prepared`, `max_prepared_age`, `quarantine_cap`, breaker N/W. |
| §12 | Metrics of dm §8; heartbeat; loopback metrics endpoint (OD17). |
| §13 | Trust tiers, reachability matrix, residual risks (JIT visible to workflow code by design; hypervisor escape), operator authz (OD20). |

### 11.2 tasks.md

New tasks:

| ID | Title | Depends on | Model | Accept |
|:--|:--|:--|:--|:--|
| GH-00 | (owner) Capture a live scale-set session in a disposable org | — | sonnet | redacted traces of open, poll, acquire, JIT, job start/complete, cancel, 409, expiry; answers to every [VERIFY] in dm §9.1, §10 written down; GH-01 cites them |
| KER-09 | Registration model and `RunnerRegistration` aggregate | KER-03, FND-06, DDD-01 | opus | `formal/registration.qnt` (I19, I20, I21, I22's registration half; deregister-before-stop, 422 race, issue-unknown); aggregate with matrix, goldens, determinism |
| KER-10 | Operations aggregates: `HostRun`, `RecoveryPlan`, `QuarantineCase`, effective mode | KER-03, KER-07 | opus | I17, I31; mode precedence pure fn with exhaustive table; plan bindings refuse when stale |
| KER-11 | Image catalog: `Image`, `ImagePlacement` | KER-03, KER-02 | sonnet | status monotonic; single pull; eviction refused while referenced |
| CP-05 | Registration manager and reaper | KER-09, GH-03, CP-02 | opus | F06, F32 component tests; only journaled ids deleted; unrecognised reported |
| CP-06 | Breaker, quarantine and Unknown-escalation policies (no operator assertion: OD10) | KER-10, CP-03 | sonnet | F36, F46; P19 thresholds on fake clock; a quarantined VM is never deleted by a policy |
| CP-07 | Image placement manager | KER-11, DP-07 | sonnet | pin → placement on every host; withdraw refused while pinned |
| DP-07 | Image cache directives and `ImageStore` port (pull, materialise, evict, base set) | DP-04 | opus | contract suite on fake and fake-tart; base deletion refused by identity |
| OPS-05 | Watchdog, heartbeat metric, distinct exit codes | RT-02, RT-04 | sonnet | F40; lock-refused and watchdog codes distinct |

Changed tasks:

- **KER-01:** add `OwnershipProof`, `TrustTier`, `GuestOs`, `ImageRevision`, `SlotGeneration`
  (rename), image base name codec (`mfb-<digest12>`), `RunEpoch` semantics.
- **KER-02:** directives/observations of §11.1 §3.2; `Welcome{epoch, mode}`; stored vs wire
  `LaunchRunner`; link model adds "safe mode holds create/destroy kinds" (I29). Deps + KER-01.
- **KER-03:** command envelope with `run_epoch` and `actor`; the run-epoch fence (I18) as a kernel
  rule with determinism cases; "one command, one stream" as an architecture rule.
- **KER-04:** dm §6.4 lifecycle (Verifying, Prepared, Listening, Lost, `pending` overlay, no
  Draining); invariants add I22 (attempt half), I30; Accept adds "activation without a grant ref
  or fresh verification is refused".
- **KER-05:** `Host` = admission ledger with resource vector, pool terms, gate, leases (reserved/
  activated/quarantined), I24, I26, I27, I28; `Pool` with breaker; scheduling model adds
  `externalConsumptionShrinksAdmissible`, `noGrantWhileClosed`, `quarantineHoldsCapacity` (OD9: no cap that deletes); capacity from data-plane reports (OD4).
- **KER-06:** AcquirePolicy (I23), trust classification (I32), per-scale-set carve-out input,
  initial statistics authoritative, unknown message type; mutants add "acquire every offer".
- **KER-07:** rows of §11.3; safe-mode and salvage classification; I25; depends on KER-09.
- **KER-08:** supervisor gains `ProcessLost`; executor re-observes instance id before `DeleteVm`;
  DP base set; inventory `Incomplete`; safe-mode refusals.
- **CP-02:** saga per dm §6.4/P6; relay as durable consumer stamping the epoch (P7).
- **CP-03:** reconciler = P5 with carve-out, shortfall, prepared policy, hysteresis; simulation
  adds I23, I24, I26, I27.
- **CP-04:** startup = P1 with floor; `AdmissionOpened{recovery_done}`; F35, F42, F43.
- **DP-04:** per-VM shim (OD1, ADR 0037), resource ledger and refusals (OD4), `InventoryIncomplete`, `HypervisorLimitHit`, instance id.
- **DP-05:** `tart exec` + nonce challenge (OD5); lock-PID check, no SSH (ADR 0038); launcher status file.
- **GH-01:** depends on GH-00; fake built from the capture.
- **GH-04:** adds F41, F42, F44; acquire budget.
- **GH-05:** removed (OD18 decided: no classic mode, ADR 0028).
- **RT-03:** CLI additions of §11.1 §11; peer-cred check; actor in every mutating command.
- **OPS-02:** manifest fields (runner version, launcher hash), `image pull|withdraw` (with CP-07).

Removed: GH-05 (OD18). KER-04..08 must list KER-09/KER-10 where they consume their events.

### 11.3 Recovery table rows to add (KER-07)

| Journal | Provider | GitHub | Decision |
|:--|:--|:--|:--|
| any | inventory incomplete | any | wait; admission closed; nothing released (I25) |
| Deleted | owned clone present (instance id matches) | any | leaked VM: delete with proof |
| any | name present, instance id differs | any | quarantine `provider_identity_mismatch`; never delete |
| Prepared | running | registration none | adopt; re-verify before any activation |
| Listening | running | registration absent | retire (registration consumed elsewhere or expired) |
| Listening/Busy | running | unreachable | adopt and monitor; never delete; `unreachable` ≠ `absent` |
| Busy | stopped (host rebooted) | offline/abandoned | retire, delete with proof, reap registration |
| Activating | any | registration issued, not delivered | presume leaked; deregister; retire |
| Quarantined | any | any | keep; case continues |
| journal epoch < floor | any | any | safe mode, salvage plan |

### 11.4 verification.md

- **§6.1:** add I17–I32 (dm §6.0.1); I10 stays strict (OD10) and I14 reads (image revision). Model rows:
  lifecycle + I22 (attempt half), I30; scheduling + I24, I26, I27, I28; recovery + I17, I25, I29,
  I31; scaleset + I23, I32; link + I18 (fence), I29 (held kinds); **new model**
  `formal/registration.qnt` (KER-09): I19, I20, I21, I22 (registration half), model-local
  `noRemintAfterUnknown`, `deregisterBeforeStop`.
- **§8.4 new scenarios** (core marked ●):

| Id | Scenario | Nemesis | Expected | Invariants | Core |
|:--|:--|:--|:--|:--|:--|
| F32 | Crash after JIT issued, before handle issue | SIGKILL after `RegistrationIssued` | `JitHandleOrphaned` at restart; registration deregistered by id; new attempt, new name | I19, I20, I22 | ● |
| F33 | Cancel while runner registered-idle; job lands during deregistration | fake-github 422 on delete | retirement withdrawn; job served; no VM stopped before the answer | I06, I21 | ● |
| F34 | `tart list` unparseable / Tart upgraded mid-run | fake-tart garbage output, version change | `InventoryIncomplete`; no slot released; admission closed until re-probe | I25 | ● |
| F35 | Journal restored from an older backup | replace state dir with an older copy | epoch below floor → safe mode; no destructive action; salvage plan | I17, I29 | ● |
| F36 | Broken image: every guest fails to verify | fake-tart exec hangs for one digest | ≤ N attempts per window; breaker trips; pool terms closed; other pools unaffected | I27 | ● |
| F37 | Foreign macOS guest running | pre-seed a running foreign macOS VM | admissible macOS tokens = 1; a third start never attempted; `HypervisorLimitHit` handled if it occurs | I03 | |
| F38 | Host sleep/wake with many deadlines | clock jump during provisioning | no mass quarantine; re-observe; prepared VMs older than the gap retired; session reopened | I10, I09 | |
| F39 | Wrong guest answers (IP reuse / sibling clone) | fake-tart routes exec to another VM | challenge fails; no JIT minted; quarantine `guest_identity_refused` (a launch's lock, MAC or handshake check: `provider_identity_mismatch`, ADR 0082) | I22, I30 | ● |
| F40 | Daemon wedged (store writer stalls) | fault knob stalls the writer | watchdog exit with its code; restart recovers; nothing duplicated | I11, I13 | |
| F41 | 100 offers on 2 slots in one batch | none (load) | acquired ≤ deliverable at every point; others declined `no_capacity` | I23, I07 | ● |
| F42 | Restart within the session TTL | SIGKILL, fake-github keeps the old session | 409 → backoff; reopen; desired from initial statistics; no job lost or double-served | I12, I13 | ● |
| F43 | Zombie writer of an older run | replay a delayed command with run epoch e−1 | every aggregate refuses it; nothing changes | I18 | ● |
| F44 | Unknown message type | fake-github injects a new type | recorded and acked; degraded past threshold; never safe mode | I12 | |
| F45 | Recovery plan applied after the world moved | inventory changes between dry run and apply | `RecoveryPlanRefused{stale}`; no action executed | I31 | |
| F46 | Hostile workflow wedges its VM repeatedly | fake-tart stop/delete hang per attempt | quarantined VMs kept (never deleted); the pool's capacity consumed, it admits nothing; page; capacity of other pools intact | I28 | |

- Core gate becomes F01–F16, F19, F20, F24, F31, F32–F36, F39, F41–F43.
- **§9 mutants:** "acquire every offer", "mint before fresh verification", "stop before deregister",
  "release slot on incomplete inventory", "accept older run epoch".
