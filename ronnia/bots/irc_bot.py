import asyncio
import json
import logging
import os
import sqlite3
import traceback
from typing import Union

import attr
import irc.bot
from azure.servicebus.aio import ServiceBusClient
from azure.servicebus.exceptions import ServiceBusError
from irc.client import Event, ServerConnection

from ronnia.helpers.database_helper import UserDatabase, StatisticsDatabase

logger = logging.getLogger(__name__)


@attr.s
class RangeInput(object):
    range_low = attr.ib(converter=float)
    range_high = attr.ib(converter=float)


class IrcBot(irc.bot.SingleServerIRCBot):
    def __init__(self, nickname, server, port=6667, password=None):
        reconnect_strategy = irc.bot.ExponentialBackoff(min_interval=5, max_interval=30)
        irc.bot.SingleServerIRCBot.__init__(self, [(server, port, password)], nickname, nickname,
                                            recon=reconnect_strategy)
        self.users_db = UserDatabase()
        self.messages_db = StatisticsDatabase()

        self.servicebus_connection_string = os.getenv('SERVICE_BUS_CONNECTION_STR')
        self.servicebus_client = ServiceBusClient.from_connection_string(conn_str=self.servicebus_connection_string)
        self.listen_queue_name = 'twitch-to-irc'

        self._loop = asyncio.get_event_loop()
        self.environment = os.getenv('ENVIRONMENT').lower()

        self._commands = {'disable': self.disable_requests_on_channel,
                          'echo': self.toggle_notifications,
                          'feedback': self.toggle_notifications,
                          'enable': self.enable_requests_on_channel,
                          'help': self.show_help_message,
                          'setsr': self.set_sr_rating
                          }
        self.connection.set_rate_limit(1)

    def start(self):
        self._loop.run_until_complete(self.users_db.initialize())
        self._loop.run_until_complete(self.messages_db.initialize())
        logger.debug(f"Successfully initialized databases!")
        servicebus_task = self._loop.create_task(self.receive_servicebus_queue())
        super_start_future = self._loop.run_in_executor(None, super().start)
        self._loop.run_until_complete(asyncio.gather(servicebus_task, super_start_future, loop=self._loop))

    async def receive_servicebus_queue(self):
        while True:
            try:
                logger.info('Starting to listen to queue')
                receiver = self.servicebus_client.get_queue_receiver(queue_name=self.listen_queue_name)
                async for message in receiver:
                    logger.info(f'Received message from service bus: {str(message)}')
                    message_dict = json.loads(str(message))
                    target_channel = message_dict['target_channel']
                    message_contents = message_dict['message']
                    self.send_message(target_channel, message_contents)
            except ServiceBusError as e:
                logger.error(f'Error with the irc-bot receiver: {e}')
                logger.error(traceback.format_exc())
                await asyncio.sleep(5)

    def on_welcome(self, c: ServerConnection, e: Event):
        logger.info(f"Successfully connected to osu! irc as: {self._nickname}")

    def send_message(self, target: str, cmd: str):
        target = target.replace(" ", "_")
        logger.info(f"Sending request in-game to {target}: {cmd}")
        self.connection.privmsg(target, cmd)

    def on_privmsg(self, c: ServerConnection, e: Event):
        logger.debug(f'Received message from irc: {e}')
        try:
            self._loop.create_task(self.do_command(e))
        except Exception as exp:
            logger.error(f'Error while processing command {e}: {exp}')
            traceback.print_exc()

    async def do_command(self, e: Event):
        if self.environment == 'testing':
            return

        # Check if command starts with !
        cmd = e.arguments[0]
        if not cmd.startswith('!'):
            return

        # Make command lowercase
        cmd = cmd.lower()
        cmd = cmd[1:]
        args = cmd.split(' ')[1:]
        cmd = cmd.split(' ')[0]

        db_nick = e.source.nick.lower()

        # Check if the user is registered
        existing_user = await self.users_db.get_user_from_osu_username(db_nick)
        if existing_user is None:
            self.send_message(e.source.nick,
                              f'Please register your osu! account (from here)[https://ronnia.me/].')
            return
        else:
            # Check if command is valid
            try:
                await self._commands[cmd](e, *args, user_details=existing_user)
            except KeyError:
                self.send_message(e.source.nick, f'Sorry, I couldn\'t understand what {cmd} means')

    async def disable_requests_on_channel(self, event: Event, *args, user_details: Union[dict, sqlite3.Row]):
        """
        Disables requests on twitch channel
        :param event: Event of the current message
        :param user_details: User Details Sqlite row factory
        """
        twitch_username = user_details['twitch_username']
        osu_username = user_details['osu_username']
        logger.debug(f'Disable requests on channel: {osu_username}')
        await self.users_db.disable_channel(twitch_username)
        self.send_message(event.source.nick,
                          f'I\'ve disabled requests for now. '
                          f'If you want to re-enable requests, type !enable anytime.')
        await self.messages_db.add_command('disable', 'osu_irc', event.source.nick)

    async def enable_requests_on_channel(self, event: Event, *args, user_details: Union[dict, sqlite3.Row]):
        """
        Enables requests on twitch channel
        :param user_details: User Details Sqlite row factory
        """
        twitch_username = user_details['twitch_username']
        logger.debug(f'Enable requests on channel - Current user details: {user_details}')
        await self.users_db.enable_channel(twitch_username)
        self.send_message(event.source.nick,
                          f'I\'ve enabled requests. Have fun!')
        await self.messages_db.add_command('enable', 'osu_irc', event.source.nick)

    async def toggle_notifications(self, event: Event, *args, user_details: Union[dict, sqlite3.Row]):
        """
        Toggles echo notifications on twitch channel when requesting beatmaps
        :param user_details: User Details Sqlite row factory
        """
        twitch_username = user_details['twitch_username']
        logger.debug(f'Toggle notifications on channel: {event.source.nick}')
        new_echo_status = await self.users_db.toggle_echo(twitch_username)
        if new_echo_status is True:
            self.send_message(event.source.nick, f'I\'ve enabled the beatmap request '
                                                 f'information feedback on your twitch chat!')
        else:
            self.send_message(event.source.nick, f'I\'ve disabled the beatmap request '
                                                 f'information feedback on your channel.')
        await self.messages_db.add_command('echo', 'osu_irc', event.source.nick)
        pass

    async def show_help_message(self, event: Event, *args, user_details: Union[dict, sqlite3.Row]):
        """
        Shows help message to user
        :param user_details: User Details Sqlite row factory
        :return:
        """
        logger.debug(f'Showing help message on channel: {event.source.nick}')
        self.send_message(event.source.nick,
                          f'Check out the (project page)[https://github.com/aticie/ronnia] for more information. '
                          f'List of available commands are (listed here)'
                          f'[https://github.com/aticie/ronnia/wiki/Commands]. '
                          f'(Click here)[https://ronnia.me/ to access your dashboard. )')
        await self.messages_db.add_command('help', 'osu_irc', event.source.nick)

    async def set_sr_rating(self, event: Event, *args, user_details: Union[dict, sqlite3.Row]):
        sr_text = ' '.join(args)
        try:
            range_input = RangeInput(*(sr_text.split('-')))
        except ValueError as e:
            self.send_message(event.source.nick, 'Invalid input.. For example, use: !sr 3.5-7.5')
            return

        twitch_username = user_details['twitch_username']
        try:
            new_low, new_high = await self.users_db.set_sr_rating(twitch_username=twitch_username,
                                                                  **attr.asdict(range_input))
        except AssertionError as e:
            self.send_message(event.source.nick, e)
            return
        self.send_message(event.source.nick, f'Changed star rating range between: {new_low:.1f} - {new_high:.1f}')
        await self.messages_db.add_command('sr_rating', 'osu_irc', event.source.nick)
