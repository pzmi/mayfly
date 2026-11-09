# 0028. Scale sets only with prepared unregistered VMs

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09)

## Context

Plan §7.3 kept a classic mode (`config.sh --ephemeral` with a registration token) as a smoke
test and fallback, and plan §7.2 counted registered-idle runners (`warm_registered`) in demand.
Classic mode is a second registration path, a second token kind and a demand signal that is not
GitHub's. Registered-idle standby runners hold a live credential ahead of demand and can be
given a job that was then cancelled ("poisoned idle"); one prepared macOS VM halves a host
(`doc/domain-model.md` OD8, OD18).

## Decision

1. **Scale sets only.** Demand comes only from GitHub Runner Scale Sets with JIT runners. There
   is no classic or `--ephemeral` registration mode in any form, not even as a smoke test: no
   `[github] mode` key, no registration-token client. The `RunnerRegistry` port keeps only what
   the reaper needs (list, get and delete a runner by recorded id).
2. **Only Prepared VMs ahead of demand.** A Prepared VM is booted, verified, unregistered, holds
   no secret and never ran workflow code. Pools set `min_prepared` (default 0; macOS pools
   default 0 and need an explicit per-pool override for more). There is no registered standby:
   `desired = min(deliverable, assigned)`. Prepared VMs expire after `max_prepared_age` and on
   host wake.

## Consequences

- GH-05 is removed; plan §7.3 is gone; F07 is redefined for JIT registrations; the
  `RunnerRegistry` contract row loses its classic adapter; fake-github drops the
  registration-token endpoint. KER-06 and CP-03 implement the demand formula.
- Latency for the first job on a cold pool is a clone and boot; operators who want less set
  `min_prepared`.

## Alternatives considered

- Classic mode as smoke test only (DDD-01 recommendation) or fully supported.
- Registered standby (`min_standby`): lower latency for more exposure and GitHub budget.
