-- 0001: the journal (doc/event-store.md section 4.1). Forward-only; never edited once released.
-- Constraint names are the PostgreSQL schema's (ES-09), so a refusal is classified by the
-- constraint a statement guards and `cargo xtask schema-parity` can compare the two.

CREATE TABLE schema_meta (
    version INTEGER NOT NULL,
    applied_at INTEGER NOT NULL,
    compat_floor INTEGER NOT NULL,
    CONSTRAINT schema_meta_pkey PRIMARY KEY (version)
) STRICT, WITHOUT ROWID;

CREATE TABLE stream_head (
    stream_id TEXT NOT NULL COLLATE BINARY,
    aggregate TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 0),
    required_epoch INTEGER NOT NULL CHECK (required_epoch >= 0),
    CONSTRAINT stream_head_pkey PRIMARY KEY (stream_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE journal_identity (
    event_id TEXT NOT NULL CHECK (length(event_id) = 32),
    stream_id TEXT NOT NULL COLLATE BINARY,
    revision INTEGER NOT NULL CHECK (revision >= 0),
    CONSTRAINT journal_identity_pkey PRIMARY KEY (event_id),
    CONSTRAINT journal_identity_place_key UNIQUE (event_id, stream_id, revision)
) STRICT, WITHOUT ROWID;

CREATE TABLE journal_event (
    stream_id TEXT NOT NULL COLLATE BINARY,
    revision INTEGER NOT NULL CHECK (revision >= 0),
    event_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    schema_version INTEGER NOT NULL CHECK (schema_version >= 0),
    payload TEXT NOT NULL,
    recorded_at INTEGER NOT NULL,
    CONSTRAINT journal_event_pkey PRIMARY KEY (stream_id, revision),
    CONSTRAINT journal_event_identity_fkey FOREIGN KEY (event_id, stream_id, revision)
        REFERENCES journal_identity (event_id, stream_id, revision)
) STRICT, WITHOUT ROWID;

CREATE TABLE event_kind_manifest (
    event_type TEXT NOT NULL COLLATE BINARY,
    schema_version INTEGER NOT NULL CHECK (schema_version >= 0),
    first_recorded_at INTEGER NOT NULL,
    CONSTRAINT event_kind_manifest_pkey PRIMARY KEY (event_type, schema_version)
) STRICT, WITHOUT ROWID;

CREATE TABLE command_receipt (
    command_id TEXT NOT NULL CHECK (length(command_id) = 32),
    command_type TEXT NOT NULL,
    intent TEXT NOT NULL CHECK (length(intent) = 64),
    outcome TEXT NOT NULL,
    CONSTRAINT command_receipt_pkey PRIMARY KEY (command_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE inbox (
    source TEXT NOT NULL,
    message_id TEXT NOT NULL,
    recorded_at INTEGER NOT NULL,
    CONSTRAINT inbox_pkey PRIMARY KEY (source, message_id)
) STRICT, WITHOUT ROWID;

-- Inline projection rows as keyed documents. ES-04 brings generations; until then every row is
-- in generation 0.
CREATE TABLE projection_row (
    spec TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation >= 0),
    key TEXT NOT NULL,
    stream_id TEXT NOT NULL COLLATE BINARY,
    payload TEXT NOT NULL,
    CONSTRAINT projection_row_pkey PRIMARY KEY (spec, generation, key)
) STRICT, WITHOUT ROWID;

CREATE INDEX projection_row_stream_idx ON projection_row (spec, generation, stream_id);

-- History is never rewritten (ES-I7) and heads and epochs never fall (ES-I2, ES-I12): the
-- store refuses it as a second line behind the code (JC-27).
CREATE TRIGGER journal_event_no_update BEFORE UPDATE ON journal_event
BEGIN
    SELECT RAISE(ABORT, 'journal_event is append-only');
END;

CREATE TRIGGER journal_event_no_delete BEFORE DELETE ON journal_event
BEGIN
    SELECT RAISE(ABORT, 'journal_event is append-only');
END;

CREATE TRIGGER journal_identity_no_update BEFORE UPDATE ON journal_identity
BEGIN
    SELECT RAISE(ABORT, 'journal_identity is append-only');
END;

CREATE TRIGGER journal_identity_no_delete BEFORE DELETE ON journal_identity
BEGIN
    SELECT RAISE(ABORT, 'journal_identity is append-only');
END;

CREATE TRIGGER stream_head_no_lowering BEFORE UPDATE ON stream_head
WHEN NEW.revision < OLD.revision
    OR NEW.required_epoch < OLD.required_epoch
    OR NEW.stream_id IS NOT OLD.stream_id
    OR NEW.aggregate IS NOT OLD.aggregate
BEGIN
    SELECT RAISE(ABORT, 'stream_head never moves back');
END;

CREATE TRIGGER stream_head_no_delete BEFORE DELETE ON stream_head
BEGIN
    SELECT RAISE(ABORT, 'stream_head is never removed');
END;
