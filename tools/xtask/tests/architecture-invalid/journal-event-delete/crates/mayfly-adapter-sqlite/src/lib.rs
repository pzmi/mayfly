//! Fixture: the store adapter prunes history; a trigger naming `UPDATE ON` is not an update.

/// History is never rewritten.
pub const PRUNE: &str = "DELETE FROM journal_event WHERE stream_id = ?1";

/// The guard is allowed.
pub const GUARD: &str =
    "CREATE TRIGGER journal_event_no_update BEFORE UPDATE ON journal_event BEGIN SELECT 1; END";
