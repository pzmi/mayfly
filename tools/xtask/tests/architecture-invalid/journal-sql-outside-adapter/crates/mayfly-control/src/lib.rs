//! Fixture: the store crate reads journal rows with its own SQL instead of the port.

/// Every event of a stream, bypassing the adapter.
pub const READ: &str = "SELECT payload FROM journal_event WHERE stream_id = ?1";

/// Prose naming a table is not SQL.
pub const NOTE: &str = "the inbox key of a message";
