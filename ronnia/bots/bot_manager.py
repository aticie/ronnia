import asyncio
import logging
import os
import socket
from typing import AsyncIterable

from pymongo import UpdateOne

from ronnia.bots.twitch_bot import TwitchBot
from ronnia.clients.mongo import RonniaDatabase
from ronnia.clients.twitch import TwitchAPI
from ronnia.models.database import DBUser

STREAMING_USERS_UPDATE_SLEEP = 60

logger = logging.getLogger(__name__)


class BotManager:
    def __init__(
            self,
    ):
        self.db_client = RonniaDatabase(os.getenv("MONGODB_URL"))

        self.twitch_client_id = os.getenv("TWITCH_CLIENT_ID")
        self.twitch_client_secret = os.getenv("TWITCH_CLIENT_SECRET")

        self.twitch_bot: TwitchBot | None = None

        self._loop = asyncio.get_event_loop()

    async def start(self):
        """
        Starts the TwitchBot, and starts a task for continuously sending currently streaming users to it.
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
        async with asyncio.TaskGroup() as tg:
            tg.create_task(self.twitch_bot.start())
            await self.twitch_bot.wait_for_ready()
            await asyncio.sleep(1)  # wait for twitch_bot to initialize the server
            tg.create_task(self.listener(self.twitch_bot.server_socket))

    async def listener(self, server_sock: socket.socket):
        """
        Main coroutine of the bot manager. Checks streaming users and sends the updated list to bot every
        STREAMING_USERS_UPDATE_SLEEP seconds.
        """
        address = server_sock.getsockname()
        logger.info(f"Starting Bot Manager Listener on {address=}")
        _, writer = await asyncio.open_connection(*address[:2])
        while True:
            try:
                # Wait for STREAMING_USERS_UPDATE_SLEEP seconds before sending connected users
                await asyncio.sleep(STREAMING_USERS_UPDATE_SLEEP)

                streaming_users = await self.get_streaming_users()
                logger.info(f'Sending streaming users to the Twitch Bot: {streaming_users}')
                message = ",".join(streaming_users) + "\n"
                writer.write(message.encode())
                await writer.drain()

            except BaseException as e:
                logger.exception("Bot manager sender error exiting...", exc_info=e)
                writer.close()
                await writer.wait_closed()
                raise e

    async def get_streaming_users(self) -> set:
        """Gets the currently streaming users from TwitchAPI."""
        users = self.db_client.get_enabled_users()
        users = self.extract_user_id(users)

        streaming_usernames = set()
        async with TwitchAPI(self.twitch_client_id, self.twitch_client_secret) as twitch_api:
            streaming_twitch_user_data = twitch_api.get_streams(users)
            streaming_twitch_user_ids = []
            operations = []
            async for user in streaming_twitch_user_data:
                twitch_username = user["user_login"]
                twitch_id = int(user["user_id"])
                streaming_usernames.add(twitch_username)
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
        return streaming_usernames

    @staticmethod
    async def extract_user_id(users: AsyncIterable[DBUser]) -> AsyncIterable[int]:
        async for user in users:
            yield user.twitchId
