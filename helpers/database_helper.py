import sqlite3
import datetime
import os


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

    def __init__(self, db_path: str):
        super().__init__(db_path)

    def initialize(self):
        super().initialize()
        self.c.execute(
            f"CREATE TABLE IF NOT EXISTS users (osu_username text PRIMARY KEY NOT NULL, "
            f"twitch_username text, "
            f"enabled INTEGER, "
            f"echo INTEGER, "
            f"updated TEXT);"
        )
        self.conn.commit()

    def add_user(self, twitch_username, osu_username):
        current_date_str = datetime.datetime.now().strftime("%Y-%m-%d %H:%M:%S")
        self.c.execute(f"INSERT INTO users VALUES (?, ?, 1, 1, ?)", (twitch_username, osu_username, current_date_str))
        self.conn.commit()

    def get_user(self, osu_username):
        result = self.c.execute(f"SELECT * from users WHERE osu_username=?", (osu_username,))
        return result.fetchone()

    def toggle_echo(self, osu_username):
        result = self.c.execute(f"SELECT echo from users WHERE osu_username=?", (osu_username,))
        echo_status = bool(result.fetchone()[0][3])
        echo_status = not echo_status
        self.c.execute(f"UPDATE users SET echo=? WHERE osu_username=?", (echo_status, osu_username))
        return echo_status

    def disable_channel(self, osu_username):
        self.c.execute(f"UPDATE users SET enabled=0 WHERE osu_username=?", (osu_username,))
        self.conn.commit()

    def enable_channel(self, osu_username):
        self.c.execute(f"UPDATE users SET enabled=1 WHERE osu_username=?", (osu_username,))
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

    test_db = UserDatabase('test_users.db')
    test_db.initialize()
    test_db.add_user('heyronii', 'heyronii')
