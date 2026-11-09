# 0021. A storage-neutral, stream-scoped event store

Date: 2026-11-09

## Status

Proposed

## Context

Plan §5.2 sketches the journal around SQLite: one writer connection, an `event_journal` with a
global `sequence`, projections rebuilt "from sequence 0" with a global watermark (§5.5). That
design leans on SQLite's single writer to make the sequence equal commit order. The owner wants
the journal storage-neutral, so that a PostgreSQL adapter can follow SQLite with no change to the
port. That calls for a stream-scoped design: per-stream order on a stream head, a global event-id registry,
command receipts, durable delivery by per-consumer, per-stream checkpoints, and no global journal
order at all. A global position forces a global writer (one lock, or one
connection) or a gap-tolerant reader that can skip a late commit; both are what a second backend
cannot afford.

## Decision

- **No global journal order.** The `Journal` port has no sequence, no `read_all` after a
  position and no head of the whole journal. Readers enumerate streams by id
  (`Journal::streams`) and read each after a revision (`Journal::read_stream`); nothing pages by
  time or row order.
- **Per-stream order on a stream head.** Each append compares a stream's current revision with
  the unit's expected revision and advances it in the same transaction (the compare-and-set of a
  stream head); of two writers at one revision exactly one commits, the other gets
  `WrongExpectedRevision` with no effect. A stream without events has no head.
- **Independent streams.** Writers of different streams share only unique keys (event ids,
  command ids, inbox entries, directive ids): none is refused because another stream moved.
- **An event-id registry**: event ids are unique across the journal and resolvable
  (`Journal::lookup_event`), so a retried append whose ids are stored finds its batch.
- **Command receipts** keyed by command id, checked before revisions, so a repeat answers as the
  first run did after the stream moved. Mayfly keeps plan §5.2's receipt of refusals (`status`),
  rather than recording accepted commands only; revisit with ES-03.
- **An event-kind manifest** (`Journal::recorded_kinds`) so the safe-mode check at start-up reads
  a short list rather than every event.
- **Check order**: shape, receipt, inbox, expected revisions, event ids, outbox; the suite pins
  the parts a caller relies on.
- **The journal contract suite** holds all of the above for every adapter, including two
  concurrency cases (racing writers on one stream; independent streams), run by polling several
  appends on one task so a waiting adapter interleaves them.
- Names stay mayfly's: revision (not version), `Journal`, `UnitOfWork`, `StreamRevision`.

## Consequences

- Plan §5.2's `sequence` column and §5.5's "rebuild from sequence 0 / watermark" are replaced:
  ES-02's schema keys events by `(stream_id, revision)` with a separate event-id key; ES-04's
  projections keep a per-stream applied revision and rebuild stream by stream.
- Deferred to later tasks (not in ES-01): the durable delivery ledger (per-consumer dirty marks,
  per-stream checkpoints, leases with a fencing number), stream epochs, snapshots, multi-stream
  read sets, and a lock-order statement for the PostgreSQL adapter. The outbox stays a queue of
  effects ordered by `next_attempt_at`, not a feed of the journal.
- SQLite's single writer remains an implementation detail of ES-02, never a premise of the port
  or the suite.
