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
    asyncio.gather(bot_manager.bot_queue_receiver(), bot_manager.webserver_receiver(),
                   bot_manager.process_handler())
    loop = asyncio.get_event_loop()
    loop.run_forever()
