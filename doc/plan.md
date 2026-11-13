# mayfly: product and architecture plan

**Status:** plan, 2026-11-09; the owner's decisions on the domain model (ADRs 0024–0040) and on the
event store (ADRs 0021, 0034, 0035) applied by DEC-01. Owner: Patryk Żmigrodzki.
**What:** a headless Rust service that runs **single-use GitHub Actions runners in disposable
virtual machines** on Apple Silicon hosts. Each job gets a freshly cloned VM; the VM is destroyed
after one job. Demand comes from GitHub **Runner Scale Sets** (JIT runners); the VM layer is an
abstraction whose first provider is **Tart**.

Why the name: a mayfly lives for a day. So does every runner VM here: one job, then gone.

This document owns *what* and *how it is structured*. [`foundation.md`](foundation.md) owns the
engineering rules (workspace, enforcement, gate, commands, agent contract),
[`verification.md`](verification.md) owns the test levels, formal models and fault lab,
[`philosophy.md`](philosophy.md) says why and where the lines are,
[`domain-model.md`](domain-model.md) is upstream of this plan (contexts, language, aggregates,
invariants I01–I33, policies P1–P25: "dm §6.4"), [`event-store.md`](event-store.md) owns the
journal's design, and [`tasks.md`](tasks.md) owns order and size. A citation "plan §4.2" points
here.

---

## 1. Goals, non-goals, guardrails

### 1.1 Release goals

| ID | Goal |
|:--|:--|
| G1 | **No job-to-job state.** Every job runs in a VM cloned from a pinned, immutable image; the VM is destroyed or quarantined after it. A VM that ran workflow code never becomes Prepared again or serves a second job. |
| G2 | **No orphan storm.** After a daemon crash, host reboot, GitHub outage, VM crash or cleanup failure, the system converges to a known inventory and never starts unbounded replacements. |
| G3 | **No overcommit.** Admitted VMs never exceed configured slots, guest-OS limits (at most two running macOS guests per host, foreign ones included), memory headroom or disk reserve. The data plane accounts for and enforces these limits; the control plane allocates within what it reports (ADR 0024). |
| G4 | **No unsafe cleanup.** Only VMs with positive ownership evidence (journal record + exact name + generation + the recorded provider instance id) are deleted, and the data plane re-observes before deleting (ADR 0026). Never a base image, never a foreign VM, never by name prefix; a VM whose ownership or outcome cannot be proven is quarantined and kept, never deleted automatically (ADR 0025). |
| G5 | **No fictional success.** An effect whose outcome is unknown stays `Unknown` until externally observed. |
| G6 | **Headless and scriptable.** Configured by a TOML file, driven by `mayfly` CLI over a local socket; no GUI. |
| G7 | **Auditable.** Every decision is an event in a journal; every side effect has a durable intent and a reconciliation path; every failure mode has a deterministic test. |
| G8 | **Provider-agnostic.** Tart is one `VmProvider` adapter. The domain knows nothing about Tart, GitHub wire types, SQLite or Tokio. |

### 1.2 Non-goals (v0.x)

A GitHub Actions YAML parser or job executor (the official `actions/runner` does that); a
reimplementation of Virtualization.framework, Tart or Orchard; Kubernetes; a GUI; multi-host
scheduling before single-host recovery is proven; exactly-once job processing (GitHub may requeue;
we guarantee idempotent intent and at-least-once effects).

### 1.3 Guardrails

- **Licensing:** Tart and Orchard are FSL-1.1-ALv2. mayfly *executes* the `tart` binary; it never
  incorporates their source. Commercial hosting needs a legal review (owner).
- **macOS guests:** at most two macOS VMs per Apple-branded host; enforced by admission, not trust.
- **Untrusted code:** v0.x serves only the `trusted` tier: every pool declares `tier = "trusted"`;
  untrusted offers (forks, `pull_request_target`, `workflow_run`, `issue_comment`, unclassifiable)
  are declined before acquire; the old key `allow_untrusted_prs` is refused at load (ADR 0027).
- **No phoning home:** no telemetry, usage statistics, crash upload or update check, ever; bug
  evidence is a local, redacted `mayfly diagnostics bundle` the operator sends by hand (ADR 0036).
- **Public repository:** no secrets, internal hostnames, private-repo content or personal paths.

---

## 2. System context

```text
GitHub (Scale Set API, App auth, runner registry)
        ▲  outbound HTTPS only
        │
┌───────┴───────────────────────── mayflyd (one process in v0.x) ─────────────────────────┐
│  CONTROL PLANE                                    │  DATA PLANE                          │
│  decides: desired state, admission, lifecycle     │  does: VM clone/start/stop/delete,   │
│  owns: journal (SQLite), projections, GitHub      │  guest probe, runner launch,         │
│  session, secrets broker, operator API            │  inventory, resource ledger + caps   │
│                                                   │                                      │
│  commands → aggregates → DirectiveIssued ─ Directive ─▶ executor ─▶ VmProvider (Tart)    │
│  inbox ◀─────────────────────────────── Observation ─── observer ◀─ GuestTransport (exec)│
│                                                   │                    HostProbe         │
└───────────────────────────────────────────────────┴──────────────────────────────────────┘
        ▲ Unix socket (0600)                                       │ argv, never a shell
     mayfly CLI                                         disposable VM per job (Linux/macOS)
                                                        official actions/runner, JIT config
```

**Authorities.** GitHub is the authority on job assignment and runner registrations; the VM
provider is the authority on which VMs exist; the data plane is the authority on a host's free
resources (ADR 0024) and the last line of refusal for every effect; the control-plane journal is
the authority on **who may act on which VM**. They can disagree, so reconciliation is mandatory
and continuous.

---

## 3. Control plane / data plane split

The split exists from day one even though both planes run in one process: it is what makes the
later multi-host edition (one control plane, many data-plane hosts) a transport change rather than
a rewrite, and it gives each plane a narrow, testable contract.

### 3.1 Responsibilities

| | Control plane | Data plane |
|:--|:--|:--|
| Question it answers | *What should exist?* | *What exists, what fits, and make it so.* |
| State | Durable: event journal, receipts, inbox, delivery ledger, projections (SQLite) | Nothing durable but the epoch-floor marker in `TART_HOME` (a fence, ADR 0026), each VM's shim lock and `exit.json` (ADR 0037), and the registry of the image bases it made with their states and ownership proofs (a host fact for local delete and clone proofs, never journal state, ADR 0087). In memory: the in-flight table and the resource ledger, rebuilt from the provider's inventory and host probes on start |
| Talks to | GitHub, operator CLI, the data plane | Tart (or another provider), guests, the host OS |
| Decides | Desired capacity, admission and its order within reported capacity, which VM is retired, quarantine | Nothing about demand. Whether a directive fits the host's resources (refused if not: I33) and *how* to execute it safely (timeouts, process groups, retries within a directive's deadline) |
| Secrets | Resolves the GitHub App key through a `SecretProvider` (ADR 0033); mints JIT configs; holds them in a single-take broker scoped to `(attempt, directive, epoch)` | Receives a JIT config in memory for exactly one launch over `tart exec` stdin; never stores it |
| Failure stance | Safe mode on journal doubt; never acts on guessed ownership; never assumes an outcome | Fail closed: refuse a directive it cannot fence or validate, that does not fit, whose proof does not match after re-observation, or that safe mode holds |

### 3.2 The CP↔DP contract (`mayfly-protocol`)

Versioned, transport-agnostic, serde-encoded messages. Every message carries
`protocol_version`; a peer refuses an unsupported major version.

- **Handshake:** `Hello { host_id, boot_id, dp_version, capabilities, epoch_floor }` →
  `Welcome { epoch, mode, accepted_version }`. The control plane mints a new **run epoch** per
  data-plane session (every run and every reconnect), above both its journal and the floor; the
  data plane raises the floor in `TART_HOME` (fsync) before acknowledging and rejects any
  directive with an older epoch (**fencing**, I09, I17). `mode` is `normal` or `safe`.
- **Directive** (CP → DP): `{ directive_id, epoch, attempt_id, generation, deadline, kind }` where
  `kind` ∈ `PullImage{digest}`, `MaterialiseImage{digest, base_name, runner}`,
  `EvictImage{base_name}`, `CloneVm{base_name, vm_name}`, `StartVm{vm_name, run_options}`,
  `VerifyGuest{vm_name, nonce}`, `LaunchRunner{vm_name, launch: SecretHandle, instance_id}`,
  `ExportDiagnostics{vm_name}`, `StopVm{vm_name, grace, proof}`, `DeleteVm{vm_name,
  ownership_proof}`, `ReportInventory`, `ReportCapacity`, `ReportHostFacts`. Directives are
  **idempotent by `directive_id`** and by target name: a repeat returns the earlier outcome or
  re-observes. The stored form of `LaunchRunner` carries a handle; the wire frame that carries
  the secret is built by the link adapter and is not `Serialize`.
- **Observation** (DP → CP): `{ observation_id, epoch, directive_id?, observed_at_dp, kind }` where
  `kind` ∈ `VmCloned{instance_id}`, `VmStarted`, `GuestVerified{facts}`, `GuestRejected{reason}`,
  `RunnerLaunched`, `RunnerLaunchUnknown`, `RunnerExited{code}`, `VmStopped`, `VmDeleted`,
  `Inventory{vms}` | `InventoryIncomplete{reason}`, `CapacityReported{report_id, totals, used,
  free, foreign}`, `HostFacts{disk, wake, tart_version}`, `HypervisorLimitHit`, `ImagePulled`,
  `ImageMaterialised`, `DirectiveRefused{reason}` (definite, nothing happened in that epoch;
  reasons include `insufficient_resources`, `proof_mismatch`, `safe_mode`),
  `DirectiveOutcomeUnknown{reason}` (ambiguous: timeout, lost process, and `stale_epoch`: a frame
  of an older session is never executed and answered Unknown, never refused, ADR 0042).
  Observations are facts, deduplicated by `observation_id` in the control plane's inbox. The
  code's names (`mayfly-protocol`) are those of the `Attempt` aggregate where they differ:
  `VmBooted` for `VmStarted`, `OutcomeUnknown` for `DirectiveOutcomeUnknown`.
- **Shapes as built (KER-02).** A directive's `attempt_id` and `generation` are one optional
  `attempt` (image and host directives serve no attempt); `StopVm{grace, proof}` and
  `DeleteVm{proof}` name the VM inside the proof, whose run epoch is the directive's own; an
  observation's `directive_id?` is `answers: {directive_id, attempt?}`, echoed from the
  directive; `RunnerExited` and `VmObservedStopped` name their VM; `ImageEvicted` answers
  `EvictImage`; `Hello` lists every version the data plane speaks (`supported`) beside its build
  `dp_version`; handshake frames are written in the oldest major the writer speaks, so the peer
  can always read and refuse them; a capacity report is ordered by `(epoch, report_id)`. The
  golden fixtures of `crates/mayfly-protocol/tests/fixtures/` show every kind and reason.
- **Protocol 1.1 (DEC-04).** The refusal `capacity_unknown{dimension}` and a report's
  `unknown: [{dimension, causes}]` (ADR 0052), the unknown reason `listing_incomplete` (ADR
  0053) and a listed VM's optional `guest`, `memory_bytes` and `disk_bytes`. A frame never uses
  more than the session's minor: a 1.0 session gets `insufficient_resources`, `deadline` and no
  new fields (`ObservationKind::written_in`).
- **Protocol 1.2 (DEC-05).** The refusal `target_absent`: a `StopVm` or `StartVm` whose recorded
  VM a complete listing shows absent, which the control plane records as a desync and closes
  the attempt (`VmVanished`, ADR 0066 as amended 2026-11-11, DEC-07); a 1.1 or older session
  gets `proof_mismatch`. The
  `Welcome`'s optional `cp_clock`, the control plane's clock reading, from which the data plane
  reads every deadline of the session by an offset to its monotonic clock (ADR 0020, amended
  2026-11-10); without it, its wall clock at receipt.
- **Protocol 1.3 (DP-05).** `LaunchRunner`'s optional `instance_id`, the one `VmCloned`
  reported, which the data plane checks right before delivering the JIT config; a launch
  without it (a 1.2 session) is refused `proof_mismatch` (ADR 0038, ADR 0082).
- **Protocol 1.4 (CP-07).** `ImageMaterialised`'s optional `disk_bytes`: the disk the base
  reconstructs to, measured from its image's verified manifest (ADR 0052's amendment of
  2026-11-12), at which the control plane plans a clone when `clone_disk_gib` is not configured.
  A 1.3 or older session gets none; the control plane reads that as unknown and plans the clone
  with no disk, and the data plane's gate (I33) refuses what does not fit, as before.
- **Protocol 1.5 (DP-08).** `Inventory`'s optional `image_store: {images, bases}`: the image
  store as the same clean observer pass listed it, its verified images and its `Ready` bases
  that pass the clone proof (ADR 0087) with their images' digests and measured disks, left out
  when the store could not be listed. A base is read as materialised only from a listing of a
  later run epoch than the directive's send, since observations are not ordered. The control plane resolves a pull or a materialisation of unknown outcome from
  it, never by a new directive (P7) or an asserted outcome (ADR 0025); a VM under a base's name
  that is not a listed `Ready` base resolves nothing. A 1.4 or older session gets the inventory
  alone, and the control plane resolves as before: a materialisation or an eviction only by its
  base's absence, a pull only by its late answer.
- **Secrets cross the link only as `SecretHandle`**, resolved by the link adapter at send time from
  the control plane's in-memory broker. The `DirectiveIssued` event stores a handle id, never a
  JIT config. A crash loses the secret; the attempt is retired and a new one minted (a possibly
  leaked JIT config is never reused; its registration is deregistered by id).
- Both directions are **at-least-once**; both ends are idempotent. Ordering is not assumed:
  observations carry the directive and generation they answer, and stale ones are ignored by the
  aggregate (`StaleObservationIgnored`).

### 3.3 Transports

| Transport | When | Notes |
|:--|:--|:--|
| `inproc` | v0.x default (all-in-one `mayflyd`) | Bounded Tokio channels; same serde round-trip as the wire (messages are encoded and decoded even in-process, so a non-serialisable field cannot sneak in) |
| `uds` | v0.x, `mayflyd --role control` + `--role data` on one host, and the fault lab | Length-prefixed JSON frames over a Unix socket; lets the fault lab kill one plane without the other |
| `tls` | multi-host (phase 6) | Not designed now; Orchard's controller/worker trust model is the reference |

One **link contract suite** (verification §5) runs against every transport.

**As built (DP-02).** A transport is a *wire* (`mayfly_adapter_link::wire::Wire`: one encoded
frame per send, bounded, the peer's close observable) under shared *endpoints* that implement
the link ports: they write the handshake in the oldest major they speak, stamp session frames
with the negotiated version and write observations down to it (`written_in`), admit each frame
by its header before decoding it, and discard and count what they may not hand up. Epochs are
not judged there: a directive of any epoch is handed up and the executor fences it (ADR 0042).
A send's `Ok` means accepted; an accepted frame is delivered while the link is open, a full
wire makes the send wait, and a close reports the frames it discarded unread. `uds` (LNK-01) is
a second wire under the same endpoints.

### 3.4 Enforcement

`architecture.toml` makes the split structural: no `mayfly-control*` crate depends on a
`mayfly-data*` crate or a data-plane adapter, and vice versa; both depend only on
`mayfly-domain` (shared kernel) and `mayfly-protocol`. Only `mayfly-composition` sees both. The
data plane names no journal type; the control plane names no process API (`std::process` is
allowed only in the Tart/guest adapters, composition and test tools), with one exception
(ADR 0058, accepted by the owner 2026-11-11): the `command` module of `mayfly-adapter-secrets`,
the shell-free runner of `exec:` secret commands and the Keychain tool, which spawns nothing else
and talks to no data-plane crate. Every child, in either plane, gets a cleared environment plus an explicit allowlist
(`ARCH-child-environment`).

---

## 4. Workspace and dependency law

```text
crates/
  mayfly-domain/            shared kernel, pure: ids, VmName ownership, Generation, Epoch,
                            Fence, ImageDigest, Resources, Deadline (no clock), errors
  mayfly-protocol/          CP↔DP messages, versions, SecretHandle, link port traits
  mayfly-control-domain/    pure: aggregates (Host, Pool, Attempt, ScaleSet), commands, events,
                            decide/evolve, capacity policy, recovery decision table
  mayfly-control/           CP application: command bus, AggregateStore, projections, queries,
                            durable consumers (saga, directive relay), reconciler, listener;
                            ports: Journal, DeliveryLedger, Clock, Ids, ScaleSetApi,
                            RunnerRegistry, SecretProvider
  mayfly-data-domain/       pure: directive executor FSM, fencing, VM supervisor FSM
                            (depends on mayfly-domain and mayfly-protocol)
  mayfly-data/              DP application: executor, inventory observer, resource ledger;
                            ports: VmProvider, ImageStore, GuestTransport, HostProbe, ProcessClock
  mayfly-adapter-sqlite/    CP: journal, receipts, inbox, delivery ledger, snapshots, projections
  mayfly-adapter-postgres/  CP: the same stores on PostgreSQL (ES-09; tested, not deployed in v0.x)
  mayfly-adapter-github/    CP: App JWT/installation tokens, Scale Set client, JIT, runner REST
  mayfly-adapter-secrets/   CP: secret providers by URI: keychain, file (0600), env, exec (ADR 0033)
  mayfly-adapter-tart/      DP: Tart CLI by argv, per-VM shim, version probe, JSON parsing,
                            and the `tart exec` guest transport (no SSH, ADR 0038, ADR 0082)
  mayfly-adapter-host/      DP: disk/memory probes (cfg per OS)
  mayfly-adapter-link/      inproc and UDS transports for the protocol
  mayfly-composition/       config, wiring, runtime, supervision tree, startup sequence, roles
  mayfly-cli/               binaries `mayflyd` (daemon) and `mayfly` (operator CLI)
  mayfly-testkit/           test-only: fake clock, in-memory journal, fake ports, scenario builder
  mayfly-conformance/       test-only: Quint ITF decoding, per-model drivers, replay
  mayfly-fake-tart/         test-only binary: a `tart`-compatible CLI over a state directory
  mayfly-fake-github/       test-only: Scale Set + REST fake server, independent wire impl
tools/
  xtask/                    gate, architecture checker, adr, land, doctor (ported from site-preview)
  faultlab/                 Jepsen-style harness: controller, history, nemesis, checker; imports
                            no production crate (an independent oracle)
formal/                     Quint models (lifecycle, scheduling, recovery, scaleset, link,
                            registration, event_store)
```

Dependency direction (enforced by `cargo xtask architecture`, foundation §2):

```text
                mayfly-cli → mayfly-composition
                                │
        ┌───────────────────────┼──────────────────────────┐
  CP adapters (sqlite, github,  │                 DP adapters (tart, guest, host)
  secrets)                      │ adapter-link
        │                       │   │                      │
  mayfly-control ───────────────┼───┼──────────── mayfly-data
        │                       │   │                      │
  mayfly-control-domain    mayfly-protocol           mayfly-data-domain
        └──────────────────► mayfly-domain ◄───────────────┘
```

No new crate per bounded context in v0.x: contexts are modules of the existing crates (dm §4.1).
Pure crates (`mayfly-domain`, `mayfly-protocol`, `*-domain`) use no `std::{fs,net,process,env}`,
no clock, no randomness, no Tokio, no HTTP, no SQL. Application crates use `tokio` (`sync`, `time`
only through the `Clock` port) and their ports; no adapter types. Test crates are never reached
from production.

---

## 5. Control plane: event sourcing and CQRS

### 5.1 Streams and aggregates

Ten aggregates, defined in dm §6 (state, commands, events, the invariants each protects alone):

| Stream | Aggregate | Context | Holds |
|:--|:--|:--|:--|
| `scaleset-<pool>` | `ScaleSet` | Demand | session phase, cursor, latest statistics, deliverable and reported max, acquired-unbound request ids, tier |
| `pool-<name>` | `Pool` | Fleet | config revision, tier, image pin, limits (`max_running`, `min_prepared`), mode, breaker per digest |
| `host-<host_id>` | `Host` | Fleet | applied capacity reports, slots with monotonic generations, leases (allocation), pool terms, admission gate, drain |
| `attempt-<attempt_id>` | `Attempt` | Attempt | lifecycle (§6), VM name, generation, instance id, image digest, pending directive overlay, JIT handle phase, job binding |
| `registration-<attempt_id>` | `RunnerRegistration` | Registration | runner name and id, phase (issued … deregistered, presumed leaked) |
| `image-<digest>` | `Image` | Image | manifest, status, runner-version expiry |
| `placement-<host>-<digest12>` | `ImagePlacement` | Image | pull/materialise/evict phase, base name |
| `hostrun-<host_id>` | `HostRun` | Operations | run number, epoch, safe mode |
| `recovery-<plan_id>` | `RecoveryPlan` | Operations | bindings, actions, approval, progress |
| `quarantine-<attempt_id>` | `QuarantineCase` | Operations | reason, holds, evidence, escalation, resolution |

**One command, one stream**; every cross-aggregate rule is a policy (dm §7), eventually
consistent, through durable per-stream consumers. Every command carries `command_id`, `actor`,
`issued_at` and `run_epoch`; each aggregate refuses an epoch lower than the highest it accepted
(I18). Every aggregate exports pure `decide(&State, Command) -> Result<Vec<Event>, Rejection>` and
`evolve(State, &Event) -> State`. No clock, randomness, ids or logging inside: time and ids arrive
in the command. `decide` is deterministic, refusals included (DETERMINISM cases, verification §3).

### 5.2 Journal

The journal is the stream-scoped, storage-neutral event store of
[`event-store.md`](event-store.md) (ADR 0021, Accepted; ADR 0034): stream heads with per-stream
revisions, a global event-id registry, command receipts with intent digests (refusals bound to the
revision decided at), an inbox, durable delivery by per-consumer, per-stream checkpoints with
fenced leases, snapshots, stream epochs and online projection rebuild. There is no global order,
no `read_all` and no outbox table: a directive is a `DirectiveIssued` event delivered by the
`directive_relay` consumer. SQLite is the shipped adapter; PostgreSQL is tested in Docker (ADR
0035). `recorded_at` is the writer's control-plane `Clock` reading (ADR 0020).

**One write path.** `AggregateStore::save` is the only caller of `Journal::append`: one store
transaction holds the expected revisions, the events, inline projections, dispatch marks, the
command receipt and the inbox entry that caused it; reactors are only kicks after commit and are
never replayed. An architecture rule pins this (foundation §3).

SQLite: WAL, `foreign_keys=ON`, `synchronous=FULL`, `busy_timeout`; a single writer thread is an
implementation detail, never a premise. Migrations are forward-only, raw SQL, never edited once
released; a backup is taken automatically before every migration. Secrets never enter the journal.

### 5.3 Events

Immutable, past-tense, `#[non_exhaustive]` structs, each with `EVENT_TYPE` and `SCHEMA_VERSION`,
registered in one registry, with a **golden JSON fixture** per event and version. Evolution: add
optional fields or new event types; never rename; pure upcasters for old versions. A change an
older build must not ignore ships in two releases: **prepare** (N+1 reads it and raises its
aggregate's supported epoch) then **activate** (N+2 writes it); a stream whose epoch exceeds a
build's support refuses that build's writes and is still read (event-store §6). An unknown event
type in the journal puts the daemon in **safe mode** (it cannot prove it understands ownership).
**Upgrades and rollback (owner, 2026-11-09, ADR 0043):** any forward jump of any number of
versions is supported (every event readable for ever, every upcaster and migration kept; a CI
upgrade matrix upgrades the journal of every released version straight to the current build);
rollback one version is supported; deeper rollback restores the automatic pre-migration backup
(`mayfly restore <backup>`); an older build meeting newer data refuses to write the affected
streams and says why; a deep-rollback recovery quarantines the VMs it cannot prove and never
asserts ownership (ADR 0025). Detail: event-store §6.1a.

### 5.4 Commands, queries, reactors

- **Commands** (`mayfly-control::commands`): those of dm §6 (`ReserveSlot`, `StartAttempt`,
  `RecordObservation`, `RequestRetirement`, `Quarantine`, `StartPoolDrain`, `PausePool`,
  `RecordMessage`, `RecordCapacityReport`, `RequestRegistration`, …). Each has a `command_id`
  (idempotency via receipts). Commands never read projections and never return read DTOs; a
  policy that read another stream passes it as a validated read.
- **Queries** (`mayfly-control::queries`): read the projections of dm §8 only
  (`OperatingMode`, `AdmissionExplain`, `CapacityBoard`, `Attempts`, `PendingEffects`,
  `Inventory`, `Registrations`, `QuarantineBoard`, …). Queries never import write ports.
- **Policies / process managers** (dm §7, P1–P25) are durable consumers with per-stream
  checkpoints: the attempt saga issues the next command; the `directive_relay` sends
  `DirectiveIssued` over the link, stamped with the current epoch, holding create/destroy kinds in
  safe mode; the reconciler (P5) compares desired capacity with projections and the last capacity
  report and issues commands. Effects carry the idempotency key dm §7 names.

### 5.5 Projections

Each projection is a versioned spec (`NAME`, `VERSION`, subscribed kinds, pure `apply`), with
stream-local rows written inline by the unit; it keeps a per-stream applied revision (no global
watermark). A change of meaning is a new spec. A version mismatch starts an **online rebuild**
into a shadow generation with dual writes and a cutover fence (event-store §3.6, ADR 0034);
admission does not wait for it. Projection rebuild equals live projection (contract test, I16).

---

## 6. Attempt lifecycle (control-plane view)

One **attempt** = one VM name, one `(host, slot, generation)`, at most one registration, at most
one GitHub job. The authoritative definition is dm §6.4.

```text
Reserved → Cloning → Booting → Verifying → Prepared
Prepared → Activating → Listening → Busy → Finished
Prepared|Listening --RetirementRequested--> (Listening: deregister first) → Retiring
Finished|stage failure (budget spent)|Retiring → (diagnostics) → Deleting → Deleted
Cloning|Booting|Verifying|Activating → Backoff{n} → same stage (same name, generation)
any state ↔ pending.Unknown overlay (no transition until observed)
Verifying|Activating --GuestRejected--> Quarantined;  Deleting --proof refused--> Quarantined
any non-terminal --recorded VM absent without our delete (VmVanished, a desync)--> Lost → Closed
Quarantined --operator resolves the case--> Deleting;  Quarantined --observed absent--> Lost
```

Rules (each pinned by a test and by `formal/lifecycle.qnt`):

1. State reflects an **observed fact**, never an attempted command (persist intent as
   `DirectiveIssued`, send, transition on observation).
2. No path leads from Listening, Busy or Finished back to Prepared; a retired attempt never
   activates (I06).
3. A retry reuses the **same VM name and generation** so a lost reply cannot create a duplicate
   clone (I13); a new attempt gets a new slot generation.
4. `Unknown` leaves only on an observation (inventory, launcher status, GitHub runner state);
   timeouts never convert it to success or to `Deleted`, and there is no operator assertion
   (I10, ADR 0025). A provider that cannot answer leaves it `Unknown`, reported as a provider
   limitation.
5. **Activation** requires a Host grant (no grant while host or pool terms are closed, I26) and a
   fresh guest verification of this attempt in the current epoch; the JIT config is minted after
   both and delivered at most once (I22).
6. `Quarantined` is terminal for automation: the VM is kept, the lease keeps its resources, and
   only an operator resolves the case; nothing deletes a quarantined VM automatically (I28).
   Quarantine holds a VM that is present and does not match the record; once it is absent from a
   complete listing, the attempt is lost (rule 9).
7. Drain is not a state: it closes admission terms and retires Prepared and Listening attempts;
   Busy attempts finish.
8. Every State × Stimulus pair is either a defined transition or a defined refusal (exhaustive
   matrix test, no wildcard arms).
9. **A vanished VM converges** (owner, 2026-11-11, DEC-07, ADR 0066 as amended): a VM the
   attempt recorded and mayfly did not delete, absent from a complete listing (a stop's or
   start's `target_absent`, or the inventory), is a desync. The attempt records `VmVanished`, is
   lost and closed and never resurrected; the reconciler provisions a fresh attempt for the
   remaining demand, as a controller restores desired state. The desync is kept in the journal,
   shown (`attempts` view, `mayfly status`, the bundle) and counted by the breaker. An
   incomplete listing concludes nothing (I25).

The **data plane** has its own small FSM per directive (`Received → Executing → Done |
Refused | Unknown`) and per VM supervisor (`Absent → Present → Running → Stopped`, plus
`ProcessLost` when a shim is gone), observed from the provider and the shim's lock and
`exit.json` rather than remembered. As built (KER-08): the state is a pure function of the last
complete inventory's word and the shim's files, `Unobserved` until a complete inventory speaks
(I25); a destructive directive is re-observed in `Executing` before its effect, and so,
by name, is a `CloneVm` or `StartVm` (DP-01: a re-delivery is answered from the listing); a repeat of a
directive still executing is answered when it settles. Destructive directives are idempotent by
outcome (ADR 0053): a `DeleteVm` whose target a complete re-observation does not list is `Done`
(`VmDeleted`), a `StopVm` of a proven stopped VM is `Done` (`VmStopped`), and a re-observation
that fails is `Unknown{listing_incomplete}`, never `Done`.

## 7. GitHub integration

### 7.1 Auth

GitHub App (RS256 JWT → installation token). Four credentials (dm §9.1): App JWT, installation
token, Actions-service token, message-queue token; all in memory only, refreshed before expiry,
never logged (`Secret<T>` with redacting `Debug`/`Display`, a test proves it). The App key comes
from a `SecretProvider` reference (`keychain:`, `file:`, `env:`, `exec:`; ADR 0033). Permissions
are never guessed; the documented permission list, with the reason for each permission, is a
published commitment (ADR 0045), and `mayfly doctor` warns when the App holds more. The list is
drafted from GitHub's documentation (`doc/github-scaleset.md` §13: the organisation's
"Self-hosted runners: Read and write" and the repositories' "Actions: Read"; "Actions: Write"
only with `cancel_refused_runs`, the organisation's "Administration: Read" only with
`check_fork_pr_setting`) and verified in HW-02 before it is published as verified.

### 7.2 Scale Set session

One pool is one scale set at organisation scope (repository scope for testing), never deleted
by mayfly (ADR 0040); enterprise scope is unsupported, since GitHub App authentication does not
work there, and several organisations are the next step (ADR 0040 amended 2026-11-11, GH-07). A
scale set GitHub deletes as dormant (7 days without a session) is re-created by `ensure`.
Session states: `Unconfigured → Ensured → Opening → Active ⇄ Backoff | Degraded → Closing →
Closed`. One listener per scale set (local lease). The fake is built from
[`github-scaleset.md`](github-scaleset.md) as GH-00R settled it from public evidence.

**Two flows** (`github-scaleset.md` §6.1): github.com assigns jobs to the scale set directly
(`JobAssigned`, no offer, no acquire) up to the advertised `maxCapacity`; GHES 3.18 and 3.19
offer them (`JobAvailable`) and mayfly acquires. **Trust on github.com** (ADR 0090, owner
2026-11-11 and 2026-11-12): any plan, public repositories allowed, no runner group required.
Each `JobAssigned` must name a repository of the pool's `allowed_repositories`, and its workflow
run is looked up (`GET .../actions/runs/{run_id}`, "Actions: Read"); a runner starts only for a
job classified trusted (the pool's `allowed_events`, by default `push`, a same-repository
`pull_request`, `workflow_dispatch`, `schedule`, `merge_group`, `release`). Anything else,
anything unclassifiable and any failed lookup is refused (`AssignedJobRefused`, journaled, and
`job_refused` at WARN). The hard boundary is in the guest: every image's job-started hook
classifies the job its runner actually received and fails an untrusted one before any step
(IMG-02), so runners always run in disposable VMs and nothing holds the pool. A refused job goes
to a refusal sink, GitHub's runner in a small disposable Linux VM whose hook fails it, started
while `maxCapacity` is pinned at the assigned count (IMG-03); with `cancel_refused_runs`
("Actions: Write") mayfly cancels the run instead; failing both, GitHub's own release fails it at
the third release. `check_fork_pr_setting` ("Administration:
Read") optionally refuses to start when fork pull-request workflows run in private
repositories. A public allowed repository behind a runner group that does not admit public
repositories gets no jobs; mayfly reports it at start-up and in `doctor`.

Processing loop (the poller never waits on VM work):

1. Long-poll with `lastMessageId` and `maxCapacity` = the scale set's **reported max** (≤ its
   deliverable carve-out, I24; pause, drain and safe mode per ADR 0029).
2. GHES: classify each `JobAvailable` offer by trust tier and plan acquisition (pure
   `PlanAcquire`): acquire only while `assigned < deliverable` (I23) and only offers the pool's
   tier admits (I32); the rest are declined with a reason. github.com: classify each
   `JobAssigned` (ADR 0090); refused ones are recorded with their reason.
3. In one unit: inbox `(session, message_id)`, latest `statistics` (an older message never
   replaces newer), job facts, declined offers, classified and refused assignments and the new
   **desired** count. A redelivered message is `DuplicateMessage`; a session's first message
   below the cursor starts a new numbering (`MessageIdsRestarted`).
4. Only then `DeleteMessage` (ack). Ack failure is retried; it never re-acquires.
5. Empty poll → still reconcile. A session open recomputes desired from its initial statistics.
6. An unknown message type is recorded, acked and counted; past a threshold the session is
   `Degraded{protocol_drift}`, never safe mode.

**Demand:** `desired = min(deliverable, assigned)` where `assigned = TotalAssignedJobs`; there is
no registered standby (ADR 0028). Never count `JobAssigned` messages (≤ 50 per batch); never add
`TotalRunningJobs`. `shortfall = max(0, desired − (Activating + Listening + Busy))`; Prepared
VMs serve it first and do not count in it. A runner is activated only for a job classified
trusted that waits for one (ADR 0090); a refused job holds nothing. The service
assigns nothing beyond the advertised `maxCapacity`, so a scale set advertising 0 sees no demand
at all (`github-scaleset.md` §5).

### 7.3 Runner registrations

Scale sets only: there is no classic or `--ephemeral` mode (ADR 0028). A registration is its own
aggregate (`RunnerRegistration`, dm §6.5): the runner name is the VM name, issued at most once
(I19); a JIT config is minted once per registration; a registered-idle runner is deregistered
before its VM is stopped, and a busy answer (`JobStillRunningException`, 400 or 409, told by its
`typeName`, on the Actions-service path mayfly uses; 422 on the REST path) cancels that
retirement (I21); a 204 means the runner is gone, not that mayfly removed it; a lost mint or delivery is
presumed leaked and deregistered by its journaled id. The reaper (ADR 0029) runs at run start,
after long outages and daily, deletes only journaled ids and reports unrecognised ones.

### 7.4 Protocol fidelity

The Scale Set API is in public preview. mayfly specifies it in its own words in
[`github-scaleset.md`](github-scaleset.md) and implements its own client, informed by reading
GitHub's `actions/scaleset` but copying none of it (ADR 0048). Each fact carries its evidence
strength (server-observed, client-assumed, doc-stated) and a pinned citation; GH-00R settled
what public sources settle, and the rest is the HW-02 remainder (`github-scaleset.md` §12),
recorded by GH-00's redacting capture in HW-02's session. A client-assumed or open fact stays a
named assumption, a fake knob and a test name until it is server-observed (ADR 0048, amended
2026-11-11). The adapter is versioned; recorded request/response fixtures (redacted) are the
contract; `mayfly-fake-github` implements the wire behaviour independently (redelivery,
50-message cap, both assignment flows, session conflict and expiry, 401/403/409/429 +
Retry-After), built from that specification, and a gated live test runs against a disposable
organisation (owner-provisioned).

---

## 8. Data plane

### 8.1 VmProvider port

```rust
pub trait VmProvider: Send + Sync {
    fn inventory(&self) -> impl Future<Output = Result<Inventory /* Complete | Incomplete */, VmError>> + Send;
    fn clone_vm(&self, base: &BaseName, name: &OwnedVmName) -> impl Future<Output = Result<InstanceId, VmError>> + Send;
    fn start(&self, name: &OwnedVmName, opts: &RunOptions) -> impl Future<Output = Result<ShimHandle, VmError>> + Send;
    fn stop(&self, proof: &OwnershipProof, grace: Duration) -> impl Future<Output = Result<(), VmError>> + Send;
    fn delete(&self, proof: &OwnershipProof) -> impl Future<Output = Result<(), VmError>> + Send;
    fn instance_id(&self, name: &OwnedVmName) -> impl Future<Output = Result<Option<InstanceId>, VmError>> + Send;
}
```

`VmError` classifies `Definite` (nothing happened) vs `Ambiguous` (may have happened). An
inventory that cannot be read or parsed is `Incomplete` and never read as the absence of any VM
(I25). Image operations live on a separate `ImageStore` port (pull, materialise, evict, list
bases; DP-07). Providers: **Tart** (v0.x), fake in-memory (testkit), `mayfly-fake-tart` binary
(e2e and fault lab on Linux), Orchard or direct Virtualization.framework (later, ADR first).

**As built (DP-01).** The ports live in `mayfly-data::ports`. `inventory` returns an
`Inventory` and no error: a failed, unparseable or unfinished listing *is*
`Inventory::Incomplete`, so no caller can read an error as an empty list (I25). `start` returns
`()` (the shim is the adapter's, DP-04), `stop` takes its grace as `Millis`, and `instance_id`
is not a port call: the re-observation before an effect reads instance ids from a fresh
`inventory`. Like Tart, `clone_vm` onto an existing name and `start` of a running VM fail, so the
executor re-observes a `CloneVm` or `StartVm` by name first (ADR 0042 decision 4: a re-delivery
under a new epoch is a re-observation): a clone whose exact name is listed with an instance id is
answered `VmCloned` with it, a start of a running VM `VmBooted`, both without the effect.
`VmError` is `Definite` or `Ambiguous(Ambiguity)`, the ambiguity naming the unknown reason
(`deadline`, `process_lost`, `host_slept`); `ImageStore` and `GuestTransport` use the same error.
**An effect's future is never dropped**: at its directive's deadline the executor answers
`Unknown{deadline}` but keeps its target busy, its slot taken and listings tainted until the
future completes, so an adapter bounds each call itself and completes only once nothing it
started still runs (reads may be dropped). `ProcessClock` separates `wall()` (deadlines, read in
one place; ADR 0020, amendment proposed 2026-11-10) from `monotonic()` (every wait and
cadence). The executor (`mayfly-data::executor`) runs everything on its own task (no spawn): at
most `concurrency` directives at once, one per target; a stage that could not start before its
deadline is refused `deadline_expired`; listings start only while no effect that changes them
runs, no new such effect starts while a pass is due or running, and a listing an effect
overlapped anyway is discarded, never applied or reported (`mayfly-data::gate`).

### 8.2 Tart adapter

argv only (never `sh -c`); version handshake and capability probe (an unsupported version closes
admission; Tart 2.41.1 is the supported release, TART-01); `tart list --format json` parsed
against version-pinned fixtures; every call has a
deadline, its own process group, bounded captured output and redaction; an ambiguous result is
re-inspected. `TART_NO_AUTO_PRUNE` is set on every call. Dedicated `TART_HOME` per deployment,
`0700`, owned by the service user, carrying the deployment marker with the epoch floor (ADR 0026).

- **Per-VM shim (ADR 0037).** `StartVm` starts `mayfly vm-shim` with `setsid`; the shim runs
  `tart run` as its child, stdio to bounded files (never a pipe), holds its own lock and writes
  `exit.json` atomically. A daemon crash or upgrade leaves VMs running; on start the daemon
  re-adopts them from the journal, `tart list`, the shim locks and `exit.json`. A VM is stopped
  only with `tart stop`, never by a signal.
- **Clones** come only from local image bases (`mfb-<digest12>…`, ADR 0039), followed by `tart
  set --random-mac` (with `--random-serial` for a macOS guest, so no two run with one serial); the MAC read back is the
  provider instance id (ADR 0026).
- **Softnet** is supported (ADR 0032); its privilege model is NET-01's ADR, and the daemon stays
  non-root. Softnet isolates; it is not an egress allowlist.

### 8.3 Guest transport and runner launch

Guests are reached only by `tart exec` (ADR 0038; no SSH): `VerifyGuest` sends a per-attempt
nonce and reads typed, bounded claims; images without the guest agent are refused. The JIT config
is delivered by `tart exec -i <exact VM> <fixed in-image launcher>` **on stdin only**, after the
data plane checked that the PID holding the VM's run lock is its shim's child and the instance id
matches; afterwards the control plane confirms through GitHub that the issued runner id came
online. The data plane takes the config from an in-memory store by handle, which the link fills,
and the launcher's contract (verbs, claims, status file) is ADR 0082's. Never interpolated into a shell string, never written to disk unencrypted, never logged.
The runner is baked into the image at a pinned, SHA-checked version with self-update off; expiry
trips the pool's breaker with reason `runner_version` (ADR 0030). Runner exit after a restart is
read from the launcher's status and GitHub, not from `tart run`. Runner diagnostics are exported
before deletion (bounded, redacted; `DiagnosticsLost` and proceed).

### 8.4 Ownership

A managed VM name is `mf-<pool>-<host>-s<slot>-g<generation>-<attempt_short>`, where pool and
host ids are `[a-z0-9]{1,16}` so the `-` separators are unambiguous; the name is a hint, never
ownership. A VM is **owned** only if the journal records `VmCloned` for an attempt **and** the
observed name equals its `OwnedVmName` **and** the slot generation matches **and** the observed
provider instance id equals the recorded one (ADR 0026). The control plane attaches an
`OwnershipProof {attempt_id, host_id, run_epoch, slot, generation, vm_name, provider_instance_id}`
to every destructive directive; the data plane re-observes right before acting and refuses on any
mismatch, and refuses any name in its image-base set. A present target that differs is never
touched; an absent one, on a complete listing, answers a `DeleteVm` as done (ADR 0053), an
incomplete listing never does (I25). A `mf-*` VM unknown to the journal is
reported, never deleted (I04, F24); a VM whose ownership or outcome cannot be proven is quarantined
and kept (ADR 0025).

---

## 9. Capacity and admission (pure policy)

**The data plane accounts and enforces; the control plane allocates (ADR 0024).** Each host's data
plane keeps a resource ledger (VM slots, memory, disk ceilings of live clones and bases, the two
macOS-guest slots, operator reserves; ours and foreign VMs) rebuilt from observation, refuses any
directive that does not fit (I33) and reports `CapacityReported`. It assumes no fact it does not
have (ADR 0052): a VM whose size or guest is not described, our VMs before the first complete
inventory, images before the first image listing and a foreign VM the probe cannot size make
their dimensions unknown; what needs one is refused `capacity_unknown`, and the report lists
the unknown dimensions with their causes and nothing free in them until a listing describes
them. The control plane's `Host`
applies the latest report and allocates leases within it; the pure capacity policy computes a
`CapacityReport` per host and scale set: reported, allocated, admissible (reported minus reserves,
closed terms and quarantined holds), deliverable per scale set (a carve-out so that Σ reported max
≤ deliverable, I24) and reported max. Attempts from `Reserved` to `Deleted` count on every
dimension (I03); a stopped quarantined lease shrinks to disk + slot but is never released by
policy. `min_prepared` defaults to 0 (and 0 for macOS unless overridden per pool). Low disk →
admission stops and `reported_max` drops (F17); a pool whose capacity quarantined leases consume
admits nothing, and `AdmissionExplain` says why. The carve's properties (safe, monotone pool by pool, no
starvation) and the policy that meets them are `doc/domain-model.md` §6.3.1 (ADR 0069).

---

## 10. Recovery

Startup sequence (P1): lock state dir → open/migrate the store (automatic backup first) → verify
(epoch floor, stored kinds, structure) → `StartRun` → `MintEpoch` above journal and floor → data
plane handshake (floor raised and fsynced) → inventory, shim locks, `exit.json` and capacity →
resolve `Unknown` effects by observation only → classify each VM by the **recovery decision
table** (pure, `mayfly-control-domain::operations::recovery`, model `formal/recovery.qnt`) →
`CompleteRecovery` → admission opens → GitHub sessions.

| Journal | Provider | GitHub | Decision |
|:--|:--|:--|:--|
| Cloning | owned clone exists (instance id matches) | — | adopt, continue |
| Cloning | absent (complete inventory) | — | retry same name (budgeted) |
| any | inventory incomplete | any | wait; admission closed; nothing released (I25) |
| any | name present, instance id differs | any | quarantine `provider_identity_mismatch`; never delete |
| Prepared | running, shim alive | no registration | adopt; re-verify before any activation |
| Listening/Busy | running, shim alive | online/busy | adopt and monitor; never delete |
| Listening/Busy | running | unreachable | adopt and monitor; never delete; `unreachable` ≠ `absent` |
| Listening | running | registration absent | retire (consumed elsewhere or expired) |
| Busy | stopped (`exit.json` or host rebooted) | offline/abandoned | retire, delete with proof, reap the registration |
| Activating | any | registration issued, not delivered | presume leaked; deregister by id; retire |
| Activating | any | busy | adopt and monitor; never delete (F13); the JIT stays presumed leaked, so no further job binds |
| Prepared | any | a runner GitHub knows | quarantine `jit_presumed_leaked`, read before the VM's liveness (ADRs 0025, 0029) |
| Deleted | owned clone present | any | leaked VM: delete with proof |
| Deleting | absent | — | confirm deletion, release slot |
| Booting … Retiring (a VM recorded, no delete of ours) | absent (complete inventory) | — | mark lost: the attempt records `VmVanished`, a desync, and closes; slot released (DEC-07) |
| Deleting | present | active | suspend cleanup, inspect |
| Quarantined | any | any | keep; the case continues (its VM's absence on a complete inventory is read by the attempt: `VmVanished` for a recorded VM, DEC-07) |
| no record | `mf-*` name | any | report for operator review; never delete |
| journal epoch < floor, unknown event kind, failed verification | — | — | safe mode |

**Safe mode** (dm §5.3): no clone, start, stop, delete, pull or evict is executed and no secret
minted (I29); `reported_max = 0`, nothing acquired, only recorded messages acked (ADR 0029);
observation, status and plan proposal continue. `mayfly recover --dry-run` prints a plan bound to
the journal heads, inventory digest and epoch; `--apply <plan-id>` executes exactly that plan,
intent then outcome (I31). A plan never asserts an outcome (ADR 0025).

**Drain** (P23): close terms, `reported_max = 0`, retire Prepared and Listening, wait for Busy up
to a timeout, then alert; never force. `--force` is a separate, explicit operator action.
**Shutdown** (P24, SIGTERM or Ctrl-C): close admission, stop acquiring and acking, finish durable
writes, leave VMs running under their shims, exit before the service manager's deadline.

---

## 11. Operator interface

`mayflyd [--config path] [--role all|control|data]` runs in the foreground by hand or under launchd
(ADR 0031). `mayfly vm-shim` is internal (started by the daemon only). `mayfly` talks to the daemon
over a `0600` Unix socket (peer-uid allowlist, default the service uid); every mutating command
carries an actor and is journaled before it acts:

```text
mayfly status [--json]            mayfly pool list|drain|resume|pause|reset-breaker <name>
mayfly host drain|resume          mayfly vm list [--owned-only]   mayfly vm inspect <attempt>
mayfly vm logs <attempt>          mayfly quarantine list|inspect|resolve <case>
mayfly events --follow [--since]  mayfly doctor [--json]          mayfly config validate <path>
mayfly recover --dry-run | --apply <plan-id>                      mayfly safe-mode enter
mayfly image verify <manifest>    mayfly image refresh <pool>     mayfly image withdraw <digest>
mayfly service install|uninstall  mayfly diagnostics bundle [--out <path>]   mayfly metrics
mayfly restore <backup>           mayfly uninstall --purge [--dry-run | --apply <plan-id>]
```

Config (TOML, `schema_version = 1`): `state_dir`, `[github]` (organisation, App client id,
installation id, `private_key` as a secret reference URI), `[host]` (operator reserves for
memory and disk), `[[pools]]` (name, `tier = "trusted"`, image manifest, guest OS, cpu/memory,
`min_prepared`, `max_prepared_age`, `max_running`, breaker N and W, optional daily runner refresh),
`[provider]` (`tart` + `tart_home`), `[images.runners."X.Y.Z"]` (one SHA-256 per runner package
platform, `linux-arm64` and `osx-arm64`; the base's guest OS selects it, HW-01X; read once RT-02
wires the image store). Validated on load; refused keys (`allow_untrusted_prs`, a
classic `mode`) are refused with a message, never ignored. Secrets only by reference (ADR 0033).
Packaging and removal (owner, 2026-11-09, ADR 0044): mayfly is distributed and updated through
Homebrew and never updates itself; `brew uninstall` removes the program; `mayfly uninstall
--purge` removes exactly what mayfly created (journal and state, its VMs and image bases with the
ownership proof of §8.4, its runner registrations) and Homebrew's caveat points to it. `mayfly
restore <backup>` restores the automatic pre-migration backup (ADR 0043). Both are destructive
operations run only by the owner or on the owner's explicit request, never by an agent.
As built (RT-01, ADR 0046): unknown keys are refused at every level, every problem is reported
with its field path and its fix, `[github]` takes `organisation` and, for testing, `repository`;
`[host]` takes `reserve_memory_mib` and `reserve_disk_gib`; a pool's `breaker_failures` and
`breaker_window` are its N and W and `runner_refresh_at` is `HH:MM`; exit codes are 78 (invalid
config), 69 (lock refused) and 73 (state dir unusable). A pool's optional `capacity_policy` is
`"exact"` (default) or `"demand"` (DEC-06, ADR 0069): what it keeps of the host's free capacity
ahead of the pools after it, its whole headroom or only what it waits for. **`"demand"` is
experimental**, an explicit opt-in (ADR 0069, amended 2026-11-11): GitHub assigns nothing to a
scale set advertising `maxCapacity` 0 (`doc/github-scaleset.md` §5), so a `demand` pool carved
nothing never sees its job and can starve. An older build refuses the key (unknown keys are
refused). On github.com each pool also lists its `allowed_repositories` and may set
`allowed_events` and `cancel_refused_runs`; an organisation may set
`check_fork_pr_setting` (ADR 0090, GH-08).
As built (RT-02, ADR 0091; owner, 2026-11-12: "reasonable defaults out of the box"): `state_dir`
and `[provider] tart_home` are optional. Without them `mayflyd` uses the platform's standard
places, creating them `0700` and then running its usual checks: on macOS `~/Library/Application
Support/mayfly` with `TART_HOME` at `<state>/tart`; on Linux `$XDG_STATE_HOME/mayfly`
(`~/.local/state/mayfly`) with `TART_HOME` at `$XDG_DATA_HOME/mayfly/tart`
(`~/.local/share/mayfly/tart`). A configured `state_dir` alone puts `TART_HOME` at
`<state_dir>/tart`; the configuration overrides every path. Nothing goes in a purgeable cache
directory. Service mode configures `/Library/Application Support/mayfly` (macOS) or
`/var/lib/mayfly` (Linux) explicitly (OPS-01). `mayflyd` also requires `[images]` (repository,
pinned runners, realm hosts) and `[host]`'s `vm_slots`, `macos_guests` (at most 2),
`image_disk_gib` and `clone_disk_gib` (ADR 0052: no default for a fact of the host); each is
optional for `config validate`.
As built (OPS-02, ADR 0091): a pool's `image_manifest` is mayfly's image manifest, a TOML file
of schema 1 (`provider`, `os`, `arch = "arm64"`, `source = "<repository>@sha256:<64 hex>"`, never
a tag, `revision`, `[runner] version sha256`, `[launcher] sha256`, `[capabilities] guest_agent =
true`), distinct from the OCI manifest the data plane verifies at the pull. `mayfly image verify
<manifest>` checks one file offline. `mayflyd` refuses to start (78) when a pool's manifest does
not read or disagrees with the deployment (another repository than `[images] repository`, another
guest than `guest_os`, a runner pin that contradicts another pin of the package, two manifests for
one digest), adds each manifest's runner pin to the data plane's pin table (so `[images.runners]`
is optional), and at every run registers each pool's image, records it verified and pins the pool.
As built (RT-03): `mayfly` reaches `mayflyd` over its socket: `<state_dir>/mayfly.sock` when
`state_dir` is configured, else ADR 0091 decision 5's place (`<state>/mayfly.sock` on Linux
without an absolute `$XDG_RUNTIME_DIR`); `--socket <path>` or `--config <path>` tell the CLI.
The socket is `0600` in a directory private to the daemon's user, its peers limited to the
daemon's own uid, its wire NDJSON at API 1 (unknown fields ignored, unknown commands answered
`unknown_command`). Served now: `status`, `pool list|pause|resume|drain|reset-breaker`, `host
drain|resume`, `vm list`, `events [--follow] [--since]`, a basic `doctor` and, offline, `config
validate`; exit codes 0, 1 (refused), 64, 69 (no daemon answers), 78. `quarantine` arrives with
CP-06, `recover` and `safe-mode enter` with RT-08, `image withdraw` once its withdrawal is two-step
(OPS-02's note); the rest of the list above with their tasks.

---

## 12. Observability

`tracing` with JSON output; fixed `snake_case` event names, variable data in fields
(`attempt_id`, `pool`, `generation`, `directive_id`, `session_id`, `error_class`). A healthy daemon
logs (almost) nothing at INFO: start/stop, migrations, config changes only; a quarantine opens with
an ERROR. Metrics (bounded labels, no ids) on a loopback HTTP endpoint: `mayfly_attempt_state{pool,state}`,
`mayfly_provision_seconds{pool,stage}`, `mayfly_directives_total{kind,outcome}`,
`mayfly_capacity{pool,kind}`, `mayfly_github_requests_total{op,status_class}`,
`mayfly_github_assigned_jobs`, `mayfly_last_poll_age_seconds`, `mayfly_orphans`,
`mayfly_disk_free_bytes`, `mayfly_reconcile_errors_total{kind}`, plus dm §8's `mayfly_mode{mode}`,
`mayfly_epoch`, `mayfly_unknown_effects{age_bucket}`, `mayfly_quarantined{pool,reason}`,
`mayfly_breaker_open{pool}`, `mayfly_vms_vanished_total{pool,discovered_by}` (DEC-07),
`mayfly_registrations{phase}`, `mayfly_job_result_total{pool,result}`,
`mayfly_heartbeat_timestamp_seconds`, `mayfly_secret_handles_outstanding`. Nothing is sent
anywhere (ADR 0036). One trace spans poll → reconcile → clone → boot → verify → JIT → runner →
job → stop → delete.

---

## 13. Security

Host service runs as a non-root user (Softnet's privilege lives outside it, ADR 0032); state dir
`0700`, DB/socket `0600`; `TART_HOME` `0700` under a dedicated user; single instance via `flock`.
No host directory, socket, key or credential is shared with a guest. JIT configs are single-use,
in memory, delivered over `tart exec` stdin to the exact VM after its identity checks (ADR 0038).
No token in argv, env of child processes, logs, metrics, status, diagnostics or the diagnostics
bundle (redaction tests + secret scan). Trust: `trusted` tier only in v0.x, enforced per scale set
(ADR 0027). Residual risks stated in `doc/security.md`: a JIT config is visible to the workflow
code it serves, by design; hypervisor escape. Threat model (SEC-01): untrusted workflow code,
guest-to-host network (with and without Softnet), supply chain of images and runner, operator
mistakes.

---

## 14. Roadmap (phases; tasks.md has the order)

| Phase | Outcome | Gate |
|:--|:--|:--|
| 0 Foundation | Workspace, gate, architecture law, CI, process | `cargo xtask check` green in CI; forbidden edge fixtures fail |
| 1 Kernel | Pure domain, protocol, lifecycle FSM, aggregates, models | exhaustive FSM matrix, golden events, Quint models check |
| 2 Journal | Stream-scoped store: SQLite journal, AggregateStore, projections, inbox, durable delivery, snapshots, PostgreSQL in Docker | store contracts on memory, SQLite (five modes) and PostgreSQL |
| 3 Planes | CP reconciler/saga, DP executor, inproc link | **walking skeleton**: `mayflyd` + fake-tart + fake GitHub runs a job end to end on Linux |
| 4 GitHub | Spec from public evidence (GH-00R), Scale Set client, listener, trust on github.com, JIT, registrations and reaper | fake-GitHub contract built from the spec, redelivery/ack-loss tests; the live remainder in HW-02 |
| 5 Real world | Tart adapter, guest transport, host probes, launchd | (owner) real Tart smoke on the Mac |
| 6 Evidence | Conformance replay, fault lab F01–F31, mutants, fuzz | core faults PASS, curated mutants killed |
| 7 Operate | Packaging, signing, runbooks, canary next to Tartelet | (owner) canary on a real repository |
| 8 Later | Prepared-pool tuning, untrusted tier, UDS split roles, multi-host | ADR per step |
