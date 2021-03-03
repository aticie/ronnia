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
        osu_username = args[1]

        self.bot.users_db.add_user(osu_username=osu_username, twitch_username=twitch_username)
        await self.bot.join_channels([twitch_username])
        logger.info(f'Adding {twitch_username} - {osu_username} to user database!')

    @commands.command(name="rmuser")
    async def remove_user_from_db(self, ctx: Context, *args):
        if ctx.author.name != os.getenv('BOT_NICK'):
            return

        twitch_username = args[0]

        self.bot.users_db.remove_user(twitch_username=twitch_username)
        await self.bot.part_channels([twitch_username])
