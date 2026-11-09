# intake

Dispatch: inline

## Inputs

`$ARGUMENTS`: task id, issue number or URL, optional size override; the caller's prompt (worktree, branch, gate, target dir, reserved ADR numbers, "do not push").

## Steps

1. **Find the task.** Task id: read its section in `doc/tasks.md` and its row in the overview (dependencies, model, ⛳ mark), plus the Decisions table and "Rules for every task". Issue number: read it with MCP `issue_read` (or `gh issue view` when `gh` exists); if its title starts with a task id, use that task's section too. Absent: pick per SKILL.md "Input".
2. **Dependencies.** Every dependency must already be committed (`git log --oneline | grep " <dep>:"`). A missing one: stop and report; do not start work that builds on nothing.
3. **Read the citations.** Read every `doc/plan.md`, `doc/foundation.md` (fnd) and `doc/verification.md` (ver) section the task cites, fnd §2 (dependency law) and ver §2 (test levels), the ADRs in `doc/adr/` that touch the affected crates, and every existing file the task modifies. Not the documents whole.
4. **Plane.** Name the plane each affected crate belongs to (control, data, shared kernel/protocol, composition/cli, test-only, tool-only) from plan §4 and `architecture.toml`. A task that seems to need a control crate to touch a data crate (or the reverse) is a design question, not an edge to add.
5. **Definition of Ready** (fnd §11): observable behaviour, invariants (the I-numbers of ver §6.1 it touches), affected crates and plane, threat model when inputs, secrets, processes or deletes change, test levels, model impact (does it touch lifecycle, scheduling, recovery, the scale-set session or the link? then the Quint model changes first), contract suites it must pass, testable `Accept:`, `(owner)` criteria. Not met: ask the owner one short question; if the gap is only wording, fix it minimally in `doc/tasks.md` and say so.
6. **Classify** per `processes.md`.
7. **Branch and gate.** Stay on the branch you were given (`main`, or the worktree lane the caller named). Record the gate `doc/tasks.md` names as live, and whether you own the push (the caller said "do not push": you do not).
8. Write `work-order.json` and `intake.json`.

## Output

```
{ taskId, title, criteria[], ownerCriteria[], cites[], dependsOn[], affectedCrates[], plane: "control"|"data"|"shared"|"runtime"|"test"|"tool"|"mixed", invariants[], modelImpact: "none"|"<model>.qnt", contractSuites[], size, process, checkpoint: bool, branch, gate, pushOwner: bool, issues[] }
```

## Sensors

Task section found; every dependency committed; `criteria` non-empty (each `Accept:` clause one entry, `(owner)` ones in `ownerCriteria`).

## Gates

To plan: always.
