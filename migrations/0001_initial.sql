-- Agent Gateway initial schema.
--
-- Conventions:
--   * every timestamp column is UTC **milliseconds** since the Unix epoch,
--     stored as INTEGER. Milliseconds (rather than seconds) keep events from
--     a single fast agent turn distinguishable in the timeline.
--   * `seq` is assigned by the gateway, is gap-free per session and starts
--     at 1. The UNIQUE constraint below is what makes that a database-level
--     guarantee rather than a convention.

CREATE TABLE machines (
    id              TEXT PRIMARY KEY,
    name            TEXT    NOT NULL,
    platform        TEXT    NOT NULL,
    hostname        TEXT    NOT NULL,
    version         TEXT    NOT NULL,
    public_endpoint TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    last_seen_at    INTEGER
);

CREATE TABLE sessions (
    id             TEXT PRIMARY KEY,
    machine_id     TEXT    NOT NULL,
    agent_id       TEXT    NOT NULL,
    agent_name     TEXT    NOT NULL,
    acp_session_id TEXT,
    workspace      TEXT    NOT NULL,
    cwd            TEXT    NOT NULL,
    title          TEXT,
    origin         TEXT    NOT NULL,
    status         TEXT    NOT NULL,
    last_seq       INTEGER NOT NULL DEFAULT 0,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);

CREATE INDEX idx_sessions_machine_updated ON sessions (machine_id, updated_at DESC);

CREATE TABLE events (
    id         TEXT PRIMARY KEY,
    session_id TEXT    NOT NULL,
    seq        INTEGER NOT NULL,
    timestamp  INTEGER NOT NULL,
    event_type TEXT    NOT NULL,
    payload    TEXT    NOT NULL,
    UNIQUE (session_id, seq)
);

CREATE INDEX idx_events_session_seq ON events (session_id, seq);

CREATE TABLE devices (
    id           TEXT PRIMARY KEY,
    name         TEXT    NOT NULL,
    public_key   TEXT    NOT NULL,
    platform     TEXT    NOT NULL,
    revoked      INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    last_seen_at INTEGER
);

CREATE TABLE pairing_codes (
    code        TEXT PRIMARY KEY,
    nonce       TEXT    NOT NULL,
    expires_at  INTEGER NOT NULL,
    consumed_at INTEGER
);

CREATE TABLE ws_tickets (
    id         TEXT PRIMARY KEY,
    device_id  TEXT    NOT NULL,
    machine_id TEXT    NOT NULL,
    nonce      TEXT    NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at    INTEGER
);

CREATE INDEX idx_ws_tickets_device ON ws_tickets (device_id);
