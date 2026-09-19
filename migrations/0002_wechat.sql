CREATE TABLE wechat_bindings (
    id TEXT PRIMARY KEY,
    machine_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    chat_id TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE wechat_inbox (
    id TEXT PRIMARY KEY,
    binding_id TEXT NOT NULL,
    message_id TEXT NOT NULL UNIQUE,
    text TEXT NOT NULL,
    context_token TEXT NOT NULL,
    delivered INTEGER NOT NULL DEFAULT 0,
    received_at INTEGER NOT NULL
);

CREATE TABLE wechat_cursor (
    binding_id TEXT PRIMARY KEY,
    cursor TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE wechat_outbox (
    id TEXT PRIMARY KEY,
    binding_id TEXT NOT NULL,
    source_id TEXT NOT NULL,
    role TEXT NOT NULL,
    text TEXT NOT NULL,
    context_token TEXT NOT NULL,
    status TEXT NOT NULL,
    sent_parts INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_wechat_inbox_binding_received ON wechat_inbox(binding_id, received_at);
CREATE INDEX idx_wechat_outbox_binding_status ON wechat_outbox(binding_id, status);
