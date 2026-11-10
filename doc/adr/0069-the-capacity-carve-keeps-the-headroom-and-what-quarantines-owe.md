# 0069. The capacity carve keeps the headroom and what quarantines owe

Date: 2026-11-10

## Status

Proposed (KER-05D). Refines the capacity policy of ADR 0024 (the control plane's allocation
within the data plane's report); the data plane's accounting is unchanged.

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
3. **Demand-aware keeping waits for GH-00.** Keeping room only for an earlier pool's waiting
   jobs (`DEMAND`) would let a later pool run beside an idle one, but our demand is
   `min(deliverable, totalAssignedJobs)`; if `X-ScaleSetMaxCapacity` caps `totalAssignedJobs`
   (`doc/github-scaleset.md` §12 item 6, unverified), a pool carved nothing never sees its job
   and starves. Owner's decision once item 6 is answered.

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
