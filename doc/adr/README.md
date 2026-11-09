# Architecture decision records

This directory holds the decisions behind mayfly, one file per decision, in Michael Nygard's lightweight ADR format: `# NNNN. Title`, a date, then **Status**, **Context**, **Decision** and **Consequences**. To add a record, run `cargo xtask adr new "<title>"` (or `--number N` for a number the orchestrator reserved): it copies [`template.md`](template.md) with the next free number and adds the row here; commit both together. `cargo xtask adr index` checks the rows against the files and `cargo xtask adr index --fix` rewrites them. A decision is never edited away: a reversal is a new ADR, and the old record's status becomes `Superseded by NNNN`. **Accepted** records are owner decisions. **Proposed** records are planner defaults that the owner may override. `doc/tasks.md` cites decision D*n* as ADR *n*+1.

| ADR | Title | Status |
|---|---|---|
| [0017](0017-serde-derive-in-the-shared-kernel.md) | Serde derive in the shared kernel | Proposed |
| [0018](0018-event-codec-and-aggregate-registry.md) | Event codec and aggregate registry | Proposed |
| [0019](0019-plane-separation-over-dev-closures-and-policy-driven-planes.md) | Plane separation over dev closures and policy-driven planes | Proposed |
| [0020](0020-control-plane-time-and-the-journal-s-recorded-at.md) | Control-plane time and the journal's recorded_at | Proposed |
| [0021](0021-a-storage-neutral-stream-scoped-event-store.md) | A storage-neutral, stream-scoped event store | Proposed |
