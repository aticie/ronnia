import logging
import time
import asyncio
from queue import Queue
from threading import Lock

import irc.bot
from irc.client import Event, ServerConnection

from helpers.database_helper import UserDatabase

logger = logging.getLogger('ronnia')


class IrcBot(irc.bot.SingleServerIRCBot):
    def __init__(self, channel, nickname, server, port=6667, password=None, shared_message_queue=None):
        irc.bot.SingleServerIRCBot.__init__(self, [(server, port, password)], nickname, nickname)
        self.channel = channel
        self.users_db = UserDatabase()
        self.users_db.initialize()

        self.shared_message_queue: Queue = shared_message_queue
        self.message_lock = Lock()

        self._commands = {'disable': self.disable_requests_on_channel,
                          'echo': self.toggle_notifications,
                          'enable': self.enable_requests_on_channel,
                          'register': self.register_bot_on_channel}

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

        # Check if the user is registered
        existing_user = self.users_db.get_user(e.target)
        if existing_user is not None and cmd != 'register':
            await self.send_message(f'Sorry, you are not registered to this bot. '
                                    f'I\'m not allowing automatic registrations as of yet. '
                                    f'You can dm me on discord about it (heyronii#9925). '
                                    f'For more info, type !help')
            return
        else:
            # Check if command is valid
            try:
                await self._commands[cmd](e)
            except KeyError:
                await self.send_message(e.target, f'Sorry, I couldn\'t understand what {cmd} means')
                pass

    async def disable_requests_on_channel(self, event: Event):
        """
        Disables requests on twitch channel
        :param event: Required parameter for event target (osu! username)
        """
        logger.debug(f'Disable requests on channel: {event.target}')
        self.shared_message_queue.put(('disable', event.target))
        await self.send_message(event.target,
                                f'I\'ve disabled requests for now. '
                                f'If you want to re-enable requests, type !enable anytime.')

        pass

    async def register_bot_on_channel(self, event: Event):
        """
        Registers bot on twitch channel
        :param event: Required parameter for event target (osu! username)

        Currently not supported...
        """
        logger.debug(f'Register bot on channel: {event.target}')
        await self.send_message(event.target,
                                f'Hello! Thanks for your interest in this bot! '
                                f'But, registering for the bot automatically is not supported currently. '
                                f'I\'m hosting this bot with the free tier compute engine... '
                                f'So, if it gets too many requests it might blow up! '
                                f'That\'s why I\'m manually allowing requests right now. '
                                f'(Check out the project page if you haven\'t already.)'
                                f'[https://github.com/aticie/ronnia] '
                                f'Contact me on discord and I can enable it for you! heyronii#9925')
        pass

    async def enable_requests_on_channel(self, event: Event):
        """
        Enables requests on twitch channel
        :param event: Required parameter for event target (osu! username)
        """
        logger.debug(f'Enable requests on channel: {event.target}')
        self.shared_message_queue.put(('enable', event.target))
        await self.send_message(event.target,
                                f'I\'ve enabled requests. Have fun!')

    async def toggle_notifications(self, event: Event):
        """
        Toggles echo notifications on twitch channel when requesting beatmaps
        :param event: Required parameter for event target (osu! username)
        """
        logger.debug(f'Toggle notifications on channel: {event.target}')
        self.shared_message_queue.put(('echo', event.target))
        await self.send_message(event.target, f'I\'ve toggled echo. Check out your twitch chat!')
        pass
