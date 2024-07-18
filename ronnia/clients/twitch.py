import asyncio
from typing import Dict, List

import aiohttp
from tenacity import retry, stop_after_attempt, wait_exponential, retry_if_exception_type

from utils.singleton import SingletonMeta


class TwitchAPI(metaclass=SingletonMeta):
    def __init__(self, client_id: str, client_secret: str, max_concurrent: int = 4):
        self.client_id = client_id
        self.client_secret = client_secret
        self.access_token = None
        self.base_url = "https://api.twitch.tv/helix"
        self.session = None
        self.semaphore = asyncio.Semaphore(max_concurrent)
        self.auth_lock = asyncio.Lock()
        self.authenticating = False

    async def __aenter__(self):
        self.session = aiohttp.ClientSession()
        await self.authenticate()
        return self

    async def __aexit__(self, exc_type, exc_val, exc_tb):
        await self.session.close()

    @retry(
        stop=stop_after_attempt(3),
        wait=wait_exponential(multiplier=1, min=4, max=10),
        retry=retry_if_exception_type((aiohttp.ClientError, aiohttp.ServerConnectionError))
    )
    async def _make_request(self, method: str, url: str, **kwargs) -> Dict:
        async with self.semaphore:
            while self.authenticating:
                await asyncio.sleep(0.1)
            async with self.session.request(method, url, **kwargs) as response:
                if response.status == 200:
                    return await response.json()
                elif response.status == 401 and method != "POST":
                    # Token might be expired, try to re-authenticate
                    await self.authenticate()
                    kwargs['headers']["Authorization"] = f"Bearer {self.access_token}"
                    return await self._make_request(method, url, **kwargs)
                else:
                    response.raise_for_status()

    async def _auth_request(self, url: str, params: Dict) -> Dict:
        async with self.session.post(url, params=params) as response:
            if response.status == 200:
                return await response.json()
            else:
                response.raise_for_status()

    async def authenticate(self):
        async with self.auth_lock:
            if self.access_token:
                return

            auth_url = "https://id.twitch.tv/oauth2/token"
            params = {
                "client_id": self.client_id,
                "client_secret": self.client_secret,
                "grant_type": "client_credentials"
            }

            data = await self._auth_request(auth_url, params)
            self.access_token = data["access_token"]

    async def get_streams_batch(self, user_ids: List[int]) -> Dict:
        if not self.access_token:
            await self.authenticate()

        headers = {
            "Client-ID": self.client_id,
            "Authorization": f"Bearer {self.access_token}"
        }

        user_id_params = "&".join(f"user_id={uid}" for uid in user_ids)
        # game_id=21465 is osu!
        url = f"{self.base_url}/streams?first=100&game_id=21465&{user_id_params}"

        return await self._make_request("GET", url, headers=headers)

    async def get_streams(self, user_ids: List[int]) -> List[Dict]:
        # Split user_ids into batches
        batches = [user_ids[i:i + 100] for i in range(0, len(user_ids), 100)]

        # Create tasks for each batch
        tasks = [self.get_streams_batch(batch) for batch in batches]

        # Run all tasks concurrently, but limited by the semaphore
        results = await asyncio.gather(*tasks)

        # Combine results
        combined_data = {"data": []}
        for result in results:
            combined_data["data"].extend(result.get("data", []))

        return combined_data["data"]
