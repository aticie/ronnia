import logging
import time
import asyncio
from threading import Lock

import irc.bot
from irc.client import Event, ServerConnection

from helpers.database_helper import UserDatabase

logger = logging.getLogger('ronnia')


class IrcBot(irc.bot.SingleServerIRCBot):
    def __init__(self, channel, nickname, server, port=6667, password=None):
        irc.bot.SingleServerIRCBot.__init__(self, [(server, port, password)], nickname, nickname)
        self.channel = channel
        self.users_db = UserDatabase()
        self.users_db.initialize()

        self.message_lock = Lock()

        self._commands = {'disable': self.disable_requests_on_channel,
                          'echo': self.toggle_notifications,
                          'enable': self.enable_requests_on_channel,
                          'register': self.register_bot_on_channel,
                          'help': self.show_help_message
                          }

    def on_welcome(self, c: ServerConnection, e: Event):
        c.join(self.channel)

    async def send_message(self, target: str, cmd: str):
        logger.debug(f"Sending irc message: {cmd}")
        with self.message_lock:
            c = self.connection
            c.privmsg(target, cmd)
            time.sleep(2)

    def on_privmsg(self, c: ServerConnection, e: Event):
        asyncio.run(self.do_command(e))

    async def do_command(self, e: Event):
        # Check if command starts with !
        cmd = e.arguments[0]
        if not cmd.startswith('!'):
            return

        # Make command lowercase
        cmd = cmd.lower()
        cmd = cmd[1:]
        cmd = cmd.split(' ')[0]

        db_nick = e.source.nick.lower()

        # Check if the user is registered
        existing_user = self.users_db.get_user_from_osu_username(db_nick)
        if existing_user is None and cmd == 'register':
            await self.send_message(e.source.nick,
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
            await self.send_message(e.source.nick, f'Sorry, you are not registered...')
        else:
            # Check if command is valid
            try:
                await self._commands[cmd](e, user_details=existing_user)
            except KeyError:
                await self.send_message(e.source.nick, f'Sorry, I couldn\'t understand what {cmd} means')
                pass

    async def disable_requests_on_channel(self, event: Event, user_details: tuple):
        """
        Disables requests on twitch channel
        :param event: Event of the current message
        :param user_details: Tuple of user details (user_id, osu! username, twitch username, enabled flag)
        """
        _, osu_username, twitch_username, enabled = user_details
        logger.debug(f'Disable requests on channel: {osu_username}')
        if enabled:
            self.users_db.disable_channel(twitch_username)
            await self.send_message(event.source.nick,
                                    f'I\'ve disabled requests for now. '
                                    f'If you want to re-enable requests, type !enable anytime.')
        else:
            await self.send_message(event.source.nick,
                                    f'Your requests are already disabled. If you want to enable them, type !enable.')

    async def register_bot_on_channel(self, event: Event, user_details: tuple):
        """
        Registers bot on twitch channel
        :param event: Event of the current message
        :param user_details: Tuple of user details (user_id, osu! username, twitch username, enabled flag)

        Currently not supported... TODO: Register user -> ask twitch
        """
        logger.debug(f'Register bot on channel: {user_details}')

    async def enable_requests_on_channel(self, event: Event, user_details: tuple):
        """
        Enables requests on twitch channel
        :param user_details: Tuple of user details (user_id, osu! username, twitch username, enabled flag)
        """
        _, osu_username, twitch_username, enabled = user_details
        logger.debug(f'Enable requests on channel: {osu_username}')
        if enabled:
            self.users_db.enable_channel(twitch_username)
            await self.send_message(event.source.nick,
                                    f'I\'ve enabled requests. Have fun!')
        else:
            await self.send_message(event.source.nick,
                                    f'Your requests are already enabled. If you want to disable them, type !disable.')

    async def toggle_notifications(self, event: Event, user_details: tuple):
        """
        Toggles echo notifications on twitch channel when requesting beatmaps
        :param user_details: Tuple of user details (user_id, osu! username, twitch username, enabled flag)
        """
        _, osu_username, twitch_username, enabled = user_details
        logger.debug(f'Toggle notifications on channel: {event.source.nick}')
        self.users_db.toggle_echo(twitch_username)
        await self.send_message(event.source.nick, f'I\'ve toggled echo. Check out your twitch chat!')
        pass

    async def show_help_message(self, event: Event, user_details: tuple):
        """
        Shows help message to user
        :param user_details: Tuple of user details (user_id, osu! username, twitch username, enabled flag)
        :return:
        """
        logger.debug(f'Showing help message on channel: {event.source.nick}')
        await self.send_message(event.source.nick,
                                f'Check out the (project page)[https://github.com/aticie/ronnia] for more information. '
                                f'List of available commands are (listed here)'
                                f'[https://github.com/aticie/ronnia/wiki/Commands].')
