-- A WITHOUT ROWID table is allowed (doc/event-store.md section 4.4); INSERT OR IGNORE in a
-- comment is not SQL.
CREATE TABLE event_kind_manifest (event_type TEXT NOT NULL PRIMARY KEY) STRICT, WITHOUT ROWID;
