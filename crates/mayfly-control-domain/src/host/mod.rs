//! The `Host` aggregate (`doc/domain-model.md` §6.3): one host's capacity reports, slots,
//! leases, pool terms and admission gate, stream `host-<host_id>`.
//!
//! **Authority (ADR 0024):** the data plane accounts for and enforces the host's resources and
//! reports them; `Host` *allocates* within the last report it applied. Its leases are
//! allocation bookkeeping: when they disagree with the data plane, the data plane's refusal
//! wins. Plain data and two pure functions (ADR 0022); admission arithmetic is
//! [`crate::capacity`]'s, shared with the capacity policy so the two never disagree.
//!
//! The model is `formal/scheduling.qnt`; its invariants and the tests that pin them here:
//!
//! - **I01, I02, generationsMonotonic**: a reservation takes the lowest slot no lease holds,
//!   under that slot's next generation; a generation is never reused
//!   (`tests/host/properties.rs::should_strictly_increase_generations_per_slot_when_commands_are_random`).
//! - **I03, allocationWithinReport, I33's control-plane half**: every live lease, activated
//!   and quarantined ones included, counts on every dimension against the last applied report;
//!   a reservation that does not fit is refused, and an older report is ignored
//!   (`tests/host/properties.rs::should_allocate_within_the_last_report_when_commands_are_random`,
//!   `tests/host/rules.rs::should_never_allocate_beyond_the_newer_report_when_a_stale_one_arrives`).
//! - **G3, macosGuestsAtMostTwo**: at most two macOS guests, foreign ones included
//!   (`tests/host/rules.rs::should_admit_two_macos_guests_at_most_when_foreign_guests_run`).
//! - **I08**: no reservation and no grant after drain start; the drain completes only with no
//!   lease left (`tests/host/rules.rs::should_admit_nothing_when_the_host_drains`).
//! - **I26, noGrantWhileClosed**: no reservation and no grant while the gate or the pool's
//!   terms are closed (`tests/host/rules.rs::should_refuse_a_grant_when_any_term_or_cause_is_closed`).
//! - **I28, quarantineHoldsCapacity**: a quarantined lease keeps its slot and counts against
//!   capacity and its pool's `max_running` until its VM is observed gone; nothing releases it on
//!   a timer or a cap (`tests/host/rules.rs::should_hold_capacity_when_a_lease_is_quarantined`).
//! - **I18**: an older run epoch is refused (`stale_run`); a newer one is recorded as
//!   `HostEpochAdvanced` ahead of the command's events.
//!
//! Slots are one table across guest-OS classes (a slot number is unique on the host); the
//! macOS class is a limit over it (G3), not a table of its own.

mod cases;
mod decide;
mod events;
mod state;

pub use decide::{Decision, decide, evolve};
pub use events::{
    AdmissionClosed, AdmissionOpened, CapacityReportApplied, DiskLowWaterCrossed, DiskRecovered,
    DrainForced, HostDrainCancelled, HostDrainCompleted, HostDrainStarted, HostEpochAdvanced,
    HostEvent, HostRegistered, LeaseActivated, LeaseQuarantined, PoolTermsApplied, SlotReleased,
    SlotReserved,
};
pub use state::{
    AdmissionCause, AppliedReport, ApplyPoolTerms, Drain, ForceScope, Gate, Holds, HostCommand,
    HostPoolTerms, HostRejection, HostState, Lease, LeasePhase, PoolDemand, ReleaseEvidence,
    ReserveSlot, ResourceThreshold,
};

use crate::aggregate::{Aggregate, DeterminismCase, DeterminismCases};
use crate::command::Command;

/// The `Host` aggregate: a marker grouping its data and functions (ADR 0022).
#[derive(Debug, Clone, Copy, Default)]
pub struct Host;

impl Aggregate for Host {
    const NAME: &'static str = "host";
    type State = HostState;
    type Body = HostCommand;
    type Event = HostEvent;
    type Rejection = HostRejection;

    fn initial() -> HostState {
        HostState::initial()
    }

    fn decide(state: &HostState, command: Command<HostCommand>) -> Decision {
        decide::decide(state, command)
    }

    fn evolve(state: HostState, event: &HostEvent) -> HostState {
        decide::evolve(state, event)
    }
}

impl DeterminismCases for Host {
    fn determinism_cases() -> Vec<DeterminismCase<Self>> {
        cases::cases()
    }
}
