CREATE TABLE media_snapshots (
    id TEXT PRIMARY KEY NOT NULL,
    industry TEXT NOT NULL,
    starts TEXT NOT NULL,
    ends TEXT NOT NULL,
    observed_at TEXT NOT NULL,
    recorded_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    payload TEXT NOT NULL
) STRICT;
