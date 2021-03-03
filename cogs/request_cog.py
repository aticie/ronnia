from twitchio.ext import commands
from bots.twitch_bot import TwitchBot

@commands.cog()
class ExampleCog:
    def __init__(self, bot: TwitchBot):
        self.bot = bot

    @commands.command(aliases=["disable"])
    async def autocog_test(self, ctx):
        self.bot.users_db
        await ctx.send("Disabled requests!")
