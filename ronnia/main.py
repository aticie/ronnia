import asyncio
import logging
import multiprocessing
import os
import platform
import sys

from ronnia.bots.bot_manager import BotManager
from utils.logger import CustomJsonFormatter

if __name__ == "__main__":

    if os.getenv("SENTRY_DSN"):
        import sentry_sdk
        from sentry_sdk.integrations.logging import LoggingIntegration
        sentry_sdk.init(
            dsn=os.getenv("SENTRY_DSN"),
            traces_sample_rate=1.0,
            profiles_sample_rate=1.0,
            integrations=[
                LoggingIntegration(),
            ],
        )

    # Required for multiprocessing to work on Linux
    multiprocessing.set_start_method("spawn")

    logger = logging.getLogger()

    logger.setLevel(os.getenv("LOG_LEVEL", logging.INFO))
    logHandler = logging.StreamHandler(sys.stdout)
    formatter = CustomJsonFormatter('%(timestamp)s %(level)s %(name)s %(message)s')
    logHandler.setFormatter(formatter)
    logger.addHandler(logHandler)

    if platform.system() == 'Windows':
        loop_policy = asyncio.WindowsProactorEventLoopPolicy()
        logger.info(f"Windows platform detected, setting event loop policy to {loop_policy.__class__.__name__}")
        asyncio.set_event_loop_policy(loop_policy)

    bot_manager = BotManager()
    asyncio.run(bot_manager.start())
