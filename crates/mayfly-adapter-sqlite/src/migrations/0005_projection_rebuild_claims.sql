-- 0005: projection rebuild claims (ES-04F; doc/event-store.md section 3.6). Forward-only; never
-- edited once released. Expand-only: one row per spec while a rebuild of it runs, naming the
-- generation that rebuild claimed. `begin_rebuild` claims a generation above the active one and
-- above every one handed out before, replacing any earlier claim; `apply_stream` and `cutover`
-- are refused for a generation that is not the claimed one, so a second rebuild supersedes the
-- first instead of sharing its generation; the cutover removes the row.

CREATE TABLE projection_rebuild (
    spec TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation >= 1),
    CONSTRAINT projection_rebuild_pkey PRIMARY KEY (spec)
) STRICT, WITHOUT ROWID;

-- A spec's claims only rise: a superseded rebuild can never claim its generation back.
CREATE TRIGGER projection_rebuild_claim_rises BEFORE UPDATE ON projection_rebuild
WHEN NEW.generation <= OLD.generation OR NEW.spec IS NOT OLD.spec
BEGIN
    SELECT RAISE(ABORT, 'projection_rebuild claims only rise');
END;
