import asyncio
import datetime
import json
import logging
import os
from typing import Union

import aiohttp

from helpers.database_helper import StatisticsDatabase

logger = logging.getLogger('ronnia')


class OsuApi:

    def __init__(self, messages_db: StatisticsDatabase):
        self._osu_api_key = os.getenv('OSU_API_KEY')
        self._last_request_time = datetime.datetime.now() - datetime.timedelta(seconds=100)
        self._cooldown_seconds = 1
        self._messages_db = messages_db

    async def get_beatmap_info(self, api_params: dict):
        endpoint = 'get_beatmaps'
        params = {"k": self._osu_api_key}
        merged_params = {**params, **api_params}
        result = await self._get_endpoint(merged_params, endpoint)
        try:
            return result[0]
        except IndexError:
            logger.debug(f'No beatmap found. Api returned: \n {result}')
            if result is not None:
                await self._messages_db.add_error(error_type='osu_beatmap_error', error_text=json.dumps(result))
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
            if result is not None:
                await self._messages_db.add_error(error_type='osu_user_error', error_text=json.dumps(result))
            return None

    async def _get_endpoint(self, params: dict, endpoint: str):
        await self._wait_for_rate_limit()
        timeout = aiohttp.ClientTimeout(total=5)
        async with aiohttp.ClientSession(timeout=timeout) as session:
            async with session.get(f'http://osu.ppy.sh/api/{endpoint}', params=params) as response:
                try:
                    r = await response.json()
                except asyncio.TimeoutError as e:
                    await self._messages_db.add_error(error_type='osu_timeout_error', error_text=None)
                    return None

        await self._messages_db.add_api_usage(endpoint)
        return r

    async def _wait_for_rate_limit(self):
        now = datetime.datetime.now()
        time_passed = now - self._last_request_time
        if time_passed.total_seconds() < self._cooldown_seconds:
            await asyncio.sleep(self._cooldown_seconds - time_passed.total_seconds())

        self._last_request_time = datetime.datetime.now()

        return
