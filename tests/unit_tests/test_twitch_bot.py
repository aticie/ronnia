import os
import unittest
from unittest.mock import patch, MagicMock

from bots.twitch_bot import TwitchBot


def Any(cls):
    class Any(cls):
        def __eq__(self, other):
            return True

    return Any()


@unittest.mock.patch.dict(os.environ, {'BOT_NICK': 'test_user',
                                       'TMI_TOKEN': 'test_tmi_token',
                                       'CLIENT_ID': 'test_client_id',
                                       'CLIENT_SECRET': 'test_client_secret',
                                       'BOT_PREFIX': 'test_bot_prefix'
                                       })
class TestTwitchBot(unittest.TestCase):

    @patch('bots.twitch_bot.UserDatabase')
    @patch('bots.twitch_bot.IrcBot')
    def test_inform_user_on_updates_sends_irc_message_to_osu_username(self, mock_irc_bot, mock_user_db):
        twitch_bot = TwitchBot()

        mocked_send_msg = twitch_bot.irc_bot.send_message
        osu_username = 'test_osu_username'
        twitch_username = 'test_twitch_username'
        twitch_bot.inform_user_on_updates(osu_username=osu_username,
                                          twitch_username=twitch_username,
                                          is_updated=False)

        mocked_send_msg.assert_called_once_with(osu_username, Any(str))
        pass

    @patch('bots.twitch_bot.UserDatabase')
    @patch('bots.twitch_bot.IrcBot')
    def test_inform_user_on_updates_calls_db_set_channel_updated_with_twitch_username(self, mock_irc_bot, mock_user_db):
        twitch_bot = TwitchBot()

        mocked_set_channel_updated = twitch_bot.users_db.set_channel_updated
        osu_username = 'test_osu_username'
        twitch_username = 'test_twitch_username'
        twitch_bot.inform_user_on_updates(osu_username=osu_username,
                                          twitch_username=twitch_username,
                                          is_updated=False)

        mocked_set_channel_updated.assert_called_once_with(twitch_username)
        pass
