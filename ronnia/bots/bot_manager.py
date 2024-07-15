import asyncio
import logging
import os
import socket
from typing import List, Dict

import aiohttp
from pymongo import UpdateOne
from tenacity import retry, stop_after_attempt, wait_exponential, retry_if_exception_type

from helpers.database_helper import DBUser, RonniaDatabase
from ronnia.bots.twitch_bot import TwitchBot

STREAMING_USERS_UPDATE_SLEEP = 120

logger = logging.getLogger(__name__)


class TwitchAPI:
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


class BotManager:
    def __init__(
            self,
    ):
        self.db_client = RonniaDatabase(os.getenv("MONGODB_URL"))

        self.twitch_client_id = os.getenv("TWITCH_CLIENT_ID")
        self.twitch_client_secret = os.getenv("TWITCH_CLIENT_SECRET")

        self.twitch_bot: TwitchBot | None = None

        self._loop = asyncio.get_event_loop()

        self.users: List[DBUser] = []

    async def start(self):
        """
        Starts the bot manager.

        Creates an IRCBot process and multiple TwitchBot processes.
        """
        await self.db_client.initialize()
        streaming_user_names = await self.get_streaming_users()

        self.twitch_bot = TwitchBot(
            initial_channel_names=streaming_user_names,
            listener_update_sleep=STREAMING_USERS_UPDATE_SLEEP
        )
        logger.info(
            f"Started Twitch bot instance for {len(streaming_user_names)} users"
        )
        twitch_bot_task = asyncio.create_task(self.twitch_bot.start())
        await self.twitch_bot.wait_for_ready()
        await asyncio.sleep(0.5)
        listener_task = asyncio.create_task(self.listener(self.twitch_bot.server_socket))
        await asyncio.gather(
            twitch_bot_task,
            listener_task
        )

    async def listener(self, server_sock: socket.socket):
        """
        Main coroutine of the bot manager. Checks streaming users and sends the updated list to bot every 30 seconds.
        """
        address = server_sock.getsockname()
        logger.info(f"Starting Bot Manager Listener on {address=}")
        _, writer = await asyncio.open_connection(*address[:2])
        while True:
            try:
                await asyncio.sleep(STREAMING_USERS_UPDATE_SLEEP)  # Wait for 30 seconds before sending connected users

                streaming_users = await self.get_streaming_users()
                logger.info(f'Sending {len(streaming_users)} streaming users to the Twitch Bot.')
                message = ",".join(streaming_users) + "\n"
                writer.write(message.encode())
                await writer.drain()

            except BaseException as e:
                logger.exception("Bot manager sender error exiting...", exc_info=e)
                writer.close()
                await writer.wait_closed()
                raise e

    async def get_streaming_users(self) -> set:
        self.users = await self.db_client.get_enabled_users()
        all_user_twitch_ids = [user.twitchId for user in self.users]

        async with TwitchAPI(self.twitch_client_id, self.twitch_client_secret) as twitch_api:
            streaming_twitch_users = await twitch_api.get_streams(all_user_twitch_ids)
            streaming_twitch_user_ids = []
            operations = []
            for user in streaming_twitch_users:
                twitch_username = user["user_login"]
                twitch_id = int(user["user_id"])
                streaming_twitch_user_ids.append(twitch_id)
                operations.append(
                    UpdateOne(
                        {"twitchId": twitch_id},
                        {"$set": {"isLive": True, "twitchUsername": twitch_username}},
                        upsert=True,
                    )
                )

        logger.info(f"Updating {len(operations)} documents with Live status.")
        await self.db_client.bulk_write_operations(
            operations=operations, col=self.db_client.users_col
        )
        await self.db_client.users_col.update_many(
            {"twitchId": {"$nin": streaming_twitch_user_ids}},
            {"$set": {"isLive": False}},
        )
        streaming_usernames = [user["user_login"] for user in streaming_twitch_users]
        return set(streaming_usernames)
