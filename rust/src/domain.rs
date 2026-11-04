use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(default)]
pub struct Settings {
    pub echo: bool,
    pub enable: bool,
    #[serde(rename = "sub-only", alias = "sub_only", alias = "subOnly")]
    pub sub_only: bool,
    #[serde(rename = "points-only", alias = "points_only", alias = "pointsOnly")]
    pub points_only: bool,
    pub test: bool,
    #[schema(minimum = 0.0, default = 30.0)]
    pub cooldown: f64,
    /// Inclusive lower and upper star bounds, default [0, -1]. Upper -1 is unlimited;
    /// upper values of 10 or more are normalized to -1.
    #[schema(min_items = 2, max_items = 2)]
    pub sr: [f64; 2],
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            echo: true,
            enable: true,
            sub_only: false,
            points_only: false,
            test: false,
            cooldown: 30.0,
            sr: [0.0, -1.0],
        }
    }
}

impl Settings {
    pub fn validate(&mut self) -> Result<()> {
        ensure!(
            self.cooldown.is_finite() && self.cooldown >= 0.0,
            "cooldown must be finite and nonnegative"
        );
        ensure!(
            self.sr.iter().all(|v| v.is_finite()),
            "star bounds must be finite"
        );
        ensure!(self.sr[0] >= 0.0, "lower star bound must be nonnegative");
        ensure!(
            self.sr[1] == -1.0 || self.sr[1] >= self.sr[0],
            "upper star bound must be -1 or at least the lower bound"
        );
        if self.sr[1] >= 10.0 {
            self.sr[1] = -1.0;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct User {
    #[serde(alias = "osu_id")]
    pub osu_id: i64,
    #[serde(alias = "osu_username")]
    pub osu_username: String,
    #[serde(default, alias = "osu_avatar_url")]
    pub osu_avatar_url: String,
    #[serde(alias = "twitch_id")]
    pub twitch_id: i64,
    #[serde(alias = "twitch_username")]
    pub twitch_username: String,
    #[serde(default, alias = "twitch_avatar_url")]
    pub twitch_avatar_url: String,
    #[serde(default, alias = "excluded_users")]
    pub excluded_users: Vec<String>,
    #[serde(default, alias = "is_live")]
    pub is_live: bool,
    #[serde(default)]
    pub settings: Settings,
}

impl User {
    pub fn validate(&mut self) -> Result<()> {
        ensure!(
            self.osu_id > 0 && self.twitch_id > 0,
            "account IDs must be positive"
        );
        ensure!(
            !self.osu_username.trim().is_empty(),
            "osu username cannot be empty"
        );
        ensure!(
            !self.twitch_username.trim().is_empty(),
            "Twitch username cannot be empty"
        );
        self.settings.validate()?;
        self.twitch_username = self.twitch_username.to_lowercase();
        for login in &mut self.excluded_users {
            ensure!(!login.trim().is_empty(), "excluded login cannot be empty");
            *login = login.to_lowercase();
        }
        self.excluded_users.sort_unstable();
        self.excluded_users.dedup();
        Ok(())
    }
}

/// A Helix stream observation guarded by the revision captured before its request.
#[derive(Clone, Debug)]
pub struct StreamSnapshot {
    pub twitch_id: i64,
    pub expected_revision: i64,
    pub online: bool,
    pub category_id: String,
    pub login: Option<String>,
}

#[derive(Clone, Debug)]
pub enum StreamEvent {
    Online,
    Offline,
    Category(String),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RequestEvent {
    #[serde(alias = "requesterChannelName")]
    pub requester_channel_name: String,
    #[serde(alias = "requestedBeatmapId")]
    pub requested_beatmap_id: i64,
    #[serde(alias = "requestedChannelName")]
    pub requested_channel_name: String,
    #[serde(default)]
    pub mods: Option<String>,
    pub timestamp: i64,
}

impl RequestEvent {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.requested_beatmap_id > 0, "beatmap ID must be positive");
        ensure!(
            !self.requester_channel_name.trim().is_empty(),
            "requester cannot be empty"
        );
        ensure!(
            !self.requested_channel_name.trim().is_empty(),
            "channel cannot be empty"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_limits_without_disabling_lower_bound() {
        let mut s = Settings {
            sr: [4.0, 10.0],
            ..Settings::default()
        };
        s.validate().unwrap();
        assert_eq!(s.sr, [4.0, -1.0]);
        s.sr = [4.0, 3.0];
        assert!(s.validate().is_err());
        s.sr = [0.0, f64::NAN];
        assert!(s.validate().is_err());
        s.sr = [0.0, -1.0];
        s.cooldown = f64::INFINITY;
        assert!(s.validate().is_err());
    }
    #[test]
    fn aliases_preserve_public_setting_keys() {
        let settings: Settings =
            serde_json::from_value(serde_json::json!({"sub_only":true,"pointsOnly":true})).unwrap();
        let value = serde_json::to_value(settings).unwrap();
        assert_eq!(value["sub-only"], true);
        assert_eq!(value["points-only"], true);
        assert!(value.get("sub_only").is_none());
    }
}
