//! The command envelope. The caller supplies the id, the time, the run epoch and the actor:
//! `decide` mints none of them.

use mayfly_domain::{CommandId, Epoch, Millis};

/// Who issued a command (`doc/domain-model.md` §5.3): recorded with every event it causes, and
/// read by `decide` where a rule names an actor (only an operator resolves a quarantine).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    /// An operator, by the Unix uid that reached the operator socket.
    Operator {
        /// The peer's uid.
        uid: u32,
    },
    /// A policy or process manager of `doc/domain-model.md` §7, by its fixed `snake_case` name
    /// (`attempt_saga`, `reconciler`, ...).
    Automation {
        /// The policy's name.
        policy: &'static str,
    },
    /// The startup use case (recovery, adoption).
    Startup,
}

/// A command addressed to one aggregate: its idempotency key, the instant the caller issued it
/// (a reading of the control plane's `Clock` port: Unix-epoch wall-clock milliseconds, ADR
/// 0020), the run epoch it was issued under, who issued it, and the aggregate-specific body.
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
    /// The run epoch of the daemon that issued it (`doc/domain-model.md` §6.0): an aggregate
    /// refuses an epoch lower than the highest it has accepted, so a zombie daemon, a copied
    /// state directory or a delayed reactor of an earlier run cannot write (I18).
    pub run_epoch: Epoch,
    /// Who issued it.
    pub actor: Actor,
    /// What the command asks of the aggregate.
    pub body: B,
}

impl<B> Command<B> {
    /// An envelope around `body`.
    #[must_use]
    pub const fn new(
        command_id: CommandId,
        issued_at: Millis,
        run_epoch: Epoch,
        actor: Actor,
        body: B,
    ) -> Self {
        Self {
            command_id,
            issued_at,
            run_epoch,
            actor,
            body,
        }
    }
}
