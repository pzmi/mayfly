# 0069. The capacity carve keeps the headroom and what quarantines owe

Date: 2026-11-10

## Status

Accepted (owner, 2026-11-10), with both policies: `EXACT` (KER-05D) and `DEMAND` (DEC-06), per
pool, the default `exact` until GH-00 answers `doc/github-scaleset.md` §12 item 6. Refines the
capacity policy of ADR 0024 (the control plane's allocation within the data plane's report);
the data plane's accounting is unchanged. Amended 2026-11-11 (owner, GH-00R): the evidence
says the header caps assignment; `demand` stays, an explicit opt-in marked experimental.

## Context

`capacity::plan` carves a host's free capacity into each pool's carve-out, which becomes its
scale set's `maxCapacity`. KER-05H made the carve monotone: an earlier pool (name order) keeps
room for `max_running` minus its live runners of its VMs before a later pool is carved, because
handing a later pool what the earlier one left let more foreign use carve more. Its cost: a
later pool can stay empty beside room the earlier pool cannot use (13 GiB free, too little for
one 15 GiB macOS VM, while Linux jobs wait). The owner (2026-11-10): "Let's keep it safe than
tightly packed. If we can be more reactive and more useful for users we should design and model
it."

`doc/domain-model.md` §6.3.1 states the properties (S: the carve-outs fit together, I24; M1: less
free capacity never carves any pool more; M2: releasing a quarantine never carves any pool less;
U: no starvation, and nothing kept that M1 and M2 do not need), shows that no policy is safe,
monotone and work conserving with a priority order, proves that with S, M1 and the order a later
pool is carved at most what the free capacity holds beyond the earlier pool's headroom, and
models six candidates in `formal/scheduling.qnt` (`carvePoliciesTest`, `carveScenariosTest`,
`sustainedLoadTest`).

KER-05H's keep counted a quarantined lease twice: its holds are already out of the free capacity,
and its pool kept a whole VM for its place again. A quarantined VM stays until an operator
resolves it (ADR 0025), so the double count lasted as long.

## Decision

1. **An open pool keeps ahead of the pools after it its headroom of its VMs, and for each
   quarantined place only what its quarantined lease does not hold** (a stopped one: its
   memory; a running one: nothing), at most one VM per place (`capacity::plan`,
   `formal/scheduling.qnt`'s `keeps`). The keep still never depends on the free capacity, so
   S, M1 (now pool by pool), M2 and U1 hold; every pool is carved at least what KER-05H carved
   it, more whenever an earlier pool has a quarantined lease. This is the most any policy with
   S, M1, M2 and the name order can carve a later pool (§6.3.1).
2. **The properties are pool by pool.** M1 and M2 are checked for each pool's carve-out, not
   only the total: a later pool that gains when an earlier one loses takes its room VM by VM
   and starves its large VMs.
3. **Demand-aware keeping is a per-pool option, off by default (owner, 2026-11-10, DEC-06:
   "implement both now").** A pool's `capacity_policy` is `"exact"` (the default: points 1 and
   2) or `"demand"`: its headroom is cut to the VMs it waits for, so it is carved, and keeps
   ahead of the pools after it, only for them; what it keeps for its quarantined places is
   unchanged (`capacity::CapacityPolicy::Demand { waiting }`, `formal/scheduling.qnt`'s
   `demandShape`). `waiting` is the pool's scale set's latest `totalAssignedJobs` beyond its live
   runners plus its `min_prepared` deficit, never capped by its own carve-out (a capped count
   feeds back: carved nothing, told nothing). For a fixed `waiting` it is `EXACT` over a smaller
   headroom, so S, M1 and M2 hold pool by pool, and no `EXACT` pool is carved less than if every
   pool were `EXACT`. **It depends on an unverified fact**: if `X-ScaleSetMaxCapacity` caps
   `totalAssignedJobs` (`doc/github-scaleset.md` §12 item 6), a pool carved nothing never sees
   its job and starves (U1 fails). So the default stays `"exact"` until GH-00 answers item 6.
   (*Public evidence says it caps it at 0: see the amendment of 2026-11-11.*)
   The key is per pool, not per host: what matters is what each earlier pool holds back, so the
   operator relaxes exactly the pool whose idle headroom starves the others (in the 13 GiB case,
   the macOS pool), and each pool's keep still depends only on its own inputs. The key is
   optional and new in schema 1; a build that predates it refuses a file that sets it (unknown
   keys are refused, ADR 0046), so a rollback fails loudly instead of carving by another
   policy.

Options (carve-outs as (macOS, Linux), §6.3.1):

| Option | Monotone pool by pool, release monotone | 13 GiB free, idle macOS pool | Running quarantined macOS VM, 49 GiB free | Sustained Linux load: macOS job starts |
|:--|:--|:--|:--|:--|
| KER-05H: a whole VM per place | yes | (0, 0) | (1, 4) | after 4 Linux VMs end |
| **This ADR: headroom + what quarantines owe** | yes | (0, 0) | (1, 8) | after 4 Linux VMs end |
| Hand the later pool the leftover | no (KER-05H's bug) | (0, 3) | (1, 8) | never |
| Leftover clipped to stay monotone in total | in total only | (0, 1) | (1, 8) | never |
| Keep only for waiting jobs | yes, for a fixed demand | (0, 3) | (1, 8) | after 4, if demand is seen beyond `maxCapacity`; never otherwise |
| Smaller VM first, leftover | no | (0, 3) | (0, 9) | never |

## Consequences

- DEC-06 (DEMAND): pinned by `formal/scheduling.qnt`'s `carveMonotoneDemandTest`,
  `demandFixesThe13GiBCaseTest` (S1: `EXACT` (0, 0), `DEMAND` (0, 3); S2 with a job waiting
  (0, 0)), `demandStarvesWhenDemandIsInvisibleTest` (sustained load: the macOS job never starts
  when only jobs assigned under `maxCapacity` are seen) and `demandLetsTheLaterPoolInTest` (the
  state machine, whose `init` now chooses each pool's policy and a fixed number of waiting jobs,
  so every invariant of `allInvariants` is checked under both policies); and by
  `tests/capacity.rs::should_fit_and_stay_monotone_when_pools_mix_policies_and_inputs_are_random`,
  `should_carve_no_pool_less_when_a_quarantine_is_released_and_pools_mix_policies`,
  `should_carve_later_pools_at_least_exact_when_earlier_pools_keep_only_for_demand`,
  `should_carve_as_exact_when_every_demand_covers_its_headroom`,
  `should_carve_a_later_pool_beside_an_idle_one_when_the_earlier_keeps_only_for_demand` and
  `should_keep_room_for_a_waiting_job_when_the_earlier_pool_keeps_only_for_demand`; the config
  key by `crates/mayfly-composition/tests/config.rs` (`full.toml`, the default, and
  `capacity_policy_invalid.toml`). A `demand` pool that waits for nothing is held
  `HoldReason::NoDemand`. The reconciler (CP-03) fills `waiting` from the scale set's
  statistics; until then the policy is built and tested, not wired.

- Operator-visible: with a quarantined lease in an earlier pool, later pools are carved more and
  their scale sets get a larger `maxCapacity` than under KER-05H; nothing else changes. Pinned
  by `tests/capacity.rs::should_keep_only_what_a_quarantined_lease_does_not_hold_when_an_earlier_pool_has_one`,
  `should_keep_the_memory_a_stopped_quarantined_lease_owes_when_slots_bind`,
  `should_carve_every_pool_at_least_the_whole_place_keep_when_inputs_are_random`,
  `should_carve_no_pool_more_when_foreign_consumption_grows_and_inputs_are_random`,
  `should_carve_no_pool_less_when_a_quarantined_lease_is_released_and_inputs_are_random`; the
  model's `carveMonotone`, `carvePoliciesTest`, `carveScenariosTest`, `sustainedLoadTest`.
- The headline cost of KER-05H stays: a later pool may wait beside room an earlier pool cannot
  use now. That room is what lets the earlier pool's large VM start under sustained load.
- A `max_running` the host can never run keeps room no VM will use (§6.3.1, S6). Capping the
  keep at the host's totals would make the carve grow when totals shrink; refusing or explaining
  such a `max_running` at configuration is left to the configuration tasks.

## Alternatives considered

The other rows of the table: their failures are checked in `formal/scheduling.qnt`
(`carvePoliciesTest`, `sustainedLoadTest`). An explicit pool priority in place of name order
was not explored: it reorders who waits and changes nothing above.

## Amended 2026-11-11 (owner, GH-00R): `demand` stays, experimental

**Context.** Decision 3 rested on whether `X-ScaleSetMaxCapacity` caps `totalAssignedJobs`
(`doc/github-scaleset.md` §12 item 6, now §5). Public evidence says it does, at least at 0: a
GitHub maintainer closed ARC #3446 on 2025-07-01 with "the back-end would not assign jobs to the
scale set anymore if the capacity is 0" (doc-stated); and on github.com scale sets advertising
0 kept `totalAssignedJobs` at 0 and received no message for matching queued jobs over 100 s,
while advertising 1 got the job assigned and counted (scaleset #133, 2026-09-30,
server-observed). That is the world of `formal/scheduling.qnt`'s
`demandStarvesWhenDemandIsInvisibleTest`: a `demand` pool carved 0 never sees its job.

**Decision.** `exact` stays the default. `demand` stays, as an **explicit, experimental
opt-in**: a pool gets it only by `capacity_policy = "demand"` in config, documented as
experimental and as able to starve the pool when its carve-out reaches 0; nothing turns it on by
default. The model's starvation test is the real behaviour and is kept. HW-02 measures what lies
beyond 0 (spec §12 item 3: whether `totalAvailableJobs` counts a job held back by 0, and what
lowering the header does), which decides whether any `demand` variant can see waiting jobs.

**Consequences.** The config documentation (`mayfly_composition::config::CapacityPolicy`, plan
§9) marks `demand` experimental; no code path changes.
