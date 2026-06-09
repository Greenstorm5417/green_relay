-- Migration v1: create full schema for all five record types.
-- Inlined into the binary at build time via `include_str!` from `db.rs`.

CREATE TABLE IF NOT EXISTS outbound_messages (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    to_number     TEXT    NOT NULL,
    body          TEXT    NOT NULL,
    status        TEXT    NOT NULL,
    part_count    INTEGER NOT NULL,
    msg_reference TEXT,
    error_code    TEXT,
    created_at    TEXT    NOT NULL,
    updated_at    TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS inbound_messages (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    from_number TEXT    NOT NULL,
    body        TEXT    NOT NULL,
    received_at TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS api_keys (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    key_hash          TEXT    NOT NULL UNIQUE,
    key_identifier    TEXT    NOT NULL UNIQUE,
    custom_rate_limit INTEGER,
    revoked           INTEGER NOT NULL DEFAULT 0,
    created_at        TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS admin_users (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    username        TEXT    NOT NULL UNIQUE,
    password_hash   TEXT    NOT NULL,
    failed_attempts INTEGER NOT NULL DEFAULT 0,
    locked_until    TEXT,
    created_at      TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS audit_log (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    event_type     TEXT    NOT NULL,
    key_identifier TEXT,
    detail         TEXT,
    created_at     TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_inbound_received_at
    ON inbound_messages (received_at DESC);
