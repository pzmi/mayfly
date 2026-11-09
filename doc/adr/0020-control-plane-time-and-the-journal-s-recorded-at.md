# 0020. Control-plane time and the journal's recorded_at

Date: 2026-11-09

## Status

Proposed

## Context

Plan §5.2 gives every journal row a `recorded_at`, the outbox a `next_attempt_at` and the inbox
a `processed_at`. KER-01 documented `Millis` as "a duration or a reading of the caller's
monotonic clock", and KER-03's `Command { issued_at: Millis }` carries the instant `decide` reads.
A monotonic reading is meaningful only within one process: after a restart it starts over, so
an instant persisted in an event, the journal or the outbox would lose its meaning. The data
plane, by contrast, needs a monotonic clock for directive deadlines that never leave the process.

Alternatives considered: a distinct wall-clock type beside `Millis` (type-safe, but every
command, event and deadline would need to choose, and `Command` would change shape now for no
caller); letting the journal read a clock itself (breaks "the journal reads no clock", makes
`recorded_at` untestable and differ from the command's own instant).

## Decision

- `Millis` stays one value type: a duration, or a reading of the caller's clock. Which clock is
  the reading port's business, and two readings compare only when they come from one clock.
- The control plane's `Clock` port (in `mayfly-control`) reads **wall-clock milliseconds since
  the Unix epoch**. Every persisted instant is such a reading: `Command::issued_at`, instants in
  events, the journal's `recorded_at`, the outbox's `next_attempt_at`, the inbox's
  `processed_at`.
- The data plane's `ProcessClock` reads a monotonic clock; its readings never leave the process
  and never cross the link as instants (a directive carries a deadline the control plane
  computed from its own `Clock`).
- The journal reads no clock: `UnitOfWork::recorded_at` is supplied by its writer
  (`AggregateStore`, from the `Clock` port) and becomes every event's `recorded_at` and the inbox
  entry's `processed_at`. Ordering never rests on these instants: a stream's order is its
  revisions (ADR 0021).

## Consequences

- A wall clock can step backwards; nothing orders by it, and deadlines tolerate the skew.
- `FakeClock` (testkit) starts at a given Unix-epoch instant.
- KER-01's `Millis` and KER-03's `Command` docs now say this; ES-03 introduces the `Clock` port.
