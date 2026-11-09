# 0018. Event codec and aggregate registry

Date: 2026-11-09

## Status

Proposed

## Context

Plan §5.3 and fnd §6.1 ask for versioned events registered in one registry, each with a golden
JSON fixture, read through pure upcasters; ver §3.1 asks every aggregate module for accepted and
refused `DETERMINISM_CASES`, and for the set of modules with cases to equal the set of
aggregates. `mayfly-control-domain` is pure (fnd §2.3): it may derive `serde` but must not name
a format such as `serde_json`. The journal adapter stores the kind (`type`, `schema_version`)
and the payload in separate columns. KER-04..06 add aggregates and must not have to touch the
harnesses to be covered.

Alternatives considered: a global, string-keyed table of decode function pointers (a pointer
cannot be generic over a `Deserializer`, so it would force a format into the pure crate or an
erased-serde dependency); a `#[serde(tag = "type")]` enum (puts the kind inside the payload,
cannot carry a version, and duplicates the journal's columns); a `const DETERMINISM_CASES` slice
(events and commands are not `const`-constructible).

## Decision

- Each event struct implements `DomainEvent` (`EVENT_TYPE`, `SCHEMA_VERSION`). An aggregate's
  events form one enum declared with `event_set!`, which implements `EventSet`: the current and
  legacy kinds as constants, `kind()`, and `encode`/`decode` written against serde's
  `Serializer`/`Deserializer` traits. The payload is the struct's own serde form; the kind
  travels beside it. An unknown kind decodes to `DecodeError::UnknownKind` (safe mode), never a
  guess.
- The upcaster hook is the macro's `legacy { Old => fn(Old) -> Enum }` block: legacy versions
  are decoded and upcast on load, never written.
- `Aggregate` (`NAME`, `State`, `Body`, `Event`, `Rejection`, `initial`, `decide`, `evolve`) with
  the caller-supplied `Command { command_id, issued_at, body }` envelope; `Rejection` is a trait
  each aggregate's refusal type implements with a stable `code()`.
- Cases are `DeterminismCases::determinism_cases()`. An `AggregateRegistry` is walked by an
  `AggregateVisitor` whose `visit::<A>()` requires `A: DeterminismCases`, so a registered
  aggregate without cases does not compile. `ControlAggregates` is the one registry: its
  aggregates' event sets are the event registry.
- The golden and determinism harnesses live in `mayfly-testkit` (test-only, `serde_json`) and
  walk any registry. A fixture is the payload as pretty JSON with a trailing newline,
  `<type>/v<n>.json`; a legacy kind adds `v<n>.expected.json`, its upcast current encoding.

## Consequences

An aggregate plugs in with one `visitor.visit::<A>()` line and is then covered by both
harnesses and by `known_kinds`. The pure crate names no format; ES-01/ES-02 decode journal rows
through `EventSet::decode` with their own `serde_json` deserializer. An `Aggregate` impl left
out of the registry is not caught at run time (its events would have no fixtures, and an
existing fixture would be reported unregistered); an architecture rule that every
`impl Aggregate` appears in `ControlAggregates` is needed for that, along with the hash lock
of frozen fixtures (`fixtures.lock`, ES-03). Reversal means a new ADR.
