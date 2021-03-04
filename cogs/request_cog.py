from twitchio.ext import commands
from bots.twitch_bot import TwitchBot


@commands.cog()
class RequestCog:
    def __init__(self, bot: TwitchBot):
        self.bot = bot

    @commands.command(name="disable")
    async def disable_channel(self, ctx):
        self.bot.users_db.disable_channel(ctx.author.name)
        await ctx.send("Disabled requests! If you want to enable requests, type !enable.")

    @commands.command(name="enable")
    async def enable_channel(self, ctx):
        self.bot.users_db.enable_channel(ctx.author.name)
        await ctx.send("Enabled requests! If you want to disable requests, type !disable.")

    @commands.command(name="echo", aliases=["feedback"])
    async def toggle_feedback(self, ctx):
        new_echo_status = self.bot.users_db.toggle_echo(ctx.author.name)
        if new_echo_status is False:
            await ctx.send("Disabled feedback message after requests!")
        else:
            await ctx.send("Enabled feedback message after requests!")

    @commands.command(name="help", aliases=["h"])
    async def show_help_message(self, ctx):
        await ctx.send(f'Check out the project page for more information. https://github.com/aticie/ronnia')
