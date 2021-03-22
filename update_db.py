import os
import sys
from abc import ABC

from twitchio.ext import commands
from dotenv import load_dotenv

from helpers.database_helper import UserDatabase
from helpers.osu_api_helper import OsuApiHelper

load_dotenv()


class TwitchBot(commands.Bot, ABC):

    def __init__(self):
        self.users_db_new = UserDatabase(db_path='users_new.db')
        self.users_db = UserDatabase(db_path='users.db')
        self.users_db_new.initialize()
        self.users_db.initialize()

        self.all_users = self.users_db.get_all_users()

        args = {
            'irc_token': os.getenv('TMI_TOKEN'),
            'client_id': os.getenv('CLIENT_ID'),
            'client_secret': os.getenv('CLIENT_SECRET'),
            'nick': os.getenv('BOT_NICK'),
            'prefix': os.getenv('BOT_PREFIX')
        }
        super().__init__(**args)
        self.osu_api = OsuApiHelper()

    async def event_ready(self):
        twitch_users = [user[2] for user in self.all_users]
        twitch_users_info = await self.get_users(*twitch_users)

        for _, osu_user, twitch_user, enabled in self.all_users:
            osu_user_info = await self.osu_api.get_user_info(osu_user)
            new_osu_id = osu_user_info['user_id']
            new_twitch_id = [ch.id for ch in twitch_users_info if ch.login == twitch_user][0]
            self.users_db_new.add_user(twitch_username=twitch_user,
                                       twitch_id=new_twitch_id,
                                       osu_user_id=new_osu_id,
                                       osu_username=osu_user,
                                       enabled_status=enabled)

        user_settings = self.users_db.c.execute('SELECT * FROM user_settings;').fetchall()
        for key, value, user_id in user_settings:
            self.users_db_new.c.execute('INSERT INTO user_settings VALUES (?, ?, ?)', (key, value, user_id))
        self.users_db_new.conn.commit()

        print('Migration completed.')
        sys.exit(0)


if __name__ == '__main__':
    bot = TwitchBot()
    bot.run()
