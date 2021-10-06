import asyncio
import datetime
import logging
import os
import re
import time
from abc import ABC
from threading import Thread
from typing import AnyStr, Tuple, Union, List

import aiohttp
from twitchio import Message, Channel, Chatter
from twitchio.ext import commands, routines

from bots.irc_bot import IrcBot
from helpers.beatmap_link_parser import parse_beatmap_link
from helpers.database_helper import UserDatabase, StatisticsDatabase
from helpers.osu_api_helper import OsuApi
from helpers.utils import convert_seconds_to_readable

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

        self.messages_db = StatisticsDatabase()
        self.messages_db.initialize()

        self.all_user_details = self.users_db.get_all_users()
        self.initial_channel_ids = [user['twitch_id'] for user in self.all_user_details]
        args = {
            'token': os.getenv('TMI_TOKEN'),
            'client_id': os.getenv('CLIENT_ID'),
            'client_secret': os.getenv('CLIENT_SECRET'),
            'prefix': os.getenv('BOT_PREFIX')
        }
        super().__init__(**args)

        self.main_prefix = None
        self.osu_api = OsuApi(self.messages_db)
        self.user_last_request = {}
        self.irc_bot = IrcBot("#osu", os.getenv('OSU_USERNAME'), "irc.ppy.sh", password=os.getenv("IRC_PASSWORD"))

        p = Thread(target=self.irc_bot.start)
        p.start()

        self.join_channels_first_time = True

    @staticmethod
    async def _get_access_token():
        client_id = os.getenv('CLIENT_ID'),
        client_secret = os.getenv('CLIENT_SECRET')
        grant_type = 'client_credentials'
        scope = 'chat:read chat:edit'
        payload = {'client_id': client_id,
                   'client_secret': client_secret,
                   'grant_type': grant_type,
                   'scope': scope}
        async with aiohttp.ClientSession() as session:
            async with session.post('https://id.twitch.tv/oauth2/token', data=payload) as resp:
                response_json = await resp.json()
        return response_json['access_token']

    async def event_message(self, message: Message):
        if message.author is None:
            return

        await self.handle_commands(message)
        try:
            self.check_channel_enabled(message.channel.name)
            await self.handle_request(message)
        except AssertionError as e:
            logger.debug(f'Check unsuccessful: {e}')
            self.messages_db.add_error('internal_check', str(e))

    async def handle_request(self, message: Message):
        logger.info(f"{message.channel.name} - {message.author.name}: {message.content}")
        given_mods, api_params = self._check_message_contains_beatmap_link(message)
        if given_mods is not None:
            self._check_user_cooldown(message.author)
            beatmap_info = await self.osu_api.get_beatmap_info(api_params)
            if beatmap_info:
                await self.check_request_criteria(message, beatmap_info)
                await self._update_channel(message)
                # If user has enabled echo setting, send twitch chat a message
                if self.users_db.get_echo_status(twitch_username=message.channel.name):
                    await self._send_twitch_message(message, beatmap_info)

                await self._send_irc_message(message, beatmap_info, given_mods)
                self.messages_db.add_request(requested_beatmap_id=int(beatmap_info['beatmap_id']),
                                             requested_channel_name=message.channel.name,
                                             requester_channel_name=message.author.name,
                                             mods=given_mods)

    def inform_user_on_updates(self, osu_username: str, twitch_username: str, is_updated: bool):
        if not is_updated:
            message_txt_path = os.path.join(os.getenv('DB_DIR'), 'update_message.txt')
            if os.path.exists(message_txt_path):
                with open(message_txt_path) as f:
                    update_message = f.read().strip()
                self.irc_bot.send_message(osu_username, update_message)
            else:
                logger.warning(f'Looking for {message_txt_path}, but it does not exist!')
            self.users_db.set_channel_updated(twitch_username)
        return

    def check_beatmap_star_rating(self, message: Message, beatmap_info):
        twitch_username = message.channel.name
        requester_name = message.author.name
        diff_rating = float(beatmap_info['difficultyrating'])
        range_low, range_high = self.users_db.get_range_setting(twitch_username=twitch_username, setting_key='sr')

        if range_low == -1 or range_high == -1:
            return

        assert range_low < diff_rating < range_high, \
            f'@{requester_name} Streamer is accepting requests between {range_low:.1f}-{range_high:.1f}* difficulty.' \
            f' Your map is {diff_rating:.1f}*.'

        return

    async def check_request_criteria(self, message: Message, beatmap_info: dict):
        test_status = self.users_db.get_test_status(message.channel.name)
        if not test_status:
            self.check_sub_only_mode(message)
            self.check_cp_only_mode(message)
            self.check_user_excluded(message)
            self.check_if_author_is_broadcaster(message)
            await self.check_if_streaming_osu(message.channel)

            try:
                self.check_beatmap_star_rating(message, beatmap_info)
            except AssertionError as e:
                await message.channel.send(str(e))
                raise AssertionError

    def check_user_excluded(self, message: Message):
        excluded_users = self.users_db.get_excluded_users(twitch_username=message.channel.name, return_mode='list')
        assert message.author.name.lower() not in excluded_users, f'{message.author.name} is excluded'

    def check_sub_only_mode(self, message: Message):
        is_sub_only = self.users_db.get_setting('sub-only', message.channel.name)
        if is_sub_only:
            assert message.author.is_mod or message.author.is_subscriber != '0' or 'vip' in message.author.badges, \
                'Subscriber only request mode is active.'

    def check_cp_only_mode(self, message):
        is_cp_only = self.users_db.get_setting('cp-only', message.channel.name)
        if is_cp_only:
            assert 'custom-reward-id' in message.tags, 'Channel Points only mode is active.'
        return

    async def event_command_error(self, ctx, error):
        logger.error(error)
        self.messages_db.add_error(error_type='twitch_command_error', error_text=str(error))
        pass

    async def _update_channel(self, message: Message):
        """
        Updates the user about news
        """

        logger.info('Updating user with the latest news!')
        # Get current channel details from db
        channel_details = self.users_db.get_user_from_twitch_username(twitch_username=message.channel.name)
        twitch_username = channel_details['twitch_username']
        is_channel_updated = channel_details['enabled']
        self.inform_user_on_updates(channel_details['osu_username'], twitch_username, is_channel_updated)
        return

    async def get_osu_and_twitch_details(self, osu_user_id_or_name, twitch_user_id=None, twitch_username=None):

        assert twitch_user_id is not None or twitch_username is not None, 'Twitch user id or twitch username must be given.'
        if osu_user_id_or_name.isdigit():
            # Handle ids in the string form
            osu_user_id_or_name = int(osu_user_id_or_name)

        # Get osu! username from osu! api (usernames can change)
        osu_user_info = await self.osu_api.get_user_info(osu_user_id_or_name)
        # Get twitch username from twitch api
        if twitch_user_id is None:
            twitch_info = await self.fetch_users(names=[twitch_username])
        else:
            twitch_info = await self.fetch_users(ids=[twitch_user_id])
        return osu_user_info, twitch_info

    @staticmethod
    def check_if_author_is_broadcaster(message: Message):

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

    async def check_if_streaming_osu(self, channel: Channel):
        """
        Checks if stream is on and they're playing osu!, otherwise ignores channel.
        :param channel: Channel of the message
        :return:
        """
        stream_list = await self.fetch_streams(user_logins=[channel.name])
        assert len(stream_list) == 1, f'{channel.name} stream is not on.'
        stream = stream_list[0]
        assert stream.game_name == 'osu!', f'{channel.name} stream is not playing osu!'

        return

    def check_channel_enabled(self, channel_name):
        enabled = self.users_db.get_enabled_status(twitch_username=channel_name)
        assert enabled, f'Channel:{channel_name} is not open for requests'

    def _check_user_cooldown(self, author: Chatter):
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
            seconds_since_last_request = (time_right_now - last_message_time).total_seconds()
            assert seconds_since_last_request >= TwitchBot.PER_REQUEST_COOLDOWN, \
                f'{author.name} is on cooldown.'
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
            seconds_since_last_request = (time_right_now - last_message_time).total_seconds()
            if seconds_since_last_request >= TwitchBot.PER_REQUEST_COOLDOWN:
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
        :param message: Twitch message
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
        beatmap_length = convert_seconds_to_readable(beatmap_info['hit_length'])
        beatmap_info = f"[http://osu.ppy.sh/b/{beatmap_id} {artist} - {title} [{version}]] ({bpm} BPM, {difficultyrating:.2f}*, {beatmap_length}) {given_mods}"
        extra_postfix = ""
        extra_prefix = ""

        # TODO: Check if this issue is fixed in next twitchio
        try:
            badges = message.author.badges
        except ValueError:
            badges = []

        if message.author.is_mod:
            extra_prefix += "[MOD] "
        elif message.author.is_subscriber != '0':
            # TODO: Check if this conditional changed in the next releases of tio
            extra_prefix += "[SUB] "
        elif 'vip' in badges:
            extra_prefix += "[VIP] "

        if 'custom-reward-id' in message.tags:
            extra_postfix += "+ USED POINTS"

        return f"{extra_prefix}{message.author.name} -> [{beatmap_status}] {beatmap_info} {extra_postfix}"

    async def event_ready(self):

        self.main_prefix = self._prefix

        logger.info(f'Ready | {self.nick}')

        logger.debug(f'Populating users: {self.initial_channel_ids}')
        # Get channel names from ids
        channel_names = await self.fetch_users(ids=self.initial_channel_ids)

        channels_to_join = [ch.name for ch in channel_names]

        if self.nick not in channels_to_join:
            channels_to_join.append(self.nick)

        logger.debug(f'Joining channels: {channels_to_join}')
        # Join channels
        channel_join_start = time.time()
        await self.join_channels(channels_to_join)

        logger.debug(f'Joined all channels after {time.time() - channel_join_start:.2f}s')
        # Start update users routine
        self.update_users.start()
        self.join_channels_routine.start()

        initial_extensions = ['cogs.request_cog', 'cogs.admin_cog']
        for extension in initial_extensions:
            self.load_module(extension)

    @routines.routine(hours=1)
    async def update_users(self):
        logger.info('Started updating user routine')
        user_details = self.users_db.get_all_users()
        channel_ids = [ch['twitch_id'] for ch in user_details]
        channel_details = await self.fetch_users(ids=channel_ids)

        user_details.sort(key=lambda x: int(x['twitch_id']))
        channel_details.sort(key=lambda x: x.id)

        for db_user, new_twitch_user in zip(user_details, channel_details):
            try:
                osu_details = await self.osu_api.get_user_info(db_user['osu_username'])
            except aiohttp.ClientError as client_error:
                logger.error(client_error)
                osu_details = {'user_id': db_user['osu_id'],
                               'username': db_user['osu_username']}

            new_twitch_username = new_twitch_user.name.lower()
            new_osu_username = osu_details['username'].lower().replace(' ', '_')
            twitch_id = new_twitch_user.id
            osu_user_id = osu_details['user_id']

            if new_osu_username != db_user['osu_username'] or new_twitch_username != db_user['twitch_username']:
                logger.info(f'Username change:')
                logger.info(f'osu! old: {db_user["osu_username"]} - new: {new_osu_username}')
                logger.info(f'Twitch old: {db_user["twitch_username"]} - new: {new_twitch_username}')
                self.users_db.update_user(new_twitch_username=new_twitch_username,
                                          new_osu_username=new_osu_username,
                                          twitch_id=twitch_id,
                                          osu_user_id=osu_user_id)

    @routines.routine(hours=1)
    async def join_channels_routine(self):
        logger.debug('Started join channels routine')
        if self.join_channels_first_time:
            self.join_channels_first_time = False
            return
        all_user_details = self.users_db.get_all_users()
        twitch_users = [user['twitch_username'] for user in all_user_details]
        logger.debug(f'Joining: {twitch_users}')
        await self.join_channels(twitch_users)

    async def part_channel(self, entry):
        channel = re.sub("[#]", "", entry).lower()
        await self._connection.send(f"PART #{channel}\r\n")
