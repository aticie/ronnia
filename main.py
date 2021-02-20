import json
import logging

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
logger.addHandler(ch)

if __name__ == "__main__":
    # channels is a dict of twitch_channel: osu_nickname
    with open("channels.json") as f:
        channel_mappings = json.load(f)

    bot = TwitchBot(channel_mappings)
    bot.run()
