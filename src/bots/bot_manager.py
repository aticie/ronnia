import asyncio
import datetime
import os
import sqlite3
from itertools import islice
from multiprocessing import Process, Lock
from typing import List

import requests
from azure.core.exceptions import ResourceNotFoundError
from azure.servicebus import ServiceBusMessage
from azure.servicebus.aio import ServiceBusClient
from azure.servicebus.aio.management import ServiceBusAdministrationClient

from bots.irc_bot import IrcBot
from bots.twitch_bot import TwitchBot
from helpers.logger import RonniaLogger


def batcher(iterable, batch_size):
    iterator = iter(iterable)
    while batch := list(islice(iterator, batch_size)):
        yield batch


class TwitchAPI:
    def __init__(self, client_id, client_secret):
        self.client_id = client_id
        self.client_secret = client_secret

        self.access_token = self.get_token()

    def get_token(self):
        """
        Gets access token from Twitch API
        """
        url = "https://id.twitch.tv/oauth2/token?client_id={}&client_secret={}&grant_type=client_credentials".format(
            self.client_id, self.client_secret)
        response = requests.post(url)
        return response.json()['access_token']

    def get_streams(self, user_ids: List[int]):
        """
        Gets streams from Twitch API helix/streams only users playing osu!
        """
        headers = {'Authorization': 'Bearer {}'.format(self.access_token),
                   'Client-ID': self.client_id}
        streams = []
        for user_id in batcher(user_ids, 100):
            # game_id = 21465 is osu!
            url = f"https://api.twitch.tv/helix/streams?first=100&game_id=21465&" + "&".join(
                [f"user_id={user}" for user in user_id])
            response = requests.get(url, headers=headers)
            streams += response.json()['data']
        return streams


class TwitchProcess(Process):
    def __init__(self, user_list: List[str], join_lock: Lock):
        super().__init__()
        self.join_lock = join_lock
        self.user_list = user_list
        self.bot = None

    def initialize(self):
        self.bot = TwitchBot(initial_channel_ids=self.user_list, join_lock=self.join_lock)

    def run(self) -> None:
        self.initialize()
        self.bot.run()


class IRCProcess(Process):
    def __init__(self):
        super().__init__()
        self.bot = None

    def initialize(self) -> None:
        self.bot = IrcBot("#osu", os.getenv('OSU_USERNAME'), "irc.ppy.sh", password=os.getenv("IRC_PASSWORD"))

    def run(self) -> None:
        self.initialize()
        self.bot.start()


class BotManager:
    def __init__(self, ):
        self.users_db = sqlite3.connect(os.path.join(os.getenv('DB_DIR'), 'users.db'))
        self.join_lock = Lock()
        self.instance_message_queue = None

        self.twitch_client = TwitchAPI(os.getenv('TWITCH_CLIENT_ID'), os.getenv('TWITCH_CLIENT_SECRET'))
        self._loop = asyncio.get_event_loop()

        self.servicebus_connection_string = os.getenv('SERVICE_BUS_CONN_STRING')
        self.servicebus_webserver_queue_name = 'webserver-signups'
        self.servicebus_webserver_reply_queue_name = 'webserver-signups-reply'
        self.servicebus_bot_queue_name = 'bot-signups'
        self.servicebus_bot_reply_queue_name = 'bot-signups-reply'
        self.servicebus_queues = {'webserver-signups': {'max_delivery_count': 100,
                                                        'default_message_time_to_live': datetime.timedelta(seconds=10)},
                                  'webserver-signups-reply': {'max_delivery_count': 100,
                                                              'default_message_time_to_live': datetime.timedelta(
                                                                  seconds=10)},
                                  'bot-signups': {'max_delivery_count': 100,
                                                  'default_message_time_to_live': datetime.timedelta(seconds=10)},
                                  'bot-signups-reply': {'max_delivery_count': 100,
                                                        'default_message_time_to_live': datetime.timedelta(seconds=10)},
                                  'twitch-to-irc': {'max_delivery_count': 100,
                                                    'default_message_time_to_live': datetime.timedelta(seconds=10)},
                                  }

        self.servicebus_mgmt = ServiceBusAdministrationClient.from_connection_string(self.servicebus_connection_string)
        self.servicebus_client = ServiceBusClient.from_connection_string(conn_str=self.servicebus_connection_string)

        self.bot_instances = []
        self.bot_processes = []

        self.irc_process = IRCProcess()

    def start(self):
        self._loop.run_until_complete(self.initialize_queues())

        all_users = self.users_db.execute('SELECT * FROM users;').fetchall()
        all_user_twitch_ids = [user[4] for user in all_users]
        streaming_user_ids = [user['user_id'] for user in self.twitch_client.get_streams(all_user_twitch_ids)]

        for user_id in all_user_twitch_ids:
            if user_id not in streaming_user_ids:
                streaming_user_ids.append(user_id)

        self.irc_process.start()

        for user_id_list in batcher(streaming_user_ids, 100):
            p = TwitchProcess(user_id_list, self.join_lock)
            p.start()
            self.bot_processes.append(p)

    async def initialize_queues(self):
        """
        Initializes webserver & bot, signup and reply queues
        """
        logger.info('Initializing queues...')
        for queue_name, queue_properties in self.servicebus_queues.items():
            try:
                queue_details = await self.servicebus_mgmt.get_queue(queue_name)
            except ResourceNotFoundError:
                await self.servicebus_mgmt.create_queue(queue_name, **queue_properties)

    async def run_service_bus_receiver(self):
        """
        Creates the receiver for the webserver queue
        Forwards incoming messages to the bot instance
        Replies to the webserver with a reply queue
        """
        receiver = self.servicebus_client.get_queue_receiver(queue_name=self.servicebus_webserver_queue_name)
        logger.info('Started servicebus receiver, listening for messages...')
        async for message in receiver:
            await self.receive_and_parse_message(message)
            await receiver.complete_message(message)

    async def receive_and_parse_message(self, message):
        """
        Receive a message from the webserver signup queue and parse it, forward it to bot queue.
        """
        logger.info(f'Received signup message: {message}')
        async with ServiceBusClient.from_connection_string(self.servicebus_connection_string) as sb_client:
            sender = sb_client.get_queue_sender(queue_name=self.servicebus_bot_queue_name)
            logger.debug(f'Sending message to bot: {message}')
            await sender.send_messages(message)


if __name__ == '__main__':
    logger = RonniaLogger(__name__)

    bot_manager = BotManager()
    bot_manager.start()
    asyncio.run(bot_manager.run_service_bus_receiver())
