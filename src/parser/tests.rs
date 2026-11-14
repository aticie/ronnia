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
fn unicode_case_folding_does_not_recognize_mods() {
    let parsed = parse_message("https://osu.ppy.sh/b/42 +ſO +ſD +HDſO +HD +sO").unwrap();
    assert_eq!(parsed.mods, "+HDSO");
}

#[test]
fn quoted_and_semicolon_terminated_links() {
    for link in [
        "\"https://osu.ppy.sh/b/42\";",
        "'https://osu.ppy.sh/beatmapsets/1#osu/42';",
        "https://old.ppy.sh/p/beatmap?b=42;",
    ] {
        let parsed = parse_message(&format!("{link} +HD")).unwrap();
        assert_eq!(parsed.id, 42, "{link}");
        assert_eq!(parsed.mods, "+HD", "{link}");
    }
}

#[test]
fn durations() {
    assert_eq!(readable_duration(0), "0:00");
    assert_eq!(readable_duration(61), "1:01");
    assert_eq!(readable_duration(3661), "1:01:01");
}
