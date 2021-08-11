import sqlite3
from typing import Union

import attr
import logging
from threading import Lock

import irc.bot
from irc.client import Event, ServerConnection

from helpers.database_helper import UserDatabase, StatisticsDatabase

logger = logging.getLogger('ronnia')


@attr.s
class RangeInput(object):
    range_low = attr.ib(converter=float)
    range_high = attr.ib(converter=float)


class IrcBot(irc.bot.SingleServerIRCBot):
    def __init__(self, channel, nickname, server, port=6667, password=None):
        reconnect_strategy = irc.bot.ExponentialBackoff(min_interval=5, max_interval=30)
        irc.bot.SingleServerIRCBot.__init__(self, [(server, port, password)], nickname, nickname,
                                            recon=reconnect_strategy)
        self.channel = channel
        self.users_db = UserDatabase()
        self.users_db.initialize()

        self.messages_db = StatisticsDatabase()
        self.messages_db.initialize()

        self.message_lock = Lock()

        self._commands = {'disable': self.disable_requests_on_channel,
                          'echo': self.toggle_notifications,
                          'feedback': self.toggle_notifications,
                          'enable': self.enable_requests_on_channel,
                          'register': self.register_bot_on_channel,
                          'help': self.show_help_message,
                          'setsr': self.set_sr_rating
                          }
        self.connection.set_rate_limit(1)

    def on_welcome(self, c: ServerConnection, e: Event):
        logger.info(f"Successfully joined irc!")
        c.join(self.channel)

    def send_message(self, target: str, cmd: str):
        target = target.replace(" ", "_")
        logger.info(f"Sending request in-game to {target}: {cmd}")
        self.connection.privmsg(target, cmd)

    def on_privmsg(self, c: ServerConnection, e: Event):
        self.do_command(e)

    def do_command(self, e: Event):
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
        existing_user = self.users_db.get_user_from_osu_username(db_nick)
        if existing_user is None and cmd == 'register':
            self.send_message(e.source.nick,
                              f'Hello! Thanks for your interest in this bot! '
                              f'But, registering for the bot automatically is not supported currently. '
                              f'I\'m hosting this bot with the free tier compute engine... '
                              f'So, if it gets too many requests it might blow up! '
                              f'That\'s why I\'m manually allowing requests right now. '
                              f'(Check out the project page if you haven\'t already.)'
                              f'[https://github.com/aticie/ronnia] '
                              f'Contact me on discord and I can enable it for you! heyronii#9925')
            return
        elif existing_user is None:
            self.send_message(e.source.nick, f'Sorry, you are not registered. '
                                             f'(Check out the project page for details.)'
                                             f'[https://github.com/aticie/ronnia]')
        else:
            # Check if command is valid
            try:
                self._commands[cmd](e, *args, user_details=existing_user)
            except KeyError:
                self.send_message(e.source.nick, f'Sorry, I couldn\'t understand what {cmd} means')
                pass

    def disable_requests_on_channel(self, event: Event, *args, user_details: Union[dict, sqlite3.Row]):
        """
        Disables requests on twitch channel
        :param event: Event of the current message
        :param user_details: User Details Sqlite row factory
        """
        twitch_username = user_details['twitch_username']
        osu_username = user_details['osu_username']
        logger.debug(f'Disable requests on channel: {osu_username}')
        self.users_db.disable_channel(twitch_username)
        self.send_message(event.source.nick,
                          f'I\'ve disabled requests for now. '
                          f'If you want to re-enable requests, type !enable anytime.')
        self.messages_db.add_command('disable', 'osu_irc', event.source.nick)

    def register_bot_on_channel(self, event: Event, *args, user_details: Union[dict, sqlite3.Row]):
        """
        Registers bot on twitch channel
        :param event: Event of the current message
        :param user_details: User Details Sqlite row factory

        Currently not supported... TODO: Register user -> ask twitch
        """
        logger.debug(f'Register bot on channel: {user_details}')

    def enable_requests_on_channel(self, event: Event, *args, user_details: Union[dict, sqlite3.Row]):
        """
        Enables requests on twitch channel
        :param user_details: User Details Sqlite row factory
        """
        twitch_username = user_details['twitch_username']
        logger.debug(f'Enable requests on channel - Current user details: {user_details}')
        self.users_db.enable_channel(twitch_username)
        self.send_message(event.source.nick,
                          f'I\'ve enabled requests. Have fun!')
        self.messages_db.add_command('enable', 'osu_irc', event.source.nick)

    def toggle_notifications(self, event: Event, *args, user_details: Union[dict, sqlite3.Row]):
        """
        Toggles echo notifications on twitch channel when requesting beatmaps
        :param user_details: User Details Sqlite row factory
        """
        twitch_username = user_details['twitch_username']
        logger.debug(f'Toggle notifications on channel: {event.source.nick}')
        new_echo_status = self.users_db.toggle_echo(twitch_username)
        if new_echo_status is True:
            self.send_message(event.source.nick, f'I\'ve enabled the beatmap request '
                                                 f'information feedback on your twitch chat!')
        else:
            self.send_message(event.source.nick, f'I\'ve disabled the beatmap request '
                                                 f'information feedback on your channel.')
        self.messages_db.add_command('echo', 'osu_irc', event.source.nick)
        pass

    def show_help_message(self, event: Event, *args, user_details: Union[dict, sqlite3.Row]):
        """
        Shows help message to user
        :param user_details: User Details Sqlite row factory
        :return:
        """
        logger.debug(f'Showing help message on channel: {event.source.nick}')
        self.send_message(event.source.nick,
                          f'Check out the (project page)[https://github.com/aticie/ronnia] for more information. '
                          f'List of available commands are (listed here)'
                          f'[https://github.com/aticie/ronnia/wiki/Commands].')
        self.messages_db.add_command('help', 'osu_irc', event.source.nick)

    def set_sr_rating(self, event: Event, *args, user_details: Union[dict, sqlite3.Row]):
        sr_text = ' '.join(args)
        try:
            range_input = RangeInput(*(sr_text.split('-')))
        except ValueError as e:
            self.send_message(event.source.nick, 'Invalid input.. For example, use: !sr 3.5-7.5')
            return

        twitch_username = user_details['twitch_username']
        try:
            new_low, new_high = self.users_db.set_sr_rating(twitch_username=twitch_username,
                                                            **attr.asdict(range_input))
        except AssertionError as e:
            self.send_message(event.source.nick, e)
            return
        self.send_message(event.source.nick, f'Changed star rating range between: {new_low:.1f} - {new_high:.1f}')
        self.messages_db.add_command('sr_rating', 'osu_irc', event.source.nick)
