import os
from threading import Thread
from abc import ABC
from typing import AnyStr, Tuple, Union
import logging
import datetime

from twitchio.ext import commands
from twitchio import Message, User, Channel

from helpers.beatmap_link_parser import parse_beatmap_link
from helpers.osu_api_helper import OsuApiHelper
from helpers.database_helper import UserDatabase
from bots.irc_bot import IrcBot

logger = logging.getLogger('ronnia')


class TwitchBot(commands.Bot, ABC):
    PER_REQUEST_COOLDOWN = 30  # each request has 30 seconds cooldown
    BEATMAP_STATUS_DICT = {"0": 'Pending',
                           "1": 'Ranked',
                           "2": 'Approved',
                           "3": 'Qualified',
                           "4": 'Loved',
                           "-1": 'WIP',
                           "-2": 'Graveyard'}

    def __init__(self):
        self.users_db = UserDatabase()
        self.users_db.initialize()

        self.channel_mappings = {user[2]: user[1] for user in self.users_db.get_all_users()}
        self.initial_channel_ids = [twitch for twitch, osu in self.channel_mappings.items()]

        args = {
            'irc_token': os.getenv('TMI_TOKEN'),
            'client_id': os.getenv('CLIENT_ID'),
            'client_secret': os.getenv('CLIENT_SECRET'),
            'nick': os.getenv('BOT_NICK'),
            'prefix': os.getenv('BOT_PREFIX')
        }
        super().__init__(**args)

        self.osu_api = OsuApiHelper()
        self.user_last_request = {}
        self.irc_bot = IrcBot("#osu", os.getenv('OSU_USERNAME'), "irc.ppy.sh", password=os.getenv("IRC_PASSWORD"))

        p = Thread(target=self.irc_bot.start)
        p.start()

    async def event_message(self, message: Message):
        await self.handle_commands(message)
        try:
            self.check_channel_enabled(message.channel.name)
            await self.handle_request(message)
        except AssertionError as e:
            logger.debug(f'Check unsuccessful: {e}')

    async def handle_request(self, message):
        logger.info(f"{message.channel} - {message.author.name}: {message.content}")
        given_mods, api_params = self._check_message_contains_beatmap_link(message)
        if given_mods is not None:
            self._check_user_cooldown(message.author)
            beatmap_info = await self.osu_api.get_beatmap_info(api_params)
            if beatmap_info:
                await self._update_channel(message)
                await self.check_request_criteria(message)
                if self.users_db.get_echo_status(twitch_username=message.channel.name):
                    await self._send_twitch_message(message, beatmap_info)
                await self._send_irc_message(message, beatmap_info, given_mods)

    async def check_request_criteria(self, message: Message):
        test_status = self.users_db.get_test_status(message.channel.name)
        self.check_if_author_is_broadcaster(message, test_status)
        await self.check_if_streaming_osu(message.channel, test_status)

    async def event_command_error(self, ctx, error):
        logger.error(error)
        pass

    async def _update_channel(self, message: Message):
        """
        Updates channel twitch and osu! usernames every day
        :param message: Message from twitch
        :return: None
        """
        # Get current channel details from db
        channel_details = self.users_db.get_user_from_twitch_username(twitch_username=message.channel.name)
        channel_last_updated = channel_details['updated_at']
        osu_user_id = channel_details['osu_id']
        twitch_id = channel_details['twitch_id']
        time_passed_since_last_update = datetime.datetime.now() - channel_last_updated
        # Check if user has been updated since yesterday
        if time_passed_since_last_update.days >= 1:
            osu_user_info, twitch_info = await self.get_osu_and_twitch_details(osu_user_id, twitch_id)
            try:
                new_osu_username = osu_user_info['username'].lower()
            except:
                new_osu_username = channel_details['osu_username']
            new_twitch_username = twitch_info[0].login

            # Update database with new information
            self.users_db.update_user(new_twitch_username=new_twitch_username, new_osu_username=new_osu_username,
                                      twitch_id=twitch_id, osu_user_id=osu_user_id)

        return

    async def get_osu_and_twitch_details(self, osu_user_id_or_name, twitch_id_or_name):

        if osu_user_id_or_name.isdigit():
            # Handle ids in the string form
            osu_user_id_or_name = int(osu_user_id_or_name)

        # Get osu! username from osu! api (usernames can change)
        osu_user_info = await self.osu_api.get_user_info(osu_user_id_or_name)
        # Get twitch username from twitch api
        twitch_info = await self.get_users(*[twitch_id_or_name])
        return osu_user_info, twitch_info

    @staticmethod
    def check_if_author_is_broadcaster(message: Message, test_status: bool = False):
        if test_status is True:
            return

        assert message.author.name != message.channel.name, 'Author is broadcaster and not in test mode.'

        return

    async def global_before_hook(self, ctx):
        """
        Global hook that runs before every command.
        :param ctx: Message context
        :return:
        """
        user = self.users_db.get_user_from_twitch_username(ctx.author.name)
        assert user is not None, 'User does not exist'
        assert ctx.message.channel.name == ctx.author.name, 'Message is not in author\'s channel'

    @staticmethod
    async def check_if_streaming_osu(channel: Channel, test_status: bool = False):
        """
        Checks if stream is on and they're playing osu!, otherwise ignores channel.
        :param channel: Channel of the message
        :param test_status: Flag for if account set for test.
        :return:
        """
        if test_status is True:
            return

        stream = await channel.get_stream()
        assert stream is not None, 'Stream is not on.'
        assert stream.get('game_name') == 'osu!', 'Stream is not playing osu!'

        return

    def check_channel_enabled(self, channel_name):
        enabled = self.users_db.get_enabled_status(twitch_username=channel_name)
        assert enabled, 'Channel not open for requests'

    def _check_user_cooldown(self, author: User):
        """
        Check if user is on cooldown, raise an exception if the user is on request cooldown.
        :param author: Twitch user object
        :return: Exception if user has requested a beatmap before TwitchBot.PER_REQUEST_COOLDOWN seconds passed.
        """
        author_id = author.id
        time_right_now = datetime.datetime.now()

        self._prune_cooldowns(time_right_now)

        if author_id not in self.user_last_request:
            self.user_last_request[author_id] = time_right_now
        else:
            last_message_time = self.user_last_request[author_id]
            assert (
                           time_right_now - last_message_time).total_seconds() > TwitchBot.PER_REQUEST_COOLDOWN, f'{author.name} is on cooldown.'
            self.user_last_request[author_id] = time_right_now

        return

    def _prune_cooldowns(self, time_right_now: datetime.datetime):
        """
        Prunes users on that are on cooldown list so it doesn't get too cluttered.
        :param time_right_now:
        :return:
        """
        pop_list = []
        for user_id, last_message_time in self.user_last_request.items():
            if (time_right_now - last_message_time).total_seconds() > TwitchBot.PER_REQUEST_COOLDOWN:
                pop_list.append(user_id)

        for user in pop_list:
            self.user_last_request.pop(user)

        return

    async def _send_irc_message(self, message: Message, beatmap_info: dict, given_mods: str):
        """
        Sends the beatmap request message to osu!irc bot
        :param message: Twitch Message object
        :param beatmap_info: Dictionary containing beatmap information from osu! api
        :param given_mods: String of mods if they are requested, empty string instead
        :return:
        """
        irc_message = await self._prepare_irc_message(message, beatmap_info, given_mods)

        irc_target_channel = self.users_db.get_user_from_twitch_username(message.channel.name)['osu_username']
        self.irc_bot.send_message(irc_target_channel, irc_message)
        return

    @staticmethod
    async def _send_twitch_message(message: Message, beatmap_info: dict):
        """
        Sends twitch feedback message
        :param message: Twitch Message object
        :param beatmap_info: Dictionary containing beatmap information from osu! api
        :return:
        """
        artist = beatmap_info['artist']
        title = beatmap_info['title']
        version = beatmap_info['version']
        bmap_info_text = f"{artist} - {title} [{version}]"
        await message.channel.send(f"{bmap_info_text} - Request sent!")
        return

    @staticmethod
    def _check_message_contains_beatmap_link(message: Message) -> Tuple[Union[AnyStr, None], Union[dict, None]]:
        """
        Splits message by space character and checks for possible beatmap links
        :param message: Twitch Message object
        :return:
        """
        logger.debug("Checking if message contains beatmap link")
        content = message.content

        for candidate_link in content.split(' '):
            result, mods = parse_beatmap_link(candidate_link, content)
            if result:
                logger.debug(f"Found beatmap id: {result}")
                return mods, result
        else:
            logger.debug("Couldn't find beatmap in message")
            return None, None

    async def _prepare_irc_message(self, message: Message, beatmap_info: dict, given_mods: str):
        """
        Prepare beatmap request message to send to osu!irc.
        :param author: Message author name
        :param beatmap_info: Beatmap info taken from osu!api as dictionary
        :param given_mods: Mods as string
        :return:
        """
        artist = beatmap_info['artist']
        title = beatmap_info['title']
        version = beatmap_info['version']
        bpm = beatmap_info['bpm']
        beatmap_status = self.BEATMAP_STATUS_DICT[beatmap_info['approved']]
        difficultyrating = float(beatmap_info['difficultyrating'])
        beatmap_id = beatmap_info['beatmap_id']
        beatmap_info = f"[http://osu.ppy.sh/b/{beatmap_id} {artist} - {title} [{version}]] ({bpm} BPM, {difficultyrating:.2f}*) {given_mods}"
        extra_postfix = ""
        extra_prefix = ""
        if message.author.is_mod:
            extra_prefix += "[MOD] "
        elif message.author.is_subscriber:
            extra_prefix += "[SUB] "
        elif 'vip' in message.author.badges:
            extra_prefix += "[VIP] "

        if 'custom-reward-id' in message.tags:
            extra_postfix += "+ USED POINTS"

        return f"{extra_prefix}{message.author.name} -> [{beatmap_status}] {beatmap_info} {extra_postfix}"

    async def event_ready(self):
        logger.info(f'Ready | {self.nick}')

        logger.debug(f'Populating users: {self.initial_channel_ids}')
        # Get channel names from ids
        channel_names = await self.get_users(*self.initial_channel_ids)

        channels_to_join = [ch.login for ch in channel_names]
        logger.debug(f'Joining channels: {channels_to_join}')
        # Join channels
        await self.join_channels(channels_to_join)

        initial_extensions = ['cogs.request_cog', 'cogs.admin_cog']
        for extension in initial_extensions:
            self.load_module(extension)
