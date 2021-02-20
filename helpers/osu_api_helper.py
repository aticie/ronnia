import os

import aiohttp


class OsuApiHelper:

    def __init__(self):
        self._osu_api_key = os.getenv('OSU_API_KEY')

    async def get_beatmap_info(self, api_params: dict):
        params = {"k": self._osu_api_key}
        merged_params = {**params, **api_params}
        async with aiohttp.ClientSession() as session:
            async with session.get('http://osu.ppy.sh/api/get_beatmaps', params=merged_params) as response:
                r = await response.json()

        return r[0]


if __name__ == '__main__':
    import asyncio

    ex_dict = {'s': '1341551', 'm': '0'}
    o = OsuApiHelper()
    print(asyncio.run(o.get_beatmap_info(ex_dict)))
