import unittest

from models.beatmap import Beatmap, BeatmapType
from ronnia.utils.beatmap import BeatmapParser


class TestBeatmapLinkParser(unittest.TestCase):

    @classmethod
    def setUp(cls) -> None:
        official_beatmap_link = 'https://osu.ppy.sh/beatmapsets/552726#osu/1170505'
        official_beatmap_link_alt = 'https://osu.ppy.sh/beatmaps/806017?mode=osu'
        official_beatmap_link_alt_2 = 'https://osu.ppy.sh/beatmaps/806017'
        old_beatmap_link = 'https://osu.ppy.sh/b/2778999'
        old_beatmap_link_alt = 'https://old.ppy.sh/p/beatmap?b=1955170&m=2'

        official_beatmapset_link = 'https://osu.ppy.sh/beatmapsets/1341551'
        old_beatmapset_link = 'https://osu.ppy.sh/s/1341551'
        old_beatmapset_link_alt = 'https://old.ppy.sh/p/beatmap?s=1955170&m=2'

        cls.official_beatmap_link = official_beatmap_link
        cls.official_beatmap_link_alt = official_beatmap_link_alt
        cls.official_beatmap_link_alt_2 = official_beatmap_link_alt_2
        cls.old_beatmap_link = old_beatmap_link
        cls.old_beatmap_link_alt = old_beatmap_link_alt
        cls.official_beatmapset_link = official_beatmapset_link
        cls.old_beatmapset_link = old_beatmapset_link
        cls.old_beatmapset_link_alt = old_beatmapset_link_alt

        cls.beatmap_links = [official_beatmap_link, official_beatmap_link_alt, old_beatmap_link,
                             old_beatmap_link_alt]
        cls.beatmapset_links = [official_beatmapset_link, old_beatmapset_link, old_beatmapset_link_alt]

    def test_parse_single_beatmap_returns_beatmap_id_for_official_links(self):
        expected_id = '1170505'
        expected_set_id = '552726'
        result_id = BeatmapParser.parse_single_beatmap(self.official_beatmap_link)
        result_set_id = BeatmapParser.parse_beatmapset(self.official_beatmap_link)

        self.assertEqual(expected_id, result_id)
        self.assertEqual(expected_set_id, result_set_id)

    def test_parse_single_beatmap_returns_beatmap_id_for_official_links_alternate(self):
        expected_id = '806017'
        result_id = BeatmapParser.parse_single_beatmap(self.official_beatmap_link_alt)

        self.assertEqual(expected_id, result_id)

    def test_parse_single_beatmap_returns_beatmap_id_for_official_links_alternate_2(self):
        expected_id = '806017'
        result_id = BeatmapParser.parse_single_beatmap(self.official_beatmap_link_alt_2)

        self.assertEqual(expected_id, result_id)

    def test_parse_single_beatmap_returns_beatmap_id_for_old_links(self):
        expected_id = '2778999'
        result_id = BeatmapParser.parse_single_beatmap(self.old_beatmap_link)

        self.assertEqual(expected_id, result_id)

    def test_parse_single_beatmap_returns_beatmap_id_for_old_links_alternate(self):
        expected_id = '1955170'
        beatmap_id = BeatmapParser.parse_single_beatmap(self.old_beatmap_link_alt)

        self.assertEqual(expected_id, beatmap_id)

    def test_parse_beatmapset_returns_beatmapset_id_for_official_links(self):
        expected_id = '1341551'
        result_id = BeatmapParser.parse_beatmapset(self.official_beatmapset_link)

        self.assertEqual(expected_id, result_id)

    def test_parse_beatmapset_returns_beatmapset_id_for_old_links(self):
        expected_id = '1341551'
        result_id = BeatmapParser.parse_beatmapset(self.old_beatmapset_link)

        self.assertEqual(expected_id, result_id)

    def test_parse_beatmapset_returns_beatmapset_id_for_old_links_alternate(self):
        expected_id = '1955170'
        result_id = BeatmapParser.parse_beatmapset(self.old_beatmapset_link_alt)

        self.assertEqual(expected_id, result_id)

    def test_get_mod_from_text_returns_correct_mod_combination_when_no_spaces(self):
        expected_mods_text = "+HDDT"

        for beatmap_link in self.beatmap_links:
            content = f'{beatmap_link}+HDDT'
            mods_text = BeatmapParser.get_mod_from_text(content, beatmap_link)

            self.assertEqual(expected_mods_text, mods_text)

    def test_get_mod_from_text_returns_correct_mod_combination_when_spaced_with_beatmap_link(self):
        expected_mods_text = "+HRHD"

        for beatmap_link in self.beatmap_links:
            content = f'{beatmap_link} +HRHD'
            mods_text = BeatmapParser.get_mod_from_text(content, beatmap_link)

            self.assertEqual(expected_mods_text, mods_text)

    def test_get_mod_from_text_returns_correct_mod_combination_when_multiple_plus_mod_given(self):
        expected_mods_text = "+HDDTHR"

        for beatmap_link in self.beatmap_links:
            content = f'{beatmap_link} +HD +DT +HR'
            mods_text = BeatmapParser.get_mod_from_text(content, beatmap_link)

            self.assertEqual(expected_mods_text, mods_text)

    def test_get_mod_from_text_returns_correct_mod_combination_when_mod_is_given_with_space_after_plus_sign(self):
        expected_mods_text = "+HDDT"

        for beatmap_link in self.beatmap_links:
            content = f'{beatmap_link} + HD + DT'
            mods_text = BeatmapParser.get_mod_from_text(content, beatmap_link)

            self.assertEqual(expected_mods_text, mods_text)

    def test_get_mod_from_text_returns_omits_multiple_of_same_mods(self):
        expected_mods_text = "+HDDT"

        for beatmap_link in self.beatmap_links:
            content = f'{beatmap_link} +HD +HDDT +DT'
            mods_text = BeatmapParser.get_mod_from_text(content, beatmap_link)

            self.assertEqual(expected_mods_text, mods_text)

    def test_get_mod_from_text_returns_correct_mod_combination_when_mod_is_given_without_plus_sign(self):
        expected_mods_text = "+HDDT"

        for beatmap_link in self.beatmap_links:
            content = f'{beatmap_link} HDDT'
            mods_text = BeatmapParser.get_mod_from_text(content, beatmap_link)

            self.assertEqual(expected_mods_text, mods_text)

    def test_get_mod_from_text_returns_correct_mod_combination_when_mod_is_case_insensitive(self):
        expected_mods_text = "+HDDT"

        for beatmap_link in self.beatmap_links:
            content = f'{beatmap_link} +HdDt'
            mods_text = BeatmapParser.get_mod_from_text(content, beatmap_link)

            self.assertEqual(expected_mods_text, mods_text)

    def test_get_mod_from_text_returns_nomod_when_no_mod_is_given(self):
        expected_mods_text = ""

        for beatmap_link in self.beatmap_links:
            content = f'{beatmap_link}'
            mods_text = BeatmapParser.get_mod_from_text(content, beatmap_link)

            self.assertEqual(expected_mods_text, mods_text)

    def test_parse_beatmap_link(self):
        map = Beatmap(id=1170505,
                      type=BeatmapType.MAP,
                      mods="")

        returned_map = BeatmapParser.parse_beatmap_link(self.official_beatmap_link, self.official_beatmap_link)

        self.assertEqual(map.id, returned_map.id)
        self.assertEqual(map.mods, returned_map.mods)
        self.assertEqual(map.type, returned_map.type)
