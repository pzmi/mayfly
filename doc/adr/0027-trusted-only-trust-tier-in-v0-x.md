# 0027. Trusted-only trust tier in v0.x

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09). Amended by 0090, which partly supersedes it: classify-before-acquire
holds on GHES only; on github.com assigned jobs are classified before a runner starts.

## Context

Runners of one scale set are interchangeable: GitHub gives a job to any idle runner of the set,
so trust can be enforced only per scale set, never per runner. Plan §1.3 had a boolean
`allow_untrusted_prs`. Untrusted code on a shared host needs network isolation (ADR 0032), caps
and no co-residency, none of which exists yet (`doc/domain-model.md` OD3).

## Decision

Every pool declares a `tier`; in v0.x the only accepted value is `trusted`. The listener
classifies each offer before acquiring it (*on GHES only since ADR 0090: github.com assigns
jobs directly*) (pure `demand::trust`); an offer classified untrusted,
an offer triggered by `pull_request_target`, `workflow_run` or `issue_comment`, and an offer the
classifier cannot classify are declined (`declined{reason: tier}`) and never acquired. The
documentation requires GitHub's fork-approval settings on every repository the scale set serves.
The key `allow_untrusted_prs` is refused at config load with a message naming `tier`; it is
never silently ignored.

## Consequences

- I32 (a pool serves one tier; offers are acquired only by a pool whose tier admits them).
  KER-06 (classification, mutant "acquire every offer"), RT-01 (config), SEC-01 (fork settings,
  reachability) implement it. What an offer carries for classification is confirmed by GH-00.
- An untrusted tier later is a new ADR with its own scale set, caps, Softnet (ADR 0032) and the
  reachability tests of SEC-01 as its gate.

## Alternatives considered

- An untrusted tier now: needs isolation and tests that do not exist yet.
- Keep `allow_untrusted_prs`: a toggle that widens trust for a whole pool with no isolation.
