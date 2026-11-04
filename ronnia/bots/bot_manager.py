import asyncio
import logging
import os
import socket
from collections.abc import AsyncIterator

from pymongo import UpdateOne

from ronnia.bots.announcements import LiveAnnouncementTracker
from ronnia.bots.twitch_bot import TwitchBot
from ronnia.clients.legacy_api import LegacyAPI, env_flag
from ronnia.clients.mongo import RonniaDatabase
from ronnia.clients.twitch import TwitchAPI

STREAMING_USERS_UPDATE_SLEEP = 60

logger = logging.getLogger(__name__)


class BotManager:
    def __init__(
            self,
            legacy_api: LegacyAPI | None = None,
    ):
        try:
            self.legacy_api = legacy_api or LegacyAPI.from_env()
        except RuntimeError:
            logger.critical("Invalid legacy bridge configuration", exc_info=True)
            raise

        self.db_client = RonniaDatabase(
            os.getenv("MONGODB_URL"), legacy_api=self.legacy_api
        )

        self.twitch_client_id = os.getenv("TWITCH_CLIENT_ID")
        self.twitch_client_secret = os.getenv("TWITCH_CLIENT_SECRET")
        self.migration_url = os.getenv("RONNIA_MIGRATION_URL")

        self.twitch_bot: TwitchBot | None = None

        environment = os.getenv("ENVIRONMENT")
        announcements_enabled = bool(
            self.legacy_api
            and self.migration_url
            and environment != "testing"
            and not env_flag("DRY_RUN")
        )
        if self.legacy_api and not self.migration_url:
            logger.warning(
                "Legacy bridge is enabled but RONNIA_MIGRATION_URL is unset; "
                "live migration announcements are disabled"
            )
        self.announcement_tracker = LiveAnnouncementTracker(
            enabled=announcements_enabled
        )

        if self.legacy_api is not None:
            logger.info(
                "Legacy bridge enabled: users/settings are read from %s; "
                "Mongo Users/Settings are not used",
                self.legacy_api.base_url,
            )
        else:
            logger.warning(
                "Legacy bridge disabled: using the old Mongo Users/Settings implementation"
            )

        self._loop = asyncio.get_event_loop()

    async def start(self):
        """
        Starts the TwitchBot, and starts a task for continuously sending currently streaming users to it.
        """
        await self.db_client.initialize()
        streaming_user_names = await self.get_streaming_users()

        self.twitch_bot = TwitchBot(
            initial_channel_names=streaming_user_names,
            listener_update_sleep=STREAMING_USERS_UPDATE_SLEEP,
            database=self.db_client,
            legacy_api=self.legacy_api,
        )
        logger.info(
            f"Started Twitch bot instance for {len(streaming_user_names)} users"
        )
        twitch_bot_task = asyncio.create_task(self.twitch_bot.start())
        await self.twitch_bot.wait_for_ready()
        await asyncio.sleep(1)  # wait for twitch_bot to initialize the server
        await self.listener(self.twitch_bot.server_socket)

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
                streaming_users = await self.get_streaming_users()
                logger.info(f'Sending streaming users to the Twitch Bot: {streaming_users}')
                message = ",".join(streaming_users) + "\n"
                writer.write(message.encode())
                await writer.drain()
                await self.announce_pending()
            except Exception as e:
                logger.exception("An exception occurred in listener", exc_info=e)
                if writer is not None and not writer.is_closing():
                    writer.close()
                    try:
                        await writer.wait_closed()
                    except Exception:
                        logger.exception("Another exception occurred in listener while trying to close writer",
                                         exc_info=e)
                        pass  # Suppress exceptions during cleanup
                logger.info("Initializing connection to TwitchBot...")
                _, writer = await asyncio.open_connection(*address[:2])
            finally:
                # Wait for STREAMING_USERS_UPDATE_SLEEP seconds before sending connected users
                await asyncio.sleep(STREAMING_USERS_UPDATE_SLEEP)

    async def announce_pending(self):
        """Flush queued one-per-session migration announcements."""
        if (
                not self.announcement_tracker.enabled
                or self.twitch_bot is None
                or not self.migration_url
        ):
            return

        pending = self.announcement_tracker.take_pending()
        if not pending:
            return

        logger.info(
            "Sending migration announcement to %d newly live legacy user(s)",
            len(pending),
        )
        try:
            await self.twitch_bot.announce_migration(pending, self.migration_url)
        except Exception:
            logger.exception("Failed to flush migration announcements")

    async def get_streaming_users(self) -> set:
        """Gets the currently streaming users from TwitchAPI."""
        users = [user async for user in self.db_client.get_enabled_users()]

        async def user_ids() -> AsyncIterator[int]:
            for user in users:
                yield user.twitchId

        streaming_usernames = set()
        streaming_twitch_user_ids = []
        live_logins: dict[int, str] = {}
        operations = []
        async with TwitchAPI(self.twitch_client_id, self.twitch_client_secret) as twitch_api:
            streaming_twitch_user_data = twitch_api.get_streams(user_ids())
            async for user in streaming_twitch_user_data:
                twitch_username = user["user_login"]
                twitch_id = int(user["user_id"])
                streaming_usernames.add(twitch_username)
                streaming_twitch_user_ids.append(twitch_id)
                live_logins[twitch_id] = twitch_username
                operations.append(
                    UpdateOne(
                        {"twitchId": twitch_id},
                        {"$set": {"isLive": True, "twitchUsername": twitch_username}}
                    )
                )

        if self.legacy_api is None:
            logger.info(f"Updating {len(operations)} documents with Live status.")
            await self.db_client.bulk_write_operations(
                operations=operations, col=self.db_client.users_col
            )
            await self.db_client.users_col.update_many(
                {"isLive": True, "twitchId": {"$nin": streaming_twitch_user_ids}},
                {"$set": {"isLive": False}},
            )
        else:
            logger.debug(
                "Legacy bridge mode: not writing isLive state to Mongo Users"
            )

        if self.legacy_api is not None:
            users_by_id = {user.twitchId: user for user in users}
            for twitch_id, twitch_username in live_logins.items():
                live_user = users_by_id.get(twitch_id)
                if live_user is not None:
                    # Announcements must use the streamer's current login, not
                    # the (possibly stale) name read from the dashboard.
                    live_user.twitchUsername = twitch_username

            try:
                await self.legacy_api.report_live(
                    sorted(live_logins.items())
                )
            except Exception:
                logger.exception(
                    "Failed to report the live legacy channel set; "
                    "continuing with the polled membership"
                )

        self.announcement_tracker.observe(users, streaming_twitch_user_ids)
        return streaming_usernames
