import logging
import os
import sys


class RonniaLogger(object):
    def __new__(cls, name, *args, **kwargs):
        logger = logging.getLogger()
        logger.setLevel(os.getenv("LOG_LEVEL", "INFO").upper())
        loggers_formatter = logging.Formatter(
            '[{asctime}] [{levelname:<8}] {name}: {message}', style='{',
            datefmt="%Y-%m-%d %H:%M:%S",
        )

        ch = logging.StreamHandler(stream=sys.stdout)
        ch.setFormatter(loggers_formatter)
        logger.addHandler(ch)

        logger.propagate = True

        return logger
