# 0020. Control-plane time and the journal's recorded_at

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09); amendment proposed 2026-11-10

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
  computed from its own `Clock`). *(Amendment proposed 2026-11-10: it reads the clock directive
  deadlines are written in; see below.)*
- The journal reads no clock: `UnitOfWork::recorded_at` is supplied by its writer
  (`AggregateStore`, from the `Clock` port) and becomes every event's `recorded_at` and the inbox
  entry's `processed_at`. Ordering never rests on these instants: a stream's order is its
  revisions (ADR 0021).

## Consequences

- A wall clock can step backwards; nothing orders by it, and deadlines tolerate the skew.
- `FakeClock` (testkit) starts at a given Unix-epoch instant.
- KER-01's `Millis` and KER-03's `Command` docs now say this; ES-03 introduces the `Clock` port.

## Amended 2026-11-09 (planner): no outbox, no inbox instant (accepted by the owner 2026-11-09)

ADR 0034 replaced the outbox table with `DirectiveIssued` events delivered by a durable consumer,
so there is no persisted `next_attempt_at`: a directive's deadline is an instant in its event,
and a retry's backoff belongs to the delivery ledger (ES-05, `doc/event-store.md` §5.1).
The landed journal (ES-01F) keeps no `processed_at` either: the inbox records only that a key was
consumed (`Journal::inbox_contains`). The decision stands: every persisted instant is a
`Clock` reading, and `UnitOfWork::recorded_at` is supplied by its writer.

## Amendment proposed 2026-11-10 (planner, DP-01): how the data plane reads a deadline

A directive's deadline is a reading of the control plane's `Clock` (wall-clock milliseconds
since the Unix epoch), and `DataPlane::receive` refuses a directive whose deadline `now` has
reached (KER-08). A monotonic `now` cannot be compared with it: a monotonic reading starts near
zero at each boot. Two ways to compare deadlines, for the owner to choose:

1. **Wall clock at receipt.** The data plane reads the host's wall clock where it compares a
   deadline (at receipt), converts the time left into a monotonic instant there, and waits and
   times out on the monotonic clock only. Cost: a wall-clock step between the control plane
   writing a deadline and the data plane reading it lengthens or shortens that directive's time
   by the step (an effect refused `deadline_expired` or answered `Unknown{deadline}` early or
   late, never a success claimed); it holds only while both planes read one host's clock, so
   the multi-host edition must revisit it.
2. **Clock offset at the handshake (recommended).** The `Welcome` carries the control plane's
   clock reading (a new optional field, protocol compatibility rules: an older peer ignores it,
   and without it the data plane falls back to option 1); the data plane records the offset to
   its monotonic clock when it enters the session and compares every deadline of the session in
   monotonic time. Cost: one additive protocol field and a session-long offset that a
   wall-clock step on either host after the handshake does not follow (a reconnect re-takes
   it); in exchange no step moves a deadline mid-session, and it works across hosts.

What does not depend on the choice, as built by DP-01: the observer's cadence, the wait for a
`ReportInventory`'s deadline and every timer run on the monotonic clock (`ProcessClock`
separates `wall()` from `monotonic()`), and deadlines are read in one place
(`mayfly-data::executor::admission_now` and `local_due`), so switching is a change there.
DP-01 builds option 1, the smaller step; the planner recommends option 2. Proposed; the owner
decides.
