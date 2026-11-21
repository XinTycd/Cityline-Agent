ALTER TABLE media_snapshots ADD COLUMN source TEXT NOT NULL DEFAULT 'legacy'
    CHECK (source IN ('legacy', 'manual', 'platform'));
ALTER TABLE media_snapshots ADD COLUMN refresh_id TEXT;
ALTER TABLE media_snapshots ADD COLUMN media_mask INTEGER NOT NULL DEFAULT 0
    CHECK (media_mask BETWEEN 0 AND 3);

UPDATE media_snapshots SET media_mask =
    (EXISTS (SELECT 1 FROM json_each(payload, '$.stock') WHERE json_extract(value, '$.medium') = 'bus')
      OR EXISTS (SELECT 1 FROM json_each(payload, '$.coverage') WHERE json_extract(value, '$.medium') = 'bus'))
    + 2 * (EXISTS (SELECT 1 FROM json_each(payload, '$.stock') WHERE json_extract(value, '$.medium') = 'metro')
      OR EXISTS (SELECT 1 FROM json_each(payload, '$.coverage') WHERE json_extract(value, '$.medium') = 'metro'));

CREATE UNIQUE INDEX media_snapshots_refresh_id ON media_snapshots(refresh_id);
CREATE INDEX media_snapshots_industry_period ON media_snapshots(industry, starts, ends);
