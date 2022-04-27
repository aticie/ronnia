import unittest

from ronnia.helpers.utils import convert_seconds_to_readable


class TestConvertSecondsToReadable(unittest.TestCase):

    def test_convert_seconds_to_readable_happy_path(self):
        expected_value = '2:26'
        return_value = convert_seconds_to_readable('146')
        self.assertEqual(expected_value, return_value)

    def test_convert_seconds_to_readable_outputs_hours(self):
        expected_value = '1:02:26'
        return_value = convert_seconds_to_readable('3746')
        self.assertEqual(expected_value, return_value)

    def test_convert_seconds_to_readable_less_than_minute(self):
        expected_value = '0:23'
        return_value = convert_seconds_to_readable('23')
        self.assertEqual(expected_value, return_value)

    def test_convert_seconds_to_readable_exact_minute(self):
        expected_value = '1:00'
        return_value = convert_seconds_to_readable('60')
        self.assertEqual(expected_value, return_value)

    def test_convert_seconds_to_readable_exact_hour(self):
        expected_value = '1:00:00'
        return_value = convert_seconds_to_readable('3600')
        self.assertEqual(expected_value, return_value)
