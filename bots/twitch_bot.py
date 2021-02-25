import os
from threading import Thread
from abc import ABC
from typing import AnyStr, Tuple, Union
import logging

from twitchio.ext import commands
from twitchio import Message

from helpers.beatmap_link_parser import parse_beatmap_link
from helpers.osu_api_helper import OsuApiHelper
from bots.irc_bot import IrcBot

logger = logging.getLogger('ronnia')


class TwitchBot(commands.Bot, ABC):

    def __init__(self, channel_mappings: dict):
        initial_channels = [c for c, _ in channel_mappings.items()]
        args = {
            'irc_token': os.getenv('TMI_TOKEN'),
            'client_id': os.getenv('CLIENT_ID'),
            'nick': os.getenv('BOT_NICK'),
            'prefix': os.getenv('BOT_PREFIX'),
            'initial_channels': initial_channels
        }
        super().__init__(**args)

        self.osu_api = OsuApiHelper()
        self.irc_bot = IrcBot("#osu", "heyronii", "irc.ppy.sh", password=os.getenv("IRC_PASSWORD"))
        p = Thread(target=self.irc_bot.start)
        p.start()
        self.channel_mappings = channel_mappings

    async def event_message(self, message: Message):
        logger.info(f"Received message from {message.channel} - {message.author}: {message.content}")
        given_mods, api_params = self._check_message_contains_beatmap_link(message)
        if given_mods is not None:
            beatmap_info = await self.osu_api.get_beatmap_info(api_params)
            if beatmap_info:
                await self._send_twitch_message(message, beatmap_info)
                await self._send_irc_message(message, beatmap_info, given_mods)
                await self.handle_commands(message)

    async def _send_irc_message(self, message, beatmap_info, given_mods):
        irc_message = self._prepare_irc_message(message.author.name, beatmap_info, given_mods)

        irc_target_channel = self.channel_mappings[message.channel.name]
        self.irc_bot.send_message(irc_target_channel, irc_message)
        return

    @staticmethod
    async def _send_twitch_message(message, beatmap_info):
        artist = beatmap_info['artist']
        title = beatmap_info['title']
        version = beatmap_info['version']
        bmap_info_text = f"{artist} - {title} [{version}]"
        await message.channel.send(f"{bmap_info_text} - Request sent!")
        return

    @staticmethod
    def _check_message_contains_beatmap_link(message: Message) -> Tuple[Union[AnyStr, None], Union[dict, None]]:
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
    def _prepare_irc_message(author, beatmap_info, given_mods):

        artist = beatmap_info['artist']
        title = beatmap_info['title']
        version = beatmap_info['version']
        bpm = beatmap_info['bpm']
        difficultyrating = float(beatmap_info['difficultyrating'])
        beatmap_id = beatmap_info['beatmap_id']
        beatmap_info = f"[http://osu.ppy.sh/b/{beatmap_id} {artist} - {title} [{version}]] ({bpm} BPM, {difficultyrating:.2f}*) {given_mods}"
        return f"{author} -> {beatmap_info}"

    @staticmethod
    def _parse_beatmap_link(word: str):
        if word.startswith('https://osu.ppy.sh/beatmapsets/'):
            parts = word.split('/')
            candidate_beatmap_id = parts[-1]

            if candidate_beatmap_id.isnumeric():
                return candidate_beatmap_id

            else:
                candidate_beatmap_id = parts[-2]
                if candidate_beatmap_id.isnumeric():
                    return candidate_beatmap_id

                else:
                    return None
        elif word.startswith('https://osu.ppy.sh/b/'):
            parts = word.split('/')
            candidate_beatmap_id = parts[-1]
            if candidate_beatmap_id.isnumeric():
                return candidate_beatmap_id
            else:
                return None
        else:
            return None

    async def event_ready(self):
        logger.info(f'Ready | {self.nick}')
