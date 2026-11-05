-- Live presence reported by the Python IRC relay for channels that have not
-- migrated. Rows are replaced on every poll and refreshed by the relay; public
-- live listings union fresh rows with the new bot's own live state.
CREATE TABLE legacy_live (
    twitch_id INTEGER PRIMARY KEY REFERENCES users(twitch_id) ON DELETE CASCADE ON UPDATE CASCADE,
    login TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX legacy_live_updated ON legacy_live(updated_at);
