import asyncio

from bots.bot_manager import BotManager
from helpers.logger import RonniaLogger

logger = RonniaLogger(__name__)

if __name__ == "__main__":
    bot_manager = BotManager()
    bot_manager.start()
    asyncio.run(bot_manager.webserver_receiver())
