# 0030. Runner baked into the image with refresh tooling

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09)

## Context

`actions/runner` is either baked into the image or downloaded and self-updated in the guest. A
runner with self-update off must be updated within 30 days of a GitHub runner release that
GitHub marks as required, or GitHub refuses it. Baked runners make every VM reproducible but give
images a shelf life (`doc/domain-model.md` OD12); the owner wants refreshing them to be
convenient.

## Decision

1. The runner is baked into the image at a pinned, SHA-checked version with self-update
   disabled. The image manifest records the runner version.
2. Expiry is detected (`ImageRunnerVersionExpired`, from the minimum GitHub publishes or a
   runner refusal signature) and trips the pool's breaker for that digest with reason
   `runner_version`: admission stops with a named reason, never silent queueing. Runner age is
   shown in `status` and warned about ahead of expiry.
3. **Refresh is one command**: `mayfly image refresh <pool>` builds a new local image base with
   the current pinned runner, verifies it, and moves the pool's pin through the journal,
   keeping the previous digest for rollback. Its steps are ADR 0039's (pull by digest, local
   service-owned bases, the runner installed through `tart exec`).
4. The image maintainer (the owner) keeps a refresh rhythm: monthly and on every required runner
   release.

## Consequences

KER-11, OPS-02 (manifest), OPS-06 (ADR 0039) (`image refresh`), P18 in the domain model implement it. A VM
never does work that is not the job; a forgotten refresh stops admission loudly instead of
failing jobs.

## Alternatives considered

- Runner self-update in the guest: the VM does non-job work, slower and not reproducible.
- Download the runner at job time: network, latency and supply-chain exposure per job.
