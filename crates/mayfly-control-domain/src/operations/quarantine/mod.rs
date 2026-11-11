//! The `QuarantineCase` aggregate (`doc/domain-model.md` §6.10): the operator's case for one
//! quarantined attempt, stream `quarantine-<attempt_id>`. There is no retention deadline and no
//! automatic deletion (OD9, ADR 0025).
//!
//! Plain data and two pure functions (ADR 0022). It protects alone:
//!
//! - **Resolution needs an operator** (I28): no timer, cap, policy or the startup resolves a
//!   case; the dispositions are `delete_with_proof` (the data plane re-checks the proof, and
//!   the case needs the recorded instance id), `released_observed_absent` (only on a complete
//!   inventory that did not list the VM) and `keep`
//!   (`tests/quarantine/rules.rs::should_resolve_only_by_an_operator_when_any_actor_tries`,
//!   `tests/quarantine/properties.rs::should_never_resolve_or_act_when_no_operator_commands`,
//!   `tests/quarantine/matrix.rs::should_define_every_pair_when_each_phase_meets_each_command`).
//! - **Escalation never acts**: it records a higher level and nothing else
//!   (`tests/quarantine/properties.rs::should_never_resolve_or_act_when_no_operator_commands`).
//! - **The closed reason set**, as the attempt's [`QuarantineReason`]; since ADR 0066's
//!   amendment a vanished VM is not quarantined, so no case opens for
//!   `vm_vanished_externally`, while a case whose VM a complete inventory shows absent may
//!   still be released on that observation
//!   (`tests/quarantine/rules.rs::should_refuse_to_open_a_case_when_the_vm_vanished`).
//! - **I18**: an older run epoch is refused (`stale_run`); a newer one is recorded as
//!   `QuarantineEpochAdvanced`.
//!
//! It does not account for capacity (the `Host`'s lease) nor free the slot (the attempt's, on
//! an observed absence). Not modelled in `formal/recovery.qnt` beyond `quarantineNeverDeleted`;
//! see `doc/formal-models.md` (recovery, "Not modelled").
//!
//! [`QuarantineReason`]: crate::attempt::QuarantineReason

mod cases;
mod decide;
mod events;
mod state;

pub use decide::{Decision, decide, evolve};
pub use events::{
    QuarantineCaseEscalated, QuarantineCaseEvent, QuarantineCaseInspected, QuarantineCaseOpened,
    QuarantineCaseResolved, QuarantineCaseResolvedV1, QuarantineEpochAdvanced,
};
pub use state::{
    Case, CasePhase, Disposition, EscalationLevel, Holds, QuarantineCaseCommand,
    QuarantineCaseRejection, QuarantineCaseState,
};

use crate::aggregate::{Aggregate, DeterminismCase, DeterminismCases};
use crate::command::Command;

/// The `QuarantineCase` aggregate: a marker grouping its data and functions (ADR 0022).
#[derive(Debug, Clone, Copy, Default)]
pub struct QuarantineCase;

impl Aggregate for QuarantineCase {
    const NAME: &'static str = "quarantine";
    type State = QuarantineCaseState;
    type Body = QuarantineCaseCommand;
    type Event = QuarantineCaseEvent;
    type Rejection = QuarantineCaseRejection;

    fn initial() -> QuarantineCaseState {
        QuarantineCaseState::initial()
    }

    fn decide(state: &QuarantineCaseState, command: Command<QuarantineCaseCommand>) -> Decision {
        decide::decide(state, command)
    }

    fn evolve(state: QuarantineCaseState, event: &QuarantineCaseEvent) -> QuarantineCaseState {
        decide::evolve(state, event)
    }
}

impl DeterminismCases for QuarantineCase {
    fn determinism_cases() -> Vec<DeterminismCase<Self>> {
        cases::cases()
    }
}
