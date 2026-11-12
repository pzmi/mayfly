# 0045. The GitHub App permission list is a published commitment

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09). Decides philosophy P4. Amended 2026-11-12 (owner, GH-00S): the
permission list, drafted in GH-00R and accepted by the owner on 2026-11-12 (ADR 0090).

## Context

mayfly runs jobs on the operator's machines with a GitHub App's authority. The narrower the
App's permissions, the less a compromised key or a mayfly bug can do (philosophy §5.3).

## Decision

1. The **exact minimal permission list**, with the reason for each permission, is published in
   the documentation as a commitment.
2. **Widening it needs the documentation and an ADR first**, then the code.
3. `mayfly doctor` **warns when the installed App holds more** than the list needs.
4. The list itself is **to be verified**: GH-00 and HW-02 confirm it against a real App in a
   disposable organisation, and fill it in. Until then the documentation says "unverified" and
   no permission is guessed (plan §7.1).

## Consequences

- GH-02 adds the permission check that doctor uses; the documented list is pinned by a test
  against the fake-github fixture, and by HW-02 on a real App.
- An organisation-wide scale set needs some organisation-level permission; the list says which
  and why, rather than defaulting to a broad one.

## Alternatives considered

- Documenting permissions loosely: lets them widen by drift, which this record forbids.

## Amended 2026-11-12 (owner, GH-00S): the permission list

Drafted by the planner on 2026-11-11 (GH-00R), revised for ADR 0090, and accepted by the owner on
2026-11-12.

The list is drafted from GitHub's documentation in `doc/github-scaleset.md` §13 and follows ADR
0090 (owner, 2026-11-12):

- **base**: the organisation's **"Self-hosted runners: Read and write"** (the runner-registration
  exchange, scale sets, sessions, JIT configs, runner reads and removal) and the repositories'
  **"Actions: Read"** (the workflow-run lookup of every assigned job);
- **optional**: **"Actions: Write"** in place of Read, only for a pool with
  `cancel_refused_runs`; the organisation's **"Administration: Read"**, only with
  `check_fork_pr_setting`.

The runner-group read of ADR 0090's public-repository report is covered by "Self-hosted
runners". "Metadata: Read-only" comes with any repository permission and is not used. The
refusal sink of ADR 0090 needs nothing more: it is a JIT runner like any other. Repository scope (tests only) needs "Administration: Read and write"
and "Metadata: Read-only". `doctor` compares the App's grant with what the configured options
need. The list stays unverified (`PUBLISHED_PERMISSIONS` is `Unverified`) until HW-02 runs the
whole chain with exactly these permissions (spec §12 item 7); decision 4 stands.
