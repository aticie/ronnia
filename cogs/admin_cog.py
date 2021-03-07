import os
import logging

from twitchio.ext import commands
from twitchio import Context
from bots.twitch_bot import TwitchBot

logger = logging.getLogger('ronnia')


@commands.cog()
class AdminCog:

    def __init__(self, bot: TwitchBot):
        self.bot = bot

    @commands.command(name="adduser")
    async def add_user_to_db(self, ctx: Context, *args):
        if ctx.author.name != os.getenv('BOT_NICK'):
            return

        twitch_username = args[0]
        osu_username = args[1].lower()

        self.bot.users_db.add_user(osu_username=osu_username, twitch_username=twitch_username)
        self.bot.channel_mappings[twitch_username] = osu_username
        await self.bot.join_channels([twitch_username])
        logger.info(f'Adding {twitch_username} - {osu_username} to user database!')
        await ctx.send(f'Added {twitch_username} -> {osu_username}.')

    @commands.command(name="rmuser")
    async def remove_user_from_db(self, ctx: Context, *args):
        if ctx.author.name != os.getenv('BOT_NICK'):
            return

        twitch_username = args[0]

        self.bot.users_db.remove_user(twitch_username=twitch_username)
        self.bot.channel_mappings.pop(twitch_username)
        await self.bot.part_channels([twitch_username])
        await ctx.send(f'Removed {twitch_username}.')
        logger.info(f'Removed {twitch_username}!')

    @commands.command(name="test")
    async def toggle_test_for_user(self, ctx: Context, *args):
        if ctx.author.name != os.getenv('BOT_NICK'):
            return

        twitch_username = args[0]
        new_value = self.bot.users_db.toggle_setting('test', twitch_username)
        await ctx.send(f'Setting test to {new_value} for {twitch_username}.')
        logger.info(f'Setting test to {new_value} for {twitch_username}.')
