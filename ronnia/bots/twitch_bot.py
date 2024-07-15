import asyncio
import datetime
import logging
import os
from typing import AnyStr, Tuple, Union

from twitchio import Message, Channel, Chatter, Client
from twitchio.ext import routines

from ronnia.helpers.osu_api_helper import OsuApiV2, OsuChatApiV2
from ronnia.helpers.beatmap_link_parser import parse_beatmap_link
from ronnia.helpers.database_helper import RonniaDatabase
from ronnia.helpers.utils import convert_seconds_to_readable

logger = logging.getLogger(__name__)


class TwitchBot(Client):
    def __init__(self, initial_channel_names: set[str]):
        self.ronnia_db = RonniaDatabase(os.getenv("MONGODB_URL"))
        self.osu_api = OsuApiV2(
            os.getenv("OSU_CLIENT_ID"), os.getenv("OSU_CLIENT_SECRET")
        )
        self.osu_chat_api = OsuChatApiV2(
            os.getenv("OSU_CLIENT_ID"), os.getenv("OSU_CLIENT_SECRET")
        )

        initial_channels = [os.getenv("BOT_NICK"), *initial_channel_names]
        self.joined_channels = {user for user in initial_channels}
        token = os.getenv("TMI_TOKEN").replace("oauth:", "")
        args = {
            "token": token,
            "client_secret": os.getenv("TWITCH_CLIENT_SECRET"),
            "initial_channels": initial_channels,
        }
        logger.debug(f"Sending args to super().__init__: {args}")
        super().__init__(**args)

        self.environment = os.getenv("ENVIRONMENT")

        self._join_lock = asyncio.Lock()

        self.main_prefix = None
        self.user_last_request = {}

    async def streaming_channel_receiver(self):
        logger.info("Starting streaming channels message receiver")

        address = ("127.0.0.1", 31313)
        server = await asyncio.start_server(self.handle_bot_manager_message, *address)
        addr = server.sockets[0].getsockname()
        logger.info(f'TwitchBot started serving on {addr}')

        async with server:
            await server.serve_forever()

    async def handle_bot_manager_message(self, reader, writer):
        addr = writer.get_extra_info('peername')
        logger.info(f"TwitchBot received a new connection from {addr}")
        try:
            while True:
                try:
                    data = await asyncio.wait_for(reader.readline(), timeout=35)  # 30 seconds + 5 seconds buffer
                    raw_message = data.decode()
                    streaming_users = raw_message.strip("\n").split(",")
                    logger.info(f"Twitch Bot received {len(streaming_users)} from {addr}")
                    await self.join_streaming_channels(streaming_users)

                except asyncio.TimeoutError as e:
                    logger.exception(f"Timeout waiting for message from {addr}", exc_info=e)
                    continue

                except asyncio.IncompleteReadError as e:
                    logger.exception(f"Client {addr} disconnected", exc_info=e)
                    break

                except Exception as e:
                    logger.exception(f"Error handling client {addr}", exc_info=e)
                    break

        finally:
            print(f"Closing connection from {addr!r}")
            writer.close()
            await writer.wait_closed()

    async def join_streaming_channels(self, message: list[str]):
        streaming_users_set = set(message)
        new_channels = list(streaming_users_set.difference(self.joined_channels))
        closed_channels = list(self.joined_channels.difference(streaming_users_set))

        logger.info(f"Joining new channels: {new_channels}")
        logger.info(f"Parting closed channels: {closed_channels}")

        async with self._join_lock:
            await self.join_channels(new_channels)
            await self.part_channels(closed_channels)

        self.joined_channels = streaming_users_set
        return

    async def event_message(self, message: Message):
        if message.author is None:
            logger.info(f"{message.channel.name}: {message.content}")
            return
        logger.info(
            f"{message.channel.name} - {message.author.name}: {message.content}"
        )

        if self.environment == "testing":
            return

        try:
            await self.check_channel_enabled(message.channel.name)
            await self.handle_request(message)
        except AssertionError as e:
            logger.info(f"Check unsuccessful: {e}")

    async def handle_request(self, message: Message):
        given_mods, api_params = self._check_message_contains_beatmap_link(message)
        if given_mods is not None:
            if "s" in api_params:
                beatmap_info, beatmapset_info = await self.osu_api.get_beatmapset(
                    api_params["s"]
                )
            else:
                beatmap_info, beatmapset_info = await self.osu_api.get_beatmap(
                    api_params["b"]
                )

            if beatmap_info:
                await self.check_request_criteria(message, beatmap_info)
                # If user has enabled echo setting, send twitch chat a message

                logger.info(f"Sending beatmap {beatmap_info['id']} to user {message.channel.name}")
                if await self.ronnia_db.get_echo_status(
                        twitch_username=message.channel.name
                ):
                    logger.info(f"Sending echo message to {message.channel.name}")
                    await self._send_twitch_message(
                        message=message,
                        beatmap_info=beatmap_info,
                        beatmapset_info=beatmapset_info,
                    )

                await self._send_beatmap_to_in_game(
                    message=message,
                    beatmap_info=beatmap_info,
                    beatmapset_info=beatmapset_info,
                    given_mods=given_mods,
                )
                await self.ronnia_db.add_request(
                    requested_beatmap_id=int(beatmap_info["id"]),
                    requested_channel_name=message.channel.name,
                    requester_channel_name=message.author.name,
                    mods=given_mods,
                )
                logger.info(f"Adding beatmap {beatmap_info['id']} to database.")
                await self.ronnia_db.add_beatmap(
                    beatmap_info=beatmap_info
                )

    async def check_beatmap_star_rating(self, message: Message, beatmap_info):
        twitch_username = message.channel.name
        requester_name = message.author.name
        diff_rating = float(beatmap_info["difficulty_rating"])
        range_low, range_high = await self.ronnia_db.get_setting(
            twitch_username_or_id=twitch_username, setting_key="sr"
        )

        if range_low == -1 or range_high == -1:
            return

        assert range_low < diff_rating < range_high, (
            f"@{requester_name} Streamer is accepting requests between {range_low:.1f}-{range_high:.1f}* difficulty."
            f" Your map is {diff_rating:.1f}*."
        )

        return

    async def check_request_criteria(self, message: Message, beatmap_info: dict):
        test_status = await self.ronnia_db.get_test_status(message.channel.name)
        if not test_status and self.environment != "testing":
            await self.check_if_author_is_broadcaster(message)
            await self.check_if_streaming_osu(message.channel)
            await self._check_user_cooldown(
                author=message.author, channel=message.channel
            )

        await self.check_sub_only_mode(message)
        await self.check_cp_only_mode(message)
        await self.check_user_excluded(message)
        try:
            await self.check_beatmap_star_rating(message, beatmap_info)
        except AssertionError as e:
            await message.channel.send(str(e))
            raise AssertionError

    async def check_user_excluded(self, message: Message):
        excluded_users = await self.ronnia_db.get_excluded_users(
            twitch_username=message.channel.name
        )
        assert (
                message.author.name.lower() not in excluded_users
        ), f"{message.author.name} is excluded"

    async def check_sub_only_mode(self, message: Message):
        is_sub_only = await self.ronnia_db.get_setting("sub-only", message.channel.name)
        if is_sub_only:
            assert (
                    message.author.is_mod
                    or message.author.is_subscriber != "0"
                    or "vip" in message.author.badges
            ), "Subscriber only request mode is active."

    async def check_cp_only_mode(self, message):
        is_cp_only = await self.ronnia_db.get_setting(
            "points-only", message.channel.name
        )
        if is_cp_only:
            assert (
                    "custom-reward-id" in message.tags
            ), "Channel Points only mode is active."
        return

    async def event_error(self, error: Exception, data: str = None):
        logger.error(error, data)
        await super(TwitchBot, self).event_error(error, data)
        pass

    @staticmethod
    async def check_if_author_is_broadcaster(message: Message):
        assert (
                message.author.name != message.channel.name
        ), "Author is broadcaster and not in test mode."

        return

    async def global_before_hook(self, ctx):
        """
        Global hook that runs before every command.
        :param ctx: Message context
        :return:
        """
        user = await self.ronnia_db.get_user_from_twitch_username(ctx.author.name)
        assert user is not None, "User does not exist"
        assert (
                ctx.message.channel.name == ctx.author.name
        ), "Message is not in author's channel"

    async def check_if_streaming_osu(self, channel: Channel):
        """
        Checks if stream is on and they're playing osu!, otherwise ignores channel.
        :param channel: Channel of the message
        :return:
        """
        stream_list = await self.fetch_streams(user_logins=[channel.name])
        assert len(stream_list) == 1, f"{channel.name} stream is not on."
        stream = stream_list[0]
        assert stream.game_name == "osu!", f"{channel.name} stream is not playing osu!"

        return

    async def check_channel_enabled(self, channel_name):
        enabled = await self.ronnia_db.get_enabled_status(twitch_username=channel_name)
        assert enabled, f"Channel:{channel_name} is not open for requests"

    async def _check_user_cooldown(self, author: Chatter, channel: Channel):
        """
        Check if user is on cooldown, raise an exception if the user is on request cooldown.
        :param author: Twitch user object
        :return: Exception if user has requested a beatmap before channel_cooldown seconds passed.
        """
        author_id = author.id
        time_right_now = datetime.datetime.now()

        channel_cooldown = await self.ronnia_db.get_setting("cooldown", channel.name)
        await self._prune_cooldowns(time_right_now, channel_cooldown)

        if author_id not in self.user_last_request:
            self.user_last_request[author_id] = time_right_now
        else:
            last_message_time = self.user_last_request[author_id]
            seconds_since_last_request = (
                    time_right_now - last_message_time
            ).total_seconds()
            assert (
                    seconds_since_last_request >= channel_cooldown
            ), f"{author.name} is on cooldown for {channel_cooldown - seconds_since_last_request}."
            self.user_last_request[author_id] = time_right_now

        return

    async def _prune_cooldowns(
            self, time_right_now: datetime.datetime, channel_cooldown: int
    ):
        """
        Prunes users on that are on cooldown list so it doesn't get too cluttered.
        :param time_right_now:
        :return:
        """
        pop_list = []
        for user_id, last_message_time in self.user_last_request.items():
            seconds_since_last_request = (
                    time_right_now - last_message_time
            ).total_seconds()
            if seconds_since_last_request >= channel_cooldown:
                logger.info(
                    f"Removing cooldown for {user_id} "
                    f"since last request was {seconds_since_last_request} "
                    f"above {channel_cooldown}s cooldown"
                )
                pop_list.append(user_id)

        for user in pop_list:
            self.user_last_request.pop(user)

        return

    async def _send_beatmap_to_in_game(
            self,
            message: Message,
            beatmap_info: dict,
            beatmapset_info: dict,
            given_mods: str,
    ):
        """
        Sends the beatmap request message to osu!irc bot
        :param message: Twitch Message object
        :param beatmap_info: Dictionary containing beatmap information from osu! api
        :param given_mods: String of mods if they are requested, empty string instead
        :return:
        """
        irc_message = await self._prepare_irc_message(
            message=message,
            beatmap_info=beatmap_info,
            beatmapset_info=beatmapset_info,
            given_mods=given_mods,
        )
        target_id = (
            await self.ronnia_db.get_user_from_twitch_username(message.channel.name)
        ).osuId
        await self.osu_chat_api.send_message(target_id=target_id, message=irc_message)
        return

    @staticmethod
    async def _send_twitch_message(
            message: Message, beatmapset_info: dict, beatmap_info: dict
    ):
        """
        Sends twitch feedback message
        :param message: Twitch Message object
        :param beatmap_info: Dictionary containing beatmap information from osu! api
        :return:
        """
        artist = beatmapset_info["artist"]
        title = beatmapset_info["title"]
        version = beatmap_info["version"]
        bmap_info_text = f"{artist} - {title} [{version}]"
        await message.channel.send(f"{bmap_info_text} - Request sent!")
        return

    @staticmethod
    def _check_message_contains_beatmap_link(
            message: Message,
    ) -> Tuple[Union[AnyStr, None], Union[dict, None]]:
        """
        Splits message by space character and checks for possible beatmap links
        :param message: Twitch Message object
        :return:
        """
        logger.debug("Checking if message contains beatmap link")
        content = message.content

        for candidate_link in content.split(" "):
            result, mods = parse_beatmap_link(candidate_link, content)
            if result:
                logger.info(f"Found beatmap id: {result}")
                return mods, result
        else:
            logger.info("Couldn't find beatmap in message")
            return None, None

    @staticmethod
    async def _prepare_irc_message(
            message: Message,
            beatmap_info: dict,
            beatmapset_info: dict,
            given_mods: str,
    ):
        """
        Prepare beatmap request message to send to osu!irc.
        :param message: Twitch message
        :param beatmap_info: Beatmap info taken from osu!api as dictionary
        :param given_mods: Mods as string
        :return:
        """
        artist = beatmapset_info["artist"]
        title = beatmapset_info["title"]
        version = beatmap_info["version"]
        bpm = beatmap_info["bpm"]
        beatmap_status = str(beatmap_info["status"]).capitalize()
        difficultyrating = float(beatmap_info["difficulty_rating"])
        beatmap_id = beatmap_info["id"]
        beatmap_length = convert_seconds_to_readable(beatmap_info["hit_length"])
        beatmap_info = (
            f"[https://osu.ppy.sh/b/{beatmap_id} {artist} - {title} [{version}]] "
            f"({bpm} BPM, {difficultyrating:.2f}*, {beatmap_length}) {given_mods}"
        )
        extra_postfix = ""
        extra_prefix = ""

        badges = message.author.badges

        if message.author.is_mod:
            extra_prefix += "[MOD] "
        elif message.author.is_subscriber:
            extra_prefix += "[SUB] "
        elif "vip" in badges:
            extra_prefix += "[VIP] "

        if "custom-reward-id" in message.tags:
            extra_postfix += "+ USED POINTS"

        return f"{extra_prefix}{message.author.name} -> [{beatmap_status}] {beatmap_info} {extra_postfix}"

    async def event_ready(self):
        logger.info(f"Connected channels: {self.connected_channels}")
        logger.info("Successfully initialized bot!")
        logger.info(f"Ready | {self.nick}")
        _ = self.loop.create_task(self.streaming_channel_receiver())
        self.routine_show_connected_channels.start(stop_on_error=False)

    @routines.routine(minutes=1)
    async def routine_show_connected_channels(self):
        connected_channel_names = [
            channel.name for channel in list(filter(None, self.connected_channels))
        ]
        logger.info(f"Connected channels: {connected_channel_names}")
