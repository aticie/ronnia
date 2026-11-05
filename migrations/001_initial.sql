CREATE TABLE users (
    osu_id INTEGER PRIMARY KEY CHECK(osu_id > 0),
    osu_username TEXT NOT NULL CHECK(length(osu_username) > 0),
    osu_avatar_url TEXT NOT NULL DEFAULT '',
    twitch_id INTEGER NOT NULL UNIQUE CHECK(twitch_id > 0),
    twitch_username TEXT COLLATE NOCASE NOT NULL CHECK(length(twitch_username) > 0),
    twitch_avatar_url TEXT NOT NULL DEFAULT '',
    is_live INTEGER NOT NULL DEFAULT 0 CHECK(is_live IN (0,1)),
    settings_json TEXT NOT NULL CHECK(json_valid(settings_json)),
    raw_json TEXT CHECK(raw_json IS NULL OR json_valid(raw_json))
);
CREATE INDEX users_login ON users(twitch_username);
CREATE TABLE exclusions (
    twitch_id INTEGER NOT NULL REFERENCES users(twitch_id) ON DELETE CASCADE ON UPDATE CASCADE,
    login TEXT COLLATE NOCASE NOT NULL CHECK(length(login) > 0),
    PRIMARY KEY(twitch_id,login)
);
CREATE TABLE settings_metadata (
    name TEXT PRIMARY KEY,
    value_json TEXT NOT NULL CHECK(json_valid(value_json)),
    description TEXT NOT NULL,
    type TEXT NOT NULL,
    raw_json TEXT NOT NULL CHECK(json_valid(raw_json))
);
CREATE TABLE statistics (
    id INTEGER PRIMARY KEY,
    source_id TEXT UNIQUE,
    requester_channel_name TEXT NOT NULL,
    requested_beatmap_id INTEGER NOT NULL CHECK(requested_beatmap_id > 0),
    requested_channel_name TEXT NOT NULL,
    mods TEXT,
    timestamp INTEGER NOT NULL,
    raw_json TEXT CHECK(raw_json IS NULL OR json_valid(raw_json))
);
CREATE INDEX statistics_time_map ON statistics(timestamp,requested_beatmap_id);
CREATE INDEX statistics_map ON statistics(requested_beatmap_id);
CREATE TABLE beatmaps (
    id INTEGER PRIMARY KEY CHECK(id > 0),
    beatmapset_id INTEGER NOT NULL CHECK(beatmapset_id > 0),
    fetched_at INTEGER NOT NULL,
    raw_json TEXT NOT NULL CHECK(json_valid(raw_json)),
    source_json TEXT CHECK(source_json IS NULL OR json_valid(source_json))
);
CREATE INDEX beatmaps_set ON beatmaps(beatmapset_id);
CREATE TABLE mapset_cache (
    set_id INTEGER PRIMARY KEY CHECK(set_id > 0),
    beatmap_id INTEGER NOT NULL REFERENCES beatmaps(id) ON DELETE CASCADE,
    fetched_at INTEGER NOT NULL
);
CREATE TABLE sessions (
    token_hash TEXT PRIMARY KEY,
    twitch_id INTEGER NOT NULL REFERENCES users(twitch_id) ON DELETE CASCADE ON UPDATE CASCADE,
    expires_at INTEGER NOT NULL
);
CREATE INDEX sessions_expiry ON sessions(expires_at);
CREATE TABLE oauth_attempts (
    state_hash TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    return_to TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    session_hash TEXT,
    signup_hash TEXT
);
CREATE INDEX oauth_expiry ON oauth_attempts(expires_at);
CREATE TABLE signup_attempts (
    token_hash TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    identity_json TEXT NOT NULL CHECK(json_valid(identity_json)),
    expires_at INTEGER NOT NULL
);
CREATE INDEX signup_expiry ON signup_attempts(expires_at);
CREATE TABLE deliveries (
    id INTEGER PRIMARY KEY,
    twitch_message_id TEXT UNIQUE NOT NULL,
    target_osu_id INTEGER NOT NULL,
    message TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('pending','sending','sent','uncertain','failed')),
    error TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX deliveries_status_created ON deliveries(status,created_at);
CREATE TABLE request_cooldowns (
    channel_id INTEGER NOT NULL REFERENCES users(twitch_id) ON DELETE CASCADE ON UPDATE CASCADE,
    requester_id TEXT NOT NULL,
    last_success REAL NOT NULL,
    PRIMARY KEY(channel_id,requester_id)
);
