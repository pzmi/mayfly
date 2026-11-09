// -*- mode: Bluespec; -*-
/**
 * The smoke model (FND-06): a bounded counter, the smallest model that exercises every stage of
 * the formal toolchain (typecheck, `quint test`, simulation and Apalache verification).
 * It models no part of mayfly. Real models follow doc/formal-models.md: one `allInvariants`
 * value combining every invariant, a `step` relation, and named tests.
 */
module smoke {
  // The counter never exceeds `LIMIT`; `reset` returns it to zero.
  pure val LIMIT = 3

  var count: int

  action init = {
    count' = 0
  }

  action increment = all {
    count < LIMIT,
    count' = count + 1,
  }

  action reset = all {
    count > 0,
    count' = 0,
  }

  action step = any {
    increment,
    reset,
  }

  val withinLimit = count >= 0 and count <= LIMIT

  // Every invariant of the model, so a new one cannot be left out of a stage.
  val allInvariants = and {
    withinLimit,
  }

  // The counter climbs to the limit and no further.
  run climbsToLimitTest = {
    init.then(increment).then(increment).then(increment)
      .then(all { assert(count == LIMIT), count' = count })
      .then(increment.fail())
  }

  // A reset returns the counter to zero.
  run resetReturnsToZeroTest = {
    init.then(increment).then(reset)
      .then(all { assert(count == 0), count' = count })
  }

  // Only a non-zero counter can be reset.
  run resetNeedsNonZeroTest = {
    init.then(reset.fail())
  }
}
