import unittest

from helpers.beatmap_link_parser import parse_single_beatmap, parse_beatmapset, get_mod_from_text


class TestBeatmapLinkParser(unittest.TestCase):

    @classmethod
    def setUp(self) -> None:
        self.official_beatmap_link = 'https://osu.ppy.sh/beatmapsets/1341551#osu/2778999'
        self.official_beatmap_link_alt = 'https://osu.ppy.sh/beatmaps/806017?mode=osu'
        self.old_beatmap_link = 'https://osu.ppy.sh/b/2778999'
        self.old_beatmap_link_alt = 'https://old.ppy.sh/p/beatmap?b=1955170&m=2'

        self.official_beatmapset_link = 'https://osu.ppy.sh/beatmapsets/1341551'
        self.old_beatmapset_link = 'https://osu.ppy.sh/s/1341551'
        self.old_beatmapset_link_alt = 'https://old.ppy.sh/p/beatmap?s=1955170&m=2'

        self.beatmap_links = [self.official_beatmap_link, self.official_beatmap_link_alt, self.old_beatmap_link,
                              self.old_beatmap_link_alt]
        self.beatmapset_links = [self.official_beatmapset_link, self.old_beatmapset_link, self.old_beatmapset_link_alt]

    def test_parse_single_beatmap_returns_beatmap_id_for_official_links(self):
        expected_id = '2778999'
        expected_mod = '0'
        result_mod, result_id = parse_single_beatmap(self.official_beatmap_link)

        self.assertEqual(expected_id, result_id)
        self.assertEqual(expected_mod, result_mod)

    def test_parse_single_beatmap_returns_beatmap_id_for_official_links_alternate(self):
        expected_id = '806017'
        expected_mod = '0'
        result_mod, result_id = parse_single_beatmap(self.official_beatmap_link_alt)

        self.assertEqual(expected_id, result_id)
        self.assertEqual(expected_mod, result_mod)

    def test_parse_single_beatmap_returns_beatmap_id_for_old_links(self):
        expected_id = '2778999'
        expected_mod = '0'
        result_mod, result_id = parse_single_beatmap(self.old_beatmap_link)

        self.assertEqual(expected_id, result_id)
        self.assertEqual(expected_mod, result_mod)

    def test_parse_single_beatmap_returns_beatmap_id_for_old_links_alternate(self):
        expected_id = '1955170'
        expected_mod = '2'
        result_mod, result_id = parse_single_beatmap(self.old_beatmap_link_alt)

        self.assertEqual(expected_id, result_id)
        self.assertEqual(expected_mod, result_mod)

    def test_parse_beatmapset_returns_beatmapset_id_for_official_links(self):
        expected_id = '1341551'
        expected_mod = '0'
        result_mod, result_id = parse_beatmapset(self.official_beatmapset_link)

        self.assertEqual(expected_id, result_id)
        self.assertEqual(expected_mod, result_mod)

    def test_parse_beatmapset_returns_beatmapset_id_for_old_links(self):
        expected_id = '1341551'
        expected_mod = '0'
        result_mod, result_id = parse_beatmapset(self.old_beatmapset_link)

        self.assertEqual(expected_id, result_id)
        self.assertEqual(expected_mod, result_mod)

    def test_parse_beatmapset_returns_beatmapset_id_for_old_links_alternate(self):
        expected_id = '1955170'
        expected_mod = '2'
        result_mod, result_id = parse_beatmapset(self.old_beatmapset_link_alt)

        self.assertEqual(expected_id, result_id)
        self.assertEqual(expected_mod, result_mod)

    def test_get_mod_from_text_returns_correct_mod_combination_when_no_spaces(self):
        expected_mods_int = 8 + 64
        expected_mods_text = "+HDDT"

        for beatmap_link in self.beatmap_links:
            content = f'{beatmap_link}+HDDT'
            mods_int, mods_text = get_mod_from_text(content, beatmap_link)

            self.assertEqual(expected_mods_int, mods_int)
            self.assertEqual(expected_mods_text, mods_text)

    def test_get_mod_from_text_returns_correct_mod_combination_when_spaced_with_beatmap_link(self):
        expected_mods_int = 16 + 8
        expected_mods_text = "+HRHD"

        for beatmap_link in self.beatmap_links:
            content = f'{beatmap_link} +HRHD'
            mods_int, mods_text = get_mod_from_text(content, beatmap_link)

            self.assertEqual(expected_mods_int, mods_int)
            self.assertEqual(expected_mods_text, mods_text)

    def test_get_mod_from_text_returns_correct_mod_combination_when_multiple_plus_mod_given(self):
        expected_mods_int = 64 + 16 + 8
        expected_mods_text = "+HDDTHR"

        for beatmap_link in self.beatmap_links:
            content = f'{beatmap_link} +HD +DT +HR'
            mods_int, mods_text = get_mod_from_text(content, beatmap_link)

            self.assertEqual(expected_mods_int, mods_int)
            self.assertEqual(expected_mods_text, mods_text)

    def test_get_mod_from_text_returns_correct_mod_combination_when_mod_is_given_with_space_after_plus_sign(self):
        expected_mods_int = 64 + 8
        expected_mods_text = "+HDDT"

        for beatmap_link in self.beatmap_links:
            content = f'{beatmap_link} + HD + DT'
            mods_int, mods_text = get_mod_from_text(content, beatmap_link)

            self.assertEqual(expected_mods_int, mods_int)
            self.assertEqual(expected_mods_text, mods_text)

    def test_get_mod_from_text_returns_correct_mod_combination_when_mod_is_given_without_plus_sign(self):
        expected_mods_int = 64 + 8
        expected_mods_text = "+HDDT"

        for beatmap_link in self.beatmap_links:
            content = f'{beatmap_link} HDDT'
            mods_int, mods_text = get_mod_from_text(content, beatmap_link)

            self.assertEqual(expected_mods_int, mods_int)
            self.assertEqual(expected_mods_text, mods_text)

    def test_get_mod_from_text_returns_correct_mod_combination_when_mod_is_case_insensitive(self):
        expected_mods_int = 64 + 8
        expected_mods_text = "+HDDT"

        for beatmap_link in self.beatmap_links:
            content = f'{beatmap_link} +HdDt'
            mods_int, mods_text = get_mod_from_text(content, beatmap_link)

            self.assertEqual(expected_mods_int, mods_int)
            self.assertEqual(expected_mods_text, mods_text)

    def test_get_mod_from_text_returns_nomod_when_no_mod_is_given(self):
        expected_mods_int = 0
        expected_mods_text = ""

        for beatmap_link in self.beatmap_links:
            content = f'{beatmap_link}'
            mods_int, mods_text = get_mod_from_text(content, beatmap_link)

            self.assertEqual(expected_mods_int, mods_int)
            self.assertEqual(expected_mods_text, mods_text)
