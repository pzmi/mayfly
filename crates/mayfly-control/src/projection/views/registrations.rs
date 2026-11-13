//! `open_registrations`: the runner registrations not settled yet, kept inline from each
//! registration stream (RT-07, CP-05's note). The registration manager's pass reads these
//! rows instead of folding every registration stream ever written once a minute: a settled
//! registration (`Deregistered`, `Absent`) has no row.

use mayfly_control_domain::registration::{
    Phase, RegistrationEvent, RegistrationState, RunnerRegistration,
};
use mayfly_control_domain::{EventKind, EventSet};
use mayfly_domain::AttemptId;
use serde::{Deserialize, Serialize};

use crate::projection::spec::{EventMeta, ProjectionSpec, RowChange, Rows, sync_rows};

/// One registration not settled. Key: the registration's stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenRegistrationRow {
    /// The attempt it registers a runner for.
    pub attempt_id: AttemptId,
    /// Its phase, `snake_case`.
    pub phase: String,
    /// The stream revision this row is the fold of.
    pub revision: u64,
}

/// The `open_registrations` projection: a row per registration stream until it settles.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenRegistrations;

impl ProjectionSpec for OpenRegistrations {
    const NAME: &'static str = "open_registrations";
    const VERSION: u32 = 1;
    const SUBSCRIBES: &'static [EventKind] = <RegistrationEvent as EventSet>::KINDS;
    type Source = RunnerRegistration;
    type Row = OpenRegistrationRow;

    /// Pinned by `tests/store/projections.rs::should_keep_a_registration_row_only_when_it_is_not_settled`.
    fn apply(
        rows: &Rows<OpenRegistrationRow>,
        _event: &RegistrationEvent,
        after: &RegistrationState,
        at: EventMeta,
    ) -> Vec<RowChange<OpenRegistrationRow>> {
        let desired = match (after.attempt_id, settled(after.phase)) {
            (Some(attempt_id), false) => [(
                String::new(),
                OpenRegistrationRow {
                    attempt_id,
                    phase: phase_code(after.phase).to_owned(),
                    revision: at.revision.get(),
                },
            )]
            .into_iter()
            .collect(),
            _ => Rows::new(),
        };
        sync_rows(rows, desired)
    }
}

/// Whether a registration is settled: GitHub holds no runner for it any more, or never did.
#[must_use]
pub const fn settled(phase: Phase) -> bool {
    matches!(phase, Phase::Deregistered | Phase::Absent)
}

const fn phase_code(phase: Phase) -> &'static str {
    match phase {
        Phase::Unrequested => "unrequested",
        Phase::Requested => "requested",
        Phase::IssueUnknown => "issue_unknown",
        Phase::Issued => "issued",
        Phase::Delivered => "delivered",
        Phase::Connected => "connected",
        Phase::Consumed => "consumed",
        Phase::Completed => "completed",
        Phase::Deregistered => "deregistered",
        Phase::Absent => "absent",
    }
}
