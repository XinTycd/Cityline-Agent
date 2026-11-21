CREATE TABLE media_tasks (
    id TEXT PRIMARY KEY NOT NULL,
    active INTEGER NOT NULL CHECK (active IN (0, 1)),
    payload TEXT NOT NULL,
    report TEXT
) STRICT;
