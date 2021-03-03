import os
import json
import logging

from dotenv import load_dotenv
from bots.twitch_bot import TwitchBot
from helpers.database_helper import UserDatabase

load_dotenv()

LOGLEVEL = os.getenv('LOG_LEVEL', 'INFO').upper()

logger = logging.getLogger('ronnia')
logger.setLevel(LOGLEVEL)
loggers_formatter = logging.Formatter(
        '%(asctime)s | %(levelname)s | %(process)d | %(name)s | %(funcName)s | %(message)s',
        datefmt='%d/%m/%Y %I:%M:%S')

ch = logging.StreamHandler()
ch.setFormatter(loggers_formatter)
logger.addHandler(ch)

if __name__ == "__main__":
    # channels is a dict of twitch_channel: osu_nickname
    if os.getenv('MIGRATE_USERS', None):
        logger.info('Migrating users to database!')
        with open("channels.json") as f:
            channel_mappings = json.load(f)

        users_db = UserDatabase()
        users_db.initialize()
        for twitch, osu in channel_mappings.items():
            users_db.add_user(osu_username=osu, twitch_username=twitch)

        logger.info('Migration Complete!')

    bot = TwitchBot()
    bot.run()
