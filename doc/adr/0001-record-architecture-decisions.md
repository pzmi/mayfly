# 0001. Record architecture decisions

Date: 2026-11-09

## Status

Proposed

## Context

Several lanes and agents change this repository, and `doc/tasks.md` carries a Decisions table
(D1-D15) whose reasons would otherwise live in chat history. Foundation section 10.3 already requires a durable record: Nygard format in `doc/adr/NNNN-kebab-title.md`, an
index checked by the `docs` stage, and a rule that a decision is never edited away. Parallel lanes
also clashed on ADR numbers and duplicated index rows, which is why `cargo xtask adr new` takes a
reserved `--number` (ported with xtask, ADR 0004).

## Decision

Architectural decisions are recorded as ADRs in `doc/adr/`, in Michael Nygard's format: a title,
a date, then Status, Context, Decision and Consequences. `cargo xtask adr new "<title>"` creates
the file and its index row; `cargo xtask adr index --fix` keeps `doc/adr/README.md` equal to the
files. A change that adds or reverses an architectural decision adds or supersedes an ADR in the
same commit. A decision is never edited away: a reversal is a new ADR and the old record becomes
`Superseded by NNNN`. A later ADR that only refines an earlier one is named in the earlier one's
Status (`Proposed (amended by NNNN)`) with a note in its text.

Status is `Accepted` for an owner decision and `Proposed` for a planner default that the owner may
override; tasks follow a Proposed decision as written until then, and only the owner flips one to
Accepted. `doc/tasks.md` cites decision D*n* as ADR *n*+1, so ADR 0001 is this record and
ADRs 0002-0016 are D1-D15.

## Consequences

The reasons behind the architecture survive the session that produced them, and the index cannot
drift from the files because a stage checks it. The cost is one small file per decision and the
discipline to write it in the commit that makes the decision. FND-03 writes the records for
D1-D15; ADRs 0017 and later come from the tasks that decided them. Enforced by `cargo xtask adr
index` in the `docs` stage; the practice is described in foundation section 10.3.
