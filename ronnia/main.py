import asyncio
import multiprocessing
import os
import platform

import sentry_sdk
from sentry_sdk.integrations.logging import LoggingIntegration

from ronnia.bots.bot_manager import BotManager
from ronnia.utils.logger import RonniaLogger

logger = RonniaLogger(__name__)

if __name__ == "__main__":

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

    if platform.system() == 'Windows':
        loop_policy = asyncio.WindowsProactorEventLoopPolicy()
        logger.info(f"Windows platform detected, setting event loop policy to {loop_policy.__class__.__name__}")
        asyncio.set_event_loop_policy(loop_policy)

    bot_manager = BotManager()
    asyncio.run(bot_manager.start())
