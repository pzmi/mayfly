-- 0002: added without its lock line.
CREATE TABLE inbox (source TEXT NOT NULL, message_id TEXT NOT NULL, PRIMARY KEY (source, message_id)) STRICT, WITHOUT ROWID;
