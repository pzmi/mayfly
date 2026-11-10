-- 0004: projection generations (ES-04; doc/event-store.md sections 3.6, 4.1). Forward-only;
-- never edited once released. Expand-only: `projection_row` keeps its shape; a spec's meta row
-- names the generation readers see and units write (no row: generation 0, where units wrote
-- before), and `projection_stream` records per generation and stream the revision its rows
-- are the fold of: the head a unit wrote them at, or the last revision a rebuild folded.

CREATE TABLE projection_meta (
    spec TEXT NOT NULL,
    version INTEGER NOT NULL CHECK (version >= 1),
    active_generation INTEGER NOT NULL CHECK (active_generation >= 0),
    written_by TEXT NOT NULL,
    CONSTRAINT projection_meta_pkey PRIMARY KEY (spec)
) STRICT, WITHOUT ROWID;

CREATE TABLE projection_stream (
    spec TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation >= 0),
    stream_id TEXT NOT NULL COLLATE BINARY,
    applied INTEGER NOT NULL CHECK (applied >= 0),
    CONSTRAINT projection_stream_pkey PRIMARY KEY (spec, generation, stream_id)
) STRICT, WITHOUT ROWID;

-- Within a generation a stream's applied revision only rises: a unit writes the stream's new
-- head, and a rebuild replaces a stream's rows by deleting them first.
CREATE TRIGGER projection_stream_applied_never_falls BEFORE UPDATE ON projection_stream
WHEN NEW.applied < OLD.applied
    OR NEW.spec IS NOT OLD.spec
    OR NEW.generation IS NOT OLD.generation
    OR NEW.stream_id IS NOT OLD.stream_id
BEGIN
    SELECT RAISE(ABORT, 'projection_stream applied never falls');
END;
