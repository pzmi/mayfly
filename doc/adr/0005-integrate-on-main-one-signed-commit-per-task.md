# 0005. Integrate on main, one signed commit per task

Date: 2026-11-09

## Status

Accepted

## Context

Plan work is cut into small tasks (`doc/tasks.md`) that several agent lanes implement in
parallel worktrees. A pull-request flow would add a review round trip per task for a
single-owner repository, while integrating without a gate would let a red commit reach `main`.
Foundation section 10.2 describes the chosen middle: each task is one commit, and the
orchestrator, not the implementing agent, integrates it.

## Decision

The whole plan is integrated directly on `main`. Each task is one commit with the subject
`<ID>: <summary>` and a DCO `Signed-off-by` trailer, followed by the attribution trailers. The
orchestrator lands it with `cargo xtask land` after a green full gate on the merged tree;
implementing agents never push, force-push, tag or merge. The checkpoint tasks (the flagged tasks
in `doc/tasks.md`) and CI on `main` are the quality net: a review range is due at each
checkpoint, and a red `main` is fixed at once by a new commit.

## Consequences

Easier: linear history, one reviewable unit per task, no branch bookkeeping. Harder: a bad
commit is caught after the fact (by the checkpoint review or CI), not before, so the local gate
must be the same as CI's (`doctor` when they disagree, ADR 0016) and landing conflicts are
auto-resolved only in `Cargo.lock` and the ADR index. An ADR number clash between lanes is
avoided by reserving numbers (`adr new --number`). Accepted: owner decision.
