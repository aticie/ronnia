import asyncio
import time
from unittest import IsolatedAsyncioTestCase
from unittest.mock import MagicMock, patch, AsyncMock

from ronnia.helpers.osu_api_helper import OsuApi


class AsyncContextManagerMock(MagicMock):

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)

        for key in ('aenter_return', 'aexit_return'):
            setattr(self, key, kwargs[key] if key in kwargs else MagicMock())

    async def __aenter__(self):
        return self.aenter_return

    async def __aexit__(self, *args):
        return self.aexit_return


class TestOsuApi(IsolatedAsyncioTestCase):
    api = None
    mock_user_response = None
    mock_beatmap_response = None

    @classmethod
    def setUpClass(cls) -> None:
        statistics_db = AsyncMock()
        cls.api = OsuApi(statistics_db)
        cls.api._cooldown_seconds = 0.1

        cls.mock_user_response = [
            {'user_id': '5642779', 'username': 'heyronii', 'join_date': '2015-01-16 22:00:13', 'count300': '32207606',
             'count100': '2921528', 'count50': '399173', 'playcount': '147182', 'ranked_score': '49687237487',
             'total_score': '319948947050', 'pp_rank': '441', 'level': '102.93', 'pp_raw': '11881.3',
             'accuracy': '98.11122131347656', 'count_rank_ss': '25', 'count_rank_ssh': '21', 'count_rank_s': '679',
             'count_rank_sh': '471', 'count_rank_a': '2346', 'country': 'TR', 'total_seconds_played': '7884189',
             'pp_country_rank': '4',
             'events': [{'display_html': '<div class="beatmapset-event beatmapset-event--ranked">Ranked</div>',
                         'beatmapset_id': '1622894', 'beatmap_id': '3311346', 'date': '2020-01-10 00:47:06',
                         'epicfactor': '0', 'mode': '0', 'rank': 'A', 'score': '319948947050', 'user_id': '5642779',
                         'mods': '0', 'completed': '1', 'date_string': '2020-01-10 00:47:06'}],
             }]
        cls.mock_beatmap_response = [{'beatmapset_id': '1621894', 'beatmap_id': '3311346', 'approved': '1',
                                      'total_length': '102',
                                      'hit_length': '100', 'version': 'Flower',
                                      'file_md5': '3f1d0d6b11f30b9e628fff23bc9074fd',
                                      'diff_size': '4.5',
                                      'diff_overall': '9.5', 'diff_approach': '9.3', 'diff_drain': '6', 'mode': '0',
                                      'count_normal': '564',
                                      'count_slider': '157', 'count_spinner': '0', 'submit_date': '2021-11-07 20:35:55',
                                      'approved_date': '2022-01-10 00:47:06', 'last_update': '2021-12-31 11:21:28',
                                      'artist': 'TOMOSUKE',
                                      'artist_unicode': 'TOMOSUKE', 'title': 'Macuilxochitl',
                                      'title_unicode': 'Macuilxochitl',
                                      'creator': 'Raijodo',
                                      'creator_id': '13400075', 'bpm': '148', 'source': 'jubeat ripples',
                                      'tags': "fanzhen0019 macuilxochitl",
                                      'genre_id': '14', 'language_id': '5', 'favourite_count': '47',
                                      'rating': '9.86667',
                                      'storyboard': '0',
                                      'video': '0', 'download_unavailable': '0', 'audio_unavailable': '0',
                                      'playcount': '224',
                                      'passcount': '47',
                                      'packs': None, 'max_combo': '887', 'diff_aim': '4.01978', 'diff_speed': '3.26121',
                                      'difficultyrating': '7.65575'}]

    async def test_get_beatmap_info_returns_beatmap_dict(self):
        beatmap_id = '3311346'
        required_beatmap_info = {'beatmap_id': '3311346',
                                 'beatmapset_id': '1621894'}

        self.api._get_endpoint = AsyncMock(return_value=self.mock_beatmap_response)
        beatmap_info = await self.api.get_beatmap_info({'b': beatmap_id})
        self.assertIsInstance(beatmap_info, dict)

        # Assert dict contains subset
        self.assertEqual(beatmap_info, beatmap_info | required_beatmap_info)

    async def test_get_beatmap_info_returns_none_if_beatmap_id_not_found(self):
        beatmap_id = '12345'
        self.api._get_endpoint = AsyncMock(return_value=[])
        beatmap_info = await self.api.get_beatmap_info({'b': beatmap_id})
        self.assertIsNone(beatmap_info)

    async def test_get_user_info_accepts_string_input(self):
        self.api._get_endpoint = AsyncMock(return_value=self.mock_user_response)
        await self.api.get_user_info('heyronii')
        self.api._get_endpoint.assert_called_once_with({'u': 'heyronii',
                                                        'k': self.api._osu_api_key,
                                                        'type': 'string'}, 'get_user')

    async def test_get_user_info_accepts_integer_input(self):
        self.api._get_endpoint = AsyncMock(return_value=self.mock_user_response)
        await self.api.get_user_info(5642779)
        self.api._get_endpoint.assert_called_once_with({'u': 5642779,
                                                   'k': self.api._osu_api_key,
                                                   'type': 'id'}, 'get_user')

    async def test_get_user_info_returns_user_dict(self):
        self.api._get_endpoint = AsyncMock(return_value=self.mock_user_response)
        user_info = await self.api.get_user_info(5642779)
        self.assertIsInstance(user_info, dict)

    async def test_get_user_info_returns_none_if_user_not_found(self):
        self.api._get_endpoint = AsyncMock(return_value=[])
        user_info = await self.api.get_user_info(4)
        self.assertIsNone(user_info)

    @patch('helpers.osu_api_helper.aiohttp.ClientSession')
    async def test__get_endpoint_waits_for_rate_limit(self, mock_session: AsyncMock):
        mock_session_object = AsyncContextManagerMock()
        mock_session.return_value.__aenter__.return_value = mock_session_object
        mock_session_object.get.return_value = AsyncContextManagerMock(side_effect=[""])
        start_time = time.time()
        await self.api._get_endpoint({}, '')
        await self.api._get_endpoint({}, '')
        end_time = time.time()

        self.assertGreater(end_time - start_time, 0.1)

    @patch('helpers.osu_api_helper.aiohttp.ClientSession')
    async def test__get_endpoint_returns_none_when_timeout_error_occurs(self, mock_session: AsyncMock):
        mock_session_object = AsyncContextManagerMock()
        mock_get_result = AsyncContextManagerMock()
        mock_session.return_value.__aenter__.return_value = mock_session_object
        mock_session_object.get.return_value.__aenter__.return_value = mock_get_result
        mock_get_result.json = AsyncMock(side_effect=asyncio.TimeoutError())
        info = await self.api._get_endpoint({}, '')
        self.assertIsNone(info)
