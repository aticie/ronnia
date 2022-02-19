import attr
from twitchio.ext import commands

from bots.twitch_bot import TwitchBot


@attr.s
class RangeInput(object):
    range_low = attr.ib(converter=float)
    range_high = attr.ib(converter=float)


class RequestCog(commands.Cog):
    def __init__(self, bot: TwitchBot):
        self.bot = bot

    @commands.command(name="disable")
    async def disable_channel(self, ctx):
        await self.bot.users_db.disable_channel(ctx.author.name)
        await self.bot.messages_db.add_command('disable', 'twitch', ctx.author.name)
        await ctx.send(f"Disabled requests! If you want to enable requests, type {self.bot.main_prefix}enable.")

    @commands.command(name="enable")
    async def enable_channel(self, ctx):
        await self.bot.users_db.enable_channel(ctx.author.name)
        await self.bot.messages_db.add_command('enable', 'twitch', ctx.author.name)
        await ctx.send(f"Enabled requests! If you want to disable requests, type {self.bot.main_prefix}disable.")

    @commands.command(name="echo", aliases=["feedback"])
    async def toggle_feedback(self, ctx):
        new_echo_status = await self.bot.users_db.toggle_echo(ctx.author.name)
        if new_echo_status is False:
            await ctx.send("Disabled feedback message after requests!")
        else:
            await ctx.send("Enabled feedback message after requests!")

        await self.bot.messages_db.add_command('echo', 'twitch', ctx.author.name)

    @commands.command(name="help", aliases=["h"])
    async def show_help_message(self, ctx):
        await ctx.send(f'Check out the project page for more information. https://github.com/aticie/ronnia')

    @commands.command(name="sub-only")
    async def sub_only(self, ctx):
        new_value = await self.bot.users_db.toggle_sub_only(ctx.author.name)
        if new_value:
            await ctx.send(
                f"Enabled sub-only mode on the channel! Type {self.bot.main_prefix}sub-only again to disable.")
        else:
            await ctx.send(
                f"Disabled sub-only mode on the channel. Type {self.bot.main_prefix}sub-only again to enable.")

    @commands.command(name='setsr')
    async def set_sr_rating(self, ctx, sr_text: str):
        try:
            range_input = RangeInput(*(sr_text.split('-')))
        except ValueError as e:
            await ctx.send('Invalid input.. For example, use: !sr 3.5-7.5')
            return

        try:
            new_low, new_high = self.bot.users_db.set_sr_rating(twitch_username=ctx.author.name,
                                                                **attr.asdict(range_input))
        except AssertionError as e:
            await ctx.send(e)
            return

        await ctx.send(f'Changed star rating range between: {new_low:.1f} - {new_high:.1f}')
        await self.bot.messages_db.add_command('sr_rating', 'twitch', ctx.author.name)


def prepare(bot: TwitchBot):
    # Load our cog with this module...
    bot.add_cog(RequestCog(bot))
