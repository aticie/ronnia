import unittest
from asyncio import Future
from unittest.mock import MagicMock, call, AsyncMock


class TestRequestCog(unittest.IsolatedAsyncioTestCase):
    ctx = None
    bot = None

    @classmethod
    def asyncSetUp(cls) -> None:
        cls.bot = MagicMock()

        cls.ctx = MagicMock()
        cls.ctx.author.name = 'test_user'
        cls.ctx.send = AsyncMock()
        cls.ctx.send.return_value = Future()
        cls.ctx.send.return_value.set_result('')
        cls.bot.users_db.disable_channel = AsyncMock()
        cls.cog = RequestCog(cls.bot)

    async def test_disable_channel_calls_db_disable(self):
        await self.cog.disable_channel._callback(self.cog, self.ctx)
        self.bot.users_db.disable_channel.assert_called_once_with('test_user')

    async def test_enable_channel_calls_db_enable(self):
        self.bot.users_db.enable_channel = AsyncMock()

        cog = RequestCog(self.bot)

        await cog.enable_channel._callback(cog, self.ctx)

        self.bot.users_db.enable_channel.assert_called_once_with('test_user')

    async def test_toggle_feedback_calls_db_toggle_echo(self):
        self.bot.users_db.toggle_echo = AsyncMock()
        self.bot.users_db.toggle_echo.return_value = False

        cog = RequestCog(self.bot)

        await cog.toggle_feedback._callback(cog, self.ctx)

        self.bot.users_db.toggle_echo.assert_called_once_with('test_user')

        self.bot.users_db.toggle_echo.return_value = True
        await cog.toggle_feedback._callback(cog, self.ctx)

        self.bot.users_db.toggle_echo.assert_has_calls([call('test_user'), call('test_user')])

    async def test_sub_only_calls_db_toggle_sub_only(self):
        self.bot.users_db.toggle_sub_only = AsyncMock()
        self.bot.users_db.toggle_sub_only.return_value = False

        cog = RequestCog(self.bot)

        await cog.sub_only._callback(cog, self.ctx)

        self.bot.users_db.toggle_sub_only.assert_called_once_with('test_user')

        self.bot.users_db.toggle_sub_only.return_value = True
        await cog.sub_only._callback(cog, self.ctx)

        self.bot.users_db.toggle_sub_only.assert_has_calls([call('test_user'), call('test_user')])

    async def test_set_sr_rating_parses_float_str_correctly(self):
        self.bot.users_db.set_sr_rating = MagicMock(return_value=(1, 1))
        test_sr_text = '5.5-6.5'
        cog = RequestCog(self.bot)

        await cog.set_sr_rating._callback(cog, self.ctx, test_sr_text)

        self.bot.users_db.set_sr_rating.assert_called_once_with(twitch_username='test_user', range_low=5.5,
                                                                range_high=6.5)

    async def test_set_sr_rating_sends_assertion_error_to_channel_when_max_less_than_min(self):
        error = AssertionError('Max value cannot be lower than min value.')
        self.bot.users_db.set_sr_rating.side_effect = error
        test_sr_text = '6.5-5.5'
        cog = RequestCog(self.bot)

        await cog.set_sr_rating._callback(cog, self.ctx, test_sr_text)

        self.ctx.send.assert_has_calls([call(error)])

    async def test_set_sr_rating_sends_value_error_when_parse_unsuccessful(self):
        test_sr_text = '5,5-6,5'
        cog = RequestCog(self.bot)

        await cog.set_sr_rating._callback(cog, self.ctx, test_sr_text)

        self.ctx.send.assert_has_calls([call('Invalid input.. For example, use: !sr 3.5-7.5')])
