import logging
import os

from twitchio.ext import commands
from twitchio.ext.commands import Context

from ronnia.bots.twitch_bot import TwitchBot

logger = logging.getLogger('ronnia')


class AdminCog(commands.Cog):

    def __init__(self, bot: TwitchBot):
        self.bot = bot

    async def cog_check(self, ctx):
        if ctx.author.name != os.getenv('BOT_NICK'):
            return False
        return True

    @commands.command(name="test")
    async def toggle_test_for_user(self, ctx: Context, *args):
        twitch_username = args[0].lower()
        new_value = self.bot.users_db.toggle_setting('test', twitch_username)
        await ctx.send(f'Setting test to {new_value} for {twitch_username}.')
        logger.info(f'Setting test to {new_value} for {twitch_username}.')


def prepare(bot: TwitchBot):
    # Load our cog with this module...
    bot.add_cog(AdminCog(bot))
