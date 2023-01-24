import asyncio
import datetime
import json
import logging
import os
import sqlite3
from multiprocessing import Process, Lock
from typing import List, Dict

import requests
from azure.servicebus import ServiceBusMessage
from requests.adapters import Retry, HTTPAdapter
from azure.core.exceptions import ResourceNotFoundError
from azure.servicebus.aio import ServiceBusClient
from azure.servicebus.aio.management import ServiceBusAdministrationClient

from helpers.database_helper import DBUser
from ronnia.bots.twitch_bot import TwitchBot
from ronnia.helpers.utils import batcher

logger = logging.getLogger(__name__)


class TwitchAPI:
    def __init__(self, client_id, client_secret):
        self.client_id = client_id
        self.client_secret = client_secret

        self.session = requests.Session()
        self.retry_policy = Retry(total=5,
                                  backoff_factor=0.1,
                                  status_forcelist=[500, 502, 503, 504])
        self.session.mount("https://", HTTPAdapter(max_retries=self.retry_policy))

        self.access_token = self.get_token()

    def get_token(self):
        """
        Gets access token from Twitch API
        """
        url = "https://id.twitch.tv/oauth2/token?client_id={}&client_secret={}&grant_type=client_credentials".format(
            self.client_id, self.client_secret)
        response = self.session.post(url)
        return response.json()['access_token']

    def get_streams(self, user_ids: List[str]):
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
            response = self.session.get(url, headers=headers)
            streams += response.json()['data']
        return streams


class TwitchProcess(Process):
    def __init__(self, user_list: List[str], join_lock: Lock):
        super().__init__()
        self.join_lock = join_lock
        self.user_list = user_list
        self.bot = None

    def initialize(self):
        self.bot = TwitchBot(initial_channel_names=self.user_list, join_lock=self.join_lock)

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

        self.bot_processes: Dict[TwitchProcess, List[str]] = {}
        self.users: List[DBUser] = []

    def start(self):
        """
        Starts the bot manager.

        Creates an IRCBot process and multiple TwitchBot processes.
        """
        self._loop.run_until_complete(self.initialize_queues())
        logger.info("ServiceBus queues initialized")
        self.users = [DBUser(*user) for user in self.users_db.execute('SELECT * FROM users;').fetchall()]

        all_user_twitch_ids = [user.twitch_id for user in self.users]
        streaming_user_names = [user['user_login'] for user in self.twitch_client.get_streams(all_user_twitch_ids)]

        logger.info(f"Collected users: {len(streaming_user_names)}")
        p = TwitchProcess(user_list=streaming_user_names, join_lock=self.join_lock)
        p.start()
        logger.info(f"Started Twitch bot instance for {len(streaming_user_names)} users")
        self.bot_processes[p] = streaming_user_names

        asyncio.run(self.main())
        loop = asyncio.get_event_loop()
        loop.run_forever()

    async def main(self):
        """
        Main coroutine of the bot manager. Checks streaming users and sends the updated list to bot every 30 seconds.
        """
        while True:
            try:
                await asyncio.sleep(30)
                self.users = [DBUser(*user) for user in self.users_db.execute('SELECT * FROM users;').fetchall()]
                all_user_twitch_ids = [user.twitch_id for user in self.users]
                streaming_users = [user["user_login"] for user in self.twitch_client.get_streams(all_user_twitch_ids)]
                await self.send_users_to_bot(streaming_users)
            except Exception as e:
                logger.error(e)

    async def initialize_queues(self):
        """
        Initializes webserver & bot, signup and reply queues
        """
        logger.info('Initializing queues...')
        for queue_name, queue_properties in self.servicebus_queues.items():
            try:
                await self.servicebus_mgmt.get_queue(queue_name)
            except ResourceNotFoundError:
                await self.servicebus_mgmt.create_queue(queue_name, **queue_properties)

    async def send_users_to_bot(self, twitch_usernames: List[str]):
        """
        Receive a message from the webserver signup queue and parse it.

        Decide whether to send the message to the bot, or create a new TwitchBot instance.
        """

        logger.info(f'Sending streaming users to bot: {twitch_usernames}')
        async with ServiceBusClient.from_connection_string(self.servicebus_connection_string) as sb_client:
            async with sb_client.get_queue_sender(queue_name=self.servicebus_bot_queue_name) as sender:
                logger.debug(f'Sending message to bot: {twitch_usernames}')
                await sender.send_messages(ServiceBusMessage(json.dumps(twitch_usernames)))
