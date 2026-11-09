# mayfly: product and architecture plan

**Status:** plan, 2026-11-09. Owner: Patryk Żmigrodzki.
**What:** a headless Rust service that runs **single-use GitHub Actions runners in disposable
virtual machines** on Apple Silicon hosts. Each job gets a freshly cloned VM; the VM is destroyed
after one job. Demand comes from GitHub **Runner Scale Sets** (JIT runners); the VM layer is an
abstraction whose first provider is **Tart**.

Why the name: a mayfly lives for a day. So does every runner VM here: one job, then gone.

This document owns *what* and *how it is structured*. [`foundation.md`](foundation.md) owns the
engineering rules (workspace, enforcement, gate, commands, agent contract),
[`verification.md`](verification.md) owns the test levels, formal models and fault lab,
[`philosophy.md`](philosophy.md) (a draft awaiting the owner) says why and where the lines are, and
[`tasks.md`](tasks.md) owns order and size. A citation "plan §4.2" points here.

---

## 1. Goals, non-goals, guardrails

### 1.1 Release goals

| ID | Goal |
|:--|:--|
| G1 | **No job-to-job state.** Every job runs in a VM cloned from a pinned, immutable image; the VM is destroyed or quarantined after it. A VM that ran workflow code never re-enters the warm pool. |
| G2 | **No orphan storm.** After a daemon crash, host reboot, GitHub outage, VM crash or cleanup failure, the system converges to a known inventory and never starts unbounded replacements. |
| G3 | **No overcommit.** Admitted VMs never exceed configured slots, guest-OS limits (at most two macOS guests per host), memory headroom or disk reserve. |
| G4 | **No unsafe cleanup.** Only VMs with positive ownership evidence (journal record + exact name + generation) are deleted. Never a base image, never a foreign VM, never by name prefix. |
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
- **Untrusted code:** public-fork PRs are off by default (`allow_untrusted_prs = false`).
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
│  session, secrets broker, operator API            │  inventory + resource observation    │
│                                                   │                                      │
│  commands → aggregates → events → outbox ─ Directive ─▶ executor ─▶ VmProvider (Tart)    │
│  inbox ◀─────────────────────────────── Observation ─── observer ◀─ GuestTransport (SSH) │
│                                                   │                    HostProbe         │
└───────────────────────────────────────────────────┴──────────────────────────────────────┘
        ▲ Unix socket (0600)                                       │ argv, never a shell
     mayfly CLI                                         disposable VM per job (Linux/macOS)
                                                        official actions/runner, JIT config
```

**Authorities.** GitHub is the authority on job assignment and runner registration; the VM
provider is the authority on which VMs exist; the control-plane journal is the authority on **who
may act on which VM**. They can disagree, so reconciliation is mandatory and continuous.

---

## 3. Control plane / data plane split

The split exists from day one even though both planes run in one process: it is what makes the
later multi-host edition (one control plane, many data-plane hosts) a transport change rather than
a rewrite, and it gives each plane a narrow, testable contract.

### 3.1 Responsibilities

| | Control plane | Data plane |
|:--|:--|:--|
| Question it answers | *What should exist?* | *What exists, and make it so.* |
| State | Durable: event journal, projections, outbox, inbox (SQLite) | None durable. In-memory in-flight table, rebuilt from the provider's inventory on start |
| Talks to | GitHub, operator CLI, the data plane | Tart (or another provider), guests, the host OS |
| Decides | Desired capacity, admission, which VM is retired, quarantine | Nothing about demand. Only *how* to execute a directive safely (timeouts, process groups, retries within a directive's deadline) |
| Secrets | GitHub App key (via secret store), mints JIT configs | Receives a JIT config in memory for exactly one launch; never stores it |
| Failure stance | Safe mode on journal doubt; never acts on guessed ownership | Fail closed: refuse a directive it cannot fence or validate |

### 3.2 The CP↔DP contract (`mayfly-protocol`)

Versioned, transport-agnostic, serde-encoded messages. Every message carries
`protocol_version`; a peer refuses an unsupported major version.

- **Handshake:** `Hello { host_id, boot_id, dp_version, capabilities }` →
  `Welcome { epoch, accepted_version }`. The control plane mints a new **host epoch** per data-plane
  session; the data plane rejects any directive with an older epoch (**fencing**, invariant I09).
- **Directive** (CP → DP): `{ directive_id, epoch, attempt_id, generation, deadline, kind }` where
  `kind` ∈ `CloneVm{image_digest, vm_name}`, `StartVm{vm_name, run_options}`,
  `ProbeGuest{vm_name}`, `LaunchRunner{vm_name, launch: SecretHandle}`,
  `StopVm{vm_name, grace}`, `DeleteVm{vm_name, ownership_proof}`, `ReportInventory`,
  `ReportResources`. Directives are **idempotent by `directive_id`** and by target name: a repeat
  returns the earlier outcome or re-observes.
- **Observation** (DP → CP): `{ observation_id, epoch, directive_id?, observed_at_dp, kind }` where
  `kind` ∈ `VmCloned`, `VmStarted{ip?}`, `GuestReady{facts}`, `RunnerStarted`,
  `RunnerExited{code}`, `VmStopped`, `VmDeleted`, `Inventory{vms}`, `Resources{mem, disk}`,
  `DirectiveRefused{reason}` (definite, nothing happened), `DirectiveOutcomeUnknown{reason}`
  (ambiguous: timeout, lost process). Observations are facts, deduplicated by `observation_id` in
  the control plane's inbox.
- **Secrets cross the link only as `SecretHandle`**, resolved by the link adapter at send time from
  the control plane's in-memory secrets broker. The outbox stores a handle, never a JIT config. A
  crash loses the secret; the attempt is retired and a new one minted (a possibly leaked JIT
  config is never reused; it expires server-side).
- Both directions are **at-least-once**; both ends are idempotent. Ordering is not assumed:
  observations carry the directive and generation they answer, and stale ones are ignored by the
  aggregate (`evolve` of an older generation is a no-op recorded as `StaleObservationIgnored`).

### 3.3 Transports

| Transport | When | Notes |
|:--|:--|:--|
| `inproc` | v0.x default (all-in-one `mayflyd`) | Bounded Tokio channels; same serde round-trip as the wire (messages are encoded and decoded even in-process, so a non-serialisable field cannot sneak in) |
| `uds` | v0.x, `mayflyd --role control` + `--role data` on one host, and the fault lab | Length-prefixed JSON frames over a Unix socket; lets the fault lab kill one plane without the other |
| `tls` | multi-host (phase 6) | Not designed now; Orchard's controller/worker trust model is the reference |

One **link contract suite** (verification §5) runs against every transport.

### 3.4 Enforcement

`architecture.toml` makes the split structural: no `mayfly-control*` crate depends on a
`mayfly-data*` crate or a data-plane adapter, and vice versa; both depend only on
`mayfly-domain` (shared kernel) and `mayfly-protocol`. Only `mayfly-composition` sees both. The
data plane names no journal type; the control plane names no process API (`std::process` is
allowed only in the Tart/guest adapters, composition and test tools).

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
                            reactors (process managers), reconciler, listener, outbox relay;
                            ports: Journal, Clock, Ids, ScaleSetApi, RunnerRegistry, SecretStore
  mayfly-data-domain/       pure: directive executor FSM, fencing, VM supervisor FSM
                            (depends on mayfly-domain and mayfly-protocol)
  mayfly-data/              DP application: executor, inventory observer, resource monitor;
                            ports: VmProvider, GuestTransport, HostProbe, ProcessClock
  mayfly-adapter-sqlite/    CP: journal, receipts, outbox, inbox, projections, migrations
  mayfly-adapter-github/    CP: App JWT/installation tokens, Scale Set client, JIT, classic
  mayfly-adapter-secrets/   CP: file (0600) and macOS Keychain secret stores
  mayfly-adapter-tart/      DP: Tart CLI by argv, version probe, JSON parsing
  mayfly-adapter-guest/     DP: SSH (pinned host key) and `tart exec` guest transports
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
formal/                     Quint models (lifecycle, scheduling, recovery, scaleset, link)
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

Pure crates (`mayfly-domain`, `mayfly-protocol`, `*-domain`) use no `std::{fs,net,process,env}`,
no clock, no randomness, no Tokio, no HTTP, no SQL. Application crates use `tokio` (`sync`, `time`
only through the `Clock` port) and their ports; no adapter types. Test crates are never reached
from production.

---

## 5. Control plane: event sourcing and CQRS

### 5.1 Streams and aggregates

| Stream | Aggregate | Holds |
|:--|:--|:--|
| `host-<host_id>` | `Host` | epoch, slots with monotonic generations, reservations, drain flag, safe-mode flag |
| `pool-<name>` | `Pool` | config revision, image digest, labels, warm policy, pause/drain |
| `attempt-<attempt_id>` | `Attempt` | lifecycle (§6), VM name, generation, image digest, runner identity, pending directive, retry budget |
| `scaleset-<id>` | `ScaleSet` | session state (§7.2), last statistics, message cursor, acquired request ids |

Every aggregate exports pure `decide(&State, Command) -> Result<Vec<Event>, Rejection>` and
`evolve(State, &Event) -> State`. No clock, randomness, ids or logging inside: time and ids arrive
in the command. `decide` is deterministic, refusals included (DETERMINISM cases, verification §3).

### 5.2 Journal (SQLite)

**Stream-scoped, with no global order (ADR 0021):** a stream head per stream holds its revision,
compared and advanced by each append; event ids are unique through a registry; readers enumerate
streams and read each after a revision; no correctness rests on a single writer, so the port is
storage-neutral (SQLite first, PostgreSQL possible). `recorded_at` is the writer's control-plane
`Clock` reading (ADR 0020). Tables: `event_stream(stream_id, revision)`,
`event_journal(stream_id, stream_revision, event_id UNIQUE, type, schema_version, payload_json,
recorded_at, PRIMARY KEY(stream_id, stream_revision))`, `command_receipt(command_id, status, result_json)`, `outbox(directive_id,
attempt_id, generation, kind, request_json, status, attempts, next_attempt_at)`,
`inbox(source, message_id, processed_at)` (GitHub messages and DP observations), projection tables
with `projection_meta(name, spec_version, watermark)`, `schema_meta`.

**One write path.** `AggregateStore::save` is the only caller of `Journal::append`: in one SQLite
transaction it checks expected revisions, appends events, updates synchronous projections, writes
outbox rows the events request, records the command receipt and the inbox entry that caused it;
commits; then notifies reactors (never replayed). An architecture rule pins this (foundation §3).

SQLite: WAL, `foreign_keys=ON`, `synchronous=FULL`, one writer connection, `busy_timeout`.
Migrations are forward-only, raw SQL, never edited once released. Secrets never enter the journal.

### 5.3 Events

Immutable, past-tense, `#[non_exhaustive]` structs in `mayfly-control-domain::events`, each with
`EVENT_TYPE` and `SCHEMA_VERSION`, registered in one registry, with a **golden JSON fixture** per
event and version. Evolution: add optional fields or new event types; never rename; pure
upcasters for old versions; an unknown event type in the journal puts the daemon in **safe mode**
(it cannot prove it understands ownership).

### 5.4 Commands, queries, reactors

- **Commands** (`mayfly-control::commands`): `ReserveSlot`, `StartAttempt`, `RecordObservation`,
  `RetireAttempt`, `QuarantineAttempt`, `DrainPool`, `ResumePool`, `UpdateDemand`, `OpenSession`,
  `RecordMessage`, … Each has a `command_id` (idempotency via receipts). Commands never read
  projections and never return read DTOs.
- **Queries** (`mayfly-control::queries`): read projections only (`pool_status`, `slot_status`,
  `vm_inventory`, `attempt_timeline`, `pending_directives`, `session_status`, `recent_failures`).
  Queries never import write ports.
- **Reactors / process managers:** the `AttemptSaga` turns lifecycle events into the next command
  or directive; the `Reconciler` compares desired capacity with projections and observations and
  issues commands; the `OutboxRelay` sends pending directives over the link with backoff. Reactors
  are idempotent and restartable from the journal.

### 5.5 Projections

Each projection is a versioned spec (`name`, `spec_version`, `tables`, `apply(event)`), rebuildable
stream by stream; it keeps a per-stream applied revision, and on start each stream's tail after it
is applied (no global watermark, ADR 0021). A change of meaning is a
new spec version, rebuilt alongside. `mayfly recover --rebuild-projections` exists and is tested.
Projection rebuild equals live projection (contract test).

---

## 6. Attempt lifecycle (control-plane view)

One **attempt** = one VM name, one generation, at most one GitHub job.

```text
Reserved → Cloning → Cloned → Booting → Booted → Probing → Warm
   Warm → Activating (JIT minted, LaunchRunner sent) → Listening → Busy → Finished
   Finished|Warm(retired)|Failed → Stopping → Deleting → Deleted (slot released)
   any active state → Unknown (outcome ambiguous) → resolved by observation
   Cloning|Booting|Probing|Activating → Backoff → (retry same name) | Quarantined
   Warm|Listening → Draining → Stopping;   Busy + drain → wait for Finished
```

Rules (each pinned by a test and by `formal/lifecycle.qnt`):

1. State reflects an **observed fact**, never an attempted command (persist intent, send directive,
   transition on observation).
2. `Busy` never returns to `Warm`; `Finished` never becomes `Activating` (I06).
3. A retry reuses the **same VM name and generation** so a lost reply cannot create a duplicate
   clone (I13); a new attempt gets a new generation.
4. `Unknown` leaves only on an observation (inventory, GitHub runner state); timeouts never
   convert `Unknown` to success or to `Deleted` (I10).
5. `Quarantined` is terminal for automation: slot held, operator decides via `mayfly recover`.
6. Every State × Stimulus pair is either a defined transition or a defined refusal (exhaustive
   matrix test, no wildcard arms).

The **data plane** has its own small FSM per directive (`Received → Executing → Done |
Refused | Unknown`) and per VM supervisor (`Absent → Present → Running → Stopped`), observed from
the provider rather than remembered.

---

## 7. GitHub integration

### 7.1 Auth

GitHub App (RS256 JWT → installation token), key from the secret store (file `0600` or Keychain).
Tokens live in memory only, are refreshed before expiry, never logged (`Secret<T>` with redacting
`Debug`/`Display`, a test proves it). Permissions are verified against a disposable installation
(owner task), never guessed.

### 7.2 Scale Set session

`Unconfigured → Authenticating → ScaleSetEnsured → SessionOpening → SessionActive ⇄ Refreshing |
Backoff | Degraded → Draining → Closed`. One listener per scale set (local lease).

Processing loop (the poller never waits on VM work):

1. Long-poll with `lastMessageId` and the **deliverable** `maxCapacity`.
2. For `JobAvailable`, `AcquireJobs` for the request ids we intend to serve (idempotent retry).
3. In one transaction: inbox `(session, message_id)`, latest `statistics`, normalised lifecycle
   events, and the new **desired count**. A redelivered message is a no-op.
4. Only then `DeleteMessage` (ack). Ack failure is retried; it never re-reserves.
5. Empty poll → still reconcile.

**Demand:** `desired = min(C, TotalAssignedJobs + warm_registered)` where `C` is admissible capacity.
Never count `JobAssigned` messages (≤50 per batch); never add `TotalRunningJobs` (already in the
total). A warm *unregistered* VM is not a GitHub-ready runner.

### 7.3 Classic mode

`config.sh --ephemeral` with a registration token fetched *after* guest readiness. A smoke-test and
fallback mode behind the same `RunnerRegistry` port; no demand signal (fixed `min_ready`).

### 7.4 Protocol fidelity

The Scale Set API is in public preview. The adapter is versioned; recorded request/response
fixtures (redacted) are the contract; `mayfly-fake-github` implements the wire behaviour
independently (redelivery, 50-message cap, session expiry, 401/403/429 + Retry-After), and a gated
live test runs against a disposable repository (owner-provisioned).

---

## 8. Data plane

### 8.1 VmProvider port

```rust
pub trait VmProvider: Send + Sync {
    fn inventory(&self) -> impl Future<Output = Result<Vec<VmObservation>, VmError>> + Send;
    fn clone_vm(&self, image: &ImageRef, name: &OwnedVmName) -> impl Future<Output = Result<(), VmError>> + Send;
    fn start(&self, name: &OwnedVmName, opts: &RunOptions) -> impl Future<Output = Result<RunHandle, VmError>> + Send;
    fn stop(&self, name: &OwnedVmName, grace: Duration) -> impl Future<Output = Result<(), VmError>> + Send;
    fn delete(&self, name: &OwnedVmName) -> impl Future<Output = Result<(), VmError>> + Send;
    fn ip(&self, name: &OwnedVmName) -> impl Future<Output = Result<Option<IpAddr>, VmError>> + Send;
}
```

`VmError` classifies `Definite` (nothing happened) vs `Ambiguous` (may have happened). Providers:
**Tart** (v0.x), fake in-memory (testkit), `mayfly-fake-tart` binary (e2e and fault lab on Linux),
Orchard or direct Virtualization.framework (later, ADR first).

### 8.2 Tart adapter

argv only (never `sh -c`); version handshake and capability probe; `tart list --format json`
parsed against version-pinned fixtures; every call has a deadline, its own process group, bounded
captured output and redaction; an ambiguous result is re-inspected. Dedicated `TART_HOME` per
deployment; cache GC explicit (`TART_NO_AUTO_PRUNE`). Softnet is optional and is *not* an egress
allowlist.

### 8.3 Guest transport and runner launch

`tart exec` where the image supports it, else SSH with a pinned per-image host key (never
`StrictHostKeyChecking no`). The runner binary is baked into the image (pinned version, self-update
off). The JIT config is passed over stdin to a fixed, reviewed launcher script; never interpolated
into a shell string, never written to disk unencrypted, never logged. Runner diagnostics are
exported before deletion (bounded, redacted).

### 8.4 Ownership

A managed VM name is `mf-<pool>-<host>-s<slot>-g<generation>-<attempt_short>`, where pool and
host ids are `[a-z0-9]{1,16}` so the `-` separators are unambiguous; ownership requires
the journal record **and** the exact name **and** the generation. The data plane refuses
`DeleteVm` without an `ownership_proof` that matches its fence; the control plane never emits one
for a VM it has no record of. A `mf-*` VM unknown to the journal is reported for operator review,
never deleted (I04, F24).

---

## 9. Capacity and admission (pure policy)

Inputs: host resources (observed), per-pool limits, global slot limit, macOS-guest limit, reserved
memory/disk, attempts in `Reserved..Deleting` (all count against capacity, I03), drain flags,
GitHub demand. Output: a `CapacityReport { physical_slots, starting, busy, warm, draining,
admissible, reported_max }` and a list of admit/retire decisions. `reported_max` sent to GitHub never
exceeds what can be delivered (I07). Low disk → admission stops and `reported_max` drops (F17).

---

## 10. Recovery

Startup sequence (composition): lock state dir → open/migrate journal → replay and verify
projections → new host epoch (no VMs launched yet) → data-plane handshake and inventory → GitHub
session → reconcile every `Unknown` outbox entry by observation → classify each owned VM (adopt /
continue / cleanup / wait / quarantine) by the **recovery decision table** (pure, in
`mayfly-control-domain::recovery`, model `formal/recovery.qnt`) → resume admission.

| Journal | Provider | GitHub | Decision |
|:--|:--|:--|:--|
| Cloning | owned clone exists | — | adopt, continue |
| Cloning | absent | — | retry same name (budgeted) |
| Listening/Busy | running | online/busy | adopt and monitor; never delete |
| Busy | absent | offline/unknown | job outcome unknown; report; release slot after GitHub check |
| Deleting | absent | absent | confirm deletion, release slot |
| Deleting | present | active | suspend cleanup, inspect |
| no record | `mf-*` name | any | report for operator review; never delete |
| any | — | — | journal untrusted (unknown event, failed verification) → safe mode |

Safe mode: no new VMs, no destructive action; `mayfly recover --dry-run` prints a plan,
`--apply <plan-id>` executes exactly that plan.

Graceful drain: stop admission, lower `reported_max`, retire idle/warm, wait for busy up to a
timeout, close the session. `--force` is a separate, explicit operator action.

---

## 11. Operator interface

`mayflyd [--config path] [--role all|control|data]`. `mayfly` talks to the daemon over a `0600`
Unix socket:

```text
mayfly status [--json]            mayfly pool list|drain|resume|pause <name>
mayfly vm list [--owned-only]     mayfly vm inspect <attempt>     mayfly vm logs <attempt>
mayfly events --follow [--since]  mayfly doctor [--json]          mayfly config validate <path>
mayfly recover --dry-run | --apply <plan-id>                      mayfly diagnostics export --redact
mayfly image verify <manifest>    mayfly metrics
```

Config (TOML, `schema_version = 1`): `state_dir`, `[github]` (mode `scaleset|classic`, scope URL,
App client id, installation id, `private_key_ref`), `[host]` (slots, macOS guest limit, memory and
disk reserves), `[[pools]]` (name/labels, image manifest, guest OS, cpu/memory, `min_warm`,
`max_running`, `allow_untrusted_prs`), `[provider]` (`tart` + `tart_home`). Validated on load;
secrets only by reference.

---

## 12. Observability

`tracing` with JSON output; fixed `snake_case` event names, variable data in fields
(`attempt_id`, `pool`, `generation`, `directive_id`, `session_id`, `error_class`). A healthy daemon
logs (almost) nothing at INFO: start/stop, migrations, config changes only. Metrics (bounded
labels, no ids): `mayfly_attempt_state{pool,state}`, `mayfly_provision_seconds{pool,stage}`,
`mayfly_directives_total{kind,outcome}`, `mayfly_outbox_backlog{status}`, `mayfly_capacity{pool,kind}`,
`mayfly_github_requests_total{op,status_class}`, `mayfly_github_assigned_jobs`,
`mayfly_last_poll_age_seconds`, `mayfly_orphans`, `mayfly_disk_free_bytes`, `mayfly_reconcile_errors_total{kind}`.
One trace spans poll → reconcile → clone → boot → probe → JIT → runner → job → stop → delete.

---

## 13. Security

Host service runs as a non-root user; state dir `0700`, DB/socket `0600`; single instance via
`flock`. No host directory, socket, key or credential is shared with a guest. JIT configs are
single-use, in memory, delivered over stdin. No token in argv, env of child processes (except the
guest launcher's stdin), logs, metrics, status, or diagnostics (redaction tests + secret scan).
Threat model (`doc/security.md`, task SEC-01): untrusted workflow code, guest-to-host network,
supply chain of images and runner, operator mistakes.

---

## 14. Roadmap (phases; tasks.md has the order)

| Phase | Outcome | Gate |
|:--|:--|:--|
| 0 Foundation | Workspace, gate, architecture law, CI, process | `cargo xtask check` green in CI; forbidden edge fixtures fail |
| 1 Kernel | Pure domain, protocol, lifecycle FSM, aggregates, models | exhaustive FSM matrix, golden events, Quint models check |
| 2 Journal | SQLite journal, AggregateStore, projections, outbox/inbox | journal contract on memory + SQLite + reopened |
| 3 Planes | CP reconciler/saga, DP executor, inproc link | **walking skeleton**: `mayflyd` + fake-tart + fake GitHub runs a job end to end on Linux |
| 4 GitHub | Scale Set client, listener, JIT, classic | fake-GitHub contract, redelivery/ack-loss tests |
| 5 Real world | Tart adapter, guest transport, host probes, launchd | (owner) real Tart smoke on the Mac |
| 6 Evidence | Conformance replay, fault lab F01–F31, mutants, fuzz | core faults PASS, curated mutants killed |
| 7 Operate | Packaging, signing, runbooks, canary next to Tartelet | (owner) canary on a real repository |
| 8 Later | Warm pools tuning, macOS images, UDS split roles, multi-host | ADR per step |
