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

        result = new_cursor.execute('SELECT COUNT(*) FROM users;').fetchone()
        self.assertEqual(4, result[0])

        self.db.remove_user(twitch_username='user_to_be_removed')

        result = new_cursor.execute('SELECT * FROM users WHERE twitch_username=?;', ('user_to_be_removed',))
        user_details = result.fetchone()
        self.assertIsNone(user_details)

        result = new_cursor.execute('SELECT COUNT(*) FROM users;').fetchone()
        self.assertEqual(3, result[0])

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

    def test_define_range_setting_creates_a_row(self):
        self.db.define_range_setting('test_range_setting', 0, 100, 'test_description')
        new_cursor = self.db.conn.cursor()

        result = new_cursor.execute('SELECT * FROM range_settings WHERE key="test_range_setting";').fetchone()

        self.assertEqual(0, result['default_low'])
        self.assertEqual(100, result['default_high'])

    def test_toggle_sub_only_toggles_setting(self):
        user = self.db.get_user_from_twitch_username('test_user_unchanged')
        user_twitch_username = user['twitch_username']
        self.db.toggle_sub_only(user_twitch_username)
        new_cursor = self.db.conn.cursor()

        result = new_cursor.execute('SELECT * FROM user_settings WHERE key="sub-only";').fetchone()
        self.assertEqual(1, result['value'])

        self.db.toggle_sub_only(user_twitch_username)
        result = new_cursor.execute('SELECT * FROM user_settings WHERE key="sub-only";').fetchone()
        self.assertEqual(0, result['value'])

    def test_get_range_setting_gets_correct_value(self):
        user = self.db.get_user_from_twitch_username('test_user_unchanged')
        user_twitch_username = user['twitch_username']
        key = 'sr'
        range_low, range_high = self.db.get_range_setting(user_twitch_username, key)

        self.assertEqual(-1, range_low)
        self.assertEqual(-1, range_high)

    def test_set_range_setting_creates_new_range_setting(self):
        user = self.db.get_user_from_twitch_username('test_user_unchanged')
        user_twitch_username = user['twitch_username']
        expected_low, expected_high = 3, 5

        self.db.set_range_setting(twitch_username=user_twitch_username,
                                  setting_key='test_range_setting',
                                  range_low=expected_low,
                                  range_high=expected_high)
        range_low, range_high = self.db.get_range_setting(twitch_username=user_twitch_username,
                                                          setting_key='test_range_setting')

        self.assertEqual(expected_low, range_low)
        self.assertEqual(expected_high, range_high)
