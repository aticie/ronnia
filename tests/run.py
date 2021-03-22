import unittest
import sys


def main():
    test_runner = unittest.TextTestRunner()
    test_loader = unittest.TestLoader()
    test_suite = test_loader.discover(start_dir='tests', pattern='test_*.py', top_level_dir='.')
    return not test_runner.run(test_suite).wasSuccessful()


if __name__ == '__main__':
    sys.exit(main())
