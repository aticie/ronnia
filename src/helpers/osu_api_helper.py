import os
import time
import logging
from typing import Union
from datetime import datetime

import aiohttp

logger = logging.getLogger('ronnia')


class OsuApiHelper:

    def __init__(self):
        self._osu_api_key = os.getenv('OSU_API_KEY')
        self._last_request_time = datetime.now()
        self._cooldown_seconds = 1

    async def get_beatmap_info(self, api_params: dict):
        endpoint = 'get_beatmaps'
        params = {"k": self._osu_api_key}
        merged_params = {**params, **api_params}
        result = await self._get_endpoint(merged_params, endpoint)
        try:
            return result[0]
        except IndexError:
            logger.debug(f'No beatmap found. Api returned: \n {result}')
            return None

    async def get_user_info(self, username: Union[str, int]):
        endpoint = 'get_user'
        params = {"k": self._osu_api_key,
                  "u": username}

        if isinstance(username, str):
            params["type"] = "string"
        elif isinstance(username, int):
            params["type"] = "id"

        result = await self._get_endpoint(params, endpoint)
        try:
            return result[0]
        except IndexError:
            logger.debug(f'No user found. Api returned: \n {result}')
            return None

    async def _get_endpoint(self, params: dict, endpoint: str):
        self._wait_for_rate_limit()
        timeout = aiohttp.ClientTimeout(total=5)
        async with aiohttp.ClientSession(timeout=timeout) as session:
            async with session.get(f'http://osu.ppy.sh/api/{endpoint}', params=params) as response:
                try:
                    r = await response.json()
                except:
                    return None
        return r

    def _wait_for_rate_limit(self):
        now = datetime.now()
        time_passed = now - self._last_request_time
        if time_passed.total_seconds() < self._cooldown_seconds:
            time.sleep(self._cooldown_seconds - time_passed.total_seconds())

        self._last_request_time = datetime.now()

        return


if __name__ == '__main__':
    import asyncio

    ex_dict = {'s': '1341551', 'm': '0'}
    o = OsuApiHelper()
    print(asyncio.run(o.get_beatmap_info(ex_dict)))
    print(asyncio.run(o.get_user_info('heyronii')))
