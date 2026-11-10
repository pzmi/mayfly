-- 0003: a dispatch mark only rises (ES-05F; doc/event-store.md sections 4.1, 5.1). Forward-only;
-- never edited once released. 0002 states it in a comment and every write keeps it (a unit's
-- and a bootstrap's upsert raise the mark only `WHERE excluded.marked > stream_dispatch.marked`,
-- an advance deletes it only when it is at most the delivered revision); this trigger makes the
-- store refuse an update that lowers it or moves the row, like the checkpoint and the fence.
-- A delete is not refused: an advance and a retire remove marks by design.

CREATE TRIGGER stream_dispatch_mark_never_falls BEFORE UPDATE ON stream_dispatch
WHEN NEW.marked < OLD.marked
    OR NEW.consumer IS NOT OLD.consumer
    OR NEW.stream_id IS NOT OLD.stream_id
BEGIN
    SELECT RAISE(ABORT, 'stream_dispatch mark never falls');
END;
