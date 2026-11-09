# 0029. Registration reaper and the wire behaviour of pause, drain and safe mode

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09)

## Context

A JIT registration outlives its VM when a reply is lost or the daemon dies, and leaked
registrations inflate GitHub's runner counts. Pause, drain and safe mode each change what mayfly
can deliver, and the scale-set session must say so without churning sessions
(`doc/domain-model.md` OD11, OD15).

## Decision

1. **Registrations are their own aggregate** (`RunnerRegistration`, one per attempt) and a
   **reaper** runs at run start, after a long GitHub outage, daily and on `mayfly doctor`, at
   most one runner listing per hour. It deregisters only journaled runner ids; a runner of our
   name shape with no journal record is reported, never deleted; nothing is deleted by name
   pattern.
2. **Wire behaviour.** Pause: `maxCapacity` = live runners of the pool, nothing acquired, the
   session stays open. Drain: `maxCapacity` = 0, nothing acquired. Safe mode with a writable
   journal: session open, `maxCapacity` = 0, nothing acquired, only recorded messages acked.
   Safe mode with an unwritable journal: polling stops (GitHub redelivers). No mode closes the
   session or deletes the scale set.

## Consequences

KER-09 (`formal/registration.qnt`, I19–I21), CP-05 (manager and reaper), KER-06 and GH-04
(wire behaviour), F06, F07, F32, F33 pin it. GH-00 records how long an unclaimed JIT
registration lingers at GitHub.

## Alternatives considered

- Rely on GitHub's own cleanup of offline runners.
- Close the session on pause or safe mode: expiry and conflict churn on resume.
