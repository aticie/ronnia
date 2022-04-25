import os
import unittest
from unittest.mock import MagicMock, AsyncMock

from ronnia.cogs.admin_cog import AdminCog


@unittest.mock.patch.dict(os.environ, {'BOT_NICK': 'test_user'})
class TestAdminCog(unittest.IsolatedAsyncioTestCase):

    @classmethod
    def setUpClass(cls) -> None:
        cls.bot = AsyncMock()

        cls.ctx = MagicMock()
        cls.ctx.author.name = 'test_user'
        cls.ctx.send = AsyncMock()
        cls.ctx.send.return_value.set_result('')

    async def test_add_user_to_db_passes_arguments_lowercase_to_get_osu_and_twitch_details(self):
        self.bot.get_osu_and_twitch_details = AsyncMock(return_value=(AsyncMock(), AsyncMock()))
        self.bot.join_channels = AsyncMock()

        cog = AdminCog(self.bot)

        test_twitch_username = 'TEST_Twitch_Username'
        test_osu_username = 'TEST_osU_Username'

        await cog.add_user_to_db._callback(cog, self.ctx, test_twitch_username, test_osu_username)

        self.bot.get_osu_and_twitch_details.assert_called_once_with(osu_user_id_or_name='test_osu_username',
                                                                    twitch_username='test_twitch_username')

    async def test_toggle_test_for_user_calls_db_toggle_setting(self):
        cog = AdminCog(self.bot)

        test_twitch_username = 'TEST_Twitch_Username'

        await cog.toggle_test_for_user._callback(cog, self.ctx, test_twitch_username)

        self.bot.users_db.toggle_setting.assert_called_once_with('test', 'test_twitch_username')
