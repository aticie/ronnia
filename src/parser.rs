//! Chat message parsing: beatmap and beatmapset URLs, mods and request shape.

use regex::Regex;
use std::sync::LazyLock;
use url::Url;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeatmapKind {
    Map,
    Mapset,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Beatmap {
    pub id: i64,
    pub kind: BeatmapKind,
    pub mods: String,
}

static MODS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?i-u:(?:EZ|HD|HR|DT|HT|NC|FL|SO|PF|SD)+)\b").expect("static mod expression")
});
const MOD_NAMES: [&str; 10] = ["EZ", "HD", "HR", "DT", "HT", "NC", "FL", "SO", "PF", "SD"];

/// The first supported URL wins; mods follow the last occurrence of that URL,
/// retaining the original bot's ordering and display-only semantics.
pub fn parse_message(content: &str) -> Option<Beatmap> {
    content
        .split(' ')
        .find_map(|candidate| parse_link(candidate, content))
}

pub fn parse_link(candidate: &str, content: &str) -> Option<Beatmap> {
    let start = [candidate.find("https://"), candidate.find("http://")]
        .into_iter()
        .flatten()
        .min()?;
    // Permit prose prefixes such as `map:` but never scan inside another URL.
    if candidate[..start].contains('/') {
        return None;
    }
    let link = candidate[start..]
        .split('+')
        .next()?
        .trim_matches(|c: char| {
            matches!(
                c,
                '<' | '>' | '(' | ')' | '[' | ']' | ',' | '.' | '!' | '"' | '\'' | ';'
            )
        });
    let url = Url::parse(link).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    let host = url.host_str()?;
    if !matches!(host, "osu.ppy.sh" | "old.ppy.sh") {
        return None;
    }
    let mut parts = url.path().trim_matches('/').split('/');
    let (Some(first), Some(second), None) = (parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    let positive = |s: &str| -> Option<i64> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse().ok().filter(|n| *n > 0)
    };
    let (id, kind) = match (first, second) {
        ("beatmaps", id) if host == "osu.ppy.sh" => (positive(id)?, BeatmapKind::Map),
        ("beatmapsets", id) if host == "osu.ppy.sh" => {
            let set_id = positive(id)?;
            match url.fragment().and_then(|f| f.split_once('/')) {
                Some(("osu" | "taiko" | "fruits" | "mania", map)) => {
                    (positive(map)?, BeatmapKind::Map)
                }
                _ => (set_id, BeatmapKind::Mapset),
            }
        }
        ("b", id) => (positive(id)?, BeatmapKind::Map),
        ("s", id) => (positive(id)?, BeatmapKind::Mapset),
        ("p", "beatmap") => {
            let mut map_id = None;
            let mut set_id = None;
            for (key, value) in url.query_pairs() {
                if key == "b" && map_id.is_none() {
                    map_id = Some(value);
                } else if key == "s" && set_id.is_none() {
                    set_id = Some(value);
                }
            }
            if let Some(id) = map_id {
                (positive(&id)?, BeatmapKind::Map)
            } else {
                (positive(&set_id?)?, BeatmapKind::Mapset)
            }
        }
        _ => return None,
    };
    let after = content.rsplit_once(link).map_or("", |(_, after)| after);
    let mut seen = 0u16;
    let mut mods = String::new();
    for found in MODS.find_iter(after) {
        for pair in found.as_str().as_bytes().as_chunks::<2>().0 {
            let index = MOD_NAMES
                .iter()
                .position(|name| pair.eq_ignore_ascii_case(name.as_bytes()))
                .expect("recognized ASCII mod");
            let bit = 1u16 << index;
            if seen & bit == 0 {
                if mods.is_empty() {
                    mods.reserve_exact(21);
                    mods.push('+');
                }
                mods.push_str(MOD_NAMES[index]);
                seen |= bit;
            }
        }
    }
    Some(Beatmap { id, kind, mods })
}

pub fn readable_duration(seconds: u64) -> String {
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests;
