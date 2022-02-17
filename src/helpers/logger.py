import logging
import os


class RonniaLogger(object):
    def __new__(cls, name, *args, **kwargs):
        logger = logging.getLogger(name)
        logger.setLevel(os.getenv('LOG_LEVEL').upper())
        loggers_formatter = logging.Formatter(
            '%(asctime)s | %(levelname)s | %(process)d | %(name)s | %(funcName)s | %(message)s',
            datefmt='%d/%m/%Y %I:%M:%S')

        ch = logging.StreamHandler()
        ch.setFormatter(loggers_formatter)
        logger.addHandler(ch)

        logger.propagate = False

        return logger
