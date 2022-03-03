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

    @commands.command(name="adduser")
    async def add_user_to_db(self, ctx: Context, *args):
        twitch_username = args[0].lower()
        osu_username = args[1].lower()

        osu_user_info, twitch_user_info = await self.bot.get_osu_and_twitch_details(osu_user_id_or_name=osu_username,
                                                                                    twitch_username=twitch_username)

        twitch_id = twitch_user_info[0].id
        osu_user_id = osu_user_info['user_id']
        await self.bot.users_db.add_user(osu_username=osu_username, twitch_username=twitch_username,
                                         twitch_id=twitch_id, osu_user_id=osu_user_id)
        await self.bot.join_channels([twitch_username])
        logger.info(f'Adding {twitch_username} - {osu_username} to user database!')
        await ctx.send(f'Added {twitch_username} -> {osu_username}.')

    @commands.command(name="test")
    async def toggle_test_for_user(self, ctx: Context, *args):
        twitch_username = args[0].lower()
        new_value = self.bot.users_db.toggle_setting('test', twitch_username)
        await ctx.send(f'Setting test to {new_value} for {twitch_username}.')
        logger.info(f'Setting test to {new_value} for {twitch_username}.')


def prepare(bot: TwitchBot):
    # Load our cog with this module...
    bot.add_cog(AdminCog(bot))
