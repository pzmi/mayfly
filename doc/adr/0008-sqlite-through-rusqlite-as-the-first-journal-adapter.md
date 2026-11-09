# 0008. SQLite through rusqlite as the first journal adapter

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09); amended by 0021

## Context

This record's original text, kept as the decision of its time (ADR 0001): persistence is SQLite
through `rusqlite` with the `bundled` feature, owned by **one writer thread** behind the async
`Journal` port; WAL, `synchronous=FULL`, forward-only raw-SQL migrations. `sqlx` was rejected
because it brings compile-time database checking and a second SQLite stack beside the one already
in use. Plan section 5.2 and foundation section 6 followed it, including "one writer connection".

Since then the event-store design was reworked (ADR 0021). The owner wants the journal
storage-neutral: the stream-scoped design has no global order, so no correctness property rests
on a single writer, and a PostgreSQL adapter is expected to follow SQLite without changing the
`Journal` port or its contract suite.

## Decision

**Amended by ADR 0021.** The `Journal` port and its contract suite are storage-neutral. SQLite
through `rusqlite` (bundled), with WAL, `synchronous=FULL` and forward-only raw-SQL migrations,
is the **first adapter** (ES-02); a PostgreSQL adapter follows later and passes the same suite.
`sqlx` stays rejected for the SQLite adapter, for the reasons above.

A single writer thread behind an async port remains how the SQLite adapter is built (a blocking
library kept off the runtime workers, foundation section 4.4), but it is an **implementation
detail of that adapter**, never a premise of the port, the contract suite or any caller: nothing
may depend on a global write order, and the suite includes racing writers and independent
streams to prove it.

## Consequences

Status is `Accepted (owner, 2026-11-09); amended by 0021`: a reader
follows ADR 0021 where the two differ. Plan section 5.2's description of one writer connection,
its global `sequence` and foundation section 4.4's channel to a writer thread should be read as the
SQLite adapter's internals. Implemented by ES-01 (port, memory journal, contract suite) and ES-02
(SQLite adapter, migrations, crash matrix); the PostgreSQL adapter has no task yet.
