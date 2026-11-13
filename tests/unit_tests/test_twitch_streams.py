import unittest
from unittest.mock import patch

from ronnia.clients.twitch import OSU_GAME_IDS, TwitchAPI


class GetStreamsBatchTestCase(unittest.IsolatedAsyncioTestCase):
    async def test_requests_both_osu_categories(self):
        api = TwitchAPI("client-id", "client-secret")
        previous_token = api.access_token
        api.access_token = "token"
        self.addCleanup(setattr, api, "access_token", previous_token)
        requested = {}

        async def fake_make_request(method, url, **kwargs):
            requested["method"] = method
            requested["url"] = url
            return {"data": []}

        with patch.object(api, "_make_request", fake_make_request):
            result = await api.get_streams_batch([10, 20])

        self.assertEqual({"data": []}, result)
        self.assertEqual("GET", requested["method"])
        self.assertIn("game_id=21465", requested["url"])
        self.assertIn("game_id=295941293", requested["url"])
        self.assertIn("user_id=10", requested["url"])
        self.assertIn("user_id=20", requested["url"])

    def test_osu_game_ids_cover_osu_and_lazer(self):
        self.assertEqual((21465, 295941293), OSU_GAME_IDS)
