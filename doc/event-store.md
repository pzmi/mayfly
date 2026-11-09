# mayfly: the event store

The design of mayfly's event store: a storage-neutral, stream-scoped journal with command
receipts, an inbox, durable at-least-once delivery, inline projections and snapshots, whose first
adapter is SQLite and whose second is PostgreSQL. The owner's decisions E1–E9 of 2026-11-09 are applied (§8.3; ADRs 0021, 0034,
0035). This document owns *what*; [`tasks.md`](tasks.md) owns *order*. It amends plan §5 and
foundation §6 where they assumed SQLite (§1.4 lists each amendment). ADR 0021 records the decision.

A bare § is a section of this document; "plan", "fnd" and "ver" are `plan.md`, `foundation.md`
and `verification.md`.

---

## 1. Decision and scope

### 1.1 The decision

1. **Storage-neutral by design.** The journal, its receipts, inbox, delivery ledger,
   snapshots and projection store are Rust traits in the application layer (`mayfly-control`),
   held to one contract suite each (`mayfly-testkit::contracts`). No correctness argument names a
   storage engine.
2. **SQLite is the first adapter** (`mayfly-adapter-sqlite`, D7): the product ships on it.
   **PostgreSQL is the second** (`mayfly-adapter-postgres`, ES-09), built later and run by the
   same contract suites, the same conformance traces and the store lab in CI. It is not a
   deployment target in v0.x; it exists to keep the design honest and to leave the multi-host
   control plane (plan §14, phase 6) a deployment change rather than a rewrite.
3. **Why this way round.** A store built on SQLite assumptions (one global writer, a global
   commit-ordered position, consumers reading "everything after position N") does not survive
   PostgreSQL, where concurrent writers commit out of position order: a global position skips
   events that commit late, a global lock becomes the throughput ceiling, and every reader needs
   a per-stream redesign. A design built for concurrent writers runs unchanged on SQLite (SQLite
   simply serialises what PostgreSQL runs in parallel); the reverse does not hold. So mayfly is
   storage-neutral from the start, SQLite first and PostgreSQL later, and no SQLite-only
   assumption leaks into the design.
4. **The test of neutrality** is mechanical, not a review item: every invariant of §2 is pinned by
   a contract case that runs on the memory store, SQLite and PostgreSQL, and by model traces
   replayed on all three (§7). A property that only holds because SQLite has one writer fails the
   PostgreSQL run, and the SQLite contract runs with two connections and two handles to catch it
   earlier (§4.3).

### 1.2 Which mechanisms mayfly builds

| Mechanism | Built | Why |
|:--|:--|:--|
| One stream per aggregate instance, per-stream optimistic concurrency on a stream head row | yes | the core of the design; it is what makes writers on different streams independent |
| No global journal order (no global position, no read-all cursor, no global counter row) | yes | the reason the store is storage-neutral (§1.1.3) |
| Globally unique event ids enforced by the store | yes | idempotency key of the append; catches a replayed batch |
| Command receipts with a canonical-intent digest | yes, with a plain digest | mayfly mints its own command ids; a digest (not a keyed MAC) suffices because commands carry no personal data |
| Inbox for external messages | yes | GitHub redelivers until acked; data-plane observations are at-least-once (plan §3.2) |
| Stream epochs (`required_epoch`, per-aggregate supported epoch) | yes | turns "this build cannot decide this stream correctly" into a per-stream refusal instead of a wrong decision |
| Event-kind manifest | yes | the start-up unknown-kind check reads it instead of scanning the journal |
| Inline projections in the append transaction, stream-local | yes | read models never lag the journal for the fields the control loop uses |
| Projection specs with versions; change of meaning is a new spec | yes | |
| Durable delivery: dispatch marks, per-consumer per-stream checkpoints, leases with fencing tokens, no skip | yes | replaces the outbox: directives are `DirectiveIssued` events (§5, E2) |
| Versioned snapshots (format, reducer version, checksum, folded kinds), asynchronous | yes, later (ES-08) | `host-*`, `pool-*` and `scaleset-*` streams grow for the daemon's lifetime |
| Online shadow-generation rebuild with dual writes and a cutover fence | **yes** (E9, ES-12) | a projection version change never blocks admission; the store stays correct when a second writer appears (multi-host edition) |
| Erasure of personal data | **no** | mayfly stores no personal data or free text (fnd §6.1); secrets never reach the store (D13). Should that change, a new ADR adds a side store, never a journal rewrite |
| Advisory locks on the write path, table partitioning, lock tuning for a high write rate | **no** | the control plane writes a few events per second (§1.3); mayfly's PostgreSQL adapter uses row locks only (§4.2) |
| Jepsen-style harness with an independent controller and four fail-closed verdicts | yes, as a store workload of the fault lab (§7.5), checked by Knossos (E5) | |

### 1.3 Non-goals

No external event store, broker or CDC daemon. No cross-stream consumer cursor. No blind write
(`expected = any` does not exist in mayfly; every write is a decision against a read state). No
claim about throughput: the control plane writes a few events per second.

### 1.4 Amendments to existing documents (land with ES-01..ES-03)

- plan §5.2: the table list becomes §4.1's schema; "one SQLite transaction" becomes "one store
  transaction"; `sequence`, `read all` and the projection `watermark` go; the outbox becomes
  `DirectiveIssued` events and a delivery consumer (§5, E2).
- plan §5.5 and fnd §6.7: "rebuild from sequence 0" becomes "rebuild by enumerating stream heads".
- fnd §6.3: "one SQLite transaction" becomes "one store transaction".
- D7: "Persistence is SQLite" becomes "the first persistence adapter is SQLite; the store is
  storage-neutral (ADR 0021)".
- ver §5's `Journal` row gains the modes and cases of §7.1; ver §6.1 gains the `event_store` model.

---

## 2. Concepts and invariants

### 2.1 Vocabulary

- **Stream**: the events of one aggregate instance, `<aggregate>-<id>` (plan §5.1). Revisions
  are `0, 1, 2, …`; an absent stream has no head (distinct from an empty one, which never exists:
  a refused or empty append creates no head).
- **Stream head**: one row per stream: `revision` (of its last event), `aggregate`,
  `required_epoch`. The only row a writer of that stream contends on.
- **Unit of work**: what one command commits atomically (§3.2).
- **Expected revision**: `NoStream` or `Exactly(r)`. Every write and every validated read carries
  one.
- **Receipt**: the recorded outcome of an accepted command, keyed by `command_id`.
- **Inbox entry**: `(source, message_id)` of an external message a unit consumed.
- **Consumer**: a durable, at-least-once reader of every stream (the directive relay, the saga,
  asynchronous projections). It has a **mark** (dirty row), a **checkpoint** and a **lease**
  per stream.
- **Store clock**: the clock that judges lease expiry. The database's own clock on PostgreSQL;
  the adapter's injected `StoreClock` on SQLite and in memory. Never a worker's clock.

### 2.2 Invariants

Each invariant names how it is pinned:
contract cases (§7.1, ids `JC-`, `DC-`, `SC-`, `PC-`), the `event_store` model invariant and
scenario (§7.3), store-lab workloads (`SW`, §7.5) and the broken implementations that must fail
(§7.6). "All stores" means memory, SQLite (all modes) and PostgreSQL.

**ES-I1 Atomic unit**. A unit commits all of: every stream's batch and head
revision and epoch, the event identities, the manifest rows, the receipt, the inbox entry, the
inline projection rows, the dispatch marks and the snapshot request; or none of them.
Pinned by JC-10, JC-23, PC-03, DC-03, SC-03 on all stores; model `ES_I1_atomicUnit`
(`batchFailsAfterSecondEvent`, `multiStreamConflict`); SW3; `PartialCommitJournal`.

**ES-I2 Stream order**. A stream's revisions are consecutive from 0 with no committed
hole; the head's revision equals the highest committed revision; of two units expecting the same
revision of a stream at most one commits. Pinned by JC-02..JC-06, JC-21; model `ES_I2_streamOrder`
(`lostRace`, `noStreamVersusEmpty`, `headQueue`); SW1; `LooseRevisionJournal`, `CachedHeadJournal`.

**ES-I3 Independent streams**. No unit is refused because of another stream it does not
name. On PostgreSQL additionally no two units on disjoint streams wait on each other (structural
half). SQLite may serialise them (one write transaction at a time); it may never refuse one for
it. Pinned by JC-22 (outcome half, all stores; blocking half, PostgreSQL only); model
`ES_I3_independentStreams` (`independentStreams`, `independentHeldWriters`); SW2;
`GlobalLockJournal`.

**ES-I4 Global event identity**. An `event_id` is bound to at most one
`(stream, revision)`; `lookup_event` resolves it; every event has its identity and every identity
its event. Pinned by JC-07, JC-08, JC-16; model `ES_I4_globalEventIdentity`
(`duplicateAcrossStreams`, `repeatedWithinBatch`); SW4.

**ES-I5 Command identity**. A repeated `command_id` with the same
intent digest is answered from its receipt with no new write, even after its streams moved; under
a different digest it is refused `CommandIdReused`, revealing nothing else; at most one outcome
commits per command id. A refusal writes a **refusal receipt bound to the stream and revision it
was decided at** (E3, ADR 0034): a repeat while that stream is still at that revision is answered
with the recorded refusal; once the stream moved, the repeat is decided afresh, never replayed, and
its outcome replaces the refusal receipt. A zero-event acceptance writes a receipt like any other.
Pinned by JC-12..JC-14, JC-28; model
`ES_I5_commandIdentity` (`receiptReplay`, `racingCommands`, `refusedThenAccepted`, `refusalReplayedAtSameRevision`); SW6, SW8;
`ForgetfulReceiptsJournal`, `IntentBlindReceipts`.

**ES-I6 Message identity**. An inbox key is consumed by at most one committed unit; a
redelivered message is answered `DuplicateMessage` with no write; a unit that only records the
entry (nothing to do) is legal. Pinned by JC-15; model `ES_I6_messageIdentity`
(`inboxRedelivery`); SW10; `ForgetfulInbox`; ver §9 mutant 7.

**ES-I7 History preservation**. No committed event, identity or head-revision step
changes or disappears; `read_stream` returns the full history whatever snapshots exist. Pinned by
JC-24, JC-27 (adapters' triggers refuse `UPDATE`/`DELETE` on journal tables); model
`ES_I7_historyPreservation`; the SW audits; `ARCH-journal-sql`.

**ES-I8 Snapshot equivalence**. For a stored snapshot at revision s, folding the tail
after s onto it equals folding the whole stream; a stale, corrupt, foreign-format or
unsupported-epoch snapshot is ignored, never trusted; a stream's stored snapshot revision per key
never decreases. Pinned by SC-01..SC-11; a property over random histories; model
`ES_I8_snapshotEquivalence` (`snapshotAndTail`, `staleSnapshotWorker`, `corruptSnapshot`,
`snapshotFormatsCoexist`); `BlindSnapshotUpsert`, `ResealingSnapshots`.

**ES-I9 Inline projection atomicity**. An accepted unit's inline projection rows are
visible with its events; a refused or failed unit leaves none. Pinned by PC-03, PC-04.

**ES-I10 Projection locality**. An inline projection reads and writes only rows keyed by
a stream the unit appends to. This is what makes rows computable outside the transaction (their
inputs are guarded by the stream's expected revision), per-stream rebuild sound, and no
projection row a cross-stream contention point. Pinned by PC-05 (the store refuses a write keyed
by another stream: `Malformed`) and `ARCH-projection-locality`.

**ES-I11 Rebuild equivalence** (online since E9; mayfly I16). A rebuilt projection
equals the live one; a rebuild enumerates stream heads, folds each stream from revision 0 into a
shadow generation while live units dual-write it, and records the revision it last *applied*, never
one read from a head; the cutover fence makes the switch atomic and refuses a unit computed for
the old generation set. Pinned by PC-01, PC-02, PC-06, PC-07..PC-09; model scenarios
`shadowRebuildDualWrite`, `cutoverFence`; property
"rebuild equals live" over random histories; F21; ver §9 mutant 18.

**ES-I12 Epoch safety**. A stream's `required_epoch` never decreases; a build never
appends to a stream whose epoch exceeds its aggregate's supported epoch; a batch requiring more
than the build supports is refused before any write; reads are never refused for an epoch.
Pinned by JC-18; model `ES_I12_epochSafety` (`epochRaise`, `refusedAboveEpoch`).

**ES-I13 Nothing secret or free-form at rest**. No secret, JIT configuration,
token, credential or upstream free text is stored in events, receipts, inbox entries, snapshots,
projection rows, dispatch rows or store errors. Pinned by `ARCH-no-secret-serialize` (a
`Secret<T>` has no `Serialize`, D13), the redaction test (RT-04), the secret scan of a store file
after each SW run, and JC-19 (store errors carry codes and ids only).

**ES-I14 Durable delivery**. For every consumer and stream: the checkpoint never passes
the head and never moves back; every committed event is delivered at least once; within a claim a
stream's events are delivered in revision order; a lease that expired or was taken over cannot move
the checkpoint, clear a mark or start an effect; each takeover hands out a strictly higher fence.
Pinned by DC-01..DC-25; model `ES_I14_durableDelivery` (`bootstrapAfterWrites`,
`markDuringProcessing`, `staleWorkerFenced`, `workerCrashRedelivers`); SW9; `UnfencedLedger`,
`ClearAllMarksLedger`.

**ES-I15 No skip**. For every consumer and every rebuild, the applied revisions of a
stream are a prefix of its committed revisions. No step drops a pending revision whatever its age:
no timeout-based or gap-based skip exists anywhere. A poison event stops its stream for that
consumer until the consumer is fixed. Pinned by DC-08, DC-18; model `ES_I15_noSkip`
(`commitBetweenFetchAndApply`, `failingConsumerNoSkip`, negative world `headTrustingReader`);
`SkippingLedger`, `HeadTrustingWorker`.

**ES-I16 Lock order**. Every store transaction takes its locks in the one order
of §4.2, so no reachable state has a wait-for cycle; every lock wait is bounded and ends in a
definite `Busy` refusal. Pinned on PostgreSQL by the model's split-transaction scenarios replayed
with the driver checking each waiter's blockers (§7.3), and by JC-22's two-unit deadlock case
(units naming the same two streams in opposite order both finish); trivially true on SQLite, where
the case still runs.

**ES-I17 No global order**.
Nothing reads the journal in a global order: the port has no read-all, no global position and no
cursor over time; every consumer and rebuild progresses per stream; no adapter query that feeds a
decision orders by a time, global position or rowid. Pinned by the port's shape (no such method
exists), `ARCH-no-global-order` (§4.3) and its negative fixtures.

**ES-I18 Honest outcomes**. Every `append` error except `OutcomeUnknown` means nothing was committed; an outcome the
store could not learn (the commit's reply was lost) is reported `OutcomeUnknown`, never as success
or as a definite failure, and is resolved only by reading (receipt, `lookup_event`). Refusals are
classified by constraint name or SQLSTATE/extended code, never by message text. Pinned by JC-25
(fault-injecting adapters), JC-09 (refusal order), SW6, SW7; `AckBeforeCommitJournal`.

**ES-I19 Durability**. A unit reported committed survives
a crash of the process (SQLite) or of the server (PostgreSQL). Each adapter verifies its
durability settings from the store itself at open and refuses to open a production store without
them (§4.1, §4.2). Pinned by JC-26, SW7.

Mapping to mayfly's system invariants: ES-I11 is I16; ES-I6 and ES-I1 carry I12 (a durable record
precedes the GitHub ack); ES-I14 and ES-I18 carry I10 (an ambiguous effect stays `Unknown`).

---

## 3. Storage-neutral ports

All ports live in `mayfly-control::store` (application layer), are native `async fn`/RPITIT
traits used through generics (D8), take and return values from `mayfly-domain` and
`mayfly-control-domain`, and name no format beyond already-encoded payload strings. Implementations:
`mayfly_testkit::store::Memory*` (full transactional semantics), `mayfly-adapter-sqlite`,
`mayfly-adapter-postgres`. Signatures below are sketches; names that ES-01 already chose win where
they mean the same.

### 3.1 Values

```rust
pub struct StreamId(String);                 // "<aggregate>-<id>", validated
pub struct Revision(u64);                    // 0..=i64::MAX (both engines store i64)
pub enum Expected { NoStream, Exactly(Revision) }
pub struct StreamHead { pub stream: StreamId, pub revision: Revision,
                        pub aggregate: &'static str /* stored as text */, pub epoch: u32 }
pub struct NewEvent { pub event_id: EventId, pub kind: StoredKind,
                      pub required_epoch: u32, pub payload: Payload }
pub struct RecordedEvent { pub stream: StreamId, pub revision: Revision, pub event_id: EventId,
                           pub kind: StoredKind, pub payload: Payload, pub recorded_at: Millis }
pub struct StoredKind { pub event_type: String, pub schema_version: u32 }  // may be unknown
pub struct IntentDigest([u8; 32]);           // canonical encoding of the command body, hashed
pub struct InboxKey { pub source: InboxSource, pub message_id: MessageId }
pub struct ConsumerName(String);             // [a-z][a-z0-9_]*
pub struct LeaseFence(u64);                  // strictly increases per (consumer, stream)
```

`recorded_at` is informational (the caller's `Clock`, ADR 0020); nothing orders or decides by it.
Payloads are JSON text produced by the kernel codec; the store never parses them.

### 3.2 The unit of work

```rust
pub struct UnitOfWork {
    pub appends: Vec<StreamAppend>,          // { stream, aggregate, expected, events: Vec<NewEvent> }
    pub reads: Vec<ReadCondition>,           // { stream, expected }: validated, not written
    pub receipt: Option<NewReceipt>,         // { command_id, command_type, intent: IntentDigest }
    pub inbox: Option<InboxKey>,
    pub projections: Vec<ProjectionWrite>,   // { spec, version, stream, key, row: Option<Payload> }
    pub snapshot_request: Option<SnapshotRequest>,
    pub supported_epochs: SupportedEpochs,   // this build's per-aggregate supported epoch
    pub recorded_at: Millis,
}
```

- Streams are checked and locked in `StreamId` order regardless of the order given (§4.2); a
  stream may not appear twice, nor in both `appends` and `reads` (`Malformed`).
- `reads` is the validated read set: a decision that read another stream (a pool's configuration
  while writing an attempt) names it with the revision it read; the unit is refused if it moved.
  A `NoStream` read is invalidated by a concurrent create.
- Dispatch marks are **not** in the unit: the store writes one per *registered* consumer (a table,
  not this build's code) for every appended stream, so a build marks consumers it does not run
  (§6.4).
- A unit with no events is legal only to record an inbox entry (a message that required nothing)
  or a refusal receipt (E3; it then carries the refused stream in `reads` at the revision decided
  at, so a moved stream refuses the receipt); it writes no head.
- The receipt's recorded outcome is the `Appended` value: per stream the first and last revision
  and the event ids.

### 3.3 `Journal`

```rust
pub trait Journal: Send + Sync {
    fn append(&self, unit: UnitOfWork) -> impl Future<Output = Result<Appended, AppendError>> + Send;
    fn head(&self, stream: &StreamId) -> impl Future<Output = Result<Option<StreamHead>, StorageError>> + Send;
    fn read_stream(&self, stream: &StreamId, after: Option<Revision>, through: Option<Revision>)
        -> impl Future<Output = Result<Vec<RecordedEvent>, StorageError>> + Send;
    fn lookup_event(&self, id: EventId) -> impl Future<Output = Result<Option<(StreamId, Revision)>, StorageError>> + Send;
    fn receipt(&self, id: CommandId) -> impl Future<Output = Result<Option<CommandReceipt>, StorageError>> + Send;
    fn inbox_contains(&self, key: &InboxKey) -> impl Future<Output = Result<bool, StorageError>> + Send;
    /// Heads in `StreamId` order after `after`: enumeration by key, never by time or position.
    fn streams(&self, after: Option<&StreamId>, limit: u32) -> impl Future<Output = Result<Vec<StreamHead>, StorageError>> + Send;
    fn stored_kinds(&self) -> impl Future<Output = Result<Vec<StoredKind>, StorageError>> + Send;
}

pub enum AppendError {
    Malformed(MalformedUnit),                         // before any storage access
    EpochAboveBuild { stream: StreamId },             // a batch needs more than this build supports
    CommandReplayed(Box<CommandReceipt>),             // same id, same intent: the recorded answer
    CommandIdReused { command_id: CommandId },        // same id, other intent
    DuplicateMessage(InboxKey),
    WrongExpectedRevision { stream: StreamId, expected: Expected, actual: Option<Revision> },
    StreamEpochUnsupported { stream: StreamId, epoch: u32 },
    DuplicateEventId(EventId),
    Busy,                                             // a bounded lock wait ran out; definite
    Storage(StorageError),                            // definite: rolled back
    OutcomeUnknown(StorageError),                     // the commit's reply was lost
}
```

The refusal order above is part of the contract (JC-09) and of the model: two stores that refuse
the same unit for different reasons diverge in conformance replay, which compares the
refusal itself, not only that one occurred. `CommandReplayed` is an `Err` so that a
caller cannot mistake it for a fresh commit; `AggregateStore` turns it into the receipt's answer.

No `read_all`, no global position, no `head()` of the journal (ES-I17). A journal-wide view for
the operator (`mayfly events --follow`, plan §11) is built in §5.6 without one.

### 3.4 `DeliveryLedger`, `Consumer`, `DeliveryWorker`

```rust
pub trait DeliveryLedger: Send + Sync {
    fn registered(&self) -> impl Future<Output = Result<Vec<ConsumerState>, StorageError>> + Send;
    fn register(&self, c: &ConsumerName) -> impl Future<Output = Result<(), StorageError>> + Send;
    /// Marks one page of existing streams for a new consumer; returns the last key, None at the end.
    fn bootstrap(&self, c: &ConsumerName, after: Option<&StreamId>, limit: u32)
        -> impl Future<Output = Result<Option<StreamId>, StorageError>> + Send;
    /// Claims up to `limit` marked streams whose lease is absent or expired (store clock),
    /// optionally only `only`; each claim carries a fresh token and the next fence.
    fn claim(&self, c: &ConsumerName, ttl: Millis, limit: u32, only: Option<&StreamId>)
        -> impl Future<Output = Result<Vec<Claim>, StorageError>> + Send;
    fn renew(&self, claim: &Claim, ttl: Millis) -> impl Future<Output = Result<(), LeaseLost>> + Send;
    /// Moves the checkpoint to `to` (only up), clears the mark only up to `to`, extends the lease.
    fn advance(&self, claim: &Claim, to: Revision, ttl: Millis) -> impl Future<Output = Result<(), LeaseLost>> + Send;
    /// Ends the claim; `retry_after` keeps the mark unclaimable until store time passes it.
    fn release(&self, claim: &Claim, retry_after: Option<Millis>) -> impl Future<Output = Result<(), LeaseLost>> + Send;
    fn retire(&self, c: &ConsumerName) -> impl Future<Output = Result<(), StorageError>> + Send;
}
pub struct Claim { pub consumer: ConsumerName, pub stream: StreamId, pub token: LeaseToken,
                   pub fence: LeaseFence, pub checkpoint: Option<Revision>, pub marked: Revision }
pub trait Consumer: Send + Sync {
    const NAME: &'static str;
    fn handle(&self, d: Delivery<'_>) -> impl Future<Output = Result<(), ConsumerError>> + Send;
}
pub struct Delivery<'a> { pub event: &'a RecordedEvent, pub fence: LeaseFence,
                          pub key: DeliveryKey /* (consumer, event_id): the effect's idempotency key */ }
```

`DeliveryWorker::drain(consumer, only)` claims, reads `read_stream(after = checkpoint, through =
marked)`, and for each event renews the lease, calls `handle`, then `advance`s; a `handle` error
releases with a backoff and leaves the checkpoint; `LeaseLost` ends that claim, not the drain.
`catch_up()` registers this build's consumers, finishes any bootstrap and drains each. Backoff
jitter comes from the `Rng` port; nothing sleeps on a wall clock.

### 3.5 `SnapshotStore` (ES-08)

```rust
pub struct SnapshotKey { pub aggregate: &'static str, pub format: u32, pub reducer: u32 }
pub struct StoredSnapshot { pub stream: StreamId, pub key: SnapshotKey, pub revision: Revision,
    pub epoch: u32, pub kinds: Vec<(StoredKind, bool /* folded */)>, pub state: Payload, pub checksum: [u8; 32] }
pub trait SnapshotStore: Send + Sync {
    fn load(&self, stream: &StreamId, key: &SnapshotKey) -> impl Future<Output = Result<Option<StoredSnapshot>, StorageError>> + Send;
    /// Writes only when `s.revision` is above the stored one for its key; returns whether it wrote.
    fn save_if_newer(&self, s: StoredSnapshot) -> impl Future<Output = Result<bool, StorageError>> + Send;
    fn requests(&self, after: Option<&StreamId>, limit: u32) -> impl Future<Output = Result<Vec<SnapshotRequest>, StorageError>> + Send;
    fn served(&self, stream: &StreamId, through: Revision) -> impl Future<Output = Result<(), StorageError>> + Send;
}
```

The request is written by the unit whose batch crosses the interval (default 256 events); a
worker folds to the requested revision through the aggregate's pure `evolve`, checks the folded
kinds, saves if newer, and marks the request served up to that revision only. `AggregateStore`
alone calls the restore path (checksum, key, epoch, kinds, contiguity of the tail), so no other
code can trust a snapshot.

### 3.6 Projections

```rust
pub trait ProjectionSpec {
    const NAME: &'static str;
    const VERSION: u32;                       // compatible change: bump; change of meaning: new NAME
    const SUBSCRIBES: &'static [EventKind];   // exact kinds, no catch-all
    /// Pure: this stream's current rows and one event in, row writes out (keys of this stream only).
    fn apply(rows: &StreamRows, event: &RecordedEvent) -> Vec<RowWrite>;
}
pub trait ProjectionStore: Send + Sync {
    fn rows(&self, spec: &str, stream: &StreamId) -> impl Future<Output = Result<StreamRows, StorageError>> + Send;
    fn get(&self, spec: &str, key: &str) -> impl Future<Output = Result<Option<Payload>, StorageError>> + Send;
    fn list(&self, spec: &str, after_key: Option<&str>, limit: u32) -> impl Future<Output = Result<Vec<(String, Payload)>, StorageError>> + Send;
    fn meta(&self) -> impl Future<Output = Result<Vec<ProjectionMeta>, StorageError>> + Send;
    fn rebuild(&self, spec: &str, version: u32) -> impl Future<Output = Result<RebuildHandle, StorageError>> + Send;
}
```

Projection rows are documents `(spec, generation, stream, key) → payload`: the adapters store
them without projection-specific SQL, so no projection can depend on an engine's dialect.
Cross-stream views (`pool_status` counting attempts) are computed at query time over stream-local
rows or maintained by a durable consumer (§5); they are never an inline write (ES-I10). A rebuild
writes a new (shadow) generation stream by stream (`RebuildHandle::apply_stream(stream, rows,
applied)`) **online** (E9): while it runs, every unit computes its inline rows for the active and
the shadow generation and writes both (dual writes); when every enumerated stream is done, a
cutover fence flips the active generation in one transaction, and a unit that computed rows for the
previous generation set is refused (`ProjectionGenerationMoved`, retried like
`WrongExpectedRevision`). Readers see the active generation only.

### 3.7 What `AggregateStore::save` does, and transaction scopes

`AggregateStore::save` (ES-03) is the only caller of `Journal::append` (`ARCH-single-append`):

1. Read the head; refuse a stream whose epoch exceeds this build's (`StreamEpochUnsupported`)
   **before** any receipt answer, so a rolled-back build never claims a newer
   build's outcome.
2. Read the receipt for `command_id`: same digest → its recorded answer; other digest →
   `CommandIdReused`.
3. Load: latest trusted snapshot plus tail, or the whole stream; decode; an unknown kind refuses
   the command and raises safe mode (§6.3).
4. `decide` **outside** any transaction; derive event ids from `(command_id, ordinal)` by a pure,
   golden-tested function, so a retried command derives the same ids; compute the intent digest;
   run inline projections over the stream's rows read at the expected revision.
5. `append` the unit. On `WrongExpectedRevision` only, `with_revision_retry` repeats steps 1–5
   (load and decide inside the closure; 3 attempts; full-jitter backoff through the `Rng` port).
   `Busy`, `Storage` and refusals are returned, never retried here. `OutcomeUnknown` is resolved
   by `receipt` (or `lookup_event` for a receipt-less unit) before answering.
6. After commit: telemetry and the reactor notification (at most once, a kick only, §5.5).

| Operation | Transaction | Isolation needed |
|:--|:--|:--|
| `append` | one write transaction | per-row write locks on heads (CAS) + unique keys; no range or predicate locks |
| `head`, `read_stream`, `lookup_event`, `receipt` | one statement each | statement snapshot |
| load = snapshot + tail | two statements | none: the tail must start at `snapshot.revision + 1` and end at the head it reports, else load whole |
| `claim`, `renew`, `advance`, `release` | one short write transaction each | row lock on the lease row; store clock read inside |
| bootstrap page, rebuild stream, snapshot save | one write transaction each | row locks on the rows written |
| migration | one transaction per migration | DDL transactional on both engines |

No operation needs `SERIALIZABLE`, and none may rely on it as its only guard (SSI has had
anomalies; a constraint or a CAS is the guard).

---

## 4. Adapter mapping

### 4.1 Logical schema (both adapters; constraint names identical)

| Table | Key | Columns | Notes |
|:--|:--|:--|:--|
| `stream_head` | `stream_id` | `aggregate`, `revision`, `required_epoch` | `stream_head_pkey` |
| `journal_identity` | `event_id` | `stream_id`, `revision` | `journal_identity_pkey`: global uniqueness |
| `journal_event` | `(stream_id, revision)` | `event_id`, `event_type`, `schema_version`, `payload`, `recorded_at` | `journal_event_pkey`; FK `(event_id, stream_id, revision)` → identity |
| `event_kind_manifest` | `(event_type, schema_version)` | `first_recorded_at` | insert-if-absent in the unit |
| `command_receipt` | `command_id` | `command_type`, `intent`, `outcome` | `command_receipt_pkey` |
| `inbox` | `(source, message_id)` | `recorded_at` | `inbox_pkey` |
| `delivery_consumer` | `consumer` | `registered_at`, `bootstrapped` | |
| `stream_dispatch` | `(consumer, stream_id)` | `marked`, `retry_at` | the mark |
| `consumer_checkpoint` | `(consumer, stream_id)` | `revision` | |
| `consumer_lease` | `(consumer, stream_id)` | `token`, `lease_until`, `fence` | fence never reset |
| `aggregate_snapshot` | `(stream_id, aggregate, format, reducer)` | `revision`, `epoch`, `kinds`, `state`, `checksum` | |
| `snapshot_request` | `stream_id` | `target` | target only rises |
| `projection_row` | `(spec, generation, key)` | `stream_id`, `payload` | index `(spec, generation, stream_id)` |
| `projection_meta` | `spec` | `version`, `active_generation`, `written_by` | |
| `schema_meta` | `version` | `applied_at`, `compat_floor` | §6.5 |

`ES-02` and `ES-09` each own their migration set with the same numbers and the same logical
catalogue; `cargo xtask schema-parity` compares tables, columns, keys and constraint names
across both and fails on a drift. Statement triggers on both engines refuse `UPDATE`/`DELETE` of
`journal_event` and `journal_identity` and any `UPDATE` that lowers `stream_head.revision` or
`required_epoch` (ES-I7, ES-I12's second line).

### 4.2 Mechanism by mechanism

| Mechanism | SQLite | PostgreSQL |
|:--|:--|:--|
| Unit transaction | `BEGIN IMMEDIATE` (takes the write lock up front; no upgrade deadlock); `busy_timeout` bounded → `Busy` | `BEGIN` at `READ COMMITTED`; `lock_timeout` and `statement_timeout` bounded → `Busy` (SQLSTATE `55P03`, `57014`) |
| Head CAS, existing stream | `UPDATE stream_head SET revision = ?new, required_epoch = max(required_epoch, ?e) WHERE stream_id = ? AND revision = ?expected`; 0 rows → `WrongExpectedRevision` | the same statement; under `READ COMMITTED` a waiter re-evaluates the `WHERE` on the committed row, so the loser sees 0 rows. Heads first locked in `StreamId` order with `SELECT … FOR NO KEY UPDATE` (no key change, so FK checks are not blocked) |
| New stream (`NoStream`) | `INSERT INTO stream_head …`; `stream_head_pkey` violation → `WrongExpectedRevision` | the same; a concurrent creator waits on the unique index, then gets the violation |
| Read set | `SELECT revision FROM stream_head WHERE stream_id = ?` inside the write transaction | `SELECT … FOR SHARE` in `StreamId` order with the written heads, so a validated read cannot be overtaken before commit |
| Event identity | insert identity rows ascending by `event_id`; PK violation → `DuplicateEventId` (extended code `SQLITE_CONSTRAINT_PRIMARYKEY` plus the index name) | the same order (a batch waiting on an id another open unit inserted holds only lower ids: no cycle); `23505` on `journal_identity_pkey` |
| Receipt, inbox | plain `INSERT`; PK violation classified by constraint name; `CommandReplayed`/`DuplicateMessage` checked by a read first so the common case refuses before any write | the same; a concurrent identical command waits on the key and then fails by name |
| Manifest | `INSERT … ON CONFLICT DO NOTHING` (portable form; never `INSERT OR IGNORE`) | the same |
| Journal tables | never `ON CONFLICT DO NOTHING`: a conflict must abort | the same |
| Dispatch marks | `INSERT … ON CONFLICT (consumer, stream_id) DO UPDATE SET marked = max(marked, excluded.marked)` per registered consumer (the list read in the same transaction); no write when none is registered | the same; rows keyed by the writer's own stream, so no two streams' writers meet |
| Lease claim | `INSERT … ON CONFLICT DO UPDATE … WHERE consumer_lease.lease_until <= :now RETURNING fence`; `:now` from `StoreClock` read inside the transaction | the same with `now()` in a one-statement transaction (the database's clock) |
| Advance / renew / release | in one `BEGIN IMMEDIATE`: check `token = ? AND lease_until > :now`; then checkpoint `= max(…)`, `DELETE FROM stream_dispatch WHERE … AND marked <= ?to`, extend | `SELECT … FROM consumer_lease WHERE token = $t AND lease_until > now() FOR UPDATE`; no row → `LeaseLost`; then the same writes |
| Snapshot save | `INSERT … ON CONFLICT DO UPDATE … WHERE excluded.revision > aggregate_snapshot.revision` | the same |
| Durability at open | `PRAGMA journal_mode` = `wal`, `synchronous` = 2 (`FULL`), `foreign_keys` = 1, read back and checked | `SHOW fsync`, `synchronous_commit`, `full_page_writes` = `on`, read back and checked |
| Commit outcome | an error from `COMMIT` → `OutcomeUnknown` (a lost `SQLITE_IOERR` on commit is ambiguous) | an error or lost connection after `COMMIT` was sent → `OutcomeUnknown`; any error before it → definite |
| Payload type | `TEXT` holding JSON | `jsonb`; NUL (`\u0000`) refused at encode for both (JC-19), since `jsonb` refuses it |
| Ids | 32-hex `TEXT` | `uuid` or 32-hex `text` (ES-09 decides; parity test compares the logical type) |

**Lock order (ES-I16), one for every transaction:** (1) stream heads and read-set heads, sorted by
`StreamId`; (2) receipt key; (3) inbox key; (4) event identities, ascending `event_id`; (5)
journal rows; (6) manifest rows; (7) snapshot request; (8) projection rows (stream-local); (9)
dispatch marks (stream-local). Delivery workers take a lease row, then its checkpoint, then its
mark, and never a head; snapshot workers take their snapshot row, then the request, and never a
head. Nothing awaits network I/O while holding any of them (decide is outside, §3.7).

**Advisory locks.** None on any write path. The one advisory lock is the **owner lease** of
§4.5, a session-level lock held for the daemon's lifetime, which no unit waits on.

### 4.3 Rules that keep SQLite from leaking into the design

1. **No global order** (ES-I17). No `rowid`, `AUTOINCREMENT`, `sequence` or commit-ordered
   column is read by any consumer, rebuild or test assertion. `ARCH-no-global-order` refuses in
   adapter SQL any `ORDER BY` on `rowid`, a time column or a revision across streams, and any
   method on a store port that returns events of several streams; the dispatch, lease and
   checkpoint SQL orders by key only (`stream_id`).
2. **No reliance on the single writer for correctness.** One writer thread is an implementation
   detail of the SQLite adapter (D7) and may serialise, never decide. Every refusal comes from data:
   a CAS on the head, a primary key, a fenced lease row. The SQLite contract runs in a
   **two-connection mode** (two writer threads on one file) and a **two-handle mode** (two
   `SqliteJournal` values on one file, as two processes would be), so a check that only works
   because one thread saw both writes fails there.
3. **No cached truth in an adapter.** No head revision, receipt, consumer list, lease or epoch is
   cached across transactions (`CachedHeadJournal` must fail JC-21 in two-handle mode).
4. **No SQL in shared code; portable SQL in adapters.** `mayfly-control` and the testkit contain
   no SQL. Each adapter owns its statements; they keep to the subset both engines accept
   (`ON CONFLICT … DO UPDATE … WHERE`, `RETURNING`, no `INSERT OR …`, no `REPLACE`, no
   engine-specific JSON functions on any correctness path). `ARCH-store-sql` lists the forbidden
   SQLite-only forms for the SQLite adapter's statements.
5. **Time and ids from ports.** Event ids are derived (§3.7) or minted by the `Ids` port; the
   store mints none. Lease expiry reads the store clock only; `recorded_at` comes in the unit.
6. **Values both engines can store.** Revisions and fences as `i64` (refuse above `i64::MAX`),
   no NUL in text, JSON from serde (no `NaN`), ids as fixed-width hex.
7. **Bounded waits are definite failures.** `SQLITE_BUSY` after `busy_timeout` and PostgreSQL's
   lock/statement timeouts map to `Busy`, which the caller reports and does not retry blindly.
8. **Classification by name.** Constraint names are part of the schema parity; messages are never
   parsed.

### 4.4 Engine-specific allowances

SQLite may use `WITHOUT ROWID` tables, WAL checkpoints and one writer thread. PostgreSQL may use
`FOR NO KEY UPDATE`, `FOR SHARE`, `fillfactor` on `stream_head` and `jsonb`. Neither may expose
these through a port or rely on them for an invariant the other engine must also keep.

### 4.5 One control plane per store

A journal has one control-plane owner at a time (plan §10's state-dir lock). On SQLite it is the
state-dir lock; on PostgreSQL a session-level advisory lock on a fixed key, taken at open and held
for the process's life, plus an `owner_epoch` row incremented at each takeover. Append correctness
never depends on it (two concurrent writers still satisfy §2); only the start-up safe-mode
classification relies on it, and says so. Projection rebuild is online (E9) and does not.

---

## 5. Durable delivery

### 5.1 Mechanism (ES-I14, ES-I15)

1. Every unit marks each registered consumer for each stream it appended (`marked` = the new head).
2. A worker claims marked streams whose lease is absent or expired by the store clock and whose
   `retry_at` has passed; each claim carries a fresh token and the next fence.
3. It reads the stream after its checkpoint through `marked`, and per event: `renew` (refused once
   expired or taken: no effect starts without the lease), `handle`, `advance` to that revision.
4. `advance` moves the checkpoint only up, clears the mark only up to the delivered revision (a
   mark set during processing survives), and is fenced by the live token.
5. A `handle` error releases the claim with `retry_after` (full-jitter backoff from the `Rng`
   port, capped), leaves the checkpoint, and the event is delivered again. Nothing skips it.
6. Discovery reads `stream_dispatch` by `(consumer, stream_id)` key, round-robin from the last key
   claimed, never ordered by time; `retry_at` is a filter, not an order.

**What the lease fences, and what it cannot.** It fences the checkpoint and the mark. It cannot
fence an effect already in flight when its lease ran out: across a takeover a consumer may see a
late repeat of an older revision. Every consumer therefore either is idempotent by
`Delivery.key = (consumer, event_id)` or fences its own resource on `Delivery.fence` (stores the
highest fence seen per stream and refuses a lower one: Kleppmann's fencing token).

**Waking without polling the store blindly.** `catch_up` runs at start-up (after the safe-mode
check) and on a coarse timer of the supervision tree (default 30 s, config). Low latency comes from
the **kick**: after a commit, `AggregateStore` notifies each running consumer's worker with the
streams it wrote; a kick lost with its process loses nothing, the mark is durable.

### 5.2 Consumers of the control plane

| Consumer | Reacts to | Effect | Idempotency / fence |
|:--|:--|:--|:--|
| `attempt_saga` | lifecycle events of `attempt-*` | issues the next command (plan §5.4) | derived `command_id` = f(consumer, event_id): a redelivery is a receipt replay |
| `directive_relay` | `DirectiveIssued` events (§5.3) | sends the directive over the link | `directive_id` (derived from the event id); the data plane dedupes by it and fences by host epoch (I09) |
| `github_effects` | events needing a GitHub call (delete runner by recorded id, release a job) | REST/scale-set call | the API's own idempotency (runner id, request id), recorded outcome via inbox |
| async projections (if any) | their subscribed kinds | rows in a consumer-owned generation | fenced by `Delivery.fence` stored per stream |

The reconciler is not a consumer: it reads projections and observations and issues commands
(plan §5.4).

### 5.3 The outbox as delivery (decided, E2, ADR 0034)

plan §5.2 and the in-flight ES-01 draft keep a mutable `outbox` table (`status`, `attempts`,
`next_attempt_at`) scanned by status and time. The port replaces it:

- A directive is a fact: the `Attempt` aggregate's decision emits `DirectiveIssued { directive_id,
  attempt_id, generation, kind, deadline, secret: Option<SecretHandle> }` in the unit that decided
  it. The `SecretHandle` is a reference; the secret itself stays in memory (D13, ES-I13).
- The `directive_relay` consumer delivers it at least once; the host epoch is stamped by the link
  adapter at send time, not stored.
- Its *status* is not a row: `Sent` is the relay's checkpoint, and `Done`/`Refused`/`Unknown` are
  the `Attempt`'s state, recorded by observation commands through the inbox (I10 stays an
  aggregate invariant, modelled in `lifecycle.qnt`). A deadline expiry is a command
  (`RecordDirectiveTimeout`) issued by the saga from a timer, which moves the attempt to `Unknown`.
- On restart, nothing is "re-sent because Pending": the relay's checkpoint says what was handed to
  the link; recovery reconciles `Unknown` by inventory first (plan §10, ver §9 mutant 9).

This removes the only time-ordered scan in the store, gives the outbox the same contract, model
and fault workload as every other consumer, and keeps one at-least-once mechanism instead of two.
The owner chose this (E2); there is no `outbox` table, and ES-01F removes it from the landed
port.

### 5.4 The inbox

External messages enter only through units: a GitHub scale-set message or a data-plane
observation becomes one command whose unit carries `InboxKey { source: github | data_plane,
message_id }`. The GitHub listener acks only after that unit committed (I12); a redelivery is
`DuplicateMessage` and is acked again without a write. An observation from a stale epoch or
generation is still recorded in the inbox, with `StaleObservationIgnored` (plan §3.2), so it is
not reprocessed. Inbox rows may be pruned only by a later, separate task, and never younger than
the source's redelivery horizon.

### 5.5 Reactors

Notification after commit (`AggregateStore` step 6) is **at most once** and carries no
obligation: it only kicks workers and feeds `events --follow`. Anything that must happen is a
consumer.

### 5.6 A journal-wide view without a global order

`mayfly events --follow` shows new events from the after-commit notification (best effort) and,
for history, reads streams the operator names or enumerates (`streams` then `read_stream`),
ordered for display by `recorded_at` and labelled as such. No decision reads it.

---

## 6. Compatibility and evolution

### 6.1 Surfaces

- **Journal: backward-transitive for ever.** Every build reads every event any earlier build
  wrote, through pure upcasters on load (KER-03's `legacy` block), never on write; a bump without
  its upcaster and legacy fixture fails the golden harness.
- **Adjacent releases N ↔ N+1: compatible both ways.** N+1 reads N's journal. N over a journal
  N+1 wrote understands every event or enters safe mode (§6.3). A change N must not ignore ships in
  two releases: **prepare** (N+1 reads the new event or field, raises its aggregate's supported
  epoch, still writes the old shape) then **activate** (N+2 writes it; the event's
  `required_epoch` raises the stream's epoch).
- **Events:** optional fields or new types; never a rename, removal or retype. A change an older
  build must not skip is a **new event type**, never a `schema_version` bump (a build cannot read
  v2 while writing v1); a bump is only for a change an older build may safely ignore.
- **Store schema:** forward-only raw-SQL migrations per adapter, one transaction each, never
  edited once released (`ARCH-migration-frozen` over both sets), expand-only; a removal waits two
  releases.
- **Projections:** a compatible change bumps `VERSION`; a change of meaning is a new spec beside
  the old one (both maintained for a release, then the old one dropped by a later migration). A
  start whose spec version differs from `projection_meta.version` **in either direction**, or whose
  `written_by` names another build, starts an online rebuild (E9, §3.6); admission is not held
  for it, and reads use the old generation until the cutover (§6.4).
- **Snapshots:** keyed by format and reducer version, so two builds keep their own; a build
  ignores keys it does not know.
- **Consumers:** registered as rows, marked by every build.

### 6.2 Kernel additions (ES-03)

`Aggregate::SUPPORTED_EPOCH: u32` (default 1) and `EventSet::required_epoch(&self) -> u32` (default
1), checked by an architecture rule that every aggregate's supported epoch reaches composition and
that no golden fixture requires more than its aggregate supports.

### 6.3 The rollback rule: a rolled-back binary meets a newer event

At start-up, after migrations and before any writer, composition compares `stored_kinds()` with
`known_kinds::<ControlAggregates>()`:

1. **Every stored kind known:** normal start. Streams whose epoch exceeds this build's supported
   epoch refuse commands (`StreamEpochUnsupported`) and are still read; the recovery table treats
   their attempts as "journal untrusted" for that attempt only (no destructive action on its VM).
2. **A stored kind unknown** (an unregistered type, or a known type at a newer `schema_version`):
   **safe mode** (fnd §6.8): the daemon cannot prove it understands ownership. No new VM, no
   destructive directive, no append except the operator's explicit `recover --apply` plan, and the
   reason names the unknown kinds and their first streams. Decoding never guesses; the codec
   refuses to encode an unknown kind, so this build can never write what it could not read.
   The remedy is to roll forward; a prepare release exists precisely so that the release before
   an activate already knows the kind.
3. **A migration above the binary:** each migration declares `compat_floor`, the lowest schema
   version a binary may know and still run against it. A binary whose newest migration is below
   the store's `compat_floor` refuses to open (exit code and message naming both versions); one at
   or above it runs, ignoring the expand-only additions.
4. **Mid-run unknown kind** (reachable only if the owner lease is broken): the first decode of it
   refuses its command and raises safe mode.

### 6.4 Rollback and the derived stores

Projections rebuild (online, into a shadow generation) on any version mismatch, so N never reads rows N+1 wrote with a meaning N does
not know, and N+1 rebuilds again on its return. Dispatch marks are written for every registered
consumer, so N marks N+1's consumers and N+1 finds no gap on its return (a
release that appended without marking a consumer a newer release registered would have lost
deliveries). Snapshots under unknown keys are ignored.

### 6.5 The compatibility harness (ES-11, after the first release)

`cargo xtask compat` runs a scenario corpus as N → N+1 → N over one store file (and, in CI, one
PostgreSQL database) with the previous release's binary, and fails on a refused start, a
safe-mode entry not declared by the scenario, or a projection that differs from a rebuild.

---

## 7. Verification

### 7.1 Contract suites

Written once in `mayfly-testkit::contracts::{journal, delivery, snapshots, projections}` as
functions over a factory, each with a `check_*` form returning failed case ids so its own tests
show the broken implementations of §7.6 failing named cases (ver §5).

**Modes.** Memory; SQLite fresh file; the same file reopened between steps; SQLite two-connection;
SQLite two-handle; SQLite crash-injected (the connection dropped after each statement of a unit,
then the file reopened); PostgreSQL fresh database; PostgreSQL two-pool; PostgreSQL
crash-injected (the backend terminated after each statement). PostgreSQL runs in a throwaway
Docker container from a digest-pinned `mirror.gcr.io/library/postgres` image, locally and in CI;
without a running Docker daemon the stage fails closed (E4, ADR 0035). Every case runs in every mode
unless marked otherwise. Lease expiry is driven by the store clock: memory and SQLite advance a
`FakeClock`; PostgreSQL cases use TTL 0 for "already expired" and a long TTL for "live", so no
test sleeps.

**Journal (JC).** JC-01 an absent stream reads empty and has no head. JC-02 a batch takes
consecutive revisions from the head. JC-03 a later append continues from the head. JC-04 a stale
expected revision is refused and writes nothing in any table. JC-05 an expected revision ahead of
the head is refused. JC-06 `Exactly` on an absent stream and `NoStream` on an existing one are
refused. JC-07 an event id stored on another stream refuses the unit, nothing written. JC-08 an id
repeated within a unit is `Malformed`. JC-09 the refusal order of §3.3 for a unit that breaks
several rules. JC-10 a multi-stream unit whose second stream conflicts writes nothing on the first.
JC-11 a moved read condition refuses the unit; a `NoStream` read is invalidated by a create.
JC-12 a replayed command returns its first `Appended` after its streams moved, writing nothing.
JC-13 a reused command id with another intent is `CommandIdReused`. JC-14 a unit the store refuses
(revision, identity, busy) leaves no receipt and the same command may later be accepted; a
*domain* refusal leaves a refusal receipt bound to its revision (E3). JC-15 a redelivered inbox key is
`DuplicateMessage`; an inbox-only unit commits. JC-16 `lookup_event` finds each id at its place and
none of a refused batch. JC-17 the manifest lists every kind ever committed and none of a refused
unit. JC-18 epochs: raised to the batch maximum, never lowered, a stream above the build refuses
appends but serves reads, a batch above the build is refused before any write. JC-19 payloads
round-trip (non-ASCII, empty object, 1 MiB); a NUL is refused before any write; errors carry no
payload text. JC-20 `streams` pages through every head once, by key. JC-21 two handles decide
against the same revision; exactly one append commits. JC-22 units on disjoint streams both commit
(all stores); on PostgreSQL neither waits on the other while both are open; units naming the same
two streams in opposite orders both finish without deadlock. JC-23 crash after each statement:
nothing partial is visible after reopen. JC-24 everything (heads, receipts, inbox, manifest,
epochs) survives reopen. JC-25 a fault at `COMMIT` is `OutcomeUnknown` and every other error is
definite (fault-injecting adapter hooks, §7.5). JC-26 a store without its durability settings is
refused at open unless the test profile allows it. JC-27 raw `UPDATE`/`DELETE` of journal rows is
refused by the store (adapter modes only). JC-28 a refusal receipt answers a repeat at the same
revision with the recorded refusal; after the stream moved, the repeat is decided afresh and its
acceptance replaces the refusal receipt; a refusal receipt whose stream moved before commit is
refused (E3).

**Delivery (DC).** DC-01 no consumer: no mark and no write beyond the unit. DC-02 each registered consumer
is marked at the new head. DC-03 a refused unit leaves no mark. DC-04 bootstrap marks every
existing stream. DC-05 registering twice is one consumer. DC-06 a retired consumer is no longer
marked and its rows are gone. DC-07 a drain delivers every event once, in revision order. DC-08 a
mark set during processing survives the checkpoint move. DC-09 a live lease is not claimable. DC-10
a kick claims only its stream. DC-11 a stale worker's advance after a takeover is refused. DC-12 an
advance after expiry without takeover is refused. DC-13 the checkpoint never moves back. DC-14 a
worker dead before its effect: redelivered. DC-15 dead after the effect, before advance:
redelivered (the consumer's key absorbs it). DC-16 advanced before death: not redelivered. DC-17 a
dead worker's stream waits for its lease to end, then is delivered. DC-18 a failing consumer gets
the same event again, never the next. DC-19 a long-down worker catches up every stream in order.
DC-20 a takeover gets a higher fence. DC-21 each delivery carries its claim's fence. DC-22 renewing
an expired lease is refused. DC-23 a slow consumer that lost one stream goes on draining others.
DC-24 a consumer registered by another build is marked by this one. DC-25 a release with
`retry_after` is not claimable before the store clock passes it.

**Snapshots (SC).** SC-01 a request on crossing the interval; SC-02 one request per stream, target
only rising; SC-03 a refused unit leaves no request; SC-04 no snapshot: load whole; SC-05 snapshot
plus tail; SC-06 snapshot at the head: empty tail; SC-07 another key or a revision above the head is
not used; SC-08 a corrupt checksum is ignored; SC-09 an older save after a newer one keeps the
newer; SC-10 a request is served only up to the folded revision; SC-11 a snapshot whose folded
kinds differ from what this build would fold is ignored.

**Projections (PC).** PC-01 rebuild equals live (I16), also as a property over random histories;
PC-02 a version bump rebuilds; PC-03 inline rows commit with their events; PC-04 a refused unit
leaves none; PC-05 a row keyed by a stream outside the unit is `Malformed`; PC-06 a version below
the stored one rebuilds too (§6.1). PC-07 during an online rebuild every unit writes both
generations and readers see the active one only; PC-08 the cutover flips the generation in one
transaction and refuses a unit computed for the old generation set; PC-09 a crash mid-rebuild
resumes from the applied revisions, never from a head (E9).

### 7.2 Concurrency cases beyond the contract

- **Two writers per stream** across processes: an integration test starts two OS processes on one
  SQLite file (and two on one PostgreSQL database), each running 1,000 seeded `AggregateStore`
  commands on 4 shared streams, then audits ES-I2, ES-I4, ES-I5 with the store's own reads.
- **Cross-stream independence**: on PostgreSQL, two held units on different streams, checked by
  `pg_blocking_pids` being empty for both; on SQLite, the outcome half only.
- **Lease takeover with fencing**: worker A claims, its lease expires (store clock), B claims with
  a higher fence and advances; A's advance, renew and release are each refused, and a consumer
  fencing on `Delivery.fence` refuses A's late effect.

### 7.3 The store model (`formal/event_store.qnt`)

It states the store's mechanisms of §2 that a model can state, the online rebuild's generations
(E9) included.

- **State:** streams (lists of `(id, epoch)`), heads, identity registry, receipts (command id,
  intent, outcome), inbox keys, manifest, open transactions with their held head locks and
  reserved ids, consumers (marks, checkpoints, leases with token, fence and liveness, in-flight
  claims, effects), snapshots per (stream, key) and requests, a per-stream reader, and `last`, the
  stimulus with its outcome.
- **Actions:** `appendUnit` (one step) and `begin`/`lock`/`write`/`commit`/`rollback` (split, with
  waits recorded); `command` (with receipt judgement); `deliverInbox`; `read`; `register`,
  `bootstrap`, `claim`, `fetch`, `effect`, `advance`, `release`, `crash`, `expire`, `fail`; and
  (ES-08) `snapshotFold`, `snapshotCommit`, `snapshotCrash`, `corrupt`, `load`.
- **Relations:** `step` (full contract, concurrent held writers), `implStepSerial` (at most one held
  write transaction: what SQLite can do), and a biased relation for trace generation. SQLite is
  checked against `implStepSerial`; memory and PostgreSQL against `step`. Single-writer is thus an
  admitted restriction of the model, not a different model.
- **Invariants:** one per ES-I that the model can state (ES-I1..ES-I8, ES-I12, ES-I14..ES-I16),
  combined in `allInvariants`.
- **Scenarios** (owner-reviewed runs, exported as regression traces): `lostRace`,
  `noStreamVersusEmpty`, `independentStreams`, `batchFailsAfterSecondEvent`,
  `duplicateAcrossStreams`, `repeatedWithinBatch`, `rollback`, `epochRaise`, `refusedAboveEpoch`,
  `receiptReplay`, `racingCommands`, `refusedThenAccepted`, `inboxRedelivery`,
  `multiStreamConflict`, `readSetInvalidated`, `commitBetweenFetchAndApply`,
  `independentHeldWriters`, `headQueue`, `idWait`, `oppositeOrderUnits`, `bootstrapAfterWrites`,
  `markDuringProcessing`, `staleWorkerFenced`, `workerCrashRedelivers`, `failingConsumerNoSkip`,
  then (ES-08) `snapshotAndTail`, `staleSnapshotWorker`, `snapshotWorkerCrash`, `corruptSnapshot`,
  `snapshotFormatsCoexist`. Negative worlds where an invariant must break: `headTrustingReader`
  (ES-I15), `unfencedWorker` (ES-I14).
- **Conformance driver** (`mayfly-conformance::drivers::event_store`): repeats each stimulus on
  the real store through the ports only and compares outcome and an abstract state (per-stream
  histories via `read_stream`, heads, the identity registry via `lookup_event` over every id the
  trace names, receipts, inbox, delivery state) after every step, initial state included. Split
  transactions run on their own threads; on PostgreSQL the driver checks after each step that each
  waiter waits, by `pg_blocking_pids`, on exactly the model's holders: no timing.
- **Driver mutants** (negative controls behind a driver seam, ver §6.2.7), each failing a named
  committed trace at a named step: `unchecked-revision`, `half-unit`, `dropped-receipt`,
  `forgetful-inbox`, `late-id-check`, `global-lock`, `unfenced-advance`, `clear-all-marks`,
  `head-trusting-worker`, `blind-snapshot-upsert`, `resealing-snapshots`.
- Every invariant is shown to catch a scratch mutation of the model by `quint run` and by the
  scenario it guards; Apalache checks `allInvariants` to a bounded depth in the nightly lane
  (VER-07).

### 7.4 What the lower levels already prove, and what they cannot

Contract and conformance script the interleaving or check final state. None records what
concurrent clients *observed* while processes died and connections broke, and none checks those
observations against a model that admits operations whose outcome the client could not know. A
lost commit reply hides exactly there. That is the store lab's job.

### 7.5 The store lab (Jepsen-style)

**Placement.** A store workload family of the fault lab (ver §8), so it shares its history,
verdicts, guardrails and artefacts. Independence (D11) is kept by splitting it: the controller,
history, nemesis, audit and checker live in `tools/faultlab` and import no production crate; the
**worker** is a separate tool binary, `tools/storelab-worker`, which links only the production
store adapters and `AggregateStore` and speaks versioned JSON lines on stdin/stdout. The audit
reads the store with its own SQL.

**Commit barriers.** Each adapter is generic over a `StoreHooks` parameter defaulting to `NoHooks`
(zero cost, `before_commit`/`after_commit`); only `storelab-worker` and adapter tests may name
another (`ARCH-store-hooks`). The worker pauses at a barrier so the controller can strike there;
production code paths are otherwise untouched. Mutants live in the worker behind `--mutant`,
accepted only by scenarios declared as negative controls.

**Operations and history.** `append` (stream, expected, event ids), `command` (through
`AggregateStore`, with a command id), `head`, `read` (stream, after, through), `lookup`
(event id), `deliver` (a consumer drain step), each `invoke` then `ok`/`fail`/`info` in the ver
§8.3 schema. Classification: a returned value is `ok`; any `AppendError` except `OutcomeUnknown` is
`fail` (definite, ES-I18); `OutcomeUnknown`, a timeout or a worker death is `info`, and `info`
retires the worker; a read that errors is `fail`.

**Workloads.** SW1 single-stream CAS races. SW2 independent streams. SW3 multi-stream units, some
carrying an id already stored on a third stream as their last event (whole or nothing). SW4
identity races across streams. SW5 mixed reads (`read`, `head`, `lookup`) against writers. SW6
unknown commit: strike at `after_commit`, retry the same command id from a fresh worker (the
receipt answers). SW7 store crash: SQLite, SIGKILL every process holding the file; PostgreSQL,
immediate shutdown and restart of the lab's own cluster. SW8 receipts and intent reuse through
`AggregateStore`. SW9 delivery: two workers per consumer, kills around effect and advance, leases
expiring on the store clock. SW10 inbox redelivery storms.

**Nemesis.** Worker SIGKILL at `before_commit` (expect absent) and `after_commit` (expect present);
SIGSTOP of a worker holding the write lock or a head until others report `Busy` (definite);
PostgreSQL `pg_terminate_backend` of a worker's session at either barrier; store crash (SW7);
lease expiry by advancing the SQLite store clock or using short TTLs on PostgreSQL. No proxy or
privileged network fault is needed: PostgreSQL runs in a Docker container the lab starts, labels
with its run id and kills or restarts for SW7 (E4, ADR 0035). A fault that hit no operation does not count: each
scenario declares its required faults and their proof of hit.

**Checker (E5, ADR 0035).** Linearizability is checked by **Knossos**, pinned with its SHA-256 and
run on the JVM already pinned for Apalache, never by a home-grown checker. The history is exported
in Knossos's format with each stream an independent key whose model is a compare-and-set register
over its revision (append = cas(expected, new), read = read). Elle's list-append checker is the
named fallback if Knossos's search does not scale (switching needs an amendment of ADR 0035).
What Knossos cannot state is checked as plain assertions over the quiescent audit, which records
its own order (ES-I7); those assertions require that every `ok` append sits at its reported revision; every
`fail` with `WrongExpectedRevision` is explainable (the head was not at the expected revision at
some instant of the operation's window, given the windows of the operations that produced the
neighbouring revisions); every `fail` id is absent unless a later operation explains it; an `info`
batch is wholly present or wholly absent; every read returns a prefix that contains each operation
completed `ok` before the read began and none invoked after it ended; real-time order between
`ok` operations matches revision order. Identity: each id at most one place, and `lookup` answers
consistent with the audit. Delivery: per consumer and stream, delivered revisions cover the
committed ones, within a claim ascending, checkpoints never regress, fences strictly increase per
takeover. Porcupine-style checkers are rejected (no indeterminate operations: dropping `info` hides the
anomaly).

**Audit, verdicts, artefacts, guardrails.** As ver §8.5 and §8.6: a quiescent audit with the lab's
own SQL (heads equal the last revision, no holes, identity ↔ event one to one, trigger presence,
durability settings), annotating never rewriting the history; `PASS` only when every check holds,
every required fault hit and observation floors are met; `INCONCLUSIVE`, `FAIL` with a witness,
`HARNESS_ERROR`; stores created by the lab only, marked with the run id (a marker row in SQLite,
`COMMENT ON DATABASE` in PostgreSQL) and rechecked before every destructive step; no store URL is
ever accepted from the command line. Fixture histories with expected verdicts (legal, illegal,
ambiguous, harness-broken) run through the real checker in `check`, plus a no-fault smoke with two
workers on SQLite.

**A `FAIL`** becomes a scenario of `formal/event_store.qnt` replayed on every store (ver §6.2.9),
then a fix.

### 7.6 Broken implementations (negative controls)

| Broken implementation | Kind | Must fail |
|:--|:--|:--|
| `LooseRevisionJournal` (ignores expected revision) | contract | JC-04, JC-21; SW1 mutant `skip-revision-check` |
| `PartialCommitJournal` (commits the first stream of a unit) | contract | JC-10, JC-23; SW3 `partial-unit` |
| `ForgetfulReceiptsJournal` (receipts lost on reopen) | contract | JC-24 (reopened mode); ver §9 mutant 10 |
| `IntentBlindReceipts` (replays any intent) | contract | JC-13; SW8 `intent-blind` |
| `ForgetfulInbox` | contract | JC-15; ver §9 mutant 7; SW10 |
| `CachedHeadJournal` (head cached per handle) | contract | JC-21 (two-handle mode) |
| `GlobalLockJournal` (refuses a unit while another stream's is open) | contract | JC-22 |
| `LateIdCheckJournal` (id check after revision check) | contract | JC-09 |
| `AckBeforeCommitJournal` (reports success before `COMMIT` returns) | lab | SW6 `ack-before-commit` |
| `UnfencedLedger` (advance ignores the token) | contract | DC-11; model trace `staleWorkerFenced`; SW9 |
| `ClearAllMarksLedger` (deletes the mark whatever its revision) | contract | DC-08 |
| `HeadTrustingWorker` (advances to the head, not the delivered revision) | contract | DC-08, DC-18; negative world `headTrustingReader` |
| `SkippingLedger` (moves past a failing event after N tries) | contract | DC-18 |
| `BlindSnapshotUpsert`, `ResealingSnapshots` | contract | SC-09, SC-08 |
| `ClockReadingProjection`, `OrderDependentProjection` | contract | PC-01 |

The curated-mutant table of ver §9 gains the store rows that are not already contract fixtures.

---

## 8. Task changes (applied to `doc/tasks.md` by DEC-01)

### 8.1 The ES lane, rewritten and extended

The cards (Cites, dependencies, model, acceptance) live in `doc/tasks.md`, Phase 2; this table is
the lane's shape. ES-01 landed before the owner's decisions; ES-01F realigns it with them and with
its review.

| ID | Title | Depends on | Model | ⛳ |
|---|---|---|---|---|
| ES-01 | Storage-neutral `Journal` port, memory journal, journal contract (landed) | KER-03 | opus | |
| ES-01F | Align the landed journal port with E2/E3/E6 and the ES-01 review | ES-01 | opus | |
| ES-02 | SQLite journal adapter: schema, triggers, classification, five modes | ES-01F | opus | |
| ES-03 | `AggregateStore`: single write path, receipts, epochs, retry, reactors | ES-01F | opus | |
| ES-04 | Projections: stream-local inline specs, generations, rebuild by enumeration, first read models | ES-02, ES-03 | sonnet | ⛳ |
| ES-06 | Event-store Quint model and scenarios (before delivery, E7) | FND-06, ES-01F | opus | |
| ES-05 | Durable delivery ledger, worker and `catch_up`; the directive relay | ES-02, ES-03, ES-06 | opus | |
| ES-07 | Event-store conformance driver over memory and SQLite | ES-06, ES-05, VER-01 | opus | |
| ES-08 | Snapshots: port, memory and SQLite, worker, snapshot + tail load (E8) | ES-03, ES-07 | opus | |
| ES-09 | PostgreSQL adapter in Docker, schema parity, `contract-postgres` stage and CI job (E4) | ES-04, ES-05, ES-08 | opus | ⛳ |
| ES-10 | Store lab: worker, store workloads, Knossos checker, mutants (E5) | VER-03, ES-05, ES-09 | opus | |
| ES-11 | Compatibility harness N → N+1 → N and the upgrade matrix | OPS-03, ES-09 | sonnet | |
| ES-12 | Online projection rebuild: shadow generations, dual writes, cutover fence (E9) | ES-04, ES-05, ES-07 | opus | |

### 8.2 Existing tasks that change (applied)

- **CP-02 AttemptSaga and directive relay:** both become durable consumers on ES-05 (depends on ES-05
  instead of ES-04 alone); the relay delivers `DirectiveIssued` events; "record `Unknown` on
  timeout" becomes the saga's `RecordDirectiveTimeout` command; accept clauses unchanged plus "a
  relay killed between send and advance resends the same `directive_id`".
- **CP-04 Observation inbox and startup recovery:** observations enter through units with
  `InboxKey { data_plane, observation_id }` (ES-I6); the startup use case adds the migration
  floor check and the stored-kinds check of §6.3, then projection rebuild, then `catch_up`.
- **GH-04 Listener:** the record step is one unit with `InboxKey { github, message_id }`; the ack
  follows its commit; `DuplicateMessage` is acked without a write.
- **RT-02 Composition:** registers consumers, takes the owner lease, runs §6.3 then `catch_up`,
  owns the kick wiring and the `catch_up` timer.
- **RT-03 CLI:** `events --follow` per §5.6.
- **VER-02:** loses nothing; the recovery driver uses the SQLite journal through the new ports.
- **VER-03/VER-04:** the history schema gains the store operations (`append`, `command`, `head`,
  `read`, `lookup`, `deliver`); the checker module gains the store checkers (ES-10 implements them).
- **VER-05:** curated mutants 7 and 10 are killed by JC-15 and JC-24 (named); the store rows of
  §7.6 not already covered join the table.
- **VER-07:** adds `event_store` to the Apalache lane.
- **ver §5, §6.1, §9, §10:** the `Journal` row lists §7.1's modes and cases and adds delivery,
  snapshot and projection rows; §6.1 adds the `event_store` model and its invariants; §10's
  per-push lane adds the PostgreSQL contract stage.
- **Checkpoint:** ES-04 stays ⛳ (SQLite journal and projections); ES-09 becomes ⛳ (the store is
  proven on two engines).

### 8.3 Owner decisions

Decided by the owner on 2026-11-09. The E-numbers are the owner's; this document's earlier drafts
called them O1–O9.

| # | Question | Decision | Record |
|:--|:--|:--|:--|
| E1 | A stream-scoped, storage-neutral design, SQLite first, PostgreSQL second and CI-tested; not deploying PostgreSQL in v0.x | **accepted** | ADR 0021 (Accepted) |
| E2 | The outbox becomes `DirectiveIssued` events delivered by a durable consumer | **accepted**: no outbox table (§5.3) | ADR 0034 |
| E3 | Receipts for refusals | **keep refusal receipts, bound to the stream revision decided at**; a retry after the stream moved is re-decided, not replayed; intent digest; a reused id with another intent is refused (ES-I5, JC-28) | ADR 0034 |
| E4 | How PostgreSQL runs in tests | **Docker**, in CI and locally and in the agent sandbox, image from `mirror.gcr.io/library/postgres` pinned by digest; fails closed without a running daemon (not pinned binaries) | ADR 0035 |
| E5 | The store lab's checker | **an existing checker, Knossos** (the JVM is already pinned for Apalache), not a home-grown one; Elle as the named fallback | ADR 0035 |
| E6 | Stream epochs now or later | **now** (ES-01F, ES-03) | ADR 0034 |
| E7 | Store model before delivery | **yes**: ES-06 before ES-05 | ADR 0034 |
| E8 | Snapshots or bounded streams | **build snapshots** (ES-08); bound streams where the domain allows anyway | ADR 0034 |
| E9 | Online projection rebuild | **build it** (shadow generations, dual writes, cutover fence), not start-up only (ES-12) | ADR 0034 |
