//! Fixture: the store adapter and its migrations.

/// The first migration.
pub const M0001: &str = include_str!("migrations/0001_journal.sql");

/// The second, not yet locked.
pub const M0002: &str = include_str!("migrations/0002_inbox.sql");
