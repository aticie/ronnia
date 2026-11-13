//! Legacy IRC deliveries: the exactly-once claim and terminal-report state
//! machine that guards forwarded requests and their statistics.
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, params};

use super::{Store, statistics};
use crate::domain::RequestEvent;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimOutcome {
    Claimed,
    Rejected(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Recorded,
    NotFound,
}

fn validate(id: &str) -> Result<()> {
    ensure!(!id.trim().is_empty(), "message ID cannot be empty");
    Ok(())
}

impl Store {
    pub async fn claim_legacy_delivery(
        &self,
        twitch_message_id: String,
        twitch_id: i64,
        target_osu_id: i64,
        message: String,
    ) -> Result<ClaimOutcome> {
        validate(&twitch_message_id)?;
        ensure!(!message.trim().is_empty(), "message cannot be empty");
        ensure!(
            twitch_id > 0 && target_osu_id > 0,
            "channel and target IDs must be positive"
        );
        self.write(move |tx| {
            let user: Option<(i64, bool)> = tx.query_row("SELECT osu_id,COALESCE(json_extract(settings_json,'$.enable'),0) FROM users WHERE twitch_id=?1", [twitch_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let Some((osu_id, enabled)) = user else { return Ok(ClaimOutcome::Rejected("unknown channel")) };
            if !enabled { return Ok(ClaimOutcome::Rejected("channel disabled")); }
            let migrated: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM twitch_channel_grants WHERE twitch_id=?1)", [twitch_id], |r| r.get(0))?;
            if migrated { return Ok(ClaimOutcome::Rejected("channel migrated")); }
            if osu_id != target_osu_id { return Ok(ClaimOutcome::Rejected("target does not match channel")); }
            let inserted = tx.execute("INSERT INTO deliveries(twitch_message_id,target_osu_id,message,status,created_at,source) VALUES(?1,?2,?3,'pending',?4,'legacy') ON CONFLICT DO NOTHING", params![twitch_message_id, target_osu_id, message, chrono::Utc::now().timestamp()])?;
            Ok(if inserted == 1 { ClaimOutcome::Claimed } else { ClaimOutcome::Rejected("delivery already claimed") })
        }).await
    }
    pub async fn complete_legacy_delivery(
        &self,
        twitch_message_id: String,
        sent: bool,
        error: Option<String>,
        statistics: Option<RequestEvent>,
    ) -> Result<DeliveryOutcome> {
        validate(&twitch_message_id)?;
        ensure!(!sent || statistics.is_some(), "statistics required");
        if let Some(event) = &statistics {
            event.validate()?;
        }
        self.write(move |tx| {
            let delivery: Option<(String, String)> = tx
                .query_row(
                    "SELECT status,source FROM deliveries WHERE twitch_message_id=?1",
                    [&twitch_message_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((status, source)) = delivery else {
                return Ok(DeliveryOutcome::NotFound);
            };
            if source != "legacy" {
                return Ok(DeliveryOutcome::NotFound);
            }
            if !matches!(status.as_str(), "sent" | "failed" | "uncertain") {
                if sent {
                    tx.execute(
                        "UPDATE deliveries SET status='sent',error=NULL WHERE twitch_message_id=?1",
                        [&twitch_message_id],
                    )?;
                    if let Some(event) = statistics {
                        statistics::insert(
                            tx,
                            &event,
                            Some(format!("twitch:{twitch_message_id}")),
                        )?;
                    }
                } else {
                    tx.execute(
                        "UPDATE deliveries SET status='failed',error=?2 WHERE twitch_message_id=?1",
                        params![twitch_message_id, error],
                    )?;
                }
            }
            Ok(DeliveryOutcome::Recorded)
        })
        .await
    }
}
