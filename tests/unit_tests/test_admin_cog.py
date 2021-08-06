import asyncio
import os
import unittest
from asyncio import Future
from unittest.mock import MagicMock

from cogs.admin_cog import AdminCog


@unittest.mock.patch.dict(os.environ, {'BOT_NICK': 'test_user'})
class TestAdminCog(unittest.TestCase):

    @classmethod
    def setUpClass(cls) -> None:
        cls.bot = MagicMock()

        cls.ctx = MagicMock()
        cls.ctx.author.name = 'test_user'
        cls.ctx.send.return_value = Future()
        cls.ctx.send.return_value.set_result('')
        cls.loop = asyncio.get_event_loop()

    def test_add_user_to_db_passes_arguments_lowercase_to_get_osu_and_twitch_details(self):
        self.bot.get_osu_and_twitch_details = MagicMock(return_value=Future())
        self.bot.get_osu_and_twitch_details.return_value.set_result((MagicMock(), MagicMock()))
        self.bot.join_channels = MagicMock(return_value=Future())
        self.bot.join_channels.return_value.set_result('')

        cog = AdminCog(self.bot)

        test_twitch_username = 'TEST_Twitch_Username'
        test_osu_username = 'TEST_osU_Username'

        self.loop.run_until_complete(
            cog.add_user_to_db._callback(cog, self.ctx, test_twitch_username, test_osu_username))

        self.bot.get_osu_and_twitch_details.assert_called_once_with(osu_user_id_or_name='test_osu_username',
                                                                    twitch_username='test_twitch_username')

    def test_remove_user_from_db_calls_db_remove_user(self):
        self.bot.part_channel = MagicMock(return_value=Future())
        self.bot.part_channel.return_value.set_result('')

        cog = AdminCog(self.bot)

        test_twitch_username = 'TEST_Twitch_Username'

        self.loop.run_until_complete(
            cog.remove_user_from_db._callback(cog, self.ctx, test_twitch_username))

        self.bot.users_db.remove_user.assert_called_once_with(twitch_username='test_twitch_username')

    def test_toggle_test_for_user_calls_db_toggle_setting(self):
        self.bot.part_channels = MagicMock(return_value=Future())
        self.bot.part_channels.return_value.set_result('')

        cog = AdminCog(self.bot)

        test_twitch_username = 'TEST_Twitch_Username'

        self.loop.run_until_complete(
            cog.toggle_test_for_user._callback(cog, self.ctx, test_twitch_username))

        self.bot.users_db.toggle_setting.assert_called_once_with('test', 'test_twitch_username')
