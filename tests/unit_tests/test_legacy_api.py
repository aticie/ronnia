import time
import unittest
from typing import Any

import aiohttp
from aiohttp import web

from ronnia.clients.legacy_api import (
    LegacyAPI,
    LegacyAPIError,
    LegacyUserUnavailable,
)

TEST_TOKEN = "test-internal-token-do-not-log"


def make_user(
        twitch_id: int,
        login: str,
        osu_id: int,
        *,
        echo: bool = True,
        enable: bool = True,
) -> dict[str, Any]:
    return {
        "osuId": osu_id,
        "osuUsername": f"osu-{login}",
        "osuAvatarUrl": f"https://example.test/osu/{osu_id}.png",
        "twitchId": twitch_id,
        "twitchUsername": login,
        "twitchAvatarUrl": f"https://example.test/twitch/{twitch_id}.png",
        "excludedUsers": ["excluded"],
        "isLive": False,
        "settings": {
            "echo": echo,
            "enable": enable,
            "sub-only": False,
            "points-only": False,
            "test": False,
            "cooldown": 30.0,
            "sr": [0.0, -1.0],
        },
    }


class FakeLegacyServer:
    def __init__(self, users: list[dict[str, Any]]):
        self.users = users
        self.requests: list[dict[str, Any]] = []
        self.claim_claimed = True
        self.claim_reason = "already delivered"
        self.report_status = 200
        self.live_status = 200
        self.runner: web.AppRunner | None = None
        self.port: int | None = None

    def _record(self, request: web.Request, body: Any = None) -> None:
        self.requests.append(
            {
                "method": request.method,
                "path": request.path,
                "query": dict(request.query),
                "body": body,
                "authorization": request.headers.get("Authorization"),
            }
        )

    def _check_auth(self, request: web.Request) -> web.Response | None:
        if request.headers.get("Authorization") != f"Bearer {TEST_TOKEN}":
            return web.json_response({"detail": "unauthorized"}, status=401)
        return None

    async def _channels(self, request: web.Request) -> web.Response:
        unauthorized = self._check_auth(request)
        if unauthorized is not None:
            return unauthorized
        self._record(request)
        after = int(request.query.get("after", "0"))
        limit = int(request.query.get("limit", "100"))
        page = sorted(
            (user for user in self.users if user["twitchId"] > after),
            key=lambda user: user["twitchId"],
        )[:limit]
        return web.json_response(page)

    async def _by_login(self, request: web.Request) -> web.Response:
        unauthorized = self._check_auth(request)
        if unauthorized is not None:
            return unauthorized
        self._record(request)
        login = request.match_info["login"].lower()
        for user in self.users:
            if user["twitchUsername"].lower() == login:
                return web.json_response(user)
        return web.json_response({"detail": "not found"}, status=404)

    async def _by_id(self, request: web.Request) -> web.Response:
        unauthorized = self._check_auth(request)
        if unauthorized is not None:
            return unauthorized
        self._record(request)
        try:
            twitch_id = int(request.match_info["twitch_id"])
        except ValueError:
            return web.json_response({"detail": "invalid id"}, status=400)
        for user in self.users:
            if user["twitchId"] == twitch_id:
                return web.json_response(user)
        return web.json_response({"detail": "not found"}, status=404)

    async def _claim(self, request: web.Request) -> web.Response:
        unauthorized = self._check_auth(request)
        if unauthorized is not None:
            return unauthorized
        body = await request.json()
        self._record(request, body)
        if not self.claim_claimed:
            return web.json_response(
                {"claimed": False, "reason": self.claim_reason}, status=409
            )
        return web.json_response({"claimed": True})

    async def _live(self, request: web.Request) -> web.Response:
        unauthorized = self._check_auth(request)
        if unauthorized is not None:
            return unauthorized
        body = await request.json()
        self._record(request, body)
        if self.live_status != 200:
            return web.json_response({"detail": "boom"}, status=self.live_status)
        return web.json_response({"updated": len(body["channels"])})

    async def _delivery(self, request: web.Request) -> web.Response:
        unauthorized = self._check_auth(request)
        if unauthorized is not None:
            return unauthorized
        body = await request.json()
        self._record(request, body)
        if self.report_status != 200:
            return web.json_response({"detail": "boom"}, status=self.report_status)
        return web.json_response({"ok": True})

    async def start(self) -> str:
        app = web.Application()
        app.router.add_get("/internal/legacy/channels", self._channels)
        app.router.add_get("/internal/legacy/channels/by-login/{login}", self._by_login)
        app.router.add_get("/internal/legacy/channels/{twitch_id}", self._by_id)
        app.router.add_post("/internal/legacy/claim", self._claim)
        app.router.add_post("/internal/legacy/live", self._live)
        app.router.add_post(
            "/internal/legacy/deliveries/{message_id}", self._delivery
        )

        self.runner = web.AppRunner(app)
        await self.runner.setup()
        site = web.TCPSite(self.runner, "127.0.0.1", 0)
        await site.start()
        self.port = site._server.sockets[0].getsockname()[1]
        return f"http://127.0.0.1:{self.port}"

    async def stop(self) -> None:
        if self.runner is not None:
            await self.runner.cleanup()
            self.runner = None

    def requests_for(self, method: str, path: str) -> list[dict[str, Any]]:
        return [
            request
            for request in self.requests
            if request["method"] == method and request["path"] == path
        ]


class LegacyAPITestCase(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        self.server = FakeLegacyServer(
            [
                make_user(10, "alpha", 110),
                make_user(20, "beta", 120, echo=False),
                make_user(30, "gamma", 130),
            ]
        )
        base_url = await self.server.start()
        self.api = LegacyAPI(base_url, TEST_TOKEN)
        self.base_url = base_url

    async def asyncTearDown(self) -> None:
        await self.api.close()
        await self.server.stop()

    async def test_fetch_enabled_users_paginates_until_empty(self) -> None:
        users = await self.api.fetch_enabled_users(limit=2)

        self.assertEqual([user.twitchId for user in users], [10, 20, 30])
        self.assertEqual([user.osuId for user in users], [110, 120, 130])
        self.assertFalse(users[1].settings.echo)
        self.assertEqual(users[0].excludedUsers, ["excluded"])

        page_requests = self.server.requests_for("GET", "/internal/legacy/channels")
        self.assertEqual(
            [request["query"] for request in page_requests],
            [
                {"after": "0", "limit": "2"},
                {"after": "20", "limit": "2"},
                {"after": "30", "limit": "2"},
            ],
        )
        for request in self.server.requests:
            self.assertEqual(request["authorization"], f"Bearer {TEST_TOKEN}")

    async def test_users_are_cached_for_a_short_time(self) -> None:
        first = await self.api.get_user_by_login("Alpha")
        second = await self.api.get_user_by_login("alpha")

        self.assertEqual(first.twitchId, second.twitchId)
        self.assertEqual(
            len(self.server.requests_for("GET", "/internal/legacy/channels/by-login/alpha")),
            1,
        )
        self.assertLessEqual(self.api._cache_ttl, 2.0)

    async def test_404_is_not_cached_and_evicts_old_entry(self) -> None:
        await self.api.get_user_by_id(10)
        # Expire the cached entry without waiting two real seconds.
        for key, (_, user) in list(self.api._cache.items()):
            self.api._cache[key] = (time.monotonic() - 10, user)
        self.server.users = [user for user in self.server.users if user["twitchId"] != 10]

        with self.assertRaises(LegacyUserUnavailable):
            await self.api.get_user_by_id(10)
        self.assertNotIn(("id", 10), self.api._cache)

        self.server.users.append(make_user(10, "alpha", 110))
        user = await self.api.get_user_by_id(10)
        self.assertEqual(user.twitchUsername, "alpha")

    async def test_claim_posts_exact_body_and_returns_accepted(self) -> None:
        result = await self.api.claim(
            twitch_message_id="message-1",
            twitch_id=20,
            target_osu_id=120,
            message="viewer -> map",
        )

        self.assertTrue(result.claimed)
        request = self.server.requests_for("POST", "/internal/legacy/claim")[0]
        self.assertEqual(
            request["body"],
            {
                "twitchMessageId": "message-1",
                "twitchId": 20,
                "targetOsuId": 120,
                "message": "viewer -> map",
            },
        )

    async def test_claim_409_is_a_rejection_not_a_crash(self) -> None:
        self.server.claim_claimed = False

        result = await self.api.claim(
            twitch_message_id="message-2",
            twitch_id=20,
            target_osu_id=120,
            message="viewer -> map",
        )

        self.assertFalse(result.claimed)
        self.assertEqual(result.reason, "already delivered")

    async def test_report_live_posts_exact_current_set(self) -> None:
        reported = await self.api.report_live([(30, "gamma"), (10, "alpha")])

        self.assertTrue(reported)
        request = self.server.requests_for("POST", "/internal/legacy/live")[0]
        self.assertEqual(
            request["body"],
            {
                "channels": [
                    {"twitchId": 30, "login": "gamma"},
                    {"twitchId": 10, "login": "alpha"},
                ]
            },
        )
        self.assertEqual(request["authorization"], f"Bearer {TEST_TOKEN}")

    async def test_report_live_sends_an_empty_set(self) -> None:
        reported = await self.api.report_live([])

        self.assertTrue(reported)
        request = self.server.requests_for("POST", "/internal/legacy/live")[0]
        self.assertEqual(request["body"], {"channels": []})

    async def test_report_live_failure_is_logged_and_tolerated(self) -> None:
        self.server.live_status = 500

        with self.assertLogs("ronnia.clients.legacy_api", level="WARNING") as logs:
            reported = await self.api.report_live([(10, "alpha")])

        self.assertFalse(reported)
        self.assertIn("Failed to report", "\n".join(logs.output))
        self.assertNotIn(TEST_TOKEN, "\n".join(logs.output))

    async def test_report_delivery_posts_statistics(self) -> None:
        statistics = {
            "requesterChannelName": "viewer",
            "requestedBeatmapId": 123,
            "requestedChannelName": "alpha",
            "mods": "+HD",
            "timestamp": 1700000000,
        }
        await self.api.report_delivery(
            twitch_message_id="message-1",
            sent=True,
            statistics=statistics,
        )

        request = self.server.requests_for(
            "POST", "/internal/legacy/deliveries/message-1"
        )[0]
        self.assertEqual(
            request["body"],
            {"sent": True, "error": None, "statistics": statistics},
        )

    async def test_report_failure_is_visible(self) -> None:
        self.server.report_status = 500
        with self.assertRaises(LegacyAPIError) as raised:
            await self.api.report_delivery(
                twitch_message_id="message-1",
                sent=True,
                statistics=None,
            )
        self.assertEqual(raised.exception.status, 500)

    async def test_token_is_never_logged_on_failure(self) -> None:
        self.server.report_status = 500
        with self.assertLogs("ronnia.clients.legacy_api", level="DEBUG") as logs:
            with self.assertRaises(LegacyAPIError):
                await self.api.report_delivery(
                    twitch_message_id="message-1",
                    sent=False,
                    error="send failed",
                )
        self.assertNotIn(TEST_TOKEN, "\n".join(logs.output))


class LegacyAPIConfigTestCase(unittest.TestCase):
    def test_missing_bridge_env_without_explicit_mode_keeps_old_behaviour(self) -> None:
        with self.assertLogs("ronnia.clients.legacy_api", level="WARNING"):
            self.assertIsNone(LegacyAPI.from_env({}))

    def test_explicit_false_disables_bridge(self) -> None:
        self.assertIsNone(
            LegacyAPI.from_env(
                {
                    "RONNIA_LEGACY_MODE": "false",
                    "RONNIA_API_URL": "http://127.0.0.1:4343",
                    "RONNIA_INTERNAL_TOKEN": TEST_TOKEN,
                }
            )
        )

    def test_explicit_true_requires_both_settings(self) -> None:
        with self.assertRaises(RuntimeError) as raised:
            LegacyAPI.from_env({"RONNIA_LEGACY_MODE": "true"})
        self.assertIn("RONNIA_API_URL", str(raised.exception))
        self.assertIn("RONNIA_INTERNAL_TOKEN", str(raised.exception))

    def test_incomplete_bridge_env_fails_loudly(self) -> None:
        with self.assertRaises(RuntimeError):
            LegacyAPI.from_env({"RONNIA_API_URL": "http://127.0.0.1:4343"})
        with self.assertRaises(RuntimeError):
            LegacyAPI.from_env({"RONNIA_INTERNAL_TOKEN": TEST_TOKEN})

    def test_both_settings_enable_bridge(self) -> None:
        api = LegacyAPI.from_env(
            {
                "RONNIA_API_URL": "http://127.0.0.1:4343/",
                "RONNIA_INTERNAL_TOKEN": TEST_TOKEN,
            }
        )
        self.assertIsNotNone(api)
        self.assertEqual(api.base_url, "http://127.0.0.1:4343")
        self.assertNotIn(TEST_TOKEN, repr(api))


class ConnectRetryTestCase(unittest.IsolatedAsyncioTestCase):
    """A container swap leaves a stale pooled address behind; one retry heals it."""

    def make_api(self) -> LegacyAPI:
        return LegacyAPI(
            base_url="http://127.0.0.1:1",
            token=TEST_TOKEN,
        )

    async def test_connection_refusal_is_retried_once_on_a_fresh_connection(self) -> None:
        api = self.make_api()
        calls: list[int] = []

        async def flaky(session, method, url, path, payload, params, headers):
            calls.append(1)
            if len(calls) == 1:
                raise ConnectionRefusedError(111, "Connection refused")
            return 200, {"ok": True}

        api._send = flaky
        status, body = await api._request("POST", "/internal/legacy/claim", {"a": 1})
        await api.close()

        self.assertEqual((status, body), (200, {"ok": True}))
        self.assertEqual(len(calls), 2, "one retry after a refused connect")

    async def test_persistent_refusal_surfaces_after_the_attempt_budget(self) -> None:
        api = self.make_api()
        calls: list[int] = []

        async def dead(session, method, url, path, payload, params, headers):
            calls.append(1)
            raise ConnectionRefusedError(111, "Connection refused")

        api._send = dead
        with self.assertRaises(LegacyAPIError):
            await api._request("GET", "/internal/legacy/channels")
        await api.close()

        self.assertEqual(len(calls), 2, "the budget is two attempts, not an endless loop")

    async def test_errors_after_connect_are_not_retried(self) -> None:
        api = self.make_api()
        calls: list[int] = []

        async def dropped(session, method, url, path, payload, params, headers):
            calls.append(1)
            raise aiohttp.ServerDisconnectedError()

        api._send = dropped
        with self.assertRaises(LegacyAPIError):
            await api._request("POST", "/internal/legacy/claim", {"a": 1})
        await api.close()

        self.assertEqual(len(calls), 1, "a sent request must never be replayed")

