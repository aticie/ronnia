import asyncio
import logging
import os
from multiprocessing import Process, Lock
from multiprocessing.connection import Listener
from typing import List, Dict

import requests
from pymongo import UpdateOne
from requests.adapters import Retry, HTTPAdapter

from helpers.database_helper import DBUser, RonniaDatabase
from ronnia.bots.twitch_bot import TwitchBot
from ronnia.helpers.utils import batcher

logger = logging.getLogger(__name__)


class TwitchAPI:
    def __init__(self, client_id, client_secret):
        self.client_id = client_id
        self.client_secret = client_secret

        self.session = requests.Session()
        self.retry_policy = Retry(
            total=5, backoff_factor=0.1, status_forcelist=[500, 502, 503, 504]
        )
        self.session.mount("https://", HTTPAdapter(max_retries=self.retry_policy))

        self.access_token = self.get_token()

    def get_token(self):
        """
        Gets access token from Twitch API
        """
        url = (
            f"https://id.twitch.tv/oauth2/token?"
            f"client_id={self.client_id}&"
            f"client_secret={self.client_secret}&"
            f"grant_type=client_credentials"
        )
        response = self.session.post(url)
        return response.json()["access_token"]

    def get_streams(self, user_ids: List[int]):
        """
        Gets streams from Twitch API helix/streams only users playing osu!
        """
        headers = {
            "Authorization": "Bearer {}".format(self.access_token),
            "Client-ID": self.client_id,
        }
        streams = []
        for user_id in batcher(user_ids, 100):
            # game_id = 21465 is osu!
            url = (
                    "https://api.twitch.tv/helix/streams?first=100&game_id=21465&"
                    + "&".join([f"user_id={user}" for user in user_id])
            )
            response = self.session.get(url, headers=headers)
            streams += response.json()["data"]
        return streams


class TwitchProcess(Process):
    def __init__(self, user_list: List[str], join_lock: Lock):
        super().__init__()
        self.join_lock = join_lock
        self.user_list = user_list
        self.bot = None

    def initialize(self):
        self.bot = TwitchBot(
            initial_channel_names=self.user_list, join_lock=self.join_lock
        )

    def run(self) -> None:
        self.initialize()
        self.bot.run()


class BotManager:
    def __init__(
            self,
    ):
        self.db_client = RonniaDatabase(os.getenv("MONGODB_URL"))
        self.join_lock = Lock()

        self.twitch_client = TwitchAPI(
            os.getenv("TWITCH_CLIENT_ID"), os.getenv("TWITCH_CLIENT_SECRET")
        )
        self._loop = asyncio.get_event_loop()

        self.bot_processes: Dict[TwitchProcess, List[str]] = {}
        self.users: List[DBUser] = []

    def start(self):
        """
        Starts the bot manager.

        Creates an IRCBot process and multiple TwitchBot processes.
        """
        self._loop.run_until_complete(self.db_client.initialize())
        streaming_user_names = self._loop.run_until_complete(self.get_streaming_users())

        logger.info(f"Collected users: {len(streaming_user_names)}")
        p = TwitchProcess(user_list=streaming_user_names, join_lock=self.join_lock)
        p.start()
        logger.info(
            f"Started Twitch bot instance for {len(streaming_user_names)} users"
        )
        self.bot_processes[p] = streaming_user_names

        self._loop.run_until_complete(self.main())

    async def main(self):
        """
        Main coroutine of the bot manager. Checks streaming users and sends the updated list to bot every 30 seconds.
        """

        address = ("localhost", 31313)
        while True:
            try:
                with Listener(
                        address, authkey=os.getenv("TWITCH_CLIENT_SECRET").encode()
                ) as listener:
                    with listener.accept() as conn:
                        while True:
                            streaming_users = await self.get_streaming_users()
                            logger.info(
                                f"Sending streaming users to bot: {streaming_users}"
                            )
                            conn.send(streaming_users)
                            await asyncio.sleep(30)
            except Exception:
                logger.exception(f"Bot Manager send streaming users error.")
                await asyncio.sleep(5)

    async def get_streaming_users(self):
        self.users = await self.db_client.get_enabled_users()
        all_user_twitch_ids = [user.twitchId for user in self.users]
        streaming_twitch_users = self.twitch_client.get_streams(all_user_twitch_ids)
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

        await self.db_client.bulk_write_operations(
            operations=operations, col=self.db_client.users_col
        )
        await self.db_client.users_col.update_many(
            {"twitchId": {"$nin": streaming_twitch_user_ids}},
            {"$set": {"isLive": False}},
        )
        streaming_usernames = [user["user_login"] for user in streaming_twitch_users]
        return streaming_usernames
