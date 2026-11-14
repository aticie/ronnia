//! Request policy, chat formatting and the single durable delivery
//! transaction. Delivery is at-most-once: the `deliveries` primary key is the
//! only duplicate guard.

use super::{dispatch::Message, record_outcome, record_queue_drop};
use crate::{
    config::Config,
    domain::User,
    parser,
    providers::{ChatError, Providers},
    storage::Store,
};
use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, params};
use serde_json::Value;
use tokio::{sync::mpsc, time::Instant};

pub(super) struct Echo {
    pub(super) channel_id: i64,
    pub(super) text: String,
    pub(super) span: tracing::Span,
    pub(super) queued_at: Instant,
}

#[derive(Clone, Copy)]
pub(super) struct Clock {
    pub(super) epoch: f64,
    pub(super) started: Instant,
}

impl Clock {
    pub(super) fn new() -> Self {
        Self {
            epoch: chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
            started: Instant::now(),
        }
    }

    pub(super) fn now(self) -> f64 {
        self.epoch + self.started.elapsed().as_secs_f64()
    }
}

pub(super) fn queue_echo(echoes: &mpsc::Sender<Echo>, message: &Message, text: String) {
    if let Err(error) = echoes.try_send(Echo {
        channel_id: message.channel_id,
        text,
        span: tracing::Span::current(),
        queued_at: Instant::now(),
    }) {
        let reason = match &error {
            mpsc::error::TrySendError::Full(_) => "full",
            mpsc::error::TrySendError::Closed(_) => "closed",
        };
        record_queue_drop("feedback", reason);
        tracing::warn!(
            channel = %message.channel,
            channel_id = message.channel_id,
            message_id = %message.id,
            queue_capacity = echoes.max_capacity(),
            queue_depth = echoes.max_capacity() - echoes.capacity(),
            error = %error,
            "Twitch feedback not queued"
        );
    }
}

pub(super) fn policy(user: &User, message: &Message) -> std::result::Result<(), String> {
    if user.settings.test {
        return Ok(());
    }
    if message.author.eq_ignore_ascii_case(&message.channel) {
        return Err("Author is broadcaster and not in test mode.".into());
    }
    if user.settings.sub_only && !(message.moderator || message.subscriber || message.vip) {
        return Err("Subscriber only request mode is active.".into());
    }
    if user.settings.points_only && !message.points {
        return Err("Channel Points only mode is active.".into());
    }
    if user
        .excluded_users
        .iter()
        .any(|login| login.eq_ignore_ascii_case(&message.author))
    {
        return Err("Requester is excluded.".into());
    }
    Ok(())
}

pub(super) fn difficulty_policy(
    user: &User,
    message: &Message,
    stars: f64,
) -> std::result::Result<(), String> {
    if user.settings.test {
        return Ok(());
    }
    let [low, high] = user.settings.sr;
    if stars < low || (high != -1.0 && stars > high) {
        return Err(format!(
            "@{} Streamer is accepting requests between {low:.1}-{high:.1}* difficulty. Your map is {stars:.1}*.",
            message.author
        ));
    }
    Ok(())
}

/// Resolve, filter, deliver and record one admitted request. The `deliveries`
/// insert is the authoritative at-most-once gate; a conflicting insert means a
/// previous attempt already owns this message.
pub(super) async fn process_one(
    config: &Config,
    store: &Store,
    providers: &Providers,
    echoes: &mpsc::Sender<Echo>,
    message: &Message,
    beatmap: &parser::Beatmap,
    clock: Clock,
) -> Result<()> {
    let Some(user) = store.user_by_twitch(message.channel_id).await? else {
        return Ok(());
    };
    if !user.twitch_username.eq_ignore_ascii_case(&message.channel) {
        tracing::warn!(channel = %message.channel, room_id = message.channel_id, "request rejected: stale channel identity");
        return Ok(());
    }
    // Enable remains the master switch. Test mode additionally bypasses the
    // live/osu-category gate and the per-request policy checks below.
    if !user.settings.enable {
        tracing::debug!(
            channel = %message.channel,
            message_id = %message.id,
            "request ignored: bot is disabled for this channel"
        );
        return Ok(());
    }
    if !user.settings.test && !user.is_live {
        tracing::debug!(
            channel = %message.channel,
            author = %message.author,
            message_id = %message.id,
            "request ignored: channel is offline or not streaming osu!"
        );
        return Ok(());
    }
    if let Err(reason) = policy(&user, message) {
        tracing::info!(channel = %message.channel, requester = %message.author, reason, "request rejected");
        return Ok(());
    }
    let map = match beatmap.kind {
        parser::BeatmapKind::Map => store.cache_get(beatmap.id).await?,
        parser::BeatmapKind::Mapset => store.cache_get_set(beatmap.id).await?,
    };
    let (map, set) = match map {
        Some(map) => {
            let set = map
                .get("beatmapset")
                .context("cached beatmap omitted beatmapset")?
                .clone();
            (map, set)
        }
        None => {
            let (map, set) = match providers.get_beatmap(beatmap).await {
                Ok(metadata) => metadata,
                Err(error)
                    if error
                        .downcast_ref::<ChatError>()
                        .is_some_and(|error| error.status == Some(404)) =>
                {
                    if !config.dry_run {
                        queue_echo(
                            echoes,
                            message,
                            format!("@{} Beatmap not found.", message.author),
                        );
                    }
                    tracing::info!(
                        channel = %message.channel,
                        requester = %message.author,
                        beatmap_id = beatmap.id,
                        "request rejected: beatmap not found"
                    );
                    return Ok(());
                }
                Err(error) => {
                    return Err(error).context("fetching osu! beatmap metadata");
                }
            };
            match beatmap.kind {
                parser::BeatmapKind::Map => store.cache_put(map.clone()).await?,
                parser::BeatmapKind::Mapset => store.cache_put_set(beatmap.id, map.clone()).await?,
            }
            (map, set)
        }
    };
    let stars = map["difficulty_rating"]
        .as_f64()
        .filter(|n| n.is_finite())
        .context("invalid beatmap difficulty_rating")?;
    if let Err(reason) = difficulty_policy(&user, message, stars) {
        if !config.dry_run && reason.starts_with('@') {
            queue_echo(echoes, message, reason.clone());
        }
        tracing::info!(channel = %message.channel, requester = %message.author, reason, "request rejected");
        return Ok(());
    }
    let author_id = message.author_id.clone();
    let channel_id = user.twitch_id;
    let last = store.call(move |db| Ok(db.query_row("SELECT last_success FROM request_cooldowns WHERE channel_id=?1 AND requester_id=?2", params![channel_id, author_id], |row| row.get::<_, f64>(0)).optional()?)).await?;
    if !user.settings.test && last.is_some_and(|last| clock.now() - last < user.settings.cooldown) {
        tracing::info!(channel = %message.channel, requester = %message.author, "request rejected: cooldown");
        return Ok(());
    }
    let pm = prepare_message(message, &map, &set, &beatmap.mods)?;
    if config.dry_run {
        tracing::info!(channel = %message.channel, beatmap_id = beatmap.id, "dry run: request accepted without chat or statistics");
        return Ok(());
    }
    let id = message.id.clone();
    let target = user.osu_id;
    let text = pm.clone();
    let now = chrono::Utc::now().timestamp();
    let reserved = store.call(move |db| {
        Ok(db.execute("INSERT INTO deliveries(twitch_message_id,target_osu_id,message,status,created_at) VALUES(?1,?2,?3,'pending',?4) ON CONFLICT(twitch_message_id) DO NOTHING", params![id, target, text, now])? == 1)
    }).await?;
    if !reserved {
        return Ok(());
    }
    let id = message.id.clone();
    store.call(move |db| { db.execute("UPDATE deliveries SET status='sending' WHERE twitch_message_id=?1 AND status='pending'", [id])?; Ok(()) }).await?;
    if let Err(error) = providers.send_message(user.osu_id, &pm, false).await {
        let id = message.id.clone();
        let status = if error.uncertain {
            "uncertain"
        } else {
            "failed"
        };
        record_outcome("ronnia.osu.deliveries", status);
        let description = error.to_string();
        store
            .call(move |db| {
                db.execute(
                    "UPDATE deliveries SET status=?1,error=?2 WHERE twitch_message_id=?3",
                    params![status, description, id],
                )?;
                Ok(())
            })
            .await?;
        return Err(anyhow::Error::new(error).context("sending osu! private message"));
    }
    record_outcome("ronnia.osu.deliveries", "sent");
    // One local transaction ties confirmed delivery, statistics and cooldown.
    // A crash before this commit leaves 'sending', hence uncertain on restart.
    let id = message.id.clone();
    let requester = message.author.clone();
    let channel = message.channel.clone();
    let author_id = message.author_id.clone();
    let map_id = map["id"].as_i64().context("beatmap omitted id")?;
    let mods = beatmap.mods.clone();
    let now = chrono::Utc::now().timestamp();
    let cooldown_now = clock.now();
    let update_cooldown = !user.settings.test;
    store.call(move |db| {
        let tx = db.transaction()?;
        tx.execute("UPDATE deliveries SET status='sent',error=NULL WHERE twitch_message_id=?1", [&id])?;
        tx.execute("INSERT INTO statistics(source_id,requester_channel_name,requested_beatmap_id,requested_channel_name,mods,timestamp) VALUES(?1,?2,?3,?4,?5,?6)", params![format!("twitch:{id}"), requester, map_id, channel, mods, now])?;
        if update_cooldown {
            tx.execute("INSERT INTO request_cooldowns(channel_id,requester_id,last_success) VALUES(?1,?2,?3) ON CONFLICT(channel_id,requester_id) DO UPDATE SET last_success=excluded.last_success", params![channel_id, author_id, cooldown_now])?;
        }
        tx.commit()?; Ok(())
    }).await?;
    if user.settings.echo {
        queue_echo(
            echoes,
            message,
            format!("{} - Request sent!", map_title(&map, &set)?),
        );
    }
    tracing::info!(
        channel = %message.channel,
        channel_id = message.channel_id,
        requester = %message.author,
        requester_id = %message.author_id,
        osu_username = %user.osu_username,
        osu_id = user.osu_id,
        map_id,
        beatmapset_id = set["id"].as_i64(),
        artist = set["artist"].as_str(),
        title = set["title"].as_str(),
        difficulty = map["version"].as_str(),
        stars,
        mods = %beatmap.mods,
        message_id = %message.id,
        "osu request delivered and recorded"
    );
    Ok(())
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("beatmap metadata omitted {key}"))
}

fn map_title(map: &Value, set: &Value) -> Result<String> {
    Ok(format!(
        "{} - {} [{}]",
        field(set, "artist")?,
        field(set, "title")?,
        field(map, "version")?
    ))
}

pub(super) fn prepare_message(
    message: &Message,
    map: &Value,
    set: &Value,
    mods: &str,
) -> Result<String> {
    let badge = if message.moderator {
        "[MOD] "
    } else if message.subscriber {
        "[SUB] "
    } else if message.vip {
        "[VIP] "
    } else {
        ""
    };
    let status = field(map, "status")?;
    let status = status
        .chars()
        .next()
        .map(|first| {
            format!(
                "{}{}",
                first.to_uppercase(),
                status[first.len_utf8()..].to_lowercase()
            )
        })
        .unwrap_or_default();
    let id = map["id"].as_i64().context("beatmap omitted id")?;
    let bpm = map["bpm"].as_f64().context("beatmap omitted bpm")?;
    let stars = map["difficulty_rating"]
        .as_f64()
        .context("beatmap omitted difficulty_rating")?;
    let length = parser::readable_duration(
        map["hit_length"]
            .as_u64()
            .context("beatmap omitted hit_length")?,
    );
    let points = if message.points { "+ USED POINTS" } else { "" };
    Ok(format!(
        "{badge}{} -> [{status}] [https://osu.ppy.sh/b/{id} {}] ({bpm} BPM, {stars:.2}*, {length}) {mods} {points}",
        message.author,
        map_title(map, set)?
    ))
}

pub(super) fn sanitize_chat(text: &str, max: usize) -> String {
    let mut clean = String::with_capacity(text.len().min(max));
    for c in text.chars() {
        if clean.len() + c.len_utf8() > max {
            break;
        }
        clean.push(if c.is_control() { ' ' } else { c });
    }
    clean
}
