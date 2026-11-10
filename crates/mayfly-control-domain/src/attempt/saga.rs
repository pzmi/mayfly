//! The attempt saga's decision (policy P6 with P7's deadline, `doc/domain-model.md` §7): what
//! the saga does next for one attempt, read from the attempt's folded state alone. A pure
//! table; the durable consumer that runs it (`mayfly_control::saga`) only loads the state,
//! mints the command's id and deadline, and saves.
//!
//! Reading the state, not the event that woke the saga, is what makes a redelivered or late
//! event harmless: it is judged against where the attempt is now, so it never issues a
//! directive the aggregate would refuse, and never a second one while one is pending (I13).

use mayfly_domain::{Deadline, DirectiveId};

use super::decide::issuable;
use super::state::{AttemptState, DirectiveKind, Outcome, Pending, Phase};

/// What the saga does for an attempt in a given state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SagaStep {
    /// `IssueNext`: the aggregate would issue this directive now.
    Issue(DirectiveKind),
    /// A directive was sent: wait for its answer until `deadline`, then
    /// `RecordDirectiveTimeout` (its outcome becomes `Unknown`, never a success).
    AwaitAnswer {
        /// The pending directive.
        directive_id: DirectiveId,
        /// Its deadline.
        deadline: Deadline,
    },
    /// The VM is deleted or lost: `Close` the stream.
    Close,
    /// Nothing for the saga: the attempt waits on an observation (an `Unknown` outcome leaves
    /// only on one, I10, re-observed by P19), on another policy (activation, a job,
    /// retirement), or is closed.
    Idle,
}

/// The saga's decision table (`doc/domain-model.md` §7, P6 and P7):
///
/// | state | step |
/// |:--|:--|
/// | not started, closed | `Idle` |
/// | a directive issued, not sent yet (held by the relay) | `Idle` (no deadline runs) |
/// | a directive sent, no answer yet | `AwaitAnswer` until its deadline |
/// | a directive whose outcome is `Unknown` | `Idle` (only an observation moves it, I10) |
/// | `Deleted`, `Lost` | `Close` |
/// | nothing pending, and `IssueNext` would issue a directive ([`issuable`]) | `Issue` |
/// | anything else (`Prepared`, `Listening`, `Busy`, `Finished`, spent budgets, a `Backoff` whose retirement waits for its deregistration (KER-04G), a quarantined VM with nothing to stop) | `Idle` |
///
/// Pinned by `tests/attempt/saga.rs::should_issue_exactly_when_issue_next_would_be_accepted`
/// and `should_follow_the_decision_table_when_each_matrix_state_is_read`.
#[must_use]
pub fn next_step(state: &AttemptState) -> SagaStep {
    match state.phase {
        Phase::NotStarted | Phase::Closed => return SagaStep::Idle,
        Phase::Reserved
        | Phase::Cloning
        | Phase::Booting
        | Phase::Verifying
        | Phase::Prepared
        | Phase::Activating
        | Phase::Listening
        | Phase::Busy
        | Phase::Finished
        | Phase::Retiring
        | Phase::Deleting
        | Phase::Deleted
        | Phase::Lost
        | Phase::Backoff { .. }
        | Phase::Quarantined { .. } => {}
    }
    match state.pending {
        Some(Pending {
            directive_id,
            outcome: Outcome::InFlight,
            deadline,
            sent: true,
            ..
        }) => {
            return SagaStep::AwaitAnswer {
                directive_id,
                deadline,
            };
        }
        // issued, not sent yet (the relay holds it in safe mode, without a session, until the
        // attempt is adopted): its outcome is known, nothing happened, and no deadline runs
        Some(Pending {
            outcome: Outcome::InFlight,
            sent: false,
            ..
        }) => return SagaStep::Idle,
        Some(Pending {
            outcome: Outcome::Unknown { .. },
            ..
        }) => return SagaStep::Idle,
        None => {}
    }
    match state.phase {
        Phase::Deleted | Phase::Lost => SagaStep::Close,
        Phase::NotStarted
        | Phase::Closed
        | Phase::Reserved
        | Phase::Cloning
        | Phase::Booting
        | Phase::Verifying
        | Phase::Prepared
        | Phase::Activating
        | Phase::Listening
        | Phase::Busy
        | Phase::Finished
        | Phase::Retiring
        | Phase::Deleting
        | Phase::Backoff { .. }
        | Phase::Quarantined { .. } => match issuable(state) {
            Ok(Some(kind)) => SagaStep::Issue(kind),
            Ok(None) | Err(..) => SagaStep::Idle,
        },
    }
}
