-- Twitch's stream ID is the durable identity of a broadcast, not a local revision.
ALTER TABLE channel_stream_state ADD COLUMN stream_id TEXT;

-- Keep only the latest attempt for each channel. An interrupted attempt is never replayed.
CREATE TABLE follower_warnings (
    twitch_id INTEGER PRIMARY KEY REFERENCES users(twitch_id) ON DELETE CASCADE ON UPDATE CASCADE,
    stream_id TEXT NOT NULL CHECK(length(trim(stream_id)) > 0),
    status TEXT NOT NULL CHECK(status IN ('sending','sent','failed','uncertain')),
    error TEXT,
    attempted_at INTEGER NOT NULL
);
