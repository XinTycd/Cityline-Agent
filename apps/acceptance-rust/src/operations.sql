ALTER TABLE media_tasks ADD COLUMN review TEXT NOT NULL DEFAULT 'pending'
    CHECK (review IN ('pending', 'approved', 'returned'));
ALTER TABLE media_tasks ADD COLUMN review_note TEXT NOT NULL DEFAULT '';
ALTER TABLE media_tasks ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0 CHECK (deleted IN (0, 1));

CREATE TABLE audit_events (
    id INTEGER PRIMARY KEY,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    category TEXT NOT NULL,
    action TEXT NOT NULL,
    task_id TEXT,
    detail TEXT NOT NULL
) STRICT;

CREATE TABLE system_settings (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
) STRICT;
