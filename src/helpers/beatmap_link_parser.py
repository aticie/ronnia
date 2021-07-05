import re
from collections import OrderedDict
from typing import Sequence, AnyStr, Optional, Tuple, Union, List

legacy_mode_converter = {'osu': '0',
                         'taiko': '1',
                         'fruits': '2',
                         'mania': '3'}

mod_bit_shift_dict = {
    "NF": 0,
    "EZ": 1,
    "HD": 3,
    "HR": 4,
    "SD": 5,
    "DT": 6,
    "HT": 8,
    "NC": 9,
    "FL": 10,
    "SO": 12,
    "PF": 14,
}


def get_mod_from_text(content, candidate_link) -> Tuple[int, str]:
    text = content.split(candidate_link)[-1].strip()
    matches = re.findall(r'(?i)[-+~|]?(?:EZ|HD|HR|DT|HT|NC|FL|SO|PF|SD)+[~|]?', text)

    total_mods = []
    for mods in matches:
        mods = mods.strip("-+~|").upper()

        mods_as_list = [mods[i:i + 2] for i in range(0, len(mods), 2)]

        total_mods.extend(mods_as_list)

    if len(total_mods) == 0:
        return 0, ""

    mods_as_int = 0
    mods_as_text = "+"
    total_mods = OrderedDict.fromkeys(total_mods)
    for mod in total_mods.keys():
        if mod in mod_bit_shift_dict:
            mods_as_int |= 1 << mod_bit_shift_dict[mod]
            mods_as_text += mod

    return mods_as_int, mods_as_text


def extract_url_parameters(headers_string, desired_keys) -> Optional[List]:
    headers = {head.split('=')[0]: head.split('=')[1] for head in headers_string.split('&')}

    collected_values = []
    for key in desired_keys:
        if key not in headers:
            return None
        collected_values.append(headers[key])

    return collected_values


def parse_beatmapset(map_link: str) -> Optional[Tuple]:
    patterns = {'official': r"https?:\/\/osu.ppy.sh\/beatmapsets\/([0-9]+)",
                'old': r"https?:\/\/(osu|old).ppy.sh\/s\/([0-9]+)",
                'old_alternate': r"https?:\/\/(osu|old).ppy.sh\/p\/beatmap\?(.+)"
                }

    for link_type, pattern in patterns.items():
        result = re.search(pattern, map_link)

        # If there is no match, search for old beatmap link
        if result is None:
            continue
        else:
            if link_type == 'official':
                return '0', result.group(1)
            elif link_type == 'old':
                return '0', result.group(2)
            else:
                return extract_url_parameters(result.group(2), ['m', 's'])

    return None


def parse_single_beatmap(map_link: str) -> Optional[Sequence[AnyStr]]:
    patterns = {'official': r"https?:\/\/osu.ppy.sh\/beatmapsets\/[0-9]+\#(osu|taiko|fruits|mania)\/([0-9]+)",
                # Official osu! beatmap link
                'official_alt': r"https?:\/\/osu.ppy.sh\/beatmaps\/([0-9]+)",
                # Official alternate beatmap link
                'old_single': r"https?:\/\/(osu|old).ppy.sh\/b\/([0-9]+)",
                # Old beatmap link
                'old_alternate': r"https?:\/\/(osu|old).ppy.sh\/p\/beatmap\?(.+)"
                # Old beatmap link with converted mods
                }

    for link_type, pattern in patterns.items():

        result = re.search(pattern, map_link)

        # If there is no match, search for old beatmap link
        if result is None:
            continue
        else:
            if link_type == 'official':
                return legacy_mode_converter[result.group(1)], result.group(2)
            elif link_type == 'old_single':
                return '0', result.group(2)
            elif link_type == 'official_alt':
                return '0',  result.group(1)
            else:
                return extract_url_parameters(result.group(2), ['m', 'b'])

    return None


def parse_beatmap_link(beatmap_link: str, content: str) -> Union[Tuple[dict, str], Tuple[None, None]]:
    beatmap_link = beatmap_link.split('+')[0]
    result = parse_single_beatmap(beatmap_link)

    if result:
        mods_as_int, mods_as_text = get_mod_from_text(content, beatmap_link)
        return {'b': result[1]}, mods_as_text  # 'mods': mods_as_int}, mods_as_text
    else:
        result = parse_beatmapset(beatmap_link)
        if result:
            mods_as_int, mods_as_text = get_mod_from_text(content, beatmap_link)
            return {'s': result[1]}, mods_as_text  # 'mods': mods_as_int}, mods_as_text
        else:
            return None, None


if __name__ == '__main__':

    test_beatmap_links = ['https://osu.ppy.sh/beatmapsets/1341551#osu/2778999',
                          'https://osu.ppy.sh/beatmapsets/1341551',
                          'https://osu.ppy.sh/b/2778999',
                          'https://osu.ppy.sh/s/1341551',
                          'https://old.ppy.sh/p/beatmap?b=1955170&m=3',
                          'https://old.ppy.sh/p/beatmap?s=1955170&m=2',
                          'https://osu.ppy.sh/beatmapsets/1222983',
                          'https://www.xspdf.com/resolution/30209565.html']

    for bmap_link in test_beatmap_links:
        print(parse_beatmap_link(bmap_link, bmap_link))
