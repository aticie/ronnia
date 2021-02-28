from twitchio.ext import commands


@commands.cog()
class ExampleCog:
    def __init__(self, bot):
        self.bot = bot

    @commands.command(aliases=["disable"])
    async def autocog_test(self, ctx):
        self.bot
        await ctx.send("Disabled requests!")
