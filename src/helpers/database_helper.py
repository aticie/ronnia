import sqlite3
import os
import time
from datetime import datetime


class BaseDatabase:
    def __init__(self, db_path: str):
        self.db_path: str = db_path
        self.conn: sqlite3.Connection = None
        self.c: sqlite3.Cursor = None

    def initialize(self):
        self.conn = sqlite3.connect(self.db_path,
                                    check_same_thread=False,
                                    detect_types=sqlite3.PARSE_DECLTYPES | sqlite3.PARSE_COLNAMES)
        self.conn.row_factory = sqlite3.Row
        self.c = self.conn.cursor()

    def dispose(self):
        self.conn.close()
        os.remove(self.db_path)
        del self


class UserDatabase(BaseDatabase):

    def __init__(self, db_path=None):
        if db_path is None:
            db_path = os.path.join(os.getenv('DB_DIR'), 'users.db')
        super().__init__(db_path)

        self.sql_string_get_setting = f"SELECT value FROM user_settings " \
                                      f"INNER JOIN settings ON user_settings.key=settings.key " \
                                      f"INNER JOIN users ON users.user_id=user_settings.user_id " \
                                      f"WHERE user_settings.key=? AND users.twitch_username=?"

        self.sql_string_insert_setting = f"INSERT INTO user_settings (key, value, user_id) " \
                                         f"VALUES (?1, ?2, ?3);"

        self.sql_string_update_setting = f"UPDATE user_settings SET value=?2 WHERE key=?1 AND user_id=?3"

    def initialize(self):
        super().initialize()
        self.c.execute(
            f"CREATE TABLE IF NOT EXISTS users (user_id INTEGER PRIMARY KEY AUTOINCREMENT, "
            f"osu_username text UNIQUE NOT NULL, "
            f"twitch_username text NOT NULL, "
            f"enabled INTEGER,"
            f"twitch_id text NOT NULL,"
            f"osu_id text NOT NULL,"
            f"updated_at timestamp);"
        )

        self.c.execute(
            f"CREATE TABLE IF NOT EXISTS user_settings (key text, "
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
        self.define_setting('test', 0, 'Enables test mode on the channel.')

    def add_user(self,
                 twitch_username: str,
                 osu_username: str,
                 osu_user_id: str,
                 twitch_id: str,
                 enabled_status: bool = True) -> None:
        """
        Adds a user to database.
        """
        result = self.c.execute(f"SELECT * FROM users WHERE twitch_username=?",
                                (twitch_username,))
        user = result.fetchone()
        if user is None:
            self.c.execute(
                f"INSERT INTO users (twitch_username, twitch_id, osu_username, osu_id, enabled, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                (twitch_username, twitch_id, osu_username, osu_user_id, enabled_status, datetime.now()))
        else:
            self.c.execute(f"UPDATE users SET osu_username=?1, osu_id=?2, updated_at=?3 WHERE twitch_username=?4",
                           (osu_username, osu_user_id, datetime.now(), twitch_username))
        self.conn.commit()

    def update_user(self, new_twitch_username, new_osu_username, twitch_id, osu_user_id) -> None:
        """
        Updates an existing user in the database
        :param twitch_id: Twitch id of the user
        :param new_twitch_username: New Twitch username (possibly twitch id)
        :param osu_user_id: osu user id of the user
        :param new_osu_username: New osu username (possibly osu id)
        :return:
        """
        self.c.execute(f"UPDATE users SET twitch_username=?1, osu_username=?2, updated_at=?5 "
                       f"WHERE twitch_id=?3 AND osu_id=?4",
                       (new_twitch_username, new_osu_username, twitch_id, osu_user_id, datetime.now()))
        self.conn.commit()

    def remove_user(self, twitch_username: str) -> None:
        """
        Removes a user from database
        :param twitch_username: Twitch username.
        :return:
        """
        self.c.execute(f"DELETE FROM users WHERE twitch_username=?", (twitch_username,))
        self.conn.commit()

    def get_user_from_osu_username(self, osu_username: str) -> str:
        """
        Gets the user details from database using osu username
        :param osu_username: osu username
        :return: User details of the user associated with osu username
        """
        result = self.c.execute(f"SELECT * from users WHERE osu_username=?", (osu_username,))
        return result.fetchone()

    def get_user_from_twitch_username(self, twitch_username: str) -> str:
        """
        Gets the user details from database using Twitch username
        :param twitch_username:
        :return: User details of the user associated with twitch username
        """
        result = self.c.execute(f"SELECT * from users WHERE twitch_username=?", (twitch_username,))
        return result.fetchone()

    def define_setting(self, key: str, default_value: int, description: str) -> None:
        """
        Define a new user specific setting
        :param key: Setting key
        :param default_value: Default value of the setting
        :param description: Description of the setting
        :return:
        """
        result = self.c.execute(f"SELECT * FROM settings WHERE key=?", (key,))
        setting = result.fetchone()
        if setting is None:
            self.c.execute(f"INSERT INTO settings (key, default_value, description) VALUES (?, ?, ?)",
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
        return self.get_setting('echo', twitch_username)

    def toggle_setting(self, setting_key: str, twitch_username: str):
        """
        Toggles setting of given user
        :param setting_key: Key of the setting
        :param twitch_username: Twitch username
        :return:
        """
        # Get current status of setting
        cur_value = self.get_setting(setting_key, twitch_username)
        # Toggle it
        new_value = not cur_value
        # Set new value to setting
        self.set_setting(setting_key, twitch_username, new_value)
        # Return new value
        return new_value

    def toggle_echo(self, twitch_username: str = None):
        """
        Toggles echo status of the user
        :param twitch_username:
        :return:
        """
        return self.toggle_setting('echo', twitch_username)

    def get_enabled_status(self, twitch_username: str):
        """
        Returns if the channel has enabled requests or not
        :param twitch_username: Twitch username of the requested user
        :return: Channel enabled status
        """
        result = self.c.execute(
            f"SELECT enabled FROM users "
            f"WHERE twitch_username=?",
            (twitch_username,))

        return bool(result.fetchone()[0])

    def disable_channel(self, twitch_username: str):
        self.c.execute(f"UPDATE users SET enabled=? WHERE twitch_username=?", (False, twitch_username,))
        self.conn.commit()

    def enable_channel(self, twitch_username: str):
        self.c.execute(f"UPDATE users SET enabled=? WHERE twitch_username=?", (True, twitch_username,))
        self.conn.commit()

    def get_test_status(self, twitch_username: str):
        """
        Gets the user's setting for test mode
        :param twitch_username: Twitch username
        :return:
        """
        return self.get_setting('test', twitch_username)

    def handle_none_type_setting(self, value: str, setting_key: str):
        """
        If a setting is none, gets the default value for that setting from the database
        :param value: Current value of the key - could be None or a tuple
        :param setting_key: Requested setting key
        :return: Default or current value of the setting
        """
        if value is None:
            r = self.c.execute(f"SELECT default_value FROM settings WHERE key=?", (setting_key,))
            value = r.fetchone()
        return bool(value[0])

    def get_setting(self, setting_key: str, twitch_username: str):
        """
        Get the setting's current value for user
        :param setting_key: Key of the setting
        :param twitch_username: Twitch username
        :return:
        """
        result = self.c.execute(self.sql_string_get_setting, (setting_key, twitch_username))
        value = result.fetchone()
        return self.handle_none_type_setting(value, setting_key)

    def set_setting(self, setting_key, twitch_username, new_value):
        """
        Set a new value for a setting of user
        :param setting_key: Setting key
        :param twitch_username: Twitch username
        :param new_value: New value of the desired setting
        :return:
        """
        user_details = self.get_user_from_twitch_username(twitch_username)
        user_id = user_details['user_id']
        result = self.c.execute(self.sql_string_get_setting, (setting_key, twitch_username))
        value = result.fetchone()
        if value is None:
            self.c.execute(self.sql_string_insert_setting, (setting_key, new_value, user_id))
        else:
            self.c.execute(self.sql_string_update_setting, (setting_key, new_value, user_id))
        self.conn.commit()
        return new_value

    def get_all_users(self):
        """
        Gets all users in db
        :return:
        """
        result = self.c.execute("SELECT * FROM users;")
        value = result.fetchall()
        return value


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

    tw_username = 'heyronii'

    test_db.add_user('heyronii', 'heyronii')

    print(
        f'{tw_username} - Echo: {test_db.get_echo_status(tw_username)} - Enabled: {test_db.get_enabled_status(twitch_username=tw_username)}')
    print(f'Disabled requests')
    test_db.disable_channel(tw_username)
    print(
        f'{tw_username} - Echo: {test_db.get_echo_status(tw_username)} - Enabled: {test_db.get_enabled_status(twitch_username=tw_username)}')
    print(f'Enabled requests')
    test_db.enable_channel(tw_username)
    print(
        f'{tw_username} - Echo: {test_db.get_echo_status(tw_username)} - Enabled: {test_db.get_enabled_status(twitch_username=tw_username)}')
    print(f'Enabled requests')
    test_db.enable_channel(tw_username)
    print(
        f'{tw_username} - Echo: {test_db.get_echo_status(tw_username)} - Enabled: {test_db.get_enabled_status(twitch_username=tw_username)}')
    for _ in range(3):
        print(f'Toggling echo')
        test_db.toggle_echo(tw_username)
        print(
            f'{tw_username} - Echo: {test_db.get_echo_status(tw_username)} - Enabled: {test_db.get_enabled_status(twitch_username=tw_username)}')

    print(f'Testing done in {time.time() - start}')