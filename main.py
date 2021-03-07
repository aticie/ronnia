import logging
from logging.handlers import TimedRotatingFileHandler

from dotenv import load_dotenv
from bots.twitch_bot import TwitchBot

load_dotenv()

logger = logging.getLogger('ronnia')
logger.setLevel(logging.DEBUG)
loggers_formatter = logging.Formatter(
    '%(asctime)s | %(levelname)s | %(process)d | %(name)s | %(funcName)s | %(message)s',
    datefmt='%d/%m/%Y %I:%M:%S')

ch = logging.StreamHandler()
ch.setFormatter(loggers_formatter)
ch.setLevel(logging.INFO)

fh = TimedRotatingFileHandler(
    filename='ronnia.log', when='midnight', backupCount=30)
fh.setFormatter(loggers_formatter)
fh.setLevel(logging.DEBUG)

logger.addHandler(ch)
logger.addHandler(fh)

if __name__ == "__main__":
    bot = TwitchBot()
    bot.run()
