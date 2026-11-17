-- Preserve existing deliveries; request identity is available for new sends only.
ALTER TABLE deliveries ADD COLUMN channel_id INTEGER;
ALTER TABLE deliveries ADD COLUMN requester_id TEXT;
ALTER TABLE deliveries ADD COLUMN beatmap_kind TEXT CHECK(beatmap_kind IN ('map','mapset'));
ALTER TABLE deliveries ADD COLUMN beatmap_id INTEGER;
ALTER TABLE deliveries ADD COLUMN mods TEXT;
ALTER TABLE deliveries ADD COLUMN last_attempt_at REAL;

CREATE INDEX deliveries_request_duplicate ON deliveries(
    channel_id,requester_id,beatmap_kind,beatmap_id,mods,last_attempt_at
) WHERE source='new' AND status IN ('pending','sending','sent','uncertain');
