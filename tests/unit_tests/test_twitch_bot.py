import asyncio
import datetime
import os
import unittest
from unittest.mock import patch, MagicMock, AsyncMock

from ronnia.bots.twitch_bot import TwitchBot


def Any(cls):
    class Any(cls):
        def __eq__(self, other):
            return True

    return Any()


class TestTwitchBot(unittest.IsolatedAsyncioTestCase):
    bot = None

    @classmethod
    @patch('bots.twitch_bot.StatisticsDatabase')
    @patch('bots.twitch_bot.UserDatabase')
    @patch('bots.twitch_bot.IrcBot')
    @patch.dict(os.environ, {'BOT_NICK': 'test_user',
                             'TMI_TOKEN': 'test_tmi_token',
                             'CLIENT_ID': 'test_client_id',
                             'CLIENT_SECRET': 'test_client_secret',
                             'BOT_PREFIX': 'test_bot_prefix',
                             'DB_DIR': 'test_db_dir'
                             })
    async def asyncSetUp(self, mock_open, mock_irc_bot, mock_user_db) -> None:
        self.bot = TwitchBot()
        self.bot.initial_channel_ids = ['test_id_1', 'test_id_2', 'test_id_3']
        self.bot._prefix = '!'
        await self.bot.event_ready()

    async def asyncTearDown(self) -> None:
        await self.bot.close()

    @patch('builtins.open')
    @patch('bots.twitch_bot.os')
    async def test_inform_user_on_updates_sends_irc_message_to_osu_username(self, mock_open, mock_os):
        mocked_send_msg = self.bot.irc_bot.send_message
        osu_username = 'test_osu_username'
        twitch_username = 'test_twitch_username'
        self.bot.inform_user_on_updates(osu_username=osu_username,
                                        twitch_username=twitch_username,
                                        is_updated=False)
        mocked_send_msg.assert_called_once_with(osu_username, Any(str))
        pass

    @patch('builtins.open')
    @patch('bots.twitch_bot.os')
    async def test_inform_user_on_updates_calls_db_set_channel_updated_with_twitch_username(self, mock_open, mock_os):
        mocked_set_channel_updated = self.bot.users_db.set_channel_updated
        osu_username = 'test_osu_username'
        twitch_username = 'test_twitch_username'
        self.bot.inform_user_on_updates(osu_username=osu_username,
                                        twitch_username=twitch_username,
                                        is_updated=False)

        mocked_set_channel_updated.assert_called_once_with(twitch_username)
        pass

    async def test_handle_request_calls_check_message_contains_beatmap_link(self):
        self.bot._check_message_contains_beatmap_link = MagicMock()
        self.bot._check_message_contains_beatmap_link.return_value = (None, None)

        msg = MagicMock()

        await self.bot.handle_request(msg)
        self.bot._check_message_contains_beatmap_link.assert_called_once()

    async def test_handle_request_calls_check_user_cooldown(self):
        self.bot._check_message_contains_beatmap_link = MagicMock(return_value=(0, 'test_beatmap_id'))

        get_beatmap_info_return_value = asyncio.Future()
        get_beatmap_info_return_value.set_result(None)
        self.bot.osu_api.get_beatmap_info = MagicMock(return_value=get_beatmap_info_return_value)

        self.bot._check_user_cooldown = MagicMock()

        msg = MagicMock()

        await self.bot.handle_request(msg)
        self.bot._check_user_cooldown.assert_called_once()

    async def test_handle_request_calls_check_request_criteria(self):
        check_request_criteria_return_value = asyncio.Future()
        check_request_criteria_return_value.set_result(None)
        self.bot.check_request_criteria = MagicMock(return_value=check_request_criteria_return_value)

        send_irc_message_return_value = asyncio.Future()
        send_irc_message_return_value.set_result(None)
        self.bot._send_beatmap_to_irc = MagicMock(return_value=send_irc_message_return_value)

        self.bot._check_message_contains_beatmap_link = MagicMock(return_value=(0, 'test_beatmap_id'))

        get_beatmap_info_return_value = asyncio.Future()
        get_beatmap_info_return_value.set_result(MagicMock())
        self.bot.osu_api.get_beatmap_info = MagicMock(return_value=get_beatmap_info_return_value)

        self.bot.users_db.get_echo_status = MagicMock(return_value=False)

        self.bot._check_user_cooldown = MagicMock()

        msg = MagicMock()

        await self.bot.handle_request(msg)
        self.bot.check_request_criteria.assert_called_once()

    async def test_handle_request_calls_send_twitch_message_when_echo_enabled(self):
        check_request_criteria_return_value = asyncio.Future()
        check_request_criteria_return_value.set_result(None)
        self.bot._check_request_criteria = MagicMock(return_value=check_request_criteria_return_value)
        self.bot._check_message_contains_beatmap_link = MagicMock(return_value=(0, 'test_beatmap_id'))

        get_beatmap_info_return_value = asyncio.Future()
        get_beatmap_info_return_value.set_result(MagicMock())
        self.bot.osu_api.get_beatmap_info = MagicMock(return_value=get_beatmap_info_return_value)

        self.bot.users_db.get_echo_status = MagicMock(return_value=True)

        send_twitch_message_return_value = asyncio.Future()
        send_twitch_message_return_value.set_result(None)
        self.bot._send_twitch_message = MagicMock(return_value=send_twitch_message_return_value)

        send_irc_message_return_value = asyncio.Future()
        send_irc_message_return_value.set_result(None)
        self.bot._send_beatmap_to_irc = MagicMock(return_value=send_irc_message_return_value)

        self.bot._check_user_cooldown = MagicMock()

        msg = MagicMock()

        await self.bot.handle_request(msg)
        self.bot._send_twitch_message.assert_called_once()

    async def test_handle_request_adds_request_to_statistics_db(self):
        check_request_criteria_return_value = asyncio.Future()
        check_request_criteria_return_value.set_result(None)
        self.bot._check_request_criteria = MagicMock(return_value=check_request_criteria_return_value)
        self.bot._check_message_contains_beatmap_link = MagicMock(return_value=(0, 'test_beatmap_id'))

        get_beatmap_info_return_value = asyncio.Future()
        get_beatmap_info_return_value.set_result(MagicMock())
        self.bot.osu_api.get_beatmap_info = MagicMock(return_value=get_beatmap_info_return_value)

        self.bot.users_db.get_echo_status = MagicMock(return_value=True)

        send_twitch_message_return_value = asyncio.Future()
        send_twitch_message_return_value.set_result(None)
        self.bot._send_twitch_message = MagicMock(return_value=send_twitch_message_return_value)

        send_irc_message_return_value = asyncio.Future()
        send_irc_message_return_value.set_result(None)
        self.bot._send_beatmap_to_irc = MagicMock(return_value=send_irc_message_return_value)

        self.bot._check_user_cooldown = MagicMock()

        msg = MagicMock()

        await self.bot.handle_request(msg)
        self.bot.messages_db.add_request.assert_called_once()

    async def test__check_user_cooldown_calls__prune_cooldowns(self):
        author = MagicMock()
        self.bot.user_last_request = MagicMock()
        self.bot._prune_cooldowns = MagicMock()

        self.bot._check_user_cooldown(author)
        self.bot._prune_cooldowns.assert_called_once()

    async def test__check_user_cooldown_adds_user_to_dict_when_not_on_cooldown(self):
        author = MagicMock()
        author.id = 'test_id'
        self.bot.user_last_request = {}
        self.bot._prune_cooldowns = MagicMock()

        self.bot._check_user_cooldown(author)
        self.assertIn('test_id', self.bot.user_last_request)

    async def test__check_user_cooldown_raises_assertion_error_if_a_user_is_on_cooldown(self):
        author = MagicMock()
        author.id = 'test_id'
        self.bot.user_last_request = {'test_id': datetime.datetime.now()}
        self.bot._prune_cooldowns = MagicMock()

        with self.assertRaises(AssertionError):
            self.bot._check_user_cooldown(author)

    async def test__check_user_cooldown_updates_user_cooldown_if_not_on_cooldown(self):
        author = MagicMock()
        author.id = 'test_id'
        user_last_request_time = datetime.datetime.now() - datetime.timedelta(seconds=self.bot.PER_REQUEST_COOLDOWN)
        self.bot.user_last_request = {
            'test_id': user_last_request_time}
        self.bot._prune_cooldowns = MagicMock()

        self.bot._check_user_cooldown(author)
        self.assertNotEqual(user_last_request_time, self.bot.user_last_request['test_id'])

    async def test__prune_cooldowns_removes_user_not_on_cooldown(self):
        user_last_request_time = datetime.datetime.now() - datetime.timedelta(seconds=self.bot.PER_REQUEST_COOLDOWN)
        self.bot.user_last_request = {
            'test_id': user_last_request_time}

        self.bot._prune_cooldowns(datetime.datetime.now())
        self.assertNotIn('test_id', self.bot.user_last_request)

    async def test__prune_cooldowns_keeps_user_on_cooldown_in_list(self):
        user_last_request_time = datetime.datetime.now()
        self.bot.user_last_request = {
            'test_id': user_last_request_time}

        self.bot._prune_cooldowns(datetime.datetime.now())
        self.assertIn('test_id', self.bot.user_last_request)

    async def test__send_twitch_message_sends_message_to_context(self):
        msg = MagicMock()
        beatmap_info = MagicMock()

        msg_send_return_value = asyncio.Future()
        msg_send_return_value.set_result(None)
        msg.channel.send = MagicMock(return_value=msg_send_return_value)

        await self.bot._send_twitch_message(msg, beatmap_info)
        msg.channel.send.assert_called_once()

    async def test__send_irc_message_calls_irc_bot_send_message(self):
        msg = MagicMock()
        beatmap_info = MagicMock()
        mods = MagicMock()

        msg_send_return_value = asyncio.Future()
        msg_send_return_value.set_result(None)
        msg.channel.send = MagicMock(return_value=msg_send_return_value)

        _prepare_irc_message_return_value = asyncio.Future()
        _prepare_irc_message_return_value.set_result(None)
        self.bot._prepare_irc_message = MagicMock(return_value=_prepare_irc_message_return_value)

        await self.bot._send_beatmap_to_irc(msg, beatmap_info, mods)
        self.bot.irc_bot.send_message.assert_called_once()

    async def test__send_irc_message_calls__prepare_irc_message(self):
        msg = MagicMock()
        beatmap_info = MagicMock()
        mods = MagicMock()

        msg_send_return_value = asyncio.Future()
        msg_send_return_value.set_result(None)
        msg.channel.send = AsyncMock(return_value=msg_send_return_value)

        _prepare_irc_message_return_value = asyncio.Future()
        _prepare_irc_message_return_value.set_result(None)
        self.bot._prepare_irc_message = AsyncMock(return_value=_prepare_irc_message_return_value)

        await self.bot._send_beatmap_to_irc(msg, beatmap_info, mods)
        self.bot._prepare_irc_message.assert_called_once()

    async def test_event_ready_calls_fetch_users(self):
        join_channels_return_value = asyncio.Future()
        join_channels_return_value.set_result(None)
        self.bot.join_channels = MagicMock(return_value=join_channels_return_value)
        self.bot.load_module = MagicMock()
        self.bot.update_users = MagicMock()

        fetch_users_return_value = asyncio.Future()
        fetch_users_return_value.set_result([MagicMock()])
        self.bot.fetch_users = MagicMock(return_value=fetch_users_return_value)

        await self.bot.event_ready()
        self.bot.fetch_users.assert_called_once_with(ids=['test_id_1', 'test_id_2', 'test_id_3'])

    async def test_event_ready_calls_join_channels(self):
        join_channels_return_value = asyncio.Future()
        join_channels_return_value.set_result(None)
        self.bot.join_channels = MagicMock(return_value=join_channels_return_value)
        self.bot.load_module = MagicMock()
        self.bot.update_users = MagicMock()
        self.bot._http.nick = 'test_owner'

        fetch_users_return_value = asyncio.Future()

        test_user_1, test_user_2, test_user_3 = MagicMock(), MagicMock(), MagicMock()
        test_user_1.name, test_user_2.name, test_user_3.name = 'test_user_1', 'test_user_2', 'test_user_3'

        fetch_users_return_value.set_result([test_user_1,
                                             test_user_2,
                                             test_user_3])
        self.bot.fetch_users = MagicMock(return_value=fetch_users_return_value)

        await self.bot.event_ready()
        self.bot.join_channels.assert_called_once_with(['test_user_1', 'test_user_2', 'test_user_3', 'test_owner'])
        pass
