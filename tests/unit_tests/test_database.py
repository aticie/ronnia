import os
import shutil
from unittest import TestCase

from src.helpers.database_helper import UserDatabase


class TestDatabase(TestCase):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        test_db_path = os.path.join('tests', 'data', 'users_test.db')
        db_path = os.path.join('tests', 'data', 'users.db')
        shutil.copyfile(test_db_path, db_path)
        self.db = UserDatabase(db_path=db_path)
        self.db.initialize()

    def test_add_user_adds_a_row_in_db(self):
        self.db.add_user(twitch_username='test_twitch_username',
                         osu_username='test_osu_username',
                         twitch_id='test_twitch_id',
                         osu_user_id='test_osu_user_id')

        new_cursor = self.db.conn.cursor()
        result = new_cursor.execute('SELECT * FROM users WHERE twitch_username=?;', ('test_twitch_username',))
        user_details = result.fetchone()
        self.assertEqual(user_details['osu_username'], 'test_osu_username')
        self.assertEqual(user_details['osu_id'], 'test_osu_user_id')
        self.assertEqual(user_details['twitch_id'], 'test_twitch_id')

    def test_update_user_updates_db_fields(self):
        self.db.update_user(new_twitch_username='new_twitch_username',
                            new_osu_username='new_osu_username',
                            twitch_id='68427964',
                            osu_user_id='5642779')

        new_cursor = self.db.conn.cursor()
        result = new_cursor.execute('SELECT * FROM users WHERE twitch_id=?;', ('68427964',))
        user_details = result.fetchone()
        self.assertIsNotNone(user_details)
        self.assertEqual(user_details['osu_username'], 'new_osu_username')
        self.assertEqual(user_details['twitch_username'], 'new_twitch_username')

    def test_remove_user_removes_row_from_db(self):
        new_cursor = self.db.conn.cursor()
        result = new_cursor.execute('SELECT * FROM users WHERE twitch_username=?;', ('user_to_be_removed',))
        user_details = result.fetchone()
        self.assertIsNotNone(user_details)

        self.db.remove_user(twitch_username='user_to_be_removed')

        result = new_cursor.execute('SELECT * FROM users WHERE twitch_username=?;', ('user_to_be_removed',))
        user_details = result.fetchone()
        self.assertIsNone(user_details)

    def test_get_user_from_osu_username_returns_correct_result(self):
        user = self.db.get_user_from_osu_username('test_user_unchanged')

        self.assertEqual(user['twitch_username'], 'test_user_unchanged')
        self.assertEqual(user['twitch_id'], '1111')

    def test_get_user_from_twitch_username_returns_correct_result(self):
        user = self.db.get_user_from_twitch_username('test_user_unchanged')

        self.assertEqual(user['osu_username'], 'test_user_unchanged')
        self.assertEqual(user['osu_id'], '1111')

    def test_enable_channel_sets_enabled_key_to_true(self):
        user = self.db.get_user_from_twitch_username('test_user_unchanged')
        user_twitch_username = user['twitch_username']

        self.db.enable_channel(user_twitch_username)

        new_cursor = self.db.conn.cursor()
        result = new_cursor.execute('SELECT value FROM user_settings WHERE key="enable";').fetchone()

        self.assertEqual(1, result['value'])

    def test_disable_channel_sets_enabled_key_to_false(self):
        user = self.db.get_user_from_twitch_username('test_user_unchanged')
        user_twitch_username = user['twitch_username']
        self.db.disable_channel(user_twitch_username)

        new_cursor = self.db.conn.cursor()
        result = new_cursor.execute('SELECT value FROM user_settings WHERE key="enable";').fetchone()

        self.assertEqual(0, result['value'])

    def test_get_enabled_gets_correct_enabled_status(self):
        user = self.db.get_user_from_twitch_username('test_user_unchanged')
        user_twitch_username = user['twitch_username']
        self.db.enable_channel(user_twitch_username)

        result = self.db.get_enabled_status(user_twitch_username)
        self.assertEqual(1, result)

        self.db.disable_channel(user_twitch_username)
        result = self.db.get_enabled_status(user_twitch_username)
        self.assertEqual(0, result)
