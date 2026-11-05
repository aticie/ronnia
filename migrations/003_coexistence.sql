-- Coexistence rollout: grants activate after a short grace window and deliveries
-- record which bot produced them. Existing grants and deliveries stay valid.
ALTER TABLE twitch_channel_grants ADD COLUMN active_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE deliveries ADD COLUMN source TEXT NOT NULL DEFAULT 'new' CHECK(source IN ('new','legacy'));
