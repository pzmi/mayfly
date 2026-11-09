//! The command envelope. The caller supplies the id and the time: `decide` mints neither.

use mayfly_domain::{CommandId, Millis};

/// A command addressed to one aggregate: its idempotency key, the instant the caller issued it
/// (a reading of the control plane's `Clock` port: Unix-epoch wall-clock milliseconds, ADR
/// 0020), and the aggregate-specific body.
///
/// `decide` reads time only from `issued_at`, which is what keeps it deterministic (checked
/// for every registered aggregate by the control domain's
/// `should_decide_deterministically_when_control_aggregates_run_their_cases`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command<B> {
    /// The key of the command's receipt (fnd §6.4): a repeat returns the recorded result.
    pub command_id: CommandId,
    /// When the caller issued the command, read from the control plane's `Clock` before calling
    /// `decide`: wall-clock milliseconds since the Unix epoch, so an instant an event derives
    /// from it still means the same after a restart (ADR 0020).
    pub issued_at: Millis,
    /// What the command asks of the aggregate.
    pub body: B,
}

impl<B> Command<B> {
    /// An envelope around `body`.
    #[must_use]
    pub const fn new(command_id: CommandId, issued_at: Millis, body: B) -> Self {
        Self {
            command_id,
            issued_at,
            body,
        }
    }
}
