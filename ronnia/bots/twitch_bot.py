import asyncio
import datetime
import json
import logging
import os
import sqlite3
import time
import traceback
from abc import ABC
from multiprocessing import Lock
from typing import AnyStr, Tuple, Union, List

from azure.servicebus import ServiceBusMessage
from azure.servicebus.aio import ServiceBusClient
from azure.servicebus.exceptions import ServiceBusError
from twitchio import Message, Channel, Chatter, User
from twitchio.ext import commands, routines

from helpers.osu_api_helper import OsuApiV2, OsuChatApiV2
from ronnia.helpers.beatmap_link_parser import parse_beatmap_link
from ronnia.helpers.database_helper import UserDatabase, StatisticsDatabase
from ronnia.helpers.utils import convert_seconds_to_readable
from websocket.ws import PatchedWSConnection

logger = logging.getLogger(__name__)


class TwitchBot(commands.Bot, ABC):

    def __init__(self, initial_channel_names: List[str], join_lock: Lock, max_users: int):
        self.users_db = UserDatabase()
        self.messages_db = StatisticsDatabase()
        self.osu_api = OsuApiV2(os.getenv('OSU_CLIENT_ID'), os.getenv('OSU_CLIENT_SECRET'))
        self.osu_chat_api = OsuChatApiV2(os.getenv('OSU_CLIENT_ID'), os.getenv('OSU_CLIENT_SECRET'))
        self.channels_join_failed = []

        initial_channels = [os.getenv('BOT_NICK'), *initial_channel_names]
        token = os.getenv('TMI_TOKEN').replace("oauth:", "")
        args = {
            'token': token,
            'client_id': os.getenv('TWITCH_CLIENT_ID'),
            'client_secret': os.getenv('TWITCH_CLIENT_SECRET'),
            'prefix': os.getenv('BOT_PREFIX'),
            'initial_channels': initial_channels
        }
        logger.debug(f'Sending args to super().__init__: {args}')
        super().__init__(**args)

        self._connection = PatchedWSConnection(
            client=self,
            token=token,
            loop=self.loop,
            initial_channels=initial_channels,
            heartbeat=self._heartbeat,
        )

        self.initial_channel_names = initial_channel_names
        self.environment = os.getenv('ENVIRONMENT')
        self.servicebus_connection_string = os.getenv('SERVICE_BUS_CONNECTION_STR')
        self.servicebus_client = ServiceBusClient.from_connection_string(conn_str=self.servicebus_connection_string)
        self.signup_queue_name = 'bot-signups'
        self.signup_reply_queue_name = 'bot-signups-reply'

        self._join_lock = join_lock

        self.main_prefix = None
        self.user_last_request = {}

        self.max_users = max_users

    async def servicebus_message_receiver(self):
        """
        Start a queue listener for messages from the website sign-up.
        """
        # Each instance of bot can only have 100 users.
        if len(self.initial_channel_names) >= self.max_users:
            logger.info(f'Reached {self.max_users} members, stopped listening to sign-up queue.')
            return

        logger.info(f'Starting service bus message receiver')
        while True:
            try:
                async with self.servicebus_client.get_queue_receiver(queue_name=self.signup_queue_name) as receiver:
                    async for message in receiver:
                        logger.info(f'Received sign-up message: {message}')
                        reply_message = await self.receive_and_parse_message(message)
                        await receiver.complete_message(message)

                        async with ServiceBusClient.from_connection_string(
                                conn_str=self.servicebus_connection_string) as servicebus_client:
                            async with servicebus_client.get_queue_sender(
                                    queue_name=self.signup_reply_queue_name) as sender:
                                logger.info(f'Sending reply message to sign-up queue: {reply_message}')
                                await sender.send_messages(reply_message)

                                if len(self.initial_channel_names) == 100:
                                    logger.warning(
                                        'Reached 100 members, sending manager signal to create a new process.')
                                    bot_full_message = ServiceBusMessage("bot-full")
                                    await sender.send_messages(bot_full_message)
                                    return
            except ServiceBusError as e:
                logger.error(f'Twitch bot receiver error: {e}')
                logger.error(traceback.format_exc())
                await asyncio.sleep(5)

    async def receive_and_parse_message(self, message):
        """
        {'command': 'signup',
         'osu_username': 'heyronii',
         'osu_id': 5642779,
         'twitch_username': 'heyronii',
         'twitch_id': '68427964',
         'avatar_url': 'https://static-cdn.jtvnw.net/jtv_user_pictures/18057641-820c-44d0-af8d-032e129086fb-profile_image-300x300.png'}
        """
        message_dict = json.loads(str(message))
        twitch_username = message_dict['twitch_username']
        osu_username = message_dict['osu_username']
        osu_id = message_dict['osu_id']
        twitch_id = message_dict['twitch_id']

        self.initial_channel_names.append(twitch_username)
        await self.users_db.add_user(twitch_username=twitch_username,
                                     twitch_id=twitch_id,
                                     osu_username=osu_username,
                                     osu_user_id=osu_id)
        user_db_details = await self.users_db.get_user_from_twitch_username(twitch_username)
        with self._join_lock:
            asyncio.create_task(self._connection._join_channel(twitch_username))
        message_dict['user_id'] = user_db_details['user_id']
        return ServiceBusMessage(json.dumps(message_dict))

    def run(self):
        logger.info(f"Running bot")
        self.loop.run_until_complete(self.users_db.initialize())
        self.loop.run_until_complete(self.messages_db.initialize())
        self.loop.create_task(self.servicebus_message_receiver())
        self.routine_update_user_information.start(stop_on_error=False)
        self.routine_show_connected_channels.start(stop_on_error=False)
        super().run()

    async def event_message(self, message: Message):
        if message.author is None:
            logger.info(f"{message.channel.name}: {message.content}")
            return
        logger.info(f"{message.channel.name} - {message.author.name}: {message.content}")

        if self.environment == "testing":
            return

        await self.handle_commands(message)
        try:
            await self.check_channel_enabled(message.channel.name)
            await self.handle_request(message)
        except AssertionError as e:
            logger.info(f'Check unsuccessful: {e}')
            await self.messages_db.add_error('internal_check', str(e))

    async def handle_request(self, message: Message):
        given_mods, api_params = self._check_message_contains_beatmap_link(message)
        if given_mods is not None:
            if 's' in api_params:
                beatmap_info, beatmapset_info = await self.osu_api.get_beatmapset(api_params['s'])
            else:
                beatmap_info, beatmapset_info = await self.osu_api.get_beatmap(api_params['b'])

            if beatmap_info:
                await self.check_request_criteria(message, beatmap_info)
                # If user has enabled echo setting, send twitch chat a message
                if await self.users_db.get_echo_status(twitch_username=message.channel.name):
                    await self._send_twitch_message(message=message,
                                                    beatmap_info=beatmap_info,
                                                    beatmapset_info=beatmapset_info)

                await self._send_beatmap_to_in_game(message=message,
                                                    beatmap_info=beatmap_info,
                                                    beatmapset_info=beatmapset_info,
                                                    given_mods=given_mods)
                await self.messages_db.add_request(requested_beatmap_id=int(beatmap_info['id']),
                                                   requested_channel_name=message.channel.name,
                                                   requester_channel_name=message.author.name,
                                                   mods=given_mods)

    async def check_beatmap_star_rating(self, message: Message, beatmap_info):
        twitch_username = message.channel.name
        requester_name = message.author.name
        diff_rating = float(beatmap_info['difficulty_rating'])
        range_low, range_high = await self.users_db.get_range_setting(twitch_username=twitch_username, setting_key='sr')

        if range_low == -1 or range_high == -1:
            return

        assert range_low < diff_rating < range_high, \
            f'@{requester_name} Streamer is accepting requests between {range_low:.1f}-{range_high:.1f}* difficulty.' \
            f' Your map is {diff_rating:.1f}*.'

        return

    async def check_request_criteria(self, message: Message, beatmap_info: dict):
        test_status = await self.users_db.get_test_status(message.channel.name)
        if not test_status and self.environment != 'testing':
            await self.check_if_author_is_broadcaster(message)
            await self.check_if_streaming_osu(message.channel)
            await self._check_user_cooldown(message.author)

        await self.check_sub_only_mode(message)
        await self.check_cp_only_mode(message)
        await self.check_user_excluded(message)
        try:
            await self.check_beatmap_star_rating(message, beatmap_info)
        except AssertionError as e:
            await message.channel.send(str(e))
            raise AssertionError

    async def check_user_excluded(self, message: Message):
        excluded_users = await self.users_db.get_excluded_users(twitch_username=message.channel.name,
                                                                return_mode='list')
        assert message.author.name.lower() not in excluded_users, f'{message.author.name} is excluded'

    async def check_sub_only_mode(self, message: Message):
        is_sub_only = await self.users_db.get_setting('sub-only', message.channel.name)
        if is_sub_only:
            assert message.author.is_mod or message.author.is_subscriber != '0' or 'vip' in message.author.badges, \
                'Subscriber only request mode is active.'

    async def check_cp_only_mode(self, message):
        is_cp_only = await self.users_db.get_setting('cp-only', message.channel.name)
        if is_cp_only:
            assert 'custom-reward-id' in message.tags, 'Channel Points only mode is active.'
        return

    async def event_command_error(self, ctx, error):
        logger.error(error)
        await self.messages_db.add_error(error_type='twitch_command_error', error_text=str(error))

    async def event_error(self, error: Exception, data: str = None):
        logger.error(error, data)
        await super(TwitchBot, self).event_error(error, data)
        pass

    @staticmethod
    async def check_if_author_is_broadcaster(message: Message):

        assert message.author.name != message.channel.name, 'Author is broadcaster and not in test mode.'

        return

    async def global_before_hook(self, ctx):
        """
        Global hook that runs before every command.
        :param ctx: Message context
        :return:
        """
        user = await self.users_db.get_user_from_twitch_username(ctx.author.name)
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

    async def check_channel_enabled(self, channel_name):
        enabled = await self.users_db.get_enabled_status(twitch_username=channel_name)
        assert enabled, f'Channel:{channel_name} is not open for requests'

    async def _check_user_cooldown(self, author: Chatter):
        """
        Check if user is on cooldown, raise an exception if the user is on request cooldown.
        :param author: Twitch user object
        :return: Exception if user has requested a beatmap before channel_cooldown seconds passed.
        """
        author_id = author.id
        time_right_now = datetime.datetime.now()

        channel_cooldown = await self.users_db.get_setting("cooldown", author_id)
        await self._prune_cooldowns(time_right_now, channel_cooldown)

        if author_id not in self.user_last_request:
            self.user_last_request[author_id] = time_right_now
        else:
            last_message_time = self.user_last_request[author_id]
            seconds_since_last_request = (time_right_now - last_message_time).total_seconds()
            assert seconds_since_last_request >= channel_cooldown, \
                f'{author.name} is on cooldown.'
            self.user_last_request[author_id] = time_right_now

        return

    async def _prune_cooldowns(self, time_right_now: datetime.datetime, channel_cooldown: int):
        """
        Prunes users on that are on cooldown list so it doesn't get too cluttered.
        :param time_right_now:
        :return:
        """
        pop_list = []
        for user_id, last_message_time in self.user_last_request.items():
            seconds_since_last_request = (time_right_now - last_message_time).total_seconds()
            if seconds_since_last_request >= channel_cooldown:
                pop_list.append(user_id)

        for user in pop_list:
            self.user_last_request.pop(user)

        return

    async def _send_beatmap_to_in_game(self, message: Message, beatmap_info: dict, beatmapset_info: dict,
                                       given_mods: str):
        """
        Sends the beatmap request message to osu!irc bot
        :param message: Twitch Message object
        :param beatmap_info: Dictionary containing beatmap information from osu! api
        :param given_mods: String of mods if they are requested, empty string instead
        :return:
        """
        irc_message = await self._prepare_irc_message(message=message,
                                                      beatmap_info=beatmap_info,
                                                      beatmapset_info=beatmapset_info,
                                                      given_mods=given_mods)
        target_id = (await self.users_db.get_user_from_twitch_username(message.channel.name))['osu_id']
        await self.osu_chat_api.send_message(target_id=target_id, message=irc_message)
        return

    @staticmethod
    async def _send_twitch_message(message: Message, beatmapset_info: dict, beatmap_info: dict):
        """
        Sends twitch feedback message
        :param message: Twitch Message object
        :param beatmap_info: Dictionary containing beatmap information from osu! api
        :return:
        """
        artist = beatmapset_info['artist']
        title = beatmapset_info['title']
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
                logger.info(f"Found beatmap id: {result}")
                return mods, result
        else:
            logger.info("Couldn't find beatmap in message")
            return None, None

    async def _prepare_irc_message(self, message: Message, beatmap_info: dict, beatmapset_info: dict, given_mods: str):
        """
        Prepare beatmap request message to send to osu!irc.
        :param message: Twitch message
        :param beatmap_info: Beatmap info taken from osu!api as dictionary
        :param given_mods: Mods as string
        :return:
        """
        artist = beatmapset_info['artist']
        title = beatmapset_info['title']
        version = beatmap_info['version']
        bpm = beatmap_info['bpm']
        beatmap_status = str(beatmap_info['status']).capitalize()
        difficultyrating = float(beatmap_info['difficulty_rating'])
        beatmap_id = beatmap_info['id']
        beatmap_length = convert_seconds_to_readable(beatmap_info['hit_length'])
        beatmap_info = f"[https://osu.ppy.sh/b/{beatmap_id} {artist} - {title} [{version}]] " \
                       f"({bpm} BPM, {difficultyrating:.2f}*, {beatmap_length}) {given_mods}"
        extra_postfix = ""
        extra_prefix = ""

        badges = message.author.badges

        if message.author.is_mod:
            extra_prefix += "[MOD] "
        elif message.author.is_subscriber:
            extra_prefix += "[SUB] "
        elif 'vip' in badges:
            extra_prefix += "[VIP] "

        if 'custom-reward-id' in message.tags:
            extra_postfix += "+ USED POINTS"

        return f"{extra_prefix}{message.author.name} -> [{beatmap_status}] {beatmap_info} {extra_postfix}"

    async def event_ready(self):

        logger.info(f'Connected channels: {self.connected_channels}')
        logger.info(f'Successfully initialized bot!')
        logger.info(f'Ready | {self.nick}')

    @routines.routine(minutes=1)
    async def routine_show_connected_channels(self):
        connected_channel_names = [channel.name for channel in list(filter(None, self.connected_channels))]
        logger.info(f'Connected channels: {connected_channel_names}')

    @routines.routine(hours=1)
    async def routine_update_user_information(self):
        """
        Checks and updates user information changes. This routine runs every hour.
        :return:
        """
        logger.info('Started user information update routine')
        connected_users = await self.users_db.get_multiple_users_by_username(self.initial_channel_names)
        twitch_users = await self.fetch_users(names=self.initial_channel_names)
        twitch_users_by_id = {user.id: user for user in twitch_users}

        if len(twitch_users) != len(connected_users):
            connected_users = await self.handle_banned_users(connected_users, twitch_users)

        for user in connected_users:
            osu_info = await self.osu_api.get_user_info(user['osu_id'])
            twitch_info = twitch_users_by_id[int(user['twitch_id'])]
            await self.update_user_db_info(user, osu_info, twitch_info)

    @routine_update_user_information.error
    async def routine_update_user_information_error(error: Exception):
        logger.error(f'Error while updating user information: {error}')
        traceback.print_exc()

    async def update_user_db_info(self, user: sqlite3.Row, osu_info: dict, twitch_info: User):
        """
        Update user information in database
        :param user: User to update
        :param osu_info: Osu! user info
        :param twitch_info: Twitch user info
        :return:
        """
        osu_user_id = osu_info['id']
        osu_username = osu_info['username']
        twitch_name = twitch_info.name
        twitch_id = twitch_info.id
        await self.users_db.update_user(new_twitch_username=twitch_name, new_osu_username=osu_username,
                                        osu_user_id=osu_user_id, twitch_id=twitch_id)
        logger.debug(
            f'Updated user information for {user["twitch_username"]}: '
            f'{osu_username} | {twitch_name} | {twitch_id} | {osu_user_id}')

    async def handle_banned_users(self, connected_users: List[sqlite3.Row], twitch_users: List[User]) -> \
            List[sqlite3.Row]:
        """
        Removes banned users from self.connected_channel_ids and updates the database.
        :param connected_users: List of connected users
        :param twitch_users: List of twitch users
        :return:
        """
        logger.debug('Handling banned users...')
        twitch_user_ids = [str(user.id) for user in twitch_users]
        existing_users = []
        # Run through connected users and check if they are in the list of twitch users
        for connected_user in connected_users:
            if connected_user['twitch_id'] not in twitch_user_ids:
                logger.info(f'{connected_user["twitch_username"]} does not exist anymore!')
                self.initial_channel_names.remove(connected_user['twitch_username'])
                await self.users_db.remove_user(connected_user['twitch_username'])
            else:
                existing_users.append(connected_user)

        logger.info(f'{len(existing_users)} users are still connected.')
        return existing_users
