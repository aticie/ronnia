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
    Regex::new(r"(?i)\b(?:EZ|HD|HR|DT|HT|NC|FL|SO|PF|SD)+\b").expect("static mod expression")
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
        .trim_matches(|c: char| matches!(c, '<' | '>' | '(' | ')' | '[' | ']' | ',' | '.' | '!'));
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
mod tests {
    use super::*;
    #[test]
    fn original_forms_and_legacy_missing_keys() {
        for link in [
            "https://osu.ppy.sh/beatmapsets/1#osu/42",
            "https://osu.ppy.sh/beatmapsets/1#mania/42",
            "https://osu.ppy.sh/beatmaps/42?mode=osu",
            "http://old.ppy.sh/b/42",
            "https://old.ppy.sh/p/beatmap?m=2&b=42",
        ] {
            assert_eq!(
                parse_message(link).unwrap(),
                Beatmap {
                    id: 42,
                    kind: BeatmapKind::Map,
                    mods: String::new()
                }
            );
        }
        for link in [
            "https://osu.ppy.sh/beatmapsets/42",
            "https://old.ppy.sh/s/42",
            "https://old.ppy.sh/p/beatmap?m=2&s=42",
        ] {
            assert_eq!(parse_message(link).unwrap().kind, BeatmapKind::Mapset);
        }
        assert!(parse_message("https://old.ppy.sh/p/beatmap?m=2&broken").is_none());
    }
    #[test]
    fn mods_are_ordered_unique_and_display_only() {
        let parsed = parse_message("hello https://osu.ppy.sh/b/42+HdDT +HR +HD -HT").unwrap();
        assert_eq!(parsed.mods, "+HDDTHRHT");
        assert_eq!(
            parse_message("https://osu.ppy.sh/b/42 +HD https://osu.ppy.sh/b/42 +DT")
                .unwrap()
                .mods,
            "+DT"
        );
        assert_eq!(
            parse_message("https://osu.ppy.sh/b/42 somewhere shadow EZsomething xHD HDx")
                .unwrap()
                .mods,
            ""
        );
        assert_eq!(
            parse_message("https://osu.ppy.sh/b/42 +HDHR/HD+DT ~NC| -FL")
                .unwrap()
                .mods,
            "+HDHRDTNCFL"
        );
        assert_eq!(
            parse_message("map:https://osu.ppy.sh/b/42! +HD")
                .unwrap()
                .mods,
            "+HD"
        );
        assert_eq!(
            parse_message("please(https://osu.ppy.sh/b/42!)")
                .unwrap()
                .id,
            42
        );
    }
    #[test]
    fn urls_have_real_host_and_numeric_boundaries() {
        for bad in [
            "https://osu.ppy.sh.attacker.test/b/42",
            "https://osuXppy.sh/b/42",
            "https://osu.ppy.sh/b/42evil",
            "https://osu.ppy.sh/b/0",
            "https://old.ppy.sh/p/beatmap?b=not-a-number",
            "https://evil.test/https://osu.ppy.sh/b/42",
        ] {
            assert!(parse_message(bad).is_none(), "{bad}");
        }
        assert_eq!(parse_message("<https://osu.ppy.sh/b/42>").unwrap().id, 42);
    }
    #[test]
    fn durations() {
        assert_eq!(readable_duration(0), "0:00");
        assert_eq!(readable_duration(61), "1:01");
        assert_eq!(readable_duration(3661), "1:01:01");
    }
}
