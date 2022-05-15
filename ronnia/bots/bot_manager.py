import asyncio
import datetime
import json
import logging
import os
import sqlite3
import time
import traceback
from itertools import islice
from json import JSONDecodeError
from multiprocessing import Process, Lock
from typing import List

import requests
from azure.core.exceptions import ResourceNotFoundError
from azure.servicebus import ServiceBusMessage
from azure.servicebus.exceptions import ServiceBusError
from azure.servicebus.aio import ServiceBusClient
from azure.servicebus.aio.management import ServiceBusAdministrationClient

from ronnia.bots.twitch_bot import TwitchBot

logger = logging.getLogger(__name__)


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
    def __init__(self, user_list: List[int], join_lock: Lock):
        super().__init__()
        self.join_lock = join_lock
        self.user_list = user_list
        self.bot = None

    def initialize(self):
        self.bot = TwitchBot(initial_channel_ids=self.user_list, join_lock=self.join_lock)

    def run(self) -> None:
        self.initialize()
        self.bot.run()


class BotManager:
    def __init__(self, ):
        self.users_db = sqlite3.connect(os.path.join(os.getenv('DB_DIR'), 'users.db'))
        self.join_lock = Lock()

        self.twitch_client = TwitchAPI(os.getenv('TWITCH_CLIENT_ID'), os.getenv('TWITCH_CLIENT_SECRET'))
        self._loop = asyncio.get_event_loop()

        self.servicebus_connection_string = os.getenv('SERVICE_BUS_CONNECTION_STR')
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
                                  }

        self.servicebus_mgmt = ServiceBusAdministrationClient.from_connection_string(self.servicebus_connection_string)
        self.servicebus_client = ServiceBusClient.from_connection_string(conn_str=self.servicebus_connection_string,
                                                                         logging_enable=True)

        self.create_new_instance: bool = False

        self.bot_processes = {}

    def start(self):
        """
        Starts the bot manager.

        Creates an IRCBot process and multiple TwitchBot processes.
        """
        self._loop.run_until_complete(self.initialize_queues())
        logger.info("Queues initialized")
        all_users = self.users_db.execute('SELECT * FROM users;').fetchall()
        all_user_twitch_ids = [user[4] for user in all_users]
        streaming_user_ids = [user['user_id'] for user in self.twitch_client.get_streams(all_user_twitch_ids)]

        for user_id in all_user_twitch_ids:
            if user_id not in streaming_user_ids:
                streaming_user_ids.append(user_id)

        logger.info(f"Collected users: {len(streaming_user_ids)}")
        for user_id_list in batcher(streaming_user_ids, 100):
            p = TwitchProcess(user_id_list, self.join_lock)
            p.start()
            logger.info(f"Started Twitch bot instance for {len(user_id_list)} users")
            self.bot_processes[p] = user_id_list
            # 20 join rate per 10 seconds
            time.sleep(50.5)

    async def process_handler(self):
        """
        Checks the status of the processes and restarts them if necessary.
        """
        while True:
            await asyncio.sleep(5)
            for p in self.bot_processes:
                if not p.is_alive():
                    logger.info(f"Bot process {p.bot} died, restarting")
                    p.start()
                    logger.info(f"Bot process {p.bot} restarted")

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

    async def bot_queue_receiver(self):
        """
        Receives messages from bot reply queue
        """
        while True:
            try:
                async with self.servicebus_client.get_queue_receiver(self.servicebus_bot_reply_queue_name) as receiver:
                    logger.info(f"Receiver started for {self.servicebus_bot_reply_queue_name}")
                    async for message in receiver:
                        logger.info(f"Received message from bot reply queue: {str(message)}")
                        await self.process_bot_reply(message)
                        await receiver.complete_message(message)

                    logger.error('Exited bot reply receiver for unknown reason.')
                    logger.error(f'{receiver.__dict__}')
            except ServiceBusError as e:
                logger.error(f"Error in bot reply receiver: {e}")
                logger.error(traceback.format_exc())
                await asyncio.sleep(5)

    async def process_bot_reply(self, message: ServiceBusMessage):
        """
        Processes bot reply messages
        """
        message_contents = str(message)
        if message_contents == 'bot-full':
            logger.info('Bot is full, starting new instance')
            self.create_new_instance = True
        else:
            try:
                # Check if message is a valid json
                json.loads(message_contents)
                async with ServiceBusClient.from_connection_string(self.servicebus_connection_string) as sb_client:
                    async with sb_client.get_queue_sender(
                            queue_name=self.servicebus_webserver_reply_queue_name) as sender:
                        logger.debug(f'Sending message to {self.servicebus_webserver_reply_queue_name}: {message}')
                        await sender.send_messages(message)
            except JSONDecodeError:
                logger.error(f"Failed to decode bot reply message: {message_contents}")

        return

    async def webserver_receiver(self):
        """
        Creates the receiver for the webserver queue
        Forwards incoming messages to the bot instance
        Replies to the webserver with a reply queue
        """
        while True:
            try:
                async with self.servicebus_client.get_queue_receiver(
                        queue_name=self.servicebus_webserver_queue_name) as receiver:
                    logger.info('Started servicebus receiver, listening for messages...')
                    async for message in receiver:
                        await self.parse_and_send_message(message)
                        await receiver.complete_message(message)

                    logger.error(f'Exited webserver receiver for unknown reason.')
                    logger.error(f'{receiver.__dict__}')
            except ServiceBusError as e:
                logger.error(f"Error in webserver receiver: {e}")
                logger.error(traceback.format_exc())
                await asyncio.sleep(5)

    async def parse_and_send_message(self, message):
        """
        Receive a message from the webserver signup queue and parse it.

        Decide whether to send the message to the bot, or create a new TwitchBot instance.
        """
        if self.create_new_instance:
            p = TwitchProcess([], self.join_lock)
            p.start()
            self.bot_processes.append(p)
            logger.info(f"Started a new bot instance. This is the {len(self.bot_processes)}th instance.")
            self.create_new_instance = False

        logger.info(f'Received signup message: {message}')
        async with ServiceBusClient.from_connection_string(self.servicebus_connection_string) as sb_client:
            async with sb_client.get_queue_sender(queue_name=self.servicebus_bot_queue_name) as sender:
                logger.debug(f'Sending message to bot: {message}')
                await sender.send_messages(message)
