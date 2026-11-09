//! Fixture: a consumer query of the store adapter reads the journal in recording order.

/// One stream's events in revision order: allowed.
pub const READ_STREAM: &str =
    "SELECT payload FROM journal_event WHERE stream_id = ?1 ORDER BY revision";

/// Every stream's events by time: a global order.
pub const RECENT: &str = "SELECT stream_id, payload FROM journal_event ORDER BY recorded_at LIMIT ?1";
