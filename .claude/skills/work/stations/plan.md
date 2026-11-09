# plan

Dispatch: inline

## Inputs

intake.{taskId, criteria, ownerCriteria, cites, affectedCrates, plane, invariants, modelImpact, contractSuites, process}

## Steps

1. **Existing plan?** If `.work/<id>/plan.md` exists, reuse it and honour ticked items (resume).
2. **Explore** only what the task names: the cited sections, the files it modifies, and the nearest existing pattern (a sibling crate or module, an existing test of the same level, an existing contract suite or golden fixture).
3. **Write the checklist** to `.work/<id>/plan.md`: one line per step, `- [ ] path: what`, each traceable to an `Accept:` clause or a `CLAUDE.md` rule. It must cover, where relevant:
   - **the crate each item belongs in** per fnd §2 and plan §4: pure crates (`mayfly-domain`, `mayfly-protocol`, `*-domain`) hold types, `decide`/`evolve`, policies and FSMs with no I/O, Tokio, clock, randomness or env; application crates (`mayfly-control`, `mayfly-data`) hold use cases and port traits; adapters translate one technology and never name a sibling adapter or composition; composition is the only wiring; the CLI only parses and delegates
   - **the plane**: control and data never meet except through `mayfly-protocol` messages and composition; the data plane stores nothing durable
   - **crate-scoped steps**: each step inside one crate; a port another crate consumes, or composition wiring, is its own step
   - **a new crate or dependency**: workspace `members` entry, `architecture.toml` entry and external allowlist in the same commit, `[lints] workspace = true`
   - **model first** (ver §6) when `modelImpact` is not `none`: the Quint model and invariants, named scenarios, `MODELS` registration, the conformance driver's observation, then the Rust code; plus a negative control
   - **event sourcing**: each new event with `EVENT_TYPE`, `SCHEMA_VERSION`, registry entry and golden fixture; each aggregate with accepted and refused `DETERMINISM_CASES` and a full State × Stimulus matrix; writes only through `AggregateStore::save`; a migration as a new forward-only file
   - **effects and safety**: where an outcome can be ambiguous (`Unknown`), where an epoch or generation is checked, where ownership proof is required, which secrets flow and how they are redacted
   - **the test levels** touched (ver §2): unit, property, component, contract, architecture, integration, e2e, compile-fail, conformance; one test per `Accept:` clause at least, and each contract suite in `contractSuites` run against every implementation the task names, with a deliberately broken one
   - **new dependencies**: `cargo add`, why this crate, its licence, and whether `deny.toml` accepts it
   - **ADRs**: a step that adds or reverses an architectural decision (a dependency of consequence, a crate boundary, a protocol, security or recovery policy, a lint or rule exception) runs `cargo xtask adr new "<title>"` (with the reserved `--number`) or marks an existing one `Superseded by NNNN`, and `cargo xtask adr index --fix`
   - **the decision test**: any user-, operator- or network-visible change (a command, directive, default, metric, log field, dependency, outbound call) passes `doc/philosophy.md` §8; a conflict goes to the owner
   - **`(owner)` criteria**: what is delivered up to them (workflow, gated test, docs) and left pending
4. **Owner question.** LARGE with a genuine design fork: one short question with a recommended default, then continue on the answer. No fork: no question.
5. Write `plan.json`.

## Output

```
{ planPath, checklist[], testLevels[], newDependencies[], adrs[], issues[] }
```

## Sensors

`plan.md` exists and has at least one item per `Accept:` clause; `newDependencies` empty or each justified; a model-first item precedes the code when `modelImpact` is not `none`.

## Gates

To implement: always.
