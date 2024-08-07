import re
from collections import OrderedDict

from ronnia.models.beatmap import Beatmap, BeatmapType


class BeatmapParser:
    legacy_mode_converter = {"osu": "0", "taiko": "1", "fruits": "2", "mania": "3"}

    @staticmethod
    def get_mod_from_text(content, candidate_link) -> str:
        text = content.split(candidate_link)[-1].strip()
        matches = re.findall(r"(?i)[-+~|]?(?:EZ|HD|HR|DT|HT|NC|FL|SO|PF|SD)+[~|]?", text)

        total_mods = []
        for mods in matches:
            mods = mods.strip("-+~|").upper()

            mods_as_list = [mods[i: i + 2] for i in range(0, len(mods), 2)]

            total_mods.extend(mods_as_list)

        if len(total_mods) == 0:
            return ""

        mods_as_text = "+"
        total_mods = OrderedDict.fromkeys(total_mods)
        for mod in total_mods.keys():
            mods_as_text += mod

        return mods_as_text

    @staticmethod
    def extract_url_parameters(headers_string, desired_keys) -> list | None:
        headers = {
            head.split("=")[0]: head.split("=")[1] for head in headers_string.split("&")
        }

        collected_values = []
        for key in desired_keys:
            if key not in headers:
                return None
            collected_values.append(headers[key])

        return collected_values

    @staticmethod
    def parse_beatmapset(map_link: str) -> str | None:
        patterns = {
            "official": r"https?:\/\/osu.ppy.sh\/beatmapsets\/([0-9]+)",
            "old": r"https?:\/\/(?:osu|old).ppy.sh\/s\/([0-9]+)",
            "old_alternate": r"https?:\/\/(?:osu|old).ppy.sh\/p\/beatmap\?(.+)",
        }

        for link_type, pattern in patterns.items():
            result = re.search(pattern, map_link)

            # If there is no match, search for old beatmap link
            if result is not None:
                if link_type != "old_alternate":
                    return result.group(1)
                else:
                    return BeatmapParser.extract_url_parameters(result.group(1), ["s"])[0]

        return None

    @staticmethod
    def parse_single_beatmap(map_link: str) -> str | None:
        patterns = {
            "official": r"https?:\/\/osu.ppy.sh\/beatmapsets\/[0-9]+\#(?:osu|taiko|fruits|mania)\/([0-9]+)",
            # Official osu! beatmap link
            "official_alt": r"https?:\/\/osu.ppy.sh\/beatmaps\/([0-9]+)",
            # Official alternate beatmap link
            "old_single": r"https?:\/\/(?:osu|old).ppy.sh\/b\/([0-9]+)",
            # Old beatmap link
            "old_alternate": r"https?:\/\/(?:osu|old).ppy.sh\/p\/beatmap\?(.+)"
            # Old beatmap link with converted mods
        }

        for link_type, pattern in patterns.items():
            result = re.search(pattern, map_link)

            # If there is no match, search for old beatmap link
            if result is not None:
                if link_type != "old_alternate":
                    return result.group(1)
                else:
                    return BeatmapParser.extract_url_parameters(result.group(1), ["b"])[0]

        return None

    @staticmethod
    def parse_beatmap_link(
            beatmap_link: str, content: str
    ) -> Beatmap | None:
        beatmap_link = beatmap_link.split("+")[0]
        result = BeatmapParser.parse_single_beatmap(beatmap_link)

        if result:
            mods_as_text = BeatmapParser.get_mod_from_text(content, beatmap_link)
            return Beatmap(id=result,
                           type=BeatmapType.MAP,
                           mods=mods_as_text)

        beatmapset_result = BeatmapParser.parse_beatmapset(beatmap_link)
        if beatmapset_result:
            mods_as_text = BeatmapParser.get_mod_from_text(content, beatmap_link)
            return Beatmap(id=beatmapset_result,
                           type=BeatmapType.MAPSET,
                           mods=mods_as_text)
