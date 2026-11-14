-- Chat bans are channel moderation state, not revoked channel:bot consent.
-- Keep stream subscriptions and retry only the chat subscription periodically.
ALTER TABLE twitch_channel_grants ADD COLUMN chat_banned_at INTEGER;
ALTER TABLE twitch_channel_grants ADD COLUMN chat_retry_at INTEGER NOT NULL DEFAULT 0;
