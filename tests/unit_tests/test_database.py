import os
import shutil
from unittest import IsolatedAsyncioTestCase

from ronnia.helpers.database_helper import UserDatabase


class TestDatabase(IsolatedAsyncioTestCase):
    db = None

    @classmethod
    def setUpClass(self) -> None:
        test_db_path = os.path.join('tests', 'data', 'users_test.db')
        db_path = os.path.join('tests', 'data', 'users.db')
        if os.path.exists(db_path):
            os.remove(db_path)
        shutil.copyfile(test_db_path, db_path)
        self.db = UserDatabase(db_path=db_path)

    async def asyncSetUp(self) -> None:
        await self.db.initialize()

    async def asyncTearDown(self) -> None:
        await self.db.close()

    async def test_add_user_adds_a_row_in_db(self):
        await self.db.add_user(twitch_username='test_twitch_username',
                               osu_username='test_osu_username',
                               twitch_id='test_twitch_id',
                               osu_user_id='test_osu_user_id')

        new_cursor = await self.db.conn.cursor()
        result = await new_cursor.execute('SELECT * FROM users WHERE twitch_username=?;', ('test_twitch_username',))
        user_details = await result.fetchone()
        self.assertEqual(user_details['osu_username'], 'test_osu_username')
        self.assertEqual(user_details['osu_id'], 'test_osu_user_id')
        self.assertEqual(user_details['twitch_id'], 'test_twitch_id')

    async def test_update_user_updates_db_fields(self):
        await self.db.update_user(new_twitch_username='new_twitch_username',
                                  new_osu_username='new_osu_username',
                                  twitch_id='68427964',
                                  osu_user_id='5642779')

        new_cursor = await self.db.conn.cursor()
        result = await new_cursor.execute('SELECT * FROM users WHERE twitch_id=?;', ('68427964',))
        user_details = await result.fetchone()
        self.assertIsNotNone(user_details)
        self.assertEqual(user_details['osu_username'], 'new_osu_username')
        self.assertEqual(user_details['twitch_username'], 'new_twitch_username')

    async def test_remove_user_removes_row_from_db(self):
        new_cursor = await self.db.conn.cursor()
        result = await new_cursor.execute('SELECT * FROM users WHERE twitch_username=?;', ('user_to_be_removed',))
        user_details = await result.fetchone()
        self.assertIsNotNone(user_details)

        result = await (await new_cursor.execute('SELECT COUNT(*) FROM users;')).fetchone()
        self.assertEqual(4, result[0])

        await self.db.remove_user(twitch_username='user_to_be_removed')

        result = await new_cursor.execute('SELECT * FROM users WHERE twitch_username=?;', ('user_to_be_removed',))
        user_details = await result.fetchone()
        self.assertIsNone(user_details)

        result = await (await new_cursor.execute('SELECT COUNT(*) FROM users;')).fetchone()
        self.assertEqual(3, result[0])

    async def test_get_user_from_osu_username_returns_correct_result(self):
        user = await self.db.get_user_from_osu_username('test_user_unchanged')

        self.assertEqual(user['twitch_username'], 'test_user_unchanged')
        self.assertEqual(user['twitch_id'], '1111')

    async def test_get_user_from_twitch_username_returns_correct_result(self):
        user = await self.db.get_user_from_twitch_username('test_user_unchanged')

        self.assertEqual(user['osu_username'], 'test_user_unchanged')
        self.assertEqual(user['osu_id'], '1111')

    async def test_enable_channel_sets_enabled_key_to_true(self):
        user = await self.db.get_user_from_twitch_username('test_user_unchanged')
        user_twitch_username = user['twitch_username']

        await self.db.enable_channel(user_twitch_username)

        new_cursor = await self.db.conn.cursor()
        result = await (await new_cursor.execute('SELECT value FROM user_settings WHERE key="enable";')).fetchone()

        self.assertEqual(1, result['value'])

    async def test_disable_channel_sets_enabled_key_to_false(self):
        user = await self.db.get_user_from_twitch_username('test_user_unchanged')
        user_twitch_username = user['twitch_username']
        await self.db.disable_channel(user_twitch_username)

        new_cursor = await self.db.conn.cursor()
        result = await (await new_cursor.execute('SELECT value FROM user_settings WHERE key="enable";')).fetchone()

        self.assertEqual(0, result['value'])

    async def test_get_enabled_gets_correct_enabled_status(self):
        user = await self.db.get_user_from_twitch_username('test_user_unchanged')
        user_twitch_username = user['twitch_username']
        await self.db.enable_channel(user_twitch_username)

        result = await self.db.get_enabled_status(user_twitch_username)
        self.assertEqual(1, result)

        await self.db.disable_channel(user_twitch_username)
        result = await self.db.get_enabled_status(user_twitch_username)
        self.assertEqual(0, result)

    async def test_define_range_setting_creates_a_row(self):
        await self.db.define_range_setting('test_range_setting', 0, 100, 'test_description')
        new_cursor = await self.db.conn.cursor()

        result = await (
            await new_cursor.execute('SELECT * FROM range_settings WHERE key="test_range_setting";')).fetchone()

        self.assertEqual(0, result['default_low'])
        self.assertEqual(100, result['default_high'])

    async def test_toggle_sub_only_toggles_setting(self):
        user = await self.db.get_user_from_twitch_username('test_user_unchanged')
        user_twitch_username = user['twitch_username']
        await self.db.toggle_sub_only(user_twitch_username)
        new_cursor = await self.db.conn.cursor()

        result = await (await new_cursor.execute('SELECT * FROM user_settings WHERE key="sub-only";')).fetchone()
        self.assertEqual(1, result['value'])

        await self.db.toggle_sub_only(user_twitch_username)
        result = await (await new_cursor.execute('SELECT * FROM user_settings WHERE key="sub-only";')).fetchone()
        self.assertEqual(0, result['value'])

    async def test_get_range_setting_gets_correct_value(self):
        user = await self.db.get_user_from_twitch_username('test_user_unchanged')
        user_twitch_username = user['twitch_username']
        key = 'sr'
        range_low, range_high = await self.db.get_range_setting(user_twitch_username, key)

        self.assertEqual(-1, range_low)
        self.assertEqual(-1, range_high)

    async def test_set_range_setting_creates_new_range_setting(self):
        user = await self.db.get_user_from_twitch_username('test_user_unchanged')
        user_twitch_username = user['twitch_username']
        expected_low, expected_high = 3, 5

        await self.db.set_range_setting(twitch_username=user_twitch_username,
                                        setting_key='test_range_setting',
                                        range_low=expected_low,
                                        range_high=expected_high)
        range_low, range_high = await self.db.get_range_setting(twitch_username=user_twitch_username,
                                                                setting_key='test_range_setting')

        self.assertEqual(expected_low, range_low)
        self.assertEqual(expected_high, range_high)

    async def test_set_excluded_users_upserts_entry_to_db(self):
        excluded_users = 'test_excluded_user_1,test_excluded_user_2'
        await self.db.set_excluded_users(twitch_username='test_user_unchanged',
                                         excluded_users=excluded_users)

        new_cursor = await self.db.conn.cursor()
        response = await (
            await new_cursor.execute('SELECT excluded_user FROM exclude_list WHERE user_id=19;')).fetchone()

        value = response['excluded_user']

        self.assertEqual(excluded_users, value)

        update_excluded_users = 'test_excluded_user_1'
        await self.db.set_excluded_users(twitch_username='test_user_unchanged',
                                         excluded_users=update_excluded_users)

        response = await (
            await new_cursor.execute('SELECT excluded_user FROM exclude_list WHERE user_id=19;')).fetchone()

        updated_value = response['excluded_user']

        self.assertEqual(update_excluded_users, updated_value)

    async def test_set_excluded_users_removes_trailing_whitespaces(self):
        excluded_users = 'test excluded user 1 , test excluded user 2'
        expected_value = 'test excluded user 1,test excluded user 2'
        await self.db.set_excluded_users(twitch_username='test_user_unchanged',
                                         excluded_users=excluded_users)

        new_cursor = await self.db.conn.cursor()
        response = await (
            await new_cursor.execute('SELECT excluded_user FROM exclude_list WHERE user_id=19;')).fetchone()

        value = response['excluded_user']

        self.assertEqual(expected_value, value)

    async def test_set_excluded_users_inserts_empty_string(self):
        excluded_users = ''
        expected_value = ''
        await self.db.set_excluded_users(twitch_username='test_user_unchanged',
                                         excluded_users=excluded_users)

        new_cursor = await self.db.conn.cursor()
        response = await (
            await new_cursor.execute('SELECT excluded_user FROM exclude_list WHERE user_id=19;')).fetchone()

        value = response['excluded_user']

        self.assertEqual(expected_value, value)

    async def test_set_excluded_users_makes_entry_lowercase(self):
        excluded_users = 'TesTUser1'
        expected_value = 'testuser1'
        await self.db.set_excluded_users(twitch_username='test_user_unchanged',
                                         excluded_users=excluded_users)

        new_cursor = await self.db.conn.cursor()
        response = await (
            await new_cursor.execute('SELECT excluded_user FROM exclude_list WHERE user_id=19;')).fetchone()

        value = response['excluded_user']

        self.assertEqual(expected_value, value)

    async def test_get_excluded_users_gets_entry_from_db_in_str_return_mode(self):
        expected_value = 'get_str_test_excluded_user_1,get_str_test_excluded_user_1'
        await self.db.set_excluded_users(twitch_username='test_user_unchanged',
                                         excluded_users=expected_value)
        returned_value = await self.db.get_excluded_users(twitch_username='test_user_unchanged')

        self.assertEqual(expected_value, returned_value)

    async def test_get_excluded_users_returns_list_from_db_in_list_return_mode(self):
        expected_value = ['get_list_test_excluded_user_1', 'get_list_test_excluded_user_1']
        excluded_users = 'get_list_test_excluded_user_1, get_list_test_excluded_user_1'
        await self.db.set_excluded_users(twitch_username='test_user_unchanged',
                                         excluded_users=excluded_users)
        returned_value = await self.db.get_excluded_users(twitch_username='test_user_unchanged', return_mode='list')

        self.assertListEqual(expected_value, returned_value)

    async def test_get_excluded_users_returns_empty_string_when_value_is_null(self):
        expected_value = ['get_list_test_excluded_user_1', 'get_list_test_excluded_user_1']
        excluded_users = 'get_list_test_excluded_user_1, get_list_test_excluded_user_1'
        await self.db.set_excluded_users(twitch_username='test_user_unchanged',
                                         excluded_users=excluded_users)
        returned_value = await self.db.get_excluded_users(twitch_username='test_user_unchanged', return_mode='list')

        self.assertListEqual(expected_value, returned_value)

    async def test_get_excluded_users_returns_empty_list_in_list_mode_when_value_is_null(self):
        expected_value = []
        returned_value = await self.db.get_excluded_users(twitch_username='unknown_user', return_mode='list')

        self.assertListEqual(expected_value, returned_value)
