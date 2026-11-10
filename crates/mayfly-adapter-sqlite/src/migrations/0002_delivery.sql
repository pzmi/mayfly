-- 0002: durable delivery (ES-05; doc/event-store.md sections 3.4, 4.1, 5.1). Forward-only; never
-- edited once released. Rows are keyed by (consumer, stream_id) and read by key only: nothing
-- here is ordered by a time or a position (ES-I17), and `retry_at` is a filter.

-- The registered consumers. Every unit marks each of them for each stream it appends, in its
-- own transaction (ES-I1).
CREATE TABLE delivery_consumer (
    consumer TEXT NOT NULL COLLATE BINARY,
    registered_at INTEGER NOT NULL,
    bootstrapped INTEGER NOT NULL CHECK (bootstrapped IN (0, 1)),
    CONSTRAINT delivery_consumer_pkey PRIMARY KEY (consumer)
) STRICT, WITHOUT ROWID;

-- The mark: the stream holds events up to `marked` the consumer may not have delivered. It only
-- rises, and an advance deletes it only when it is at most the delivered revision.
CREATE TABLE stream_dispatch (
    consumer TEXT NOT NULL COLLATE BINARY,
    stream_id TEXT NOT NULL COLLATE BINARY,
    marked INTEGER NOT NULL CHECK (marked >= 0),
    retry_at INTEGER,
    CONSTRAINT stream_dispatch_pkey PRIMARY KEY (consumer, stream_id)
) STRICT, WITHOUT ROWID;

-- The last revision the consumer advanced to; it never moves back (ES-I14).
CREATE TABLE consumer_checkpoint (
    consumer TEXT NOT NULL COLLATE BINARY,
    stream_id TEXT NOT NULL COLLATE BINARY,
    revision INTEGER NOT NULL CHECK (revision >= 0),
    CONSTRAINT consumer_checkpoint_pkey PRIMARY KEY (consumer, stream_id)
) STRICT, WITHOUT ROWID;

-- The lease, judged by the store clock. The fence is also the claim's token: it rises with
-- every claim and is never reset, not even when the consumer is retired.
CREATE TABLE consumer_lease (
    consumer TEXT NOT NULL COLLATE BINARY,
    stream_id TEXT NOT NULL COLLATE BINARY,
    lease_until INTEGER NOT NULL,
    fence INTEGER NOT NULL CHECK (fence >= 1),
    CONSTRAINT consumer_lease_pkey PRIMARY KEY (consumer, stream_id)
) STRICT, WITHOUT ROWID;

CREATE TRIGGER consumer_checkpoint_never_back BEFORE UPDATE ON consumer_checkpoint
WHEN NEW.revision < OLD.revision
    OR NEW.consumer IS NOT OLD.consumer
    OR NEW.stream_id IS NOT OLD.stream_id
BEGIN
    SELECT RAISE(ABORT, 'consumer_checkpoint never moves back');
END;

CREATE TRIGGER consumer_lease_fence_never_falls BEFORE UPDATE ON consumer_lease
WHEN NEW.fence < OLD.fence
    OR NEW.consumer IS NOT OLD.consumer
    OR NEW.stream_id IS NOT OLD.stream_id
BEGIN
    SELECT RAISE(ABORT, 'consumer_lease fence never falls');
END;

CREATE TRIGGER consumer_lease_no_delete BEFORE DELETE ON consumer_lease
BEGIN
    SELECT RAISE(ABORT, 'consumer_lease keeps its fence');
END;
