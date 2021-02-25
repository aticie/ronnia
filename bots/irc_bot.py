import logging
import time
from threading import Lock

import irc.bot

logger = logging.getLogger('ronnia')


class IrcBot(irc.bot.SingleServerIRCBot):
    def __init__(self, channel, nickname, server, port=6667, password=None):
        irc.bot.SingleServerIRCBot.__init__(self, [(server, port, password)], nickname, nickname)
        self.channel = channel
        self.message_lock = Lock()

    def on_welcome(self, c, e):
        c.join(self.channel)

    def send_message(self, target, cmd):
        logger.info(f"Sending irc message: {cmd}")
        with self.message_lock:
            c = self.connection
            c.privmsg(target, cmd)
            time.sleep(2)
