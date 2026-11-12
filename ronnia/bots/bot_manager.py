import asyncio
import logging
import os
import socket
import time
from collections.abc import AsyncIterator

from pymongo import UpdateOne

from ronnia.bots.announcements import LiveAnnouncementTracker, select_announcements
from ronnia.bots.twitch_bot import TwitchBot
from ronnia.clients.legacy_api import LegacyAPI, env_flag
from ronnia.clients.mongo import RonniaDatabase
from ronnia.clients.twitch import TwitchAPI
from ronnia.models.database import DBUser

STREAMING_USERS_UPDATE_SLEEP = 60

# Migration notices are user-visible and cannot be recalled, so delivery is
# paced and de-duplicated across restarts.
ANNOUNCEMENT_GRACE_SECONDS = 120
ANNOUNCEMENT_INTERVAL_SECONDS = 5.0
ANNOUNCEMENT_BATCH = 3
ANNOUNCEMENT_DEDUPE_SECONDS = 24 * 60 * 60

logger = logging.getLogger(__name__)


def _env_float(name: str, default: float) -> float:
    raw = (os.getenv(name) or "").strip()
    if not raw:
        return default
    try:
        value = float(raw)
    except ValueError:
        logger.warning("Ignoring invalid %s=%r; using %.1f", name, raw, default)
        return default
    return value if value >= 0 else default


def _env_int(name: str, default: int) -> int:
    raw = (os.getenv(name) or "").strip()
    if not raw:
        return default
    try:
        return int(raw)
    except ValueError:
        logger.warning("Ignoring invalid %s=%r; using %d", name, raw, default)
        return default


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
            and env_flag("RONNIA_ANNOUNCE_ENABLED", True)
        )
        if self.legacy_api and not self.migration_url:
            logger.warning(
                "Legacy bridge is enabled but RONNIA_MIGRATION_URL is unset; "
                "live migration announcements are disabled"
            )
        self.announcement_tracker = LiveAnnouncementTracker(
            enabled=announcements_enabled
        )
        self.announcement_grace_seconds = _env_float(
            "RONNIA_ANNOUNCE_GRACE_SECS", ANNOUNCEMENT_GRACE_SECONDS
        )
        self.announcement_interval_seconds = _env_float(
            "RONNIA_ANNOUNCE_INTERVAL_SECS", ANNOUNCEMENT_INTERVAL_SECONDS
        )
        self.announcement_batch = max(
            1, _env_int("RONNIA_ANNOUNCE_BATCH", ANNOUNCEMENT_BATCH)
        )
        self.announcement_dedupe_seconds = _env_float(
            "RONNIA_ANNOUNCE_DEDUPE_SECS", ANNOUNCEMENT_DEDUPE_SECONDS
        )
        self.announced_at: dict[int, float] = {}
        self._announcement_state_loaded = False
        self._started_at = time.monotonic()

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
        while True:
            try:
                # The reader side is unused; the socket is one-way.
                _reader, writer = await asyncio.open_connection(*address[:2])
            except OSError as e:
                logger.warning("TwitchBot IPC is not accepting connections yet: %s", e)
                await asyncio.sleep(1)
                continue

            try:
                while True:
                    try:
                        streaming_users = await self.get_streaming_users()
                        logger.info(f'Sending streaming users to the Twitch Bot: {streaming_users}')
                        message = ",".join(streaming_users) + "\n"
                        writer.write(message.encode())
                        await writer.drain()
                        await self.announce_pending()
                    except (ConnectionError, asyncio.IncompleteReadError) as e:
                        # The receiver closed the socket. Reconnect on the next
                        # iteration instead of sleeping a whole polling cycle and
                        # then writing into a dead pipe.
                        logger.warning("TwitchBot IPC connection lost: %s", e)
                        break
                    except Exception:
                        logger.exception("Bot manager cycle failed; keeping the IPC connection")

                    await asyncio.sleep(STREAMING_USERS_UPDATE_SLEEP)
            finally:
                if not writer.is_closing():
                    writer.close()
                try:
                    await writer.wait_closed()
                except Exception:
                    pass  # The peer is already gone; nothing to salvage.

    async def _load_announcement_state(self) -> None:
        """Load the durable delivery record once, so restarts cannot repeat notices."""
        if self._announcement_state_loaded:
            return
        try:
            self.announced_at = await self.db_client.get_announcement_state()
        except Exception:
            logger.exception(
                "Could not load the migration announcement record; "
                "announcements are deferred until the store is readable"
            )
            return
        self._announcement_state_loaded = True
        logger.info(
            "Loaded %d recorded migration announcement(s)", len(self.announced_at)
        )

    async def announce_pending(self):
        """Flush queued one-per-session migration announcements."""
        if (
                not self.announcement_tracker.enabled
                or self.twitch_bot is None
                or not self.migration_url
        ):
            return

        # A crash at startup used to re-announce every live channel, which the
        # flood then made worse. Nothing is sent until the process has been up
        # long enough to be plausibly stable.
        if time.monotonic() - self._started_at < self.announcement_grace_seconds:
            return

        pending = self.announcement_tracker.take_pending()
        if not pending:
            return

        await self._load_announcement_state()

        now = time.time()
        batch = select_announcements(
            pending,
            self.announced_at,
            now,
            self.announcement_dedupe_seconds,
            self.announcement_batch,
        )
        deferred = [user for user in pending if user not in batch]
        self.announcement_tracker.requeue(deferred)
        if not batch:
            return

        logger.info(
            "Sending migration announcement to %d legacy user(s) (%d deferred)",
            len(batch),
            len(deferred),
        )
        undelivered: list[DBUser] = []
        for index, user in enumerate(batch):
            if index:
                await asyncio.sleep(self.announcement_interval_seconds)
            try:
                delivered = await self.twitch_bot.announce_migration_one(
                    user, self.migration_url
                )
            except Exception:
                logger.exception("Failed to flush migration announcement")
                delivered = False
            if not delivered:
                undelivered.append(user)
                continue
            announced_at = time.time()
            self.announced_at[user.twitchId] = announced_at
            try:
                await self.db_client.record_announcement(user.twitchId, announced_at)
            except Exception:
                logger.exception(
                    "Announcement delivered but not recorded for %s; it may be repeated",
                    user.twitchUsername,
                )
        self.announcement_tracker.requeue(undelivered)

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
