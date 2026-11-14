-- Authorization generations distinguish even same-second reauthorization.
ALTER TABLE twitch_channel_grants ADD COLUMN revision INTEGER NOT NULL DEFAULT 1;
