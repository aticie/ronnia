import os
import sqlite3
from datetime import datetime
from typing import Tuple, Optional, List, Union


class BaseDatabase:
    def __init__(self, db_path: str):
        self.db_path: str = db_path
        self.conn: Optional[sqlite3.Connection] = None
        self.c: Optional[sqlite3.Cursor] = None

    def initialize(self):
        self.conn = sqlite3.connect(self.db_path,
                                    check_same_thread=False,
                                    detect_types=sqlite3.PARSE_DECLTYPES | sqlite3.PARSE_COLNAMES)
        self.conn.row_factory = sqlite3.Row
        self.conn.execute('pragma journal_mode=wal')
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

        self.sql_string_get_range_setting = f"SELECT range_start, range_end FROM user_range_settings " \
                                            f"INNER JOIN range_settings ON user_range_settings.key=range_settings.key " \
                                            f"INNER JOIN users ON users.user_id=user_range_settings.user_id " \
                                            f"WHERE user_range_settings.key=? AND users.twitch_username=?"

        self.sql_string_insert_setting = f"INSERT INTO user_settings (key, value, user_id) " \
                                         f"VALUES (?1, ?2, ?3);"

        self.sql_string_insert_range_setting = f"INSERT INTO user_range_settings (key, range_start, range_end, user_id) " \
                                               f"VALUES (?1, ?2, ?3, ?4);"

        self.sql_string_update_setting = f"UPDATE user_settings SET value=?2 WHERE key=?1 AND user_id=?3"

        self.sql_string_update_range_setting = f"UPDATE user_range_settings SET range_start=?2, range_end=?3 " \
                                               f"WHERE key=?1 AND user_id=?4"

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
            f"CREATE TABLE IF NOT EXISTS user_range_settings (user_id INTEGER, "
            f"range_start REAL,"
            f"range_end REAL,"
            f"key text);"
        )

        self.c.execute(
            f"CREATE TABLE IF NOT EXISTS settings (id INTEGER PRIMARY KEY AUTOINCREMENT, "
            f"key text UNIQUE, "
            f"default_value INTEGER, "
            f"description text);"
        )

        self.c.execute(
            f"CREATE TABLE IF NOT EXISTS range_settings (id INTEGER PRIMARY KEY AUTOINCREMENT, "
            f"key text UNIQUE, "
            f"default_low REAL, "
            f"default_high REAL, "
            f"description text);"
        )

        self.c.execute(
            f"CREATE TABLE IF NOT EXISTS exclude_list (user_id INTEGER UNIQUE, "
            f"excluded_user text NOT NULL);"
        )

        self.conn.commit()

        self.define_setting('echo', 1, 'Setting for the feedback message sent to twitch channel on beatmap request.')
        self.define_setting('enable', 1, 'Setting to enable beatmap requests channel-wide.')
        self.define_setting('sub-only', 0, 'Setting for sub-only requests mode.')
        self.define_setting('cp-only', 0, 'Setting for channel points only requests mode.')
        self.define_setting('test', 0, 'Enables test mode on the channel.')
        self.define_range_setting('sr', -1, -1, 'Set star rating limit for requests.')

    def set_channel_updated(self, twitch_username: str):
        self.c.execute(f'UPDATE users SET enabled=? WHERE twitch_username=?', (1, twitch_username))
        self.conn.commit()
        return

    def add_user(self,
                 twitch_username: str,
                 osu_username: str,
                 osu_user_id: str,
                 twitch_id: str,
                 enabled_status: bool = True) -> None:
        """
        Adds a user to database.
        """
        twitch_username = twitch_username.lower()
        osu_username = osu_username.lower().replace(' ', '_')

        result = self.c.execute(f"SELECT * FROM users WHERE twitch_username=?",
                                (twitch_username,))
        user = result.fetchone()
        if user is None:
            self.c.execute(
                f"INSERT INTO users (twitch_username, twitch_id, osu_username, osu_id, enabled, updated_at)"
                f" VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                (twitch_username, twitch_id, osu_username, osu_user_id, enabled_status, datetime.now()))
        else:
            self.c.execute(f"UPDATE users SET osu_username=?1, osu_id=?2, updated_at=?3 WHERE twitch_username=?4",
                           (osu_username, osu_user_id, datetime.now(), twitch_username))
        self.conn.commit()

    def update_user(self, new_twitch_username: str, new_osu_username: str, twitch_id: str, osu_user_id: str) -> None:
        """
        Updates an existing user in the database
        :param twitch_id: Twitch id of the user
        :param new_twitch_username: New Twitch username (possibly twitch id)
        :param osu_user_id: osu user id of the user
        :param new_osu_username: New osu username (possibly osu id)
        :return:
        """
        new_twitch_username = new_twitch_username.lower()
        new_osu_username = new_osu_username.lower().replace(' ', '_')

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
        twitch_username = twitch_username.lower()

        self.c.execute(f"DELETE FROM users WHERE twitch_username=?", (twitch_username,))
        self.conn.commit()

    def get_user_from_osu_username(self, osu_username: str) -> sqlite3.Row:
        """
        Gets the user details from database using osu username
        :param osu_username: osu username
        :return: User details of the user associated with osu username
        """
        osu_username = osu_username.lower().replace(' ', '_')

        result = self.c.execute(f"SELECT * from users WHERE osu_username=?", (osu_username,))
        return result.fetchone()

    def get_user_from_twitch_username(self, twitch_username: str) -> sqlite3.Row:
        """
        Gets the user details from database using Twitch username
        :param twitch_username:
        :return: User details of the user associated with twitch username
        """
        twitch_username = twitch_username.lower()

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

    def define_range_setting(self, key: str, default_low: float, default_high: float, description: str) -> None:
        """
        Define a new user specific setting
        :param key: Setting key
        :param default_low: Default lower value of the range
        :param default_high: Default higher value of the range
        :param description: Description of the setting
        :return:
        """
        result = self.c.execute(f"SELECT * FROM range_settings WHERE key=?", (key,))
        setting = result.fetchone()
        if setting is None:
            self.c.execute(f"INSERT INTO range_settings (key, default_low, default_high, description)"
                           f" VALUES (?, ?, ?, ?)",
                           (key, default_low, default_high, description))
            self.conn.commit()
        return

    def get_echo_status(self, twitch_username: str) -> bool:
        """
        Gets echo setting of user
        :param twitch_username: Twitch username.
        :return: User's echo setting status
        """
        return self.get_setting('echo', twitch_username)

    def toggle_setting(self, setting_key: str, twitch_username: str):
        """
        Toggles setting of given user
        :param setting_key: Key of the setting
        :param twitch_username: Twitch username
        :return: New value of the toggled setting.
        """
        twitch_username = twitch_username.lower()

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
        return self.get_setting('enable', twitch_username)

    def disable_channel(self, twitch_username: str):
        self.set_setting(setting_key='enable', twitch_username=twitch_username, new_value=0)

    def enable_channel(self, twitch_username: str):
        self.set_setting(setting_key='enable', twitch_username=twitch_username, new_value=1)

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

    def handle_none_type_range_setting(self, value: Optional[sqlite3.Row], setting_key: str):
        """
        If a setting is none, gets the default value for that setting from the database
        :param value: Current value of the key - could be None or a tuple
        :param setting_key: Requested setting key
        :return: Default or current value of the setting
        """
        if value is None:
            r = self.c.execute(f"SELECT default_low, default_high FROM range_settings WHERE key=?", (setting_key,))
            value = r.fetchone()
            return value['default_low'], value['default_high']
        return value['range_start'], value['range_end']

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
        twitch_username = twitch_username.lower()

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

    def toggle_sub_only(self, twitch_username: str) -> bool:
        """
        Toggles sub-only mode on the channel.
        :param twitch_username: Twitch username
        :return: New sub-only setting.
        """
        return self.toggle_setting('sub-only', twitch_username=twitch_username)

    def set_sr_rating(self, twitch_username: str, range_low: float, range_high: float) -> Tuple[float, float]:
        """
        Sets star rating range for user.
        :param twitch_username: Twitch username
        :param range_low: Lower value of the range
        :param range_high: Higher value of the range
        :return: New value tuple
        """
        return self.set_range_setting(twitch_username=twitch_username,
                                      setting_key='sr',
                                      range_low=range_low,
                                      range_high=range_high)

    def get_range_setting(self, twitch_username: str, setting_key: str):
        """
        Gets the range setting from database.
        :param twitch_username: Twitch username
        :param setting_key: Setting key
        :return:
        """
        result = self.c.execute(self.sql_string_get_range_setting, (setting_key, twitch_username))
        value = result.fetchone()
        return self.handle_none_type_range_setting(value, setting_key)

    def set_range_setting(self, twitch_username: str, setting_key: str, range_low: float, range_high: float):
        """
        Sets a range setting with given key
        :param twitch_username: Twitch username
        :param setting_key: Setting key
        :param range_low: Lower value of the range
        :param range_high: Higher value of the range
        :return: Tuple: New range values
        """
        twitch_username = twitch_username.lower()

        assert range_high > range_low, 'Max value cannot be lower than min value.'

        user_details = self.get_user_from_twitch_username(twitch_username)
        user_id = user_details['user_id']
        result = self.c.execute(self.sql_string_get_range_setting, (setting_key, twitch_username))
        value = result.fetchone()
        if value is None:
            self.c.execute(self.sql_string_insert_range_setting, (setting_key, range_low, range_high, user_id))
        else:
            self.c.execute(self.sql_string_update_range_setting, (setting_key, range_low, range_high, user_id))
        self.conn.commit()
        return range_low, range_high

    def get_all_users(self):
        """
        Gets all users in db
        :return:
        """
        result = self.c.execute("SELECT * FROM users;")
        value = result.fetchall()
        return value

    def set_excluded_users(self, twitch_username: str, excluded_users: str):
        """
        Sets excluded users as comma separated list
        :param twitch_username: Twitch username
        :param excluded_users: Excluded users
        :return: Status
        """
        twitch_username = twitch_username.lower()

        user_id = self.get_user_from_twitch_username(twitch_username)['user_id']
        result = self.c.execute("SELECT * FROM exclude_list WHERE user_id=?", (user_id,))
        value = result.fetchone()

        # Make a comma separated list where every username is lowercase and stripped
        excluded_users = ','.join(map(str.lower, map(str.strip, excluded_users.split(','))))

        if value is None:
            self.c.execute("INSERT INTO exclude_list (excluded_user, user_id) VALUES (?, ?)",
                           (excluded_users, user_id))
        else:
            self.c.execute("UPDATE exclude_list SET excluded_user=? WHERE user_id=?",
                           (excluded_users, user_id))

        self.conn.commit()

    def get_excluded_users(self, twitch_username: str, return_mode='str') -> Union[List, str]:
        """
        Gets excluded user settings of a user
        :param twitch_username: Twitch username
        :param return_mode: Can be 'str' (String of comma separated values) or 'list' (List of excluded users)
        :return: Comma separated values of excluded users
        """
        result = self.c.execute("SELECT * FROM exclude_list INNER JOIN users ON users.user_id=exclude_list.user_id "
                                "WHERE twitch_username=?", (twitch_username,))

        value = result.fetchone()

        if value is None:
            if return_mode == 'list':
                excluded_users = []
            else:
                excluded_users = ''
        else:
            excluded_users = value['excluded_user']

            if return_mode == 'list':
                excluded_users = excluded_users.split(',')

        return excluded_users


class StatisticsDatabase(BaseDatabase):
    def __init__(self, db_path: Optional[str] = None):
        if db_path is None:
            db_path = os.path.join(os.getenv('DB_DIR'), 'statistics.db')
        super().__init__(db_path)

    def initialize(self):
        super().initialize()
        self.c.execute(
            f"CREATE TABLE IF NOT EXISTS beatmaps "
            f"(request_date timestamp, "
            f"beatmap_id TEXT, "
            f"requester_channel_name TEXT, "
            f"requested_channel_id TEXT, "
            f"mods TEXT);"
        )
        self.c.execute(
            f"CREATE TABLE IF NOT EXISTS commands "
            f"(timestamp timestamp, "
            f"command_name TEXT, "
            f"used_from TEXT, "
            f"username TEXT);"
        )
        self.c.execute(
            f"CREATE TABLE IF NOT EXISTS api_usage "
            f"(timestamp timestamp, "
            f"endpoint TEXT);"
        )
        self.c.execute(
            f"CREATE TABLE IF NOT EXISTS errors "
            f"(timestamp timestamp, "
            f"type TEXT, "
            f"error_text TEXT);"
        )
        self.conn.commit()

    def add_api_usage(self, endpoint_name: str):
        """
        Adds an api usage entry to database.
        :param endpoint_name: osu! Api endpoint name
        """
        self.c.execute("INSERT INTO api_usage (timestamp, endpoint) VALUES (?, ?)", (datetime.now(), endpoint_name))
        self.conn.commit()

    def add_request(self, requester_channel_name: str, requested_beatmap_id: int, requested_channel_name: str,
                    mods: Optional[str]):
        """
        Adds a beatmap request to database.
        :param requester_channel_name: Channel name of the beatmap requester
        :param requested_beatmap_id: Beatmap id of the requested beatmap
        :param requested_channel_name: Channel id of the chat where the beatmap is requested
        :param mods: Requested mods (optional)
        """
        if mods == '':
            mods = None
        self.c.execute("INSERT INTO beatmaps "
                       "(request_date, beatmap_id, requester_channel_name, requested_channel_id, mods) "
                       "VALUES (?, ?, ?, ?, ?)",
                       (datetime.now(), requested_beatmap_id, requester_channel_name, requested_channel_name, mods))
        self.conn.commit()

    def get_popular_beatmap_id(self, top_n: int = 5) -> List[sqlite3.Row]:
        """
        Gets the top_n most requested beatmap ids.

        SELECT       `column`,
                 COUNT(`column`) AS `value_occurrence`
        FROM     `my_table`
        GROUP BY `column`
        ORDER BY `value_occurrence` DESC
        LIMIT    1;
        """
        cursor = self.c.execute("SELECT beatmap_id, COUNT(beatmap_id) AS nr_of_requests FROM beatmaps "
                                "GROUP BY beatmap_id "
                                "ORDER BY nr_of_requests DESC "
                                f"LIMIT {top_n};")

        return cursor.fetchall()

    def get_popular_requesters(self, top_n: int = 5) -> List[sqlite3.Row]:
        """
        Gets the top_n most popular beatmap requester names.
        """
        cursor = self.c.execute("SELECT requester_channel_name, COUNT(requester_channel_name) AS nr_of_requests "
                                "FROM beatmaps "
                                "GROUP BY requester_channel_name "
                                "ORDER BY nr_of_requests DESC "
                                f"LIMIT {top_n};")

        return cursor.fetchall()

    def get_popular_streamer_ids(self, top_n: int = 5) -> List[sqlite3.Row]:
        """
        Gets the top_n most popular streamer channel ids.
        """
        cursor = self.c.execute("SELECT requested_channel_id, COUNT(requested_channel_id) AS nr_of_requests "
                                "FROM beatmaps "
                                "GROUP BY requested_channel_id "
                                "ORDER BY nr_of_requests DESC "
                                f"LIMIT {top_n};")

        return cursor.fetchall()

    def add_command(self, command: str, used_on: str, user: str):
        """
        Adds a command entry to database
        This is used for statistics on the usage of chatbot.
        For example, if heyronii uses !echo on twitch, it will add:
        (timestamp.now(), 'echo', 'twitch', 'heyronii') to database
        """
        self.c.execute("INSERT INTO commands (timestamp, command_name, used_from, username) VALUES (?,?,?,?)",
                       (datetime.now(), command, used_on, user))
        self.conn.commit()

    def add_error(self, error_type: str, error_text: Optional[str] = None):
        """
        Adds an error entry to database
        This is used for statistics. It will keep information about osu! api issues and twitch api issues.
        For example, if we get rate-limited by osu, we will add:
        (timestamp.now(), 'echo', 'twitch', 'heyronii') to database
        """
        self.c.execute("INSERT INTO errors (timestamp, type, error_text) VALUES (?,?,?)",
                       (datetime.now(), error_type, error_text))
        self.conn.commit()
