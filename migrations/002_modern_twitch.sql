-- Bot credentials are encrypted by the auth module; the encryption key is never persisted.
CREATE TABLE twitch_bot_auth (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    user_id TEXT NOT NULL,
    login TEXT NOT NULL,
    ciphertext BLOB NOT NULL,
    expires_at INTEGER NOT NULL,
    scopes_json TEXT NOT NULL CHECK(json_valid(scopes_json)),
    validated_at INTEGER NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE twitch_channel_grants (
    twitch_id INTEGER PRIMARY KEY REFERENCES users(twitch_id) ON DELETE CASCADE ON UPDATE CASCADE,
    granted_at INTEGER NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('granted','revoked'))
);
CREATE TABLE twitch_conduit (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    client_id TEXT NOT NULL,
    conduit_id TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE channel_stream_state (
    twitch_id INTEGER PRIMARY KEY REFERENCES users(twitch_id) ON DELETE CASCADE ON UPDATE CASCADE,
    online INTEGER NOT NULL DEFAULT 0 CHECK(online IN (0,1)),
    category_id TEXT NOT NULL DEFAULT '',
    revision INTEGER NOT NULL DEFAULT 0,
    online_event_at INTEGER NOT NULL DEFAULT 0,
    category_event_at INTEGER NOT NULL DEFAULT 0
);
-- Twitch logins are case-insensitive; preserve membership while canonicalizing legacy casing.
UPDATE exclusions SET login = lower(login) WHERE login <> lower(login) COLLATE BINARY;
-- Legacy activity is not evidence of channel:bot consent or current stream state.
-- All account/preferences/history/raw export data remain unchanged.
UPDATE users SET is_live = 0 WHERE is_live <> 0;
