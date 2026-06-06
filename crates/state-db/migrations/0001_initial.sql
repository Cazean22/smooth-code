CREATE TABLE IF NOT EXISTS threads (
    thread_id TEXT PRIMARY KEY NOT NULL,
    agent_path TEXT NULL,
    agent_nickname TEXT NULL,
    prompt_kind TEXT NULL,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    updated_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
);

CREATE TABLE IF NOT EXISTS thread_spawn_edges (
    parent_thread_id TEXT NOT NULL,
    child_thread_id TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    updated_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    PRIMARY KEY (parent_thread_id, child_thread_id)
);

CREATE INDEX IF NOT EXISTS idx_thread_spawn_edges_parent_status
    ON thread_spawn_edges (parent_thread_id, status);
