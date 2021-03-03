import os
from queue import Queue
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

    def __init__(self, channel_mappings: dict):
        initial_channels = [c for c, _ in channel_mappings.items()]
        args = {
            'irc_token': os.getenv('TMI_TOKEN'),
            'client_id': os.getenv('CLIENT_ID'),
            'client_secret': os.getenv('CLIENT_SECRET'),
            'nick': os.getenv('BOT_NICK'),
            'prefix': os.getenv('BOT_PREFIX'),
            'initial_channels': initial_channels
        }
        super().__init__(**args)

        self.osu_api = OsuApiHelper()
        self.shared_message_queue = Queue()
        self.users_db = UserDatabase()
        self.users_db.initialize()
        self.user_last_request = {}
        self.irc_bot = IrcBot("#osu", "heyronii", "irc.ppy.sh", password=os.getenv("IRC_PASSWORD"),
                              shared_message_queue=self.shared_message_queue)

        p = Thread(target=self.irc_bot.start)
        p.start()

        msg_handler_thread = Thread(target=self._handle_shared_messages)
        msg_handler_thread.start()

        self.channel_mappings = channel_mappings

    async def event_message(self, message: Message):
        self.check_channel_enabled(message.channel.name)
        await self.check_if_streaming_osu(message.channel)
        logger.debug(f"Received message from {message.channel} - {message.author.name}: {message.content}")
        given_mods, api_params = self._check_message_contains_beatmap_link(message)
        if given_mods is not None:
            await self._check_user_cooldown(message.author)
            beatmap_info = await self.osu_api.get_beatmap_info(api_params)
            if beatmap_info:
                if self.users_db.get_echo_status(twitch_username=message.channel.name):
                    await self._send_twitch_message(message, beatmap_info)
                await self._send_irc_message(message, beatmap_info, given_mods)

        await self.handle_commands(message)

    @staticmethod
    async def check_if_streaming_osu(channel: Channel):
        """
        Checks if stream is on and they're playing osu!, otherwise ignores channel.
        :param channel:
        :return:
        """

        stream = await channel.get_stream()
        if stream is None:
            raise Exception('Stream is not on.')
        if stream.get('game_name') != 'osu!':
            raise Exception('Stream is not playing osu!')

        return

    def check_channel_enabled(self, channel_name):
        enabled = self.users_db.get_enabled_status(twitch_username=channel_name)
        if not enabled:
            raise Exception('Channel not open for requests')

    async def _check_user_cooldown(self, author: User):
        """
        Check if user is on cooldown, raise an exception if the user is on request cooldown.
        :param author: Twitch user object
        :return: Exception if user has requested a beatmap before TwitchBot.PER_REQUEST_COOLDOWN seconds passed.
        """
        author_id = author.id
        time_right_now = datetime.datetime.now()

        await self._prune_cooldowns(time_right_now)

        if author_id not in self.user_last_request:
            self.user_last_request[author_id] = time_right_now
        else:
            last_message_time = self.user_last_request[author_id]
            if (time_right_now - last_message_time).total_seconds() > TwitchBot.PER_REQUEST_COOLDOWN:
                self.user_last_request[author_id] = time_right_now
            else:
                raise Exception(f'Cooldown for user {author.name}')

        return

    async def _prune_cooldowns(self, time_right_now: datetime.datetime):
        """
        Prunes cooldowned users list so it doesn't get too cluttered.
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
        irc_message = await self._prepare_irc_message(message.author.name, beatmap_info, given_mods)

        irc_target_channel = self.channel_mappings[message.channel.name]
        await self.irc_bot.send_message(irc_target_channel, irc_message)
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

    @staticmethod
    async def _prepare_irc_message(author: str, beatmap_info: dict, given_mods: str):
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
        difficultyrating = float(beatmap_info['difficultyrating'])
        beatmap_id = beatmap_info['beatmap_id']
        beatmap_info = f"[http://osu.ppy.sh/b/{beatmap_id} {artist} - {title} [{version}]] ({bpm} BPM, {difficultyrating:.2f}*) {given_mods}"
        return f"{author} -> {beatmap_info}"

    def _handle_shared_messages(self):
        while True:
            msg, osu_channel = self.shared_message_queue.get()
            logger.debug(f'Twitch bot received {msg} from {osu_channel}')

    async def event_ready(self):
        logger.info(f'Ready | {self.nick}')

        initial_extensions = ['cogs.request_cog']
        for extension in initial_extensions:
            self.load_module(extension)
