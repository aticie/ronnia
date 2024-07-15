import asyncio
import multiprocessing
import platform

from ronnia.bots.bot_manager import BotManager
from ronnia.helpers.logger import RonniaLogger

logger = RonniaLogger(__name__)

if __name__ == "__main__":
    # Required for multiprocessing to work on Linux
    multiprocessing.set_start_method("spawn")

    if platform.system() == 'Windows':
        loop_policy = asyncio.WindowsProactorEventLoopPolicy()
        logger.info(f"Windows platform detected, setting event loop policy to {loop_policy.__class__.__name__}")
        asyncio.set_event_loop_policy(loop_policy)

    bot_manager = BotManager()
    asyncio.run(bot_manager.start())
