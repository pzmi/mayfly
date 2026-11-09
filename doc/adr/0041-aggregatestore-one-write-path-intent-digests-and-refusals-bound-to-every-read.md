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

## Amendment (2026-11-09): ES-03R, the remaining store rules and the review of ES-03

The ES-03 review bypassed rule 7 without touching the journal: a path call by the concrete type
(`MemoryJournal::append(&j, u)`), by a generic (`T::append(j, u)`), by `Self::` or through an
alias (`use Journal as J; J::append(j, u)`) named no `Journal` the rule looked for, and
`Journal :: append` inside a macro's tokens was not scanned. Its exemptions were also wider than
their reasons: every `impl Journal` block anywhere, and all of an adapter's test code.

1. **Rule 7 by shape, not name.** A path call whose last segment is `append` (two segments or
   more, or a qualified self type) is refused unless its first argument, the receiver, is
   `&mut ..` (`Vec::append(&mut a, &mut b)`); an uncalled `P::append` is refused; both also in
   macro tokens. `impl Journal` blocks are exempt only in journal adapter crates, the test kit
   and test code (a production `impl Journal` elsewhere is itself refused), and an adapter's test
   code only in its contract test files (`tests/*_contract.rs`).
2. **`execute` takes no reads.** `AggregateStore::execute` retries the request as it was built,
   so a read of another stream taken before the first attempt would be re-checked at its old
   revision and conflict on every attempt. A request with reads is refused
   (`SaveError::ReadsOutsideRetry`, before any read); a decision that reads other streams builds
   them inside its own `with_revision_retry` closure. The alternative, documenting the trap and
   leaving it to callers, would turn a programming error into three wasted decisions and a
   misleading `Conflict`.
3. **Content locks.** `migrations.lock` and `fixtures.lock` at the repository root list the
   SHA-256 of every store migration (`crates/*/src/migrations/**.sql`) and every golden fixture
   (`crates/*/tests/fixtures/**`) in the `sha256sum` format. A migration is **released once its
   line is in the lock**, which the commit adding it writes; `ARCH-migration-frozen` and
   `ARCH-fixtures-lock` fail on a changed, removed or unlisted file and print the line a new file
   needs, and the checker never writes a lock. A fixture of a regenerable family is changed with
   its line in the same commit and the reason in the commit body (verification.md §4 rule 2);
   an event, protocol or migration line never changes. The rules run in the architecture stage,
   so `check` and CI already run them and no stage list changes. Considered: a `fixtures`
   stage of its own (two more places to keep in lockstep for no extra power); comparing with the
   committed lock through `git` (a checker that behaves differently in a tarball, a shallow
   clone and a fixture directory, and that a commit editing both file and line passes anyway).
   What a lock cannot refuse, a commit that changes a released file and its line together, is
   the one edit it makes conspicuous in review.
4. **`ARCH-no-global-order` and `ARCH-journal-sql`** hold what is checkable of ES-I17 and of
   "nothing rewrites history": ordering keys in the store adapter's SQL, global-read names and
   multi-stream event returns in the control plane's API; store tables named only by the store
   adapter, and no `UPDATE`/`DELETE` of `journal_event` or `journal_identity` outside the
   adapter's tests (JC-27). Their limits (SQL assembled at run time, an order taken in Rust) are
   documented in the rule modules and kept by review.

A redelivered message whose command was refused is decided again (rule 6), so the scale-set
listener acks it after the refusal receipt commits, as after an acceptance; a lost ack's
redelivery is answered by that receipt while the bound streams are unchanged (GH-04, ES-05).
