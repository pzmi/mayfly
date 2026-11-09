//! Fixture: the SQLite adapter records a manifest row with `INSERT OR IGNORE`, a form only
//! SQLite has; the portable `ON CONFLICT ... DO NOTHING` and a `WITHOUT ROWID` table are fine.

/// The manifest row, written the SQLite-only way.
pub const MANIFEST: &str = "INSERT OR IGNORE INTO event_kind_manifest (event_type) VALUES (?1)";

/// The same row, written portably.
pub const PORTABLE: &str =
    "INSERT INTO event_kind_manifest (event_type) VALUES (?1) ON CONFLICT (event_type) DO NOTHING";

/// The schema, which may use `WITHOUT ROWID`.
pub const SCHEMA: &str = include_str!("migrations/0001_schema.sql");
