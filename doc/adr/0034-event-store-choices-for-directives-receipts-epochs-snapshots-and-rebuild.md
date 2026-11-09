# 0034. Event store choices for directives, receipts, epochs, snapshots and rebuild

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09)

## Context

`doc/event-store.md` §8.3 put nine questions to the owner (E1 to E9, numbered O1 to O9 there).
E1 accepted ADR 0021; E4 and E5 are ADR 0035. This record holds the rest, several of which
change ADR 0021's consequences: ES-01 kept a mutable outbox table ordered by `next_attempt_at`,
receipts of refusals with no bound, and deferred epochs and snapshots.

## Decision

1. **Directives are events (E2).** A directive is `DirectiveIssued{directive_id, attempt_id,
   generation, kind, deadline, secret: Option<SecretHandle>}` on the attempt or placement
   stream, delivered at least once by the `directive_relay` durable consumer. There is no
   outbox table: "sent" is the relay's checkpoint, the outcome is the aggregate's state, a
   deadline is the saga's `RecordDirectiveTimeout` command.
2. **Refusal receipts bound to a revision (E3).** An accepted command's receipt answers a repeat
   for ever. A refused command writes a receipt bound to the stream and revision it was decided
   at: a repeat while the stream is still at that revision gets the recorded refusal; once the
   stream moved, the command is decided afresh, never replayed, and the new outcome replaces the
   refusal receipt. Every receipt holds the canonical intent digest; a reused command id with
   another intent is refused `CommandIdReused`. At most one accepted outcome per command id.
3. **Stream epochs now (E6)**, in ES-01F and ES-03.
4. **The store's Quint model precedes delivery (E7)**: ES-06 before ES-05.
5. **Snapshots are built (E8)**, ES-08.
6. **Projection rebuild is online (E9)**: a shadow generation is built stream by stream
   while every unit writes its inline rows to both the active and the shadow generation; a
   cutover fence flips the active generation in one transaction and refuses a unit computed for
   the old generation set. A version mismatch never blocks admission.

## Consequences

Amends ADR 0021 (outbox, refusal receipts, online rebuild) and ADR 0014 (the handle travels in an
event, not an outbox row). ES-01F realigns the landed ES-01; ES-02 to ES-12 implement the rest
(`doc/event-store.md` §8); ES-I5 and JC-14 change.

## Alternatives considered

- E2: keep the outbox table, scanned by key with a time filter.
- E3: no refusal receipts (receipts of accepted commands only), or unbound ones that replay a stale refusal.
- E9: rebuild only at start-up under the owner lease.

## Amended 2026-11-09 (ES-03; accepted by the owner 2026-11-09)

ADR 0041 refines decision 2: a refusal receipt is bound to **every** stream the
decision read, each at the revision read, not to one; it answers while all are unchanged and is
decided afresh once any moved. A refusal bound to one stream would be replayed after another
input of the decision moved.
