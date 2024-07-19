import asyncio
import datetime
import logging
from typing import Optional, Union, Any, Sequence, AsyncGenerator

from motor.motor_asyncio import (
    AsyncIOMotorClient,
    AsyncIOMotorDatabase,
    AsyncIOMotorCollection, )
from pymongo.errors import BulkWriteError

from ronnia.models.database import DBUser

logger = logging.getLogger(__name__)


class RonniaDatabase(AsyncIOMotorClient):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.db: AsyncIOMotorDatabase = self["Ronnia"]
        self.users_col: AsyncIOMotorCollection = self.db["Users"]
        self.settings_col: AsyncIOMotorCollection = self.db["Settings"]
        self.statistics_col: AsyncIOMotorCollection = self.db["Statistics"]
        self.beatmaps_col: AsyncIOMotorCollection = self.db["Beatmaps"]

    async def initialize(self):
        """Initialize the Database, define hardcoded settings."""
        async with asyncio.TaskGroup() as tg:
            tg.create_task(self.define_setting("enable", True, "Enables the bot.", "toggle"))
            tg.create_task(self.define_setting(
                "echo", True, "Enables Twitch chat acknowledge message.", "toggle"
            ))
            tg.create_task(self.define_setting(
                "sub-only", False, "Subscribers only request mode.", "toggle"
            ))
            tg.create_task(self.define_setting(
                "points-only", False, "Channel Points only request mode.", "toggle"
            ))
            tg.create_task(self.define_setting(
                "test", False, "Enables test mode. (Removes all restrictions.)", "toggle"
            ))
            tg.create_task(self.define_setting("cooldown", 30, "Cooldown for requests.", "value"))
            tg.create_task(self.define_setting(
                "sr", [0, -1], "Star rating limit for requests.", "range"
            ))

        logger.info(f"Successfully initialized {self.__class__.__name__}")

    async def get_user_from_twitch_id(self, twitch_id: int) -> DBUser:
        """
        Gets the user details from database using Twitch username
        :param twitch_id: Twitch ID
        :return: User details of the user associated with twitch username
        """
        user = await self.users_col.find_one({"twitchId": twitch_id})
        return DBUser(**user)

    async def get_user_from_twitch_username(self, twitch_username: str) -> DBUser:
        """
        Gets the user details from database using Twitch username
        :param twitch_username:
        :return: User details of the user associated with twitch username
        """
        user = await self.users_col.find_one({"twitchUsername": twitch_username})
        return DBUser(**user)

    async def define_setting(
            self, name: str, default_value: Any, description: str, _type: str
    ) -> None:
        """
        Define a new user specific setting
        :param name: Setting key
        :param default_value: Default value of the setting
        :param description: Description of the setting
        :param _type: Type of the setting
        :return:
        """
        await self.settings_col.update_one({"name": name}, {"$set": {
            "name": name,
            "value": default_value,
            "description": description,
            "type": _type,
        }}, upsert=True)

    @staticmethod
    async def bulk_write_operations(
            operations: Sequence,
            col: AsyncIOMotorCollection,
    ):
        """Bulk write multiple operations to the given collection."""
        try:
            if len(operations) != 0:
                result = await col.bulk_write(operations)
            else:
                return

        except BulkWriteError as e:
            non_duplicates_list = list(
                filter(lambda x: x["code"] != 11000, e.details["writeErrors"])
            )
            logger.info(
                f"Had duplicates, bulk_write "
                f"{len(non_duplicates_list) / len(operations)} of operations."
            )
        else:
            logger.info(
                f"Completed with no errors. Bulk_write {len(operations)} "
                f"operations to MongoDB. {result.bulk_api_result}"
            )
            return result

    async def get_echo_status(self, twitch_username: str) -> bool:
        """
        Gets echo setting of user
        :param twitch_username: Twitch username.
        :return: User's echo setting status
        """
        return await self.get_setting("echo", twitch_username)

    async def get_test_status(self, twitch_username: str):
        """
        Gets the user's setting for test mode
        :param twitch_username: Twitch username
        :return:
        """
        return await self.get_setting("test", twitch_username)

    async def get_setting(
            self, setting_key: str, twitch_username_or_id: Union[str, int]
    ):
        """
        Get the setting's current value for user
        :param setting_key: Key of the setting
        :param twitch_username_or_id: Twitch username or Twitch id
        :return:
        """
        if isinstance(twitch_username_or_id, int):
            user = await self.get_user_from_twitch_id(twitch_username_or_id)
        else:
            user = await self.get_user_from_twitch_username(twitch_username_or_id)
        settings = user.settings.dict(by_alias=True)
        return settings[setting_key]

    async def get_enabled_users(self) -> AsyncGenerator[DBUser, None]:
        """
        Gets all enabled users in db
        :return:
        """
        async for user in self.users_col.find({"settings.enable": True}):
            yield DBUser.model_validate(user)

    async def get_excluded_users(self, twitch_username: str) -> AsyncGenerator[str, None]:
        """
        Gets excluded user settings of a user
        :param twitch_username: Twitch username
        :return: List of excluded users
        """
        user = await self.get_user_from_twitch_username(twitch_username)
        for excluded_user in user.excludedUsers:
            yield excluded_user.lower()

    async def add_request(
            self,
            requester_channel_name: str,
            requested_beatmap_id: int,
            requested_channel_name: str,
            mods: Optional[str],
    ):
        """
        Adds a beatmap request to database.
        :param requester_channel_name: Channel name of the beatmap requester
        :param requested_beatmap_id: Beatmap id of the requested beatmap
        :param requested_channel_name: Channel id of the chat where the beatmap is requested
        :param mods: Requested mods (optional)
        """
        logger.debug(f"Adding request statistics to the database")
        await self.statistics_col.insert_one(
            {
                "requester_channel_name": requester_channel_name,
                "requested_beatmap_id": requested_beatmap_id,
                "requested_channel_name": requested_channel_name,
                "mods": mods,
                "timestamp": datetime.datetime.now(datetime.timezone.utc),
            }
        )

    async def add_beatmap(self, beatmap_info: dict):
        """
        Adds beatmap to database
        :param beatmap_info: Beatmap to add
        """
        beatmap_id = beatmap_info["id"]
        logger.debug(f"Adding {beatmap_id} to the database")
        await self.beatmaps_col.update_one({"id": beatmap_id}, {"$set": beatmap_info}, upsert=True)
