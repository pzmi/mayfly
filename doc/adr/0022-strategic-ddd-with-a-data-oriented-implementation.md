# 0022. Strategic DDD with a data-oriented implementation

Date: 2026-11-09

## Status

Accepted

## Context

The owner asked for domain-driven design, and DDD-01 produced `doc/domain-model.md` by event
storming: bounded contexts, a context map, a ubiquitous language and aggregates as consistency
boundaries. DDD's *tactical* patterns are often read as object-oriented ones: rich entities that
hide their fields behind methods, repositories per entity, value objects as classes, and a
judgement that a model of plain data plus functions is "anemic". The owner does not want that
style. Rust's strengths, and mayfly's verification strategy (pure `decide`/`evolve`, determinism
cases, golden fixtures, Quint conformance that compares plain state after every step), favour
transparent data transformed by pure functions.

## Decision

DDD is used **strategically**: bounded contexts, the context map, the ubiquitous language,
aggregates as consistency boundaries (one stream, one transactional decision) and policies as
process managers, as `doc/domain-model.md` defines them. Names in code follow the ubiquitous
language exactly.

The **implementation is data-oriented**:

1. Domain state, commands and events are plain data: structs with public fields and enums,
   deriving what they need (`Debug`, `Clone`, `PartialEq`, serde where allowed). No getters or
   setters, no encapsulation for its own sake; an "anemic" model is the intended shape.
2. Behaviour is free, pure functions over that data: `decide(&State, Command) -> Result<Vec<Event>,
   Rejection>`, `evolve(State, &Event) -> State`, policy functions, capacity and recovery tables.
   The `Aggregate` trait (KER-03) is only a compile-time grouping of those functions and their
   associated data types; it has no `self` and no hidden state.
3. Invariants live in the functions that decide, in exhaustive matches over enums and in
   transition tables, not in constructors that make invalid states unrepresentable through
   privacy. Validated *value types* at the boundary (ids, `OwnedVmName`, digests) are the one
   exception: they parse once and keep their check, because they guard ownership and safety.
4. Prefer tables and enums over trait hierarchies; prefer whole-state transforms and explicit
   data flow over shared mutable objects; prefer plain collections of records (`BTreeMap`, `Vec`)
   over object graphs. No inheritance-style trait layering, no repositories per entity: the
   journal and projections are the only persistence abstractions.
5. Read models are plain rows produced by folding events; they carry no behaviour.

## Consequences

- KER-04..11 write aggregates as data + free functions; reviews refuse getters/setters and
  privacy ceremony that adds no safety (`.claude/skills/review/SKILL.md`).
- Conformance and determinism checks compare plain state directly, which keeps them strong.
- `doc/domain-model.md` §6 is read as consistency boundaries and rules, not class designs.
- `CLAUDE.md` and foundation §6 state this rule; ADR 0018 (event codec and registry) stands.
