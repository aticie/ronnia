import sqlite3
import os
import time


class BaseDatabase:
    def __init__(self, db_path: str):
        self.db_path: str = db_path
        self.conn: sqlite3.Connection = None
        self.c: sqlite3.Cursor = None

    def initialize(self):
        self.conn = sqlite3.connect(self.db_path, check_same_thread=False)
        self.c = self.conn.cursor()

    def dispose(self):
        self.conn.close()
        os.remove(self.db_path)
        del self


class UserDatabase(BaseDatabase):

    def __init__(self):
        super().__init__('users.db')

    def initialize(self):
        super().initialize()
        self.c.execute(
            f"CREATE TABLE IF NOT EXISTS users (user_id INTEGER PRIMARY KEY AUTOINCREMENT, "
            f"osu_username text UNIQUE NOT NULL, "
            f"twitch_username text, "
            f"enabled INTEGER);"
        )

        self.c.execute(
            f"CREATE TABLE IF NOT EXISTS user_settings (id INTEGER PRIMARY KEY AUTOINCREMENT, "
            f"key text, "
            f"value INTEGER, "
            f"user_id INTEGER);"
        )

        self.c.execute(
            f"CREATE TABLE IF NOT EXISTS settings (id INTEGER PRIMARY KEY AUTOINCREMENT, "
            f"key text UNIQUE, "
            f"default_value INTEGER, "
            f"description text);"
        )

        self.conn.commit()

        self.define_setting('echo', 1, 'Setting for the feedback message sent to twitch channel on beatmap request.')
        self.define_setting('enable', 1, 'Setting to enable beatmap requests channel-wide.')
        self.define_setting('sub-only', 0, 'Setting for sub-only requests mode.')
        self.define_setting('cp-only', 0, 'Setting for channel points only requests mode.')

    def add_user(self, twitch_username: str, osu_username: str) -> None:
        """
        Adds a user to database
        :param twitch_username:
        :param osu_username:
        :return:
        """
        self.c.execute(f"INSERT OR IGNORE INTO users (twitch_username, osu_username, enabled) VALUES (?, ?, ?)",
                       (twitch_username, osu_username, True))
        self.conn.commit()

    def get_user(self, osu_username: str) -> str:
        """
        Gets the twitch username of user from database
        :param osu_username:
        :return: twitch username associated with osu username
        """
        result = self.c.execute(f"SELECT twitch_username from users WHERE osu_username=?", (osu_username,))
        return result.fetchone()[0]

    def define_setting(self, key: str, default_value: int, description: str) -> None:
        """
        Define a new user specific setting
        :param key:
        :param default_value:
        :param description:
        :return:
        """
        self.c.execute(f"INSERT OR IGNORE INTO settings (key, default_value, description) VALUES (?, ?, ?)",
                       (key, default_value, description))
        self.conn.commit()
        return

    def get_echo_status(self, twitch_username: str) -> bool:
        """
        Gets echo setting of user
        :param osu_username: Must be given if twitch username is not given.
        :param twitch_username: Must be given if osu username is not given.
        :return: User's echo setting status
        """
        result = self.c.execute(
            f"SELECT value FROM user_settings "
            f"JOIN settings USING (key, key) "
            f"JOIN users USING (user_id, user_id) "
            f"WHERE key=? AND osu_username=?",
            ('echo', twitch_username))

        return bool(result.fetchone()[0])

    def toggle_echo(self, osu_username: str, twitch_username: str = None):
        """
        "SELECT * from user_settings
        JOIN settings USING ("key","key")
        JOIN users USING ("user_id", "user_id")
        WHERE key="echo" AND user_id=1;"
        :param osu_username:
        :param twitch_username:
        :return:
        """
        # Get current echo status
        sql_string = f"SELECT value, user_id from user_settings " \
                     f"JOIN settings USING (key, key) " \
                     f"JOIN users USING (user_id, user_id) " \
                     f"WHERE key=?"
        if osu_username is not None:
            sql_string += f"AND osu_username=?;"
            c = self.c.execute(sql_string,
                               ('echo', osu_username,))
        else:
            sql_string += f"AND twitch_username=?;"
            c = self.c.execute(sql_string,
                               ('echo', twitch_username,))
        result = c.fetchone()

        # Handle case when user has no default setting
        if result is None:

            inner_sql = f'SELECT user_id FROM users WHERE '
            if osu_username is not None:
                inner_sql += 'osu_username=?'
                cursor = self.c.execute(inner_sql, (osu_username,))
            else:
                inner_sql += 'twitch_username=?'
                cursor = self.c.execute(inner_sql, (twitch_username,))

            user_id = cursor.fetchone()[0]
            self.c.execute(f"INSERT INTO user_settings (key, value, user_id) VALUES (?,?,?)", ('echo', False, user_id))
            self.conn.commit()
            return False

        # When user has the setting -> update it
        else:
            value, user_id = result
            new_value = not bool(value)
            self.c.execute(f"UPDATE user_settings SET value=? WHERE user_id=? AND key=?", (new_value, user_id, 'echo'))
            self.conn.commit()

            return new_value

    def get_enabled_status(self, twitch_username: str):
        result = self.c.execute(
            f"SELECT enabled FROM users "
            f"WHERE twitch_username=?",
            (twitch_username,))

        return bool(result.fetchone()[0])

    def disable_channel(self, osu_username):
        self.c.execute(f"UPDATE users SET enabled=? WHERE osu_username=?", (False, osu_username,))
        self.conn.commit()

    def enable_channel(self, osu_username):
        self.c.execute(f"UPDATE users SET enabled=? WHERE osu_username=?", (True, osu_username,))
        self.conn.commit()


class BeatmapDatabase(BaseDatabase):
    # TODO: Save beatmap requests and recommend beatmaps from other streamers
    def __init__(self, db_path: str):
        super().__init__(db_path)

    def initialize(self):
        super().initialize()
        self.c.execute(
            f"CREATE TABLE IF NOT EXISTS beatmaps "
            f"(request_date text PRIMARY KEY NOT NULL, "
            f"beatmap_link TEXT, "
            f"requested_on TEXT, "
            f");"
        )
        self.conn.commit()


if __name__ == '__main__':
    start = time.time()
    test_db = UserDatabase()
    test_db.initialize()

    osu_username = 'heyronii'

    twitch_user = test_db.get_user(osu_username)
    echo_status = test_db.get_echo_status(twitch_user)
    enabled_status = test_db.get_enabled_status(twitch_username=twitch_user)

    print(f'{twitch_user} - Echo: {test_db.get_echo_status(twitch_user)} - Enabled: {test_db.get_enabled_status(twitch_username=twitch_user)}')
    print(f'Disabled requests')
    test_db.disable_channel(osu_username)
    print(f'{twitch_user} - Echo: {test_db.get_echo_status(twitch_user)} - Enabled: {test_db.get_enabled_status(twitch_username=twitch_user)}')
    print(f'Enabled requests')
    test_db.enable_channel(osu_username)
    print(f'{twitch_user} - Echo: {test_db.get_echo_status(twitch_user)} - Enabled: {test_db.get_enabled_status(twitch_username=twitch_user)}')
    print(f'Enabled requests')
    test_db.enable_channel(osu_username)
    print(f'{twitch_user} - Echo: {test_db.get_echo_status(twitch_user)} - Enabled: {test_db.get_enabled_status(twitch_username=twitch_user)}')
    for _ in range(3):
        print(f'Toggling echo')
        test_db.toggle_echo(osu_username)
        print(f'{twitch_user} - Echo: {test_db.get_echo_status(twitch_user)} - Enabled: {test_db.get_enabled_status(twitch_username=twitch_user)}')

    print(f'Testing done in {time.time() - start}')
    # test_db.define_setting('echo', 1, 'Setting for the feedback message sent to twitch channel on beatmap request.')
    # test_db.define_setting('enable', 1, 'Setting to enable beatmap requests channel-wide.')
    # test_db.define_setting('sub-only', 0, 'Setting for sub-only requests mode.')
    # test_db.define_setting('cp-only', 0, 'Setting for channel points only requests mode.')
