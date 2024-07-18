import asyncio
import logging
import os
import socket
from typing import List

from pymongo import UpdateOne

from clients.twitch import TwitchAPI
from clients.database import RonniaDatabase
from models.database import DBUser
from ronnia.bots.twitch_bot import TwitchBot

STREAMING_USERS_UPDATE_SLEEP = 120

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
