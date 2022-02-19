import asyncio
import multiprocessing

from bots.bot_manager import BotManager
from helpers.logger import RonniaLogger

logger = RonniaLogger(__name__)

if __name__ == "__main__":
    # Required for multiprocessing to work on Linux
    multiprocessing.set_start_method('spawn')
    bot_manager = BotManager()
    bot_manager.start()
    asyncio.run(bot_manager.webserver_receiver())
