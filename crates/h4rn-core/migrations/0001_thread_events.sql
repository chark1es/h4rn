-- Initial event log for the daemon's SQLite cache (ADR-003).
-- SQLite is a cache and offline buffer; h4rn-connect's Postgres log is the
-- system of record for relay-owned data (ADR-012).

CREATE TABLE IF NOT EXISTS thread_events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id   TEXT    NOT NULL,
    seq         INTEGER NOT NULL,
    event_json  TEXT    NOT NULL,
    recorded_at TEXT    NOT NULL,
    UNIQUE (thread_id, seq)
);

CREATE INDEX IF NOT EXISTS idx_thread_events_thread_seq
    ON thread_events (thread_id, seq);

-- Idempotent command receipts: a retried command returns the sequence it
-- already produced instead of appending a second turn (ADR-003).
CREATE TABLE IF NOT EXISTS command_receipts (
    device_id  TEXT    NOT NULL,
    command_id TEXT    NOT NULL,
    thread_id  TEXT    NOT NULL,
    last_seq   INTEGER NOT NULL,
    created_at TEXT    NOT NULL,
    PRIMARY KEY (device_id, command_id)
);
