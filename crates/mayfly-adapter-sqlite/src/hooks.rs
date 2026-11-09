//! Commit barriers (`doc/event-store.md` §7.5): the points of an append's transaction where a
//! test or the store lab (ES-10) may strike. Production uses [`NoHooks`], whose every barrier
//! proceeds; the journal is generic over the hooks, so the barriers cost nothing there.
//!
//! Only this crate's tests and `tools/storelab-worker` name another implementation
//! (`ARCH-store-hooks`, ES-10). A barrier runs on the writer thread, inside the transaction,
//! while the store's write lock is held: a hook that blocks holds every other writer of the
//! file, which is how the lab makes others report `Busy`.

/// What an append does at a barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HookAction {
    /// Go on.
    Proceed,
    /// Fail as the store would before committing: roll back, report a definite `Storage`
    /// error. After the commit it can only be reported `OutcomeUnknown`.
    Fail,
    /// Fail as a lock wait that ran out: roll back, report `Busy`. After the commit,
    /// `OutcomeUnknown`.
    Busy,
    /// Lose the reply: before the commit, roll back and report `OutcomeUnknown` (the unit is
    /// absent); after it, report `OutcomeUnknown` with the unit present.
    LoseReply,
    /// The process dies here: the connection is dropped without a rollback statement (the
    /// engine discards the open transaction, as after a crash) and the caller gets
    /// `OutcomeUnknown`; the handle's later appends fail until it is reopened.
    Crash,
}

/// The barriers of one append, called on the writer thread in this order: `after_statement(0)`
/// right after `BEGIN IMMEDIATE`, `after_statement(n)` after the unit's `n`th statement,
/// `before_commit`, `COMMIT` (itself a statement, so `after_statement` follows it), then
/// `after_commit`. Statements are counted from 1 within each append; a unit refused by its own
/// reads stops early and never reaches `before_commit`.
pub trait StoreHooks: Send + Sync + 'static {
    /// After statement `statement` of the append (0: after `BEGIN`, before the first).
    fn after_statement(&self, statement: u32) -> HookAction {
        let _ = statement;
        HookAction::Proceed
    }

    /// Every statement of the unit ran; the commit is next.
    fn before_commit(&self) -> HookAction {
        HookAction::Proceed
    }

    /// The commit returned.
    fn after_commit(&self) -> HookAction {
        HookAction::Proceed
    }

    /// Whether a connection closing now checkpoints the WAL into the database file, as a clean
    /// close does; `false` leaves the WAL for the next open to recover, as a crash would.
    fn checkpoint_on_close(&self) -> bool {
        true
    }
}

/// The production hooks: every barrier proceeds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoHooks;

impl StoreHooks for NoHooks {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_proceed_at_every_barrier_when_hooks_are_the_production_ones() {
        // given
        let hooks = NoHooks;

        // when
        let actions = [
            hooks.after_statement(0),
            hooks.after_statement(7),
            hooks.before_commit(),
            hooks.after_commit(),
        ];

        // then
        assert_eq!(actions, [HookAction::Proceed; 4]);
        assert!(hooks.checkpoint_on_close());
    }
}
