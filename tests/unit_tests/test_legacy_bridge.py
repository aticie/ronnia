import unittest

from ronnia.bots.announcements import LiveAnnouncementTracker
from ronnia.bots.twitch_bot import TwitchBot
from ronnia.clients.legacy_api import ClaimResult, LegacyAPIError
from ronnia.models.beatmap import Beatmap, BeatmapType
from ronnia.models.database import DBUser


def make_db_user(*, echo: bool = True) -> DBUser:
    return DBUser(
        osuId=110,
        osuUsername="osu-alpha",
        osuAvatarUrl="https://example.test/osu.png",
        twitchId=10,
        twitchUsername="alpha",
        twitchAvatarUrl="https://example.test/twitch.png",
        excludedUsers=[],
        isLive=True,
        settings={
            "echo": echo,
            "enable": True,
            "sub-only": False,
            "points-only": False,
            "test": False,
            "cooldown": 30.0,
            "sr": [0.0, -1.0],
        },
    )


class FakeChannel:
    def __init__(self, name: str = "alpha"):
        self.name = name
        self.sent: list[str] = []

    async def send(self, content: str):
        self.sent.append(content)


class FakeAuthor:
    def __init__(self, name: str = "viewer"):
        self.name = name
        self.is_mod = False
        self.is_subscriber = False
        self.badges = []
        self.id = "viewer-id"


class FakeMessage:
    def __init__(self, *, message_id: str | None = "message-1", author=None, channel=None):
        self.id = message_id
        self.content = "https://osu.ppy.sh/b/123"
        self.author = author or FakeAuthor()
        self.channel = channel or FakeChannel()
        self.tags = {}


class FakeDatabase:
    def __init__(self, user: DBUser, legacy_api):
        self.user = user
        self.legacy_api = legacy_api

    async def get_user_from_twitch_username(self, twitch_username: str) -> DBUser:
        return self.user


class FakeLegacyAPI:
    def __init__(self):
        self.claim_result = ClaimResult(True)
        self.claim_error: Exception | None = None
        self.report_error: Exception | None = None
        self.claim_calls: list[dict] = []
        self.report_calls: list[dict] = []
        self.events: list[str] = []

    async def claim(self, **kwargs) -> ClaimResult:
        self.claim_calls.append(kwargs)
        self.events.append("claim")
        if self.claim_error is not None:
            raise self.claim_error
        return self.claim_result

    async def report_delivery(self, **kwargs) -> None:
        self.report_calls.append(kwargs)
        self.events.append("report")
        if self.report_error is not None:
            raise self.report_error


class FakeOsuChat:
    def __init__(self, fail: bool = False):
        self.fail = fail
        self.send_calls: list[dict] = []

    async def send_message(self, target_id: int, message: str, is_action: bool = False):
        self.send_calls.append(
            {"target_id": target_id, "message": message, "is_action": is_action}
        )
        if self.fail:
            raise RuntimeError("osu chat unavailable")


BETMAP_INFO = {
    "id": 123,
    "version": "Insane",
    "bpm": 180,
    "status": "ranked",
    "difficulty_rating": 5.25,
    "hit_length": 146,
}
BEATMAPSET_INFO = {"artist": "Artist", "title": "Title"}


def make_bot(*, echo: bool = True, osu_fail: bool = False) -> tuple[TwitchBot, FakeLegacyAPI, FakeOsuChat, FakeMessage]:
    legacy_api = FakeLegacyAPI()
    database = FakeDatabase(make_db_user(echo=echo), legacy_api)
    osu_chat = FakeOsuChat(fail=osu_fail)
    message = FakeMessage()

    bot = object.__new__(TwitchBot)
    bot.ronnia_db = database
    bot.legacy_api = legacy_api
    bot.osu_chat_api = osu_chat
    bot.environment = "production"
    bot.dry_run = False
    return bot, legacy_api, osu_chat, message


async def send_request(bot: TwitchBot, message: FakeMessage):
    await bot._handle_legacy_bridge_request(
        message=message,
        beatmap=Beatmap(id=123, type=BeatmapType.MAP, mods=""),
        beatmap_info=BETMAP_INFO,
        beatmapset_info=BEATMAPSET_INFO,
    )


class LegacyBridgeRequestTestCase(unittest.IsolatedAsyncioTestCase):
    async def test_rejected_claim_sends_nothing(self) -> None:
        bot, legacy_api, osu_chat, message = make_bot()
        legacy_api.claim_result = ClaimResult(False, "already delivered")

        await send_request(bot, message)

        self.assertEqual(len(legacy_api.claim_calls), 1)
        self.assertEqual(osu_chat.send_calls, [])
        self.assertEqual(legacy_api.report_calls, [])
        self.assertEqual(message.channel.sent, [])

    async def test_claim_failure_sends_nothing(self) -> None:
        bot, legacy_api, osu_chat, message = make_bot()
        legacy_api.claim_error = LegacyAPIError("bridge down")

        with self.assertLogs("ronnia.bots.twitch_bot", level="ERROR"):
            await send_request(bot, message)

        self.assertEqual(osu_chat.send_calls, [])
        self.assertEqual(legacy_api.report_calls, [])
        self.assertEqual(message.channel.sent, [])

    async def test_claim_uses_message_id_channel_twitch_and_osu_ids(self) -> None:
        bot, legacy_api, _, message = make_bot()

        await send_request(bot, message)

        claim = legacy_api.claim_calls[0]
        self.assertEqual(claim["twitch_message_id"], "message-1")
        self.assertEqual(claim["twitch_id"], 10)
        self.assertEqual(claim["target_osu_id"], 110)
        self.assertIn("viewer -> [Ranked]", claim["message"])
        self.assertIn("https://osu.ppy.sh/b/123", claim["message"])

    async def test_failed_pm_reports_failure_and_does_not_echo_or_write_stats(self) -> None:
        bot, legacy_api, osu_chat, message = make_bot(osu_fail=True)

        with self.assertLogs("ronnia.bots.twitch_bot", level="ERROR"):
            await send_request(bot, message)

        self.assertEqual(len(osu_chat.send_calls), 1)
        self.assertEqual(len(legacy_api.report_calls), 1)
        report = legacy_api.report_calls[0]
        self.assertFalse(report["sent"])
        self.assertEqual(report["error"], "osu chat unavailable")
        self.assertIsNone(report["statistics"])
        self.assertEqual(message.channel.sent, [])

    async def test_successful_pm_reports_stats_then_echoes(self) -> None:
        bot, legacy_api, osu_chat, message = make_bot(echo=True)

        await send_request(bot, message)

        self.assertEqual(len(osu_chat.send_calls), 1)
        self.assertEqual(len(legacy_api.report_calls), 1)
        report = legacy_api.report_calls[0]
        self.assertTrue(report["sent"])
        self.assertIsNone(report["error"])
        self.assertEqual(
            {key: value for key, value in report["statistics"].items() if key != "timestamp"},
            {
                "requesterChannelName": "viewer",
                "requestedBeatmapId": 123,
                "requestedChannelName": "alpha",
                "mods": None,
            },
        )
        self.assertIsInstance(report["statistics"]["timestamp"], int)
        self.assertEqual(len(message.channel.sent), 1)
        self.assertIn("Request sent!", message.channel.sent[0])
        self.assertEqual(legacy_api.events, ["claim", "report"])

    async def test_echo_disabled_does_not_send_twitch_message(self) -> None:
        bot, legacy_api, _, message = make_bot(echo=False)

        await send_request(bot, message)

        self.assertEqual(len(legacy_api.report_calls), 1)
        self.assertEqual(message.channel.sent, [])

    async def test_report_failure_does_not_crash_or_cancel_echo(self) -> None:
        bot, legacy_api, _, message = make_bot(echo=True)
        legacy_api.report_error = LegacyAPIError("stats endpoint down")

        with self.assertLogs("ronnia.bots.twitch_bot", level="ERROR"):
            await send_request(bot, message)

        self.assertEqual(len(legacy_api.report_calls), 1)
        self.assertEqual(len(message.channel.sent), 1)

    async def test_missing_twitch_message_id_does_nothing(self) -> None:
        bot, legacy_api, osu_chat, message = make_bot()
        message.id = None

        with self.assertLogs("ronnia.bots.twitch_bot", level="WARNING"):
            await send_request(bot, message)

        self.assertEqual(legacy_api.claim_calls, [])
        self.assertEqual(osu_chat.send_calls, [])


class LiveAnnouncementTrackerTestCase(unittest.TestCase):
    def test_announcement_once_per_live_session_and_rearm_after_offline(self) -> None:
        user = make_db_user()
        tracker = LiveAnnouncementTracker(enabled=True)

        first = tracker.observe([user], [user.twitchId])
        self.assertEqual([item.twitchId for item in first], [user.twitchId])
        self.assertEqual(len(tracker.pending), 1)

        # Still live on the next polling cycle: no duplicate.
        tracker.observe([user], [user.twitchId])
        self.assertEqual(len(tracker.pending), 1)

        flushed = tracker.take_pending()
        self.assertEqual([item.twitchId for item in flushed], [user.twitchId])
        self.assertEqual(tracker.pending, [])

        tracker.observe([user], [user.twitchId])
        self.assertEqual(tracker.pending, [])

        # Going offline does not announce anything and clears the session.
        tracker.observe([user], [])
        self.assertEqual(tracker.pending, [])
        self.assertEqual(tracker.announced_twitch_ids, set())

        # A new live session is announced again.
        tracker.observe([user], [user.twitchId])
        self.assertEqual(len(tracker.pending), 1)

    def test_pending_announcement_is_dropped_if_user_goes_offline_first(self) -> None:
        user = make_db_user()
        tracker = LiveAnnouncementTracker(enabled=True)

        tracker.observe([user], [user.twitchId])
        tracker.observe([user], [])

        self.assertEqual(tracker.pending, [])

    def test_testing_mode_queues_nothing(self) -> None:
        user = make_db_user()
        tracker = LiveAnnouncementTracker(enabled=False)

        tracker.observe([user], [user.twitchId])

        self.assertEqual(tracker.pending, [])
        self.assertEqual(tracker.announced_twitch_ids, {user.twitchId})


class FakeReadAPI:
    def __init__(self, users: list[DBUser]):
        self.users = users

    async def fetch_enabled_users(self) -> list[DBUser]:
        return list(self.users)

    async def get_user_by_id(self, twitch_id: int) -> DBUser:
        for user in self.users:
            if user.twitchId == twitch_id:
                return user
        raise AssertionError("unexpected id")

    async def get_user_by_login(self, login: str) -> DBUser:
        for user in self.users:
            if user.twitchUsername.lower() == login.lower():
                return user
        raise AssertionError("unexpected login")


class DatabaseBridgeTestCase(unittest.IsolatedAsyncioTestCase):
    def make_db(self, users: list[DBUser]):
        from ronnia.clients.mongo import RonniaDatabase

        db = object.__new__(RonniaDatabase)
        db.legacy_api = FakeReadAPI(users)
        return db

    async def test_enabled_users_come_from_api(self) -> None:
        user = make_db_user()
        db = self.make_db([user])

        users = [item async for item in db.get_enabled_users()]

        self.assertEqual([item.twitchId for item in users], [user.twitchId])

    async def test_user_reads_use_api(self) -> None:
        user = make_db_user()
        db = self.make_db([user])

        self.assertEqual((await db.get_user_from_twitch_id(10)).osuId, 110)
        self.assertEqual(
            (await db.get_user_from_twitch_username("alpha")).twitchId, 10
        )

    async def test_remove_user_is_a_noop_in_bridge_mode(self) -> None:
        db = self.make_db([make_db_user()])

        with self.assertLogs("ronnia.clients.mongo", level="WARNING"):
            self.assertTrue(await db.remove_user("alpha"))

    async def test_add_request_is_a_noop_in_bridge_mode(self) -> None:
        db = self.make_db([make_db_user()])

        self.assertIsNone(
            await db.add_request(
                requester_channel_name="viewer",
                requested_beatmap_id=123,
                requested_channel_name="alpha",
                mods=None,
            )
        )


class FakeDBWithUsers:
    def __init__(self, users: list[DBUser]):
        self.users = users

    async def get_enabled_users(self):
        for user in self.users:
            yield user


class FakeTwitchStreams:
    streaming_user: dict | None = None

    def __init__(self, client_id: str, client_secret: str):
        pass

    async def __aenter__(self):
        return self

    async def __aexit__(self, exc_type, exc_val, exc_tb):
        return None

    async def get_streams(self, user_ids):
        async for _ in user_ids:
            pass
        if self.streaming_user is not None:
            yield self.streaming_user


class FakeLiveReportingAPI:
    def __init__(self):
        self.live_calls: list[list[tuple[int, str]]] = []
        self.live_error: Exception | None = None

    async def report_live(self, channels: list[tuple[int, str]]) -> bool:
        self.live_calls.append(list(channels))
        if self.live_error is not None:
            raise self.live_error
        return True


class ManagerBridgeTestCase(unittest.IsolatedAsyncioTestCase):
    def make_manager(self, users: list[DBUser]):
        from unittest.mock import patch

        from ronnia.bots.bot_manager import BotManager

        legacy_api = FakeLiveReportingAPI()
        manager = object.__new__(BotManager)
        manager.legacy_api = legacy_api
        manager.db_client = FakeDBWithUsers(users)
        manager.twitch_client_id = "id"
        manager.twitch_client_secret = "secret"
        manager.migration_url = "http://localhost:3000"
        manager.twitch_bot = None
        manager.announcement_tracker = LiveAnnouncementTracker(enabled=True)
        return manager, patch(
            "ronnia.bots.bot_manager.TwitchAPI", FakeTwitchStreams
        ), legacy_api

    async def test_bridge_streaming_cycle_queues_announcement_once(self) -> None:
        user = make_db_user()
        manager, patcher, legacy_api = self.make_manager([user])
        FakeTwitchStreams.streaming_user = {"user_login": "alpha", "user_id": "10"}

        try:
            with patcher:
                names = await manager.get_streaming_users()
                self.assertEqual(names, {"alpha"})
                self.assertEqual(len(manager.announcement_tracker.pending), 1)
                self.assertEqual(legacy_api.live_calls, [[(10, "alpha")]])

                # Same live user on the next cycle: no second announcement.
                await manager.get_streaming_users()
                self.assertEqual(len(manager.announcement_tracker.pending), 1)
        finally:
            FakeTwitchStreams.streaming_user = None

    async def test_bridge_streaming_cycle_drops_pending_when_offline(self) -> None:
        user = make_db_user()
        manager, patcher, legacy_api = self.make_manager([user])
        FakeTwitchStreams.streaming_user = {"user_login": "alpha", "user_id": "10"}

        try:
            with patcher:
                await manager.get_streaming_users()
                self.assertEqual(len(manager.announcement_tracker.pending), 1)

                FakeTwitchStreams.streaming_user = None
                names = await manager.get_streaming_users()
                self.assertEqual(names, set())
                self.assertEqual(manager.announcement_tracker.pending, [])
                self.assertEqual(legacy_api.live_calls[-1], [])
        finally:
            FakeTwitchStreams.streaming_user = None

    async def test_live_report_and_announcement_use_current_twitch_login(self) -> None:
        user = make_db_user()
        manager, patcher, legacy_api = self.make_manager([user])
        FakeTwitchStreams.streaming_user = {
            "user_login": "alpha_renamed",
            "user_id": "10",
        }

        try:
            with patcher:
                names = await manager.get_streaming_users()

                self.assertEqual(names, {"alpha_renamed"})
                self.assertEqual(user.twitchUsername, "alpha_renamed")
                pending = manager.announcement_tracker.pending
                self.assertEqual(len(pending), 1)
                self.assertEqual(pending[0].twitchUsername, "alpha_renamed")
                self.assertEqual(legacy_api.live_calls, [[(10, "alpha_renamed")]])

                # The dashboard must receive an explicit empty set when nobody
                # is live so stale legacy rows disappear.
                FakeTwitchStreams.streaming_user = None
                await manager.get_streaming_users()
                self.assertEqual(legacy_api.live_calls[-1], [])
        finally:
            FakeTwitchStreams.streaming_user = None

    async def test_live_report_failure_does_not_change_membership(self) -> None:
        user = make_db_user()
        manager, patcher, legacy_api = self.make_manager([user])
        legacy_api.live_error = LegacyAPIError("live endpoint down")
        FakeTwitchStreams.streaming_user = {
            "user_login": "alpha_renamed",
            "user_id": "10",
        }

        try:
            with patcher:
                with self.assertLogs("ronnia.bots.bot_manager", level="ERROR"):
                    names = await manager.get_streaming_users()

                self.assertEqual(names, {"alpha_renamed"})
                self.assertEqual(user.twitchUsername, "alpha_renamed")
                self.assertEqual(len(manager.announcement_tracker.pending), 1)
                self.assertEqual(legacy_api.live_calls, [[(10, "alpha_renamed")]])
        finally:
            FakeTwitchStreams.streaming_user = None


class MigrationAnnouncementTestCase(unittest.IsolatedAsyncioTestCase):
    def make_bot(self, *, environment: str = "production", osu_fail: bool = False):
        channel = FakeChannel()
        osu_chat = FakeOsuChat(fail=osu_fail)
        bot = object.__new__(TwitchBot)
        bot.environment = environment
        bot.dry_run = False
        bot.osu_chat_api = osu_chat
        bot.get_channel = lambda name: channel
        bot.join_channels = self._unexpected_join
        return bot, channel, osu_chat

    async def _unexpected_join(self, names):
        raise AssertionError(f"should not need to join {names}")

    async def test_announcement_sends_twitch_and_osu_with_url(self) -> None:
        bot, channel, osu_chat = self.make_bot()
        url = "http://localhost:3000"

        await bot.announce_migration([make_db_user()], url)

        self.assertEqual(len(channel.sent), 1)
        self.assertIn(url, channel.sent[0])
        self.assertEqual(len(osu_chat.send_calls), 1)
        self.assertIn(url, osu_chat.send_calls[0]["message"])

    async def test_osu_failure_does_not_prevent_twitch_announcement(self) -> None:
        bot, channel, osu_chat = self.make_bot(osu_fail=True)

        with self.assertLogs("ronnia.bots.twitch_bot", level="ERROR"):
            await bot.announce_migration([make_db_user()], "http://localhost:3000")

        self.assertEqual(len(channel.sent), 1)
        self.assertEqual(len(osu_chat.send_calls), 1)

    async def test_testing_mode_suppresses_both_channels(self) -> None:
        bot, channel, osu_chat = self.make_bot(environment="testing")

        await bot.announce_migration([make_db_user()], "http://localhost:3000")

        self.assertEqual(channel.sent, [])
        self.assertEqual(osu_chat.send_calls, [])
