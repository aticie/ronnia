import re
from typing import Sequence, Union, AnyStr

legacy_mode_converter = {'osu': '0',
                         'taiko': '1',
                         'fruits': '2',
                         'mania': '3'}


def extract_url_headers(headers_string, desired_keys):
    headers = {head.split('=')[0]: head.split('=')[1] for head in headers_string.split('&')}

    collected_values = []
    for key in desired_keys:
        if key not in headers:
            return None
        collected_values.append(headers[key])

    return collected_values


def parse_beatmapset(map_link: str):
    # pattern_old = r"https?:\/\/osu.ppy.sh\/s"
    # pattern_new = r"https?:\/\/osu.ppy.sh\/beatmapsets\/1341551"

    patterns = {'official': r"https?:\/\/osu.ppy.sh\/beatmapsets\/([0-9]+).+",
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
                return 0, result.group(1)
            elif link_type == 'old':
                return 0, result.group(2)
            else:
                return extract_url_headers(result.group(2), ['m', 's'])

    return None


def parse_single_beatmap(map_link: str) -> Union[Sequence[AnyStr], None]:
    patterns = {'official': r"https?:\/\/osu.ppy.sh\/beatmapsets\/[0-9]+\#(osu|taiko|fruits|mania)\/([0-9]+)",
                # Official osu! beatmap link
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
                return 0, result.group(2)
            else:
                return extract_url_headers(result.group(2), ['m', 'b'])

    return None


def parse_beatmap_link(beatmap_link: str):
    result = parse_single_beatmap(beatmap_link)

    if result:
        return {'b': result[1]}
    else:
        result = parse_beatmapset(beatmap_link)
        if result:
            return {'s': result[1]}
        else:
            return None


if __name__ == '__main__':

    test_beatmap_links = ['https://osu.ppy.sh/beatmapsets/1341551#osu/2778999',
                          'https://osu.ppy.sh/beatmapsets/1341551',
                          'https://osu.ppy.sh/b/2778999',
                          'https://osu.ppy.sh/s/1341551',
                          'https://old.ppy.sh/p/beatmap?b=1955170&m=3',
                          'https://old.ppy.sh/p/beatmap?s=1955170&m=2',
                          'https://www.xspdf.com/resolution/30209565.html']

    for bmap_link in test_beatmap_links:
        print(parse_beatmap_link(bmap_link))

