//! The `Pool` aggregate (`doc/domain-model.md` §6.2): one configured pool's revision, tier,
//! image pin, limits, mode and breakers, stream `pool-<name>`.
//!
//! Plain data and two pure functions (ADR 0022). A pool decides nothing about capacity or
//! running counts (that is `Host`'s, through the terms policy P4 applies with [`terms`]); it
//! protects alone:
//!
//! - **I27**: stage failures per digest are counted once per attempt inside the window, only
//!   while the breaker is closed, and the one that reaches the threshold trips it; a tripped
//!   breaker closes the terms until an operator resets it
//!   (`tests/pool/rules.rs::should_trip_at_the_threshold_and_count_nothing_more_when_failures_repeat`,
//!   `tests/pool/rules.rs::should_close_the_terms_when_the_breaker_of_the_pin_trips`,
//!   `tests/pool/rules.rs::should_reset_only_by_an_operator_when_a_breaker_is_tripped`).
//! - **The pin is a verified digest**: `PinImage` without the image's verification is refused
//!   (`tests/pool/rules.rs::should_refuse_a_pin_when_the_image_is_not_verified`).
//! - **Tier immutability per revision** and **`min_prepared` defaults to 0** (ADR 0028)
//!   (`tests/pool/rules.rs::should_default_min_prepared_to_zero_when_the_configuration_names_none`,
//!   `tests/pool/rules.rs::should_refuse_a_configuration_when_its_revision_is_not_newer`).
//! - **I18**: an older run epoch is refused (`stale_run`); a newer one is recorded as
//!   `PoolEpochAdvanced` ahead of the command's events
//!   (`tests/pool/rules.rs::should_refuse_an_older_run_epoch_when_a_newer_one_was_accepted`).
//!
//! The model is `formal/scheduling.qnt` (`pausePool`, `resumePool`, `countFailure`,
//! `resetBreaker`, `applyTerms`).

mod cases;
mod decide;
mod events;
mod state;

pub use decide::{Decision, decide, evolve};
pub use events::{
    AttemptFailureCounted, ImagePinned, PoolBreakerReset, PoolBreakerTripped, PoolConfigured,
    PoolDecommissioned, PoolDrainCompleted, PoolDrainStarted, PoolEpochAdvanced, PoolEvent,
    PoolPaused, PoolResumed,
};
pub use state::{
    Breaker, ConfigurePool, Failure, MIN_PREPARED_DEFAULT, PoolCommand, PoolConfig, PoolMode,
    PoolRejection, PoolState, PoolTerms, TrustTier, terms,
};

use crate::aggregate::{Aggregate, DeterminismCase, DeterminismCases};
use crate::command::Command;

/// The `Pool` aggregate: a marker grouping its data and functions (ADR 0022).
#[derive(Debug, Clone, Copy, Default)]
pub struct Pool;

impl Aggregate for Pool {
    const NAME: &'static str = "pool";
    type State = PoolState;
    type Body = PoolCommand;
    type Event = PoolEvent;
    type Rejection = PoolRejection;

    fn initial() -> PoolState {
        PoolState::initial()
    }

    fn decide(state: &PoolState, command: Command<PoolCommand>) -> Decision {
        decide::decide(state, command)
    }

    fn evolve(state: PoolState, event: &PoolEvent) -> PoolState {
        decide::evolve(state, event)
    }
}

impl DeterminismCases for Pool {
    fn determinism_cases() -> Vec<DeterminismCase<Self>> {
        cases::cases()
    }
}
