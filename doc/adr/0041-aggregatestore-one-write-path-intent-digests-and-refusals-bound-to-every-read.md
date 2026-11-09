# 0041. AggregateStore: one write path, intent digests and refusals bound to every read

Date: 2026-11-09

## Status

Proposed. Amends ADR 0034 (E3) and ADR 0021.

## Context

ES-03 builds `AggregateStore`, the only caller of `Journal::append` (`doc/event-store.md`
§3.7, fnd §6.3). Four points were open when it started.

- **Refusal binding.** ADR 0034 (E3) binds a refusal receipt "to the stream and revision it was
  decided at", and the port landed by ES-01F required a refusal unit to carry exactly one read.
  But a decision may read several streams (`doc/event-store.md` §3.2: a pool's configuration
  while deciding an attempt), and a refusal can rest on any of them. Bound to one, a refusal
  whose *other* input moved would still be replayed: a stale answer the receipt exists to
  prevent.
- **Intent and command type.** ES-I5 needs a canonical intent digest per command, and the
  receipt records a command type, but the kernel's command bodies had neither a type name nor
  a canonical form, and nothing failed for a command without one.
- **What the store may encode with.** `mayfly-control` named no format; the store must encode
  events and refusals (JSON, as the adapters already store) and hash intents.
- **The single-append rule** was planned for ES-03 (fnd §3.3) with no mechanism.

## Decision

1. **A refusal receipt is bound to every stream the decision read**, the refused stream itself
   included, each at the revision read, recorded in `StreamId` order
   (`ReceiptOutcome::Refused { bound: Vec<ReadCondition>, .. }`). It answers a repeat while all
   of them are unchanged; once any moved, the repeat is decided afresh and its outcome replaces
   the receipt. A refusal unit carries no append and at least one read
   (`MalformedUnit::RefusalNotBound` otherwise). JC-28 gains a two-read half, and the broken
   `OneReadRefusalJournal`, which keeps only the lowest read, fails it. The SQLite adapter
   stores the list in the receipt's outcome document (not yet released, so no migration).
2. **Every command body implements `CommandBody`**: a `snake_case` `command_type`, the list of
   `TYPES` it returns, and `Serialize`. `Aggregate::Body` requires it, so a body without one does
   not compile; the determinism harness fails a case whose type is not listed, a listed type no
   case decides, and a malformed list. Refusals (`Rejection`) serialise and deserialise, carrying
   codes and ids only; the receipt stores their serde form.
3. **The intent digest** is SHA-256 over `mayfly-intent-v1\n`, the aggregate, the command type
   and the target stream (newline-separated) and the body as compact JSON. Command id, issued-at,
   run epoch and actor are not part of the intent. The digest therefore covers the command type,
   so the journal compares digests only. **Event ids** are the first 128 bits of SHA-256 over
   `mayfly-event-id-v1\n`, the command id and the ordinal (big-endian). Both are frozen by
   goldens (`crates/mayfly-control/tests/fixtures/intents/`, `tests/store/codec.rs`), checked
   against digests computed outside Rust.
4. **`mayfly-control` depends on `serde_json` (encoding events and refusals, as the kernel codec
   requires a format) and `sha2` (digests, ids)**, both already in the lockfile and allowed by
   `deny.toml`.
5. **The write path** follows `doc/event-store.md` §3.7: epoch check before the receipt; receipt
   replay or `CommandIdReused`; whole-stream load (an unknown kind refuses the command and
   notifies safe mode); `decide` outside any transaction; append. Only a conflict is retried
   (`with_revision_retry`: three decisions, full-jitter backoff through the `Rng` and `Clock`
   ports). `OutcomeUnknown` is settled by re-appending the identical unit, at most three times:
   a receipt answers, or `DuplicateEventId` names an id that `lookup_event` finds exactly where
   the unit would have written it; anywhere else is `EventIdCollision`, never success. A
   notification goes out after a commit only, at most once, never for a replay.
6. **A refusal does not consume its inbox key**, so a redelivered message is decided again once
   the bound streams moved instead of being dropped as a duplicate.
7. **`ARCH-single-append`** refuses, in control-plane and both-planes crates (production and
   test code), a method call `.append(x)` with one by-value, non-literal argument, the path
   `Journal::append`, and the same call in macro tokens, outside `AggregateStore`'s `save` and
   `settle`, `impl Journal` blocks, the contract suite with its own tests, and the journal
   adapters' own tests. Method calls are not type-resolved, so the argument's shape tells a unit
   of work from `Vec::append(&mut ..)` or `OpenOptions::append(true)`.

## Consequences

- ES-03 lands the store, the kernel additions of §6.2 (`SUPPORTED_EPOCH`, `REQUIRED_EPOCH`,
  `EventSet::required_epoch`), the start-up kind check feeding safe mode and the rule above.
  The epoch rule of §6.2 holds by construction (the store takes its epochs from the registry)
  and in the golden harness (a sample needing more than its aggregate supports fails).
- A command body's shape change changes its digest: a command retried across that upgrade is
  refused `CommandIdReused` rather than replayed, so a shape change of a command is a new
  command type, as for events.
- A refusal receipt written by a newer build with a code this build does not know fails to
  decode (`SaveError::Codec`); deciding afresh would hide it.
- Deferred to a follow-up task (`ES-03R` in `doc/tasks.md`): `ARCH-no-global-order`,
  `ARCH-journal-sql`, `ARCH-migration-frozen` and the golden `fixtures.lock`.

## Alternatives considered

- Restrict refusal decisions to one read stream: simpler port, but every multi-stream decision
  would have to re-read and re-decide on each repeat, and a caller could not express what the
  refusal depended on.
- Store the refusal as its code only: no serde bound on `Rejection`, but a refusal carrying an
  id could not be replayed exactly.
- A hand-written canonical byte encoding per command instead of JSON: no format in the store,
  but one more encoding to keep exhaustive by hand for every body.
- Recognise a journal append by type: the checker resolves no types; a per-file allowlist of
  callers would miss a call in a new file, which the shape test catches.
