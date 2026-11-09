# 0001. Record architecture decisions

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09); amended 2026-11-09.

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

Status is `Accepted` for an owner decision *(statuses extended by the amendment below)* and `Proposed` for a planner default that the owner may
override; tasks follow a Proposed decision as written until then, and only the owner flips one to
Accepted. `doc/tasks.md` cites decision D*n* as ADR *n*+1, so ADR 0001 is this record and
ADRs 0002-0016 are D1-D15 *(no new D-ids since the amendment below)*.

## Consequences

The reasons behind the architecture survive the session that produced them, and the index cannot
drift from the files because a stage checks it. The cost is one small file per decision and the
discipline to write it in the commit that makes the decision. FND-03 writes the records for
D1-D15; ADRs 0017 and later come from the tasks that decided them. Enforced by `cargo xtask adr
index` in the `docs` stage; the practice is described in foundation section 10.3.

## Amended 2026-11-09 (owner): records, not rules; dated amendments; Rejected; ids

- **An ADR is a decision record, not a rule.** It records a decision in its context at the time.
  When the context changes the decision may change, deliberately and on the record; an ADR is
  never cited as a reason not to do the better thing, and code never drifts from one silently.
  The rules, fixtures and tests that pin an ADR change in the same commit as the ADR.
- **Dated amendments in place.** A refinement that keeps the decision (a bound, a name, a scope)
  is appended as `## Amended YYYY-MM-DD (owner|planner): <what>` with its reason; the original
  Context and Decision stay, and a replaced passage gets an italic pointer. A refinement by a
  later record adds `amended by NNNN` to this record's status. A reversal is still a superseding
  ADR. An amendment of an Accepted record by anyone but the owner is marked `amendment proposed`
  until the owner accepts it.
- **Statuses** carry the decider and date: `Accepted (owner, YYYY-MM-DD)`, and a new
  `Rejected (owner, YYYY-MM-DD)` for a considered and declined option. The `docs` stage's parser
  accepts `Rejected` from FND-07 on; until then no record uses it.
- **Ids.** The ADR number is the decision id. D1-D15 stay as aliases (D*n* = ADR *n*+1); no new
  D-id is minted. Owner decisions of `doc/domain-model.md` (OD*n*) and `doc/event-store.md` (E*n*)
  name the ADR that records them once decided.
