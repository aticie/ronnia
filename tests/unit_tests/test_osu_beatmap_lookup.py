import asyncio
import unittest
from types import SimpleNamespace
from unittest.mock import AsyncMock

import aiohttp
from aiohttp import web

from ronnia.bots.twitch_bot import TwitchBot
from ronnia.clients.osu import OsuApiV2
from ronnia.models.beatmap import Beatmap, BeatmapType


class TestOsuBeatmapLookup(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.responses = {}
        app = web.Application()
        app.router.add_get("/api/v2/{resource}/{id}", self.respond)
        self.runner = web.AppRunner(app)
        await self.runner.setup()
        self.addAsyncCleanup(self.runner.cleanup)
        site = web.TCPSite(self.runner, "127.0.0.1", 0)
        await site.start()
        port = self.runner.addresses[0][1]

        # Bypass the singleton so each test owns its session and authentication.
        self.api = object.__new__(OsuApiV2)
        OsuApiV2.__init__(self.api, "test-client", "test-secret")
        self.api._session = aiohttp.ClientSession()
        self.addAsyncCleanup(self.api._session.close)
        self.api._api_base_url = f"http://127.0.0.1:{port}/api/v2/"
        self.api._auth_header = {}
        self.api.wait_cooldown = AsyncMock()

        self.bot = object.__new__(TwitchBot)
        self.bot.osu_api = self.api
        self.bot.ronnia_db = SimpleNamespace(
            get_beatmap=AsyncMock(return_value=None),
            add_beatmap=AsyncMock(),
        )
        self.bot.environment = "production"
        self.bot.legacy_api = None
        self.bot.check_request_criteria = AsyncMock()
        self.bot._handle_legacy_mongo_request = AsyncMock()

    async def respond(self, request):
        status, payload = self.responses[request.path]
        if isinstance(payload, str):
            return web.Response(status=status, text=payload)
        return web.json_response(payload, status=status)

    @staticmethod
    def message(url):
        return SimpleNamespace(
            content=url,
            author=SimpleNamespace(name="viewer"),
            channel=SimpleNamespace(name="streamer"),
        )

    async def test_missing_maps_and_sets_are_not_cached_or_delivered(self):
        for path, payload in (
            ("beatmaps/0", {"error": "Specified beatmap difficulty couldn't be found."}),
            ("beatmaps/123", {"error": None}),
            ("beatmapsets/456", {"error": "Specified beatmapset couldn't be found."}),
            ("beatmaps/789", "Not Found"),
        ):
            with self.subTest(path=path):
                self.responses[f"/api/v2/{path}"] = (404, payload)
                message = self.message(f"https://osu.ppy.sh/{path}")
                await self.bot.event_message(message)
                self.bot.ronnia_db.add_beatmap.assert_not_called()
                self.bot.check_request_criteria.assert_not_awaited()
                self.bot._handle_legacy_mongo_request.assert_not_awaited()

    async def test_non_not_found_failures_remain_errors(self):
        for resource, beatmap_type in (
            ("beatmaps", BeatmapType.MAP),
            ("beatmapsets", BeatmapType.MAPSET),
        ):
            for status in (401, 403, 429, 500):
                with self.subTest(resource=resource, status=status):
                    self.responses[f"/api/v2/{resource}/123"] = (status, "API failure")
                    beatmap = Beatmap(id=123, type=beatmap_type, mods="")
                    with self.assertRaises(aiohttp.ClientResponseError) as caught:
                        await self.api.get_beatmap(beatmap)
                    self.assertEqual(caught.exception.status, status)

    async def test_successful_map_and_set_return_the_requested_data(self):
        beatmapset_info = {"id": 456, "artist": "Artist", "title": "Title"}
        beatmap_info = {"id": 123, "beatmapset": beatmapset_info}
        self.responses["/api/v2/beatmaps/123"] = (200, beatmap_info)
        self.responses["/api/v2/beatmapsets/456"] = (
            200, {**beatmapset_info, "beatmaps": [beatmap_info]}
        )

        result = await self.api.get_beatmap(Beatmap(id=123, type=BeatmapType.MAP, mods=""))
        self.assertEqual(result, (beatmap_info, beatmapset_info))
        result = await self.api.get_beatmap(Beatmap(id=456, type=BeatmapType.MAPSET, mods=""))
        self.assertEqual(result, (beatmap_info, {**beatmapset_info, "beatmaps": [beatmap_info]}))

    async def test_valid_request_is_cached_and_delivered_after_a_missing_map(self):
        self.responses["/api/v2/beatmaps/0"] = (404, {"error": "Not found"})
        await self.bot.event_message(self.message("https://osu.ppy.sh/beatmaps/0"))

        beatmapset_info = {"id": 456, "artist": "Artist", "title": "Title"}
        beatmap_info = {"id": 123, "beatmapset": beatmapset_info}
        self.responses["/api/v2/beatmaps/123"] = (200, beatmap_info)
        cached = asyncio.Event()

        async def add_beatmap(info):
            cached.set()

        self.bot.ronnia_db.add_beatmap.side_effect = add_beatmap
        message = self.message("https://osu.ppy.sh/beatmaps/123")
        await self.bot.event_message(message)
        await asyncio.wait_for(cached.wait(), timeout=1)

        self.bot.ronnia_db.add_beatmap.assert_awaited_once_with(beatmap_info)
        self.bot.check_request_criteria.assert_awaited_once_with(message, beatmap_info)
        self.bot._handle_legacy_mongo_request.assert_awaited_once_with(
            message=message,
            beatmap=Beatmap(id=123, type=BeatmapType.MAP, mods=""),
            beatmap_info=beatmap_info,
            beatmapset_info=beatmapset_info,
        )
