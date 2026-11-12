"""Regression tests for migration-notice de-duplication and pacing.

The notice is user-visible: an in-memory-only record re-announced every live
channel after a restart, and the resulting burst disconnected the bot, which
restarted and announced again.
"""

import time
import unittest

from ronnia.bots.announcements import LiveAnnouncementTracker, select_announcements
from ronnia.models.database import DBUser


def make_user(twitch_id: int, login: str, osu_id: int | None = None) -> DBUser:
    return DBUser(
        osuId=osu_id or twitch_id * 10,
        osuUsername=f"osu-{login}",
        osuAvatarUrl="https://example.test/osu.png",
        twitchId=twitch_id,
        twitchUsername=login,
        twitchAvatarUrl="https://example.test/twitch.png",
        excludedUsers=[],
        isLive=True,
        settings={
            "echo": True,
            "enable": True,
            "sub-only": False,
            "points-only": False,
            "test": False,
            "cooldown": 30.0,
            "sr": [0, -1],
        },
    )


class SelectAnnouncementsTestCase(unittest.TestCase):
    def test_recently_announced_users_are_skipped(self) -> None:
        users = [make_user(10, "alpha"), make_user(11, "beta")]
        now = 1_000.0
        selected = select_announcements(
            users, {10: now - 60}, now, dedupe_seconds=3600, batch=10
        )
        self.assertEqual([user.twitchId for user in selected], [11])

    def test_dedupe_window_expiry_allows_a_new_notice(self) -> None:
        users = [make_user(10, "alpha")]
        now = 10_000.0
        selected = select_announcements(
            users, {10: now - 3601}, now, dedupe_seconds=3600, batch=10
        )
        self.assertEqual([user.twitchId for user in selected], [10])

    def test_batch_cap_is_enforced(self) -> None:
        users = [make_user(10 + index, f"u{index}") for index in range(5)]
        selected = select_announcements(users, {}, 1.0, dedupe_seconds=3600, batch=2)
        self.assertEqual(len(selected), 2)

    def test_zero_batch_sends_nothing(self) -> None:
        users = [make_user(10, "alpha")]
        self.assertEqual(select_announcements(users, {}, 1.0, 3600, 0), [])


class TrackerRequeueTestCase(unittest.TestCase):
    def test_requeue_keeps_only_live_unannounced_users(self) -> None:
        live = make_user(10, "alpha")
        offline = make_user(11, "beta")
        tracker = LiveAnnouncementTracker(enabled=True)
        tracker.observe([live, offline], [live.twitchId])
        tracker.take_pending()

        tracker.requeue([live, offline])
        self.assertEqual([user.twitchId for user in tracker.pending], [live.twitchId])

        tracker.take_pending()
        tracker.observe([live, offline], [])
        tracker.requeue([live])
        self.assertEqual(tracker.pending, [])


class FakeBot:
    def __init__(self, outcomes: list[bool] | None = None) -> None:
        self.outcomes = list(outcomes or [])
        self.calls: list[int] = []

    async def announce_migration_one(self, user: DBUser, migration_url: str) -> bool:
        self.calls.append(user.twitchId)
        return self.outcomes.pop(0) if self.outcomes else True


class FakeDB:
    def __init__(self, state: dict[int, float] | None = None) -> None:
        self.state = dict(state or {})
        self.loads = 0
        self.recorded: list[tuple[int, float]] = []

    async def get_announcement_state(self) -> dict[int, float]:
        self.loads += 1
        return dict(self.state)

    async def record_announcement(self, twitch_id: int, announced_at: float) -> None:
        self.recorded.append((twitch_id, announced_at))
        self.state[twitch_id] = announced_at


class ManagerAnnouncementTestCase(unittest.IsolatedAsyncioTestCase):
    def make_manager(
            self, *, bot: FakeBot, db: FakeDB, batch: int = 3, grace: float = 0.0,
            started_ago: float = 10_000.0,
    ):
        from ronnia.bots.bot_manager import BotManager

        manager = object.__new__(BotManager)
        manager.legacy_api = object()
        manager.migration_url = "https://ronnia.test"
        manager.twitch_bot = bot
        manager.announcement_tracker = LiveAnnouncementTracker(enabled=True)
        manager.announcement_grace_seconds = grace
        manager.announcement_interval_seconds = 0.0
        manager.announcement_batch = batch
        manager.announcement_dedupe_seconds = 3600.0
        manager.announced_at = {}
        manager._announcement_state_loaded = False
        manager._started_at = time.monotonic() - started_ago
        manager.db_client = db
        return manager

    async def test_delivery_is_recorded_durably(self) -> None:
        user = make_user(10, "alpha")
        bot, db = FakeBot([True]), FakeDB()
        manager = self.make_manager(bot=bot, db=db)
        manager.announcement_tracker.observe([user], [user.twitchId])

        await manager.announce_pending()

        self.assertEqual(bot.calls, [10])
        self.assertEqual([item[0] for item in db.recorded], [10])
        self.assertEqual(db.loads, 1)
        self.assertEqual(manager.announced_at[10], db.recorded[0][1])

    async def test_restart_does_not_repeat_a_recorded_notice(self) -> None:
        user = make_user(10, "alpha")
        bot = FakeBot()
        db = FakeDB({10: time.time()})
        manager = self.make_manager(bot=bot, db=db)
        manager.announcement_tracker.observe([user], [user.twitchId])

        await manager.announce_pending()

        self.assertEqual(bot.calls, [], "a restart must not re-announce the same channel")
        self.assertEqual(db.recorded, [])

    async def test_failed_delivery_is_requeued_and_not_recorded(self) -> None:
        user = make_user(10, "alpha")
        bot, db = FakeBot([False]), FakeDB()
        manager = self.make_manager(bot=bot, db=db)
        manager.announcement_tracker.observe([user], [user.twitchId])

        await manager.announce_pending()

        self.assertEqual(bot.calls, [10])
        self.assertEqual(db.recorded, [])
        self.assertEqual(
            [queued.twitchId for queued in manager.announcement_tracker.pending], [10]
        )

    async def test_startup_grace_defers_all_notices(self) -> None:
        user = make_user(10, "alpha")
        bot, db = FakeBot(), FakeDB()
        manager = self.make_manager(bot=bot, db=db, grace=600.0, started_ago=0.0)
        manager.announcement_tracker.observe([user], [user.twitchId])

        await manager.announce_pending()

        self.assertEqual(bot.calls, [])
        self.assertEqual(len(manager.announcement_tracker.pending), 1)

    async def test_batch_cap_defers_the_rest_to_later_cycles(self) -> None:
        users = [make_user(10 + index, f"u{index}") for index in range(4)]
        bot, db = FakeBot(), FakeDB()
        manager = self.make_manager(bot=bot, db=db, batch=2)
        manager.announcement_tracker.observe(users, [user.twitchId for user in users])

        await manager.announce_pending()

        self.assertEqual(len(bot.calls), 2)
        self.assertEqual(len(manager.announcement_tracker.pending), 2)

        await manager.announce_pending()
        self.assertEqual(len(bot.calls), 4)
        self.assertEqual(manager.announcement_tracker.pending, [])


if __name__ == "__main__":  # pragma: no cover
    unittest.main()
