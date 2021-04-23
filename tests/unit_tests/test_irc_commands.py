import unittest
from unittest.mock import patch, MagicMock

from bots.irc_bot import IrcBot


class TestIrcBot(unittest.TestCase):

    @classmethod
    def setUpClass(cls) -> None:
        with patch('bots.irc_bot.UserDatabase') as mock:
            cls.event = MagicMock()

            cls.bot = IrcBot(channel='test', nickname='test', server='test')
            cls.bot.send_message = MagicMock()
            cls.bot.users_db = MagicMock()
        return

    def test_disable_requests_on_channel_calls_db_disable_channel(self):
        self.bot.users_db.disable_channel = MagicMock()
        self.bot.disable_requests_on_channel(self.event, user_details={'twitch_username': 'test_twitch_username',
                                                                       'osu_username': 'test_osu_username'})

        self.bot.users_db.disable_channel.assert_called_once_with('test_twitch_username')

    def test_enable_requests_on_channel_calls_db_enable_channel(self):
        self.bot.users_db.enable_channel = MagicMock()
        self.bot.enable_requests_on_channel(self.event, user_details={'twitch_username': 'test_twitch_username',
                                                                      'osu_username': 'test_osu_username'})
        self.bot.users_db.enable_channel.assert_called_once_with('test_twitch_username')

    def test_toggle_notifications_on_channel_calls_db_toggle_echo(self):
        self.bot.users_db.toggle_echo = MagicMock()
        self.bot.toggle_notifications(self.event, user_details={'twitch_username': 'test_twitch_username',
                                                                'osu_username': 'test_osu_username'})
        self.bot.users_db.toggle_echo.assert_called_once_with('test_twitch_username')

    def test_set_sr_rating_calls_db_set_sr_rating(self):
        sr_text = '3.5-5.5'
        expected_call_kwargs = {'range_low': 3.5, 'range_high': 5.5}
        self.bot.users_db.set_sr_rating = MagicMock(return_value=(3.5, 5.5))
        self.bot.set_sr_rating(self.event, sr_text, user_details={'twitch_username': 'test_twitch_username',
                                                                  'osu_username': 'test_osu_username'})
        self.bot.users_db.set_sr_rating.assert_called_once_with(twitch_username='test_twitch_username',
                                                                **expected_call_kwargs)
