import os
from threading import Thread
from abc import ABC

from twitchio.ext import commands
from twitchio import Message
import aiohttp

from irc_bot import IrcBot


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
        self._osu_api_key = os.getenv('OSU_API_KEY')

        self.irc_bot = IrcBot("#osu", "heyronii", "irc.ppy.sh", password=os.getenv("IRC_PASSWORD"))
        p = Thread(target=self.irc_bot.start)
        p.start()
        self.channel_mappings = channel_mappings

    async def event_message(self, message: Message):
        print(f"Received message: {message.content}")
        link, beatmap_id = self._check_message_contains_beatmap_link(message)
        if link:
            beatmap_info = await self._get_beatmap_info(beatmap_id)

            await self._send_twitch_message(message, beatmap_info)
            await self._send_irc_message(message, beatmap_info)
            await self.handle_commands(message)

    async def _send_irc_message(self, message, beatmap_info):
        irc_message = self._prepare_irc_message(message.author.name, beatmap_info)

        irc_target_channel = self.channel_mappings[message.channel.name]
        self.irc_bot.send_message(irc_target_channel, irc_message)
        return

    @staticmethod
    async def _send_twitch_message(message, beatmap_info):
        artist = beatmap_info['artist']
        title = beatmap_info['title']
        version = beatmap_info['version']
        bmap_info_text = f"{artist} - {title} [{version}]"
        await message.channel.send(f"{bmap_info_text} - Yayıncıya ilettim!")
        return

    def _check_message_contains_beatmap_link(self, message: Message):
        print("Checking if message contains beatmap link")
        content = message.content

        for candidate_link in content.split(' '):
            beatmap_id = self._parse_beatmap_link(candidate_link)
            if beatmap_id:
                print(f"Found beatmap id: {beatmap_id}")
                return candidate_link, beatmap_id
        else:
            print("Couldn't find beatmap in message")
            return None, None

    async def _get_beatmap_info(self, beatmap_id):
        params = {"k": self._osu_api_key,
                  "b": beatmap_id}
        async with aiohttp.ClientSession() as session:
            async with session.get('http://osu.ppy.sh/api/get_beatmaps', params=params) as response:
                r = await response.json()

        return r[0]

    @staticmethod
    def _prepare_irc_message(author, beatmap_info):

        artist = beatmap_info['artist']
        title = beatmap_info['title']
        version = beatmap_info['version']
        bpm = beatmap_info['bpm']
        difficultyrating = float(beatmap_info['difficultyrating'])
        beatmap_id = beatmap_info['beatmap_id']
        beatmap_info = f"[http://osu.ppy.sh/b/{beatmap_id} {artist} - {title} [{version}]] ({bpm} BPM, {difficultyrating:.2f}*)"
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
        print(f'Ready | {self.nick}')
