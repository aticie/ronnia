import logging
import os
import sys


class RonniaLogger(object):
    def __new__(cls, name, *args, **kwargs):
        logger = logging.getLogger()
        logger.setLevel(os.getenv("LOG_LEVEL", "INFO").upper())
        loggers_formatter = logging.Formatter(
            "%(asctime)s | %(levelname)s | %(process)d | %(name)s | %(funcName)s | %(message)s",
            datefmt="%d/%m/%Y %I:%M:%S",
        )

        ch = logging.StreamHandler(stream=sys.stdout)
        ch.setFormatter(loggers_formatter)
        logger.addHandler(ch)

        logger.propagate = True

        return logger
