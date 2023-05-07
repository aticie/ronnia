import asyncio
import datetime
import logging
from typing import Union, Dict, Optional, Tuple

import aiohttp
from multidict import CIMultiDict

logger = logging.getLogger("ronnia")


class BaseOsuApiV2(aiohttp.ClientSession):
    """Async wrapper for osu! api v2"""

    def __init__(self, client_id: str, client_secret: str):
        super().__init__()
        self._client_id = client_id
        self._client_secret = client_secret
        self._api_base_url = "https://osu.ppy.sh/api/v2/"
        self._scopes = None

        self._access_token = None
        self._access_token_obtain_date = None
        self._access_token_expire_date = None

        self._last_request_time = datetime.datetime.now() - datetime.timedelta(
            weeks=100
        )
        self._cooldown_seconds = 0.5

    async def _check_token_expired(self):
        return (
            datetime.datetime.now() + datetime.timedelta(minutes=1)
            > self._access_token_expire_date
        )

    async def _get_access_token(self):
        params = {
            "client_id": self._client_id,
            "client_secret": self._client_secret,
            "grant_type": "client_credentials",
            "scope": self._scopes,
        }

        async with aiohttp.ClientSession() as c:
            async with c.post("https://osu.ppy.sh/oauth/token", json=params) as r:
                token_response = await r.json()

        self._access_token = token_response["access_token"]

        self._access_token_obtain_date = datetime.datetime.now()
        self._access_token_expire_date = (
            self._access_token_obtain_date
            + datetime.timedelta(seconds=token_response["expires_in"])
        )

        self._default_headers = CIMultiDict(
            {"Authorization": f"Bearer {self._access_token}"}
        )

    async def _get_endpoint(self, endpoint: str, params: dict = None):
        await self.wait_cooldown()

        async with self.get(f"{self._api_base_url}{endpoint}", params=params) as resp:
            contents = await resp.json()

        self._last_request_time = datetime.datetime.now()

        return contents

    async def _post_endpoint(self, endpoint: str, data: dict, params: dict = None):
        await self.wait_cooldown()

        async with self.post(
            f"{self._api_base_url}{endpoint}", params=params, json=data
        ) as resp:
            contents = await resp.json()

        self._last_request_time = datetime.datetime.now()

        return contents

    async def wait_cooldown(self):
        if self._access_token is None or await self._check_token_expired():
            await self._get_access_token()
        seconds_since_last_request = (
            datetime.datetime.now() - self._last_request_time
        ).total_seconds()
        if seconds_since_last_request < self._cooldown_seconds:
            await asyncio.sleep(self._cooldown_seconds - seconds_since_last_request)


class OsuApiV2(BaseOsuApiV2):
    def __init__(self, client_id: str, client_secret: str):
        super().__init__(client_id, client_secret)
        self._scopes = "public"

    async def get_beatmap(self, beatmap_id: int) -> Tuple[Dict, Dict]:
        """
        Gets beatmap data for the specified beatmap ID.
        :param beatmap_id: The ID of the beatmap.
        :return: Returns Beatmap object.

        This endpoint returns a single beatmap dict.
        """
        logger.debug(f"Requesting beatmap information for id: {beatmap_id}")
        beatmap_info = await self._get_endpoint(f"beatmaps/{beatmap_id}")
        beatmapset_info = beatmap_info["beatmapset"]
        return beatmap_info, beatmapset_info

    async def get_beatmapset(self, beatmapset_id: int) -> Tuple[Dict, Dict]:
        logger.debug(f"Requesting beatmapset information for id: {beatmapset_id}")
        beatmapset_info = await self._get_endpoint(f"beatmapsets/{beatmapset_id}")
        beatmap_info = beatmapset_info["beatmaps"][0]
        return beatmap_info, beatmapset_info

    async def get_beatmap_attributes(
        self, beatmap_id: int, mods: Optional[str] = None
    ) -> Dict:
        """
        Gets beatmap data for the specified beatmap ID.
        :param beatmap_id: The ID of the beatmap.
        :return: Returns Beatmap object.

        This endpoint returns a single beatmap dict.
        """
        logger.debug(f"Requesting beatmap information for id: {beatmap_id}")
        data = {"mods": mods}
        return await self._post_endpoint(f"beatmaps/{beatmap_id}/attributes", data=data)

    async def get_user_info(
        self,
        user_id: Union[str, int],
        game_mode: Optional[str] = None,
        key: Optional[str] = "id",
    ) -> Dict:
        """
        This endpoint returns the detail of specified user.
        It's highly recommended to pass key parameter to avoid getting unexpected result
        (mainly when looking up user with numeric username or nonexistent user id).
        :param user_id: Id or username of the user.
        :param key: Type of user passed in url parameter.
                    Can be either id or username to limit lookup by their respective type.
                    Passing empty or invalid value will result in id lookup followed by username lookup if not found.
        :param game_mode: GameMode. User default mode will be used if not specified.
        :return:
        """
        logger.debug(f"Requesting user information for user: {user_id}")
        params = {"key": key}
        endpoint = f"users/{user_id}/{game_mode}" if game_mode else f"users/{user_id}"
        return await self._get_endpoint(endpoint=endpoint, params=params)


class OsuChatApiV2(BaseOsuApiV2):
    def __init__(self, client_id: str, client_secret: str):
        super().__init__(client_id, client_secret)
        self._scopes = "delegate chat.write"

    async def send_message(self, target_id: int, message: str, is_action: bool = False):
        """
        This endpoint allows you to create a new PM channel.
        :param target_id: user_id of user to start PM with
        :param message: message to send
        :param is_action: whether the message is an action
        """
        data = {"target_id": target_id, "message": message, "is_action": is_action}
        await self._post_endpoint(endpoint="chat/new", data=data)
