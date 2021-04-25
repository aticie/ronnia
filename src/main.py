import logging
import os

from bots.twitch_bot import TwitchBot

logger = logging.getLogger('ronnia')
logger.setLevel(os.getenv('LOG_LEVEL').upper())
loggers_formatter = logging.Formatter(
    '%(asctime)s | %(levelname)s | %(process)d | %(name)s | %(funcName)s | %(message)s',
    datefmt='%d/%m/%Y %I:%M:%S')

ch = logging.StreamHandler()
ch.setFormatter(loggers_formatter)
logger.addHandler(ch)
logger.propagate = False

if __name__ == "__main__":
    bot = TwitchBot()
    bot.run()
