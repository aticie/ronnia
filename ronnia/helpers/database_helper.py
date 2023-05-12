import logging
import sqlite3
from datetime import datetime
from typing import Optional, List, Union, Iterable, Any, Sequence, Collection

from motor.motor_asyncio import (
    AsyncIOMotorClient,
    AsyncIOMotorDatabase,
    AsyncIOMotorCollection,
)
from pydantic import BaseModel, Field
from pymongo import UpdateOne
from pymongo.errors import BulkWriteError
from pymongo.collection import Collection, _WriteOp
from pymongo.typings import _DocumentType

logger = logging.getLogger(__name__)


class DBSettings(BaseModel):
    echo: bool = True
    enable: bool = True
    sub_only: bool = Field(False, alias="sub-only")
    points_only: bool = Field(False, alias="points-only")
    test: bool = False
    cooldown: float = 0
    sr: List[float] = [0, -1]


class DBUser(BaseModel):
    osuUsername: str
    twitchUsername: str
    twitchId: int
    osuId: int
    osuAvatarUrl: str
    twitchAvatarUrl: str
    excludedUsers: List[str] = []
    settings: DBSettings = DBSettings()
    isLive: bool = False


class RonniaDatabase(AsyncIOMotorClient):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.db: AsyncIOMotorDatabase = self["Ronnia"]
        self.users_col: AsyncIOMotorCollection = self.db["Users"]
        self.settings_col: AsyncIOMotorCollection = self.db["Settings"]
        self.statistics_col: AsyncIOMotorCollection = self.db["Statistics"]

    async def initialize(self):
        await self.define_setting("enable", True, "Enables the bot.", "toggle")
        await self.define_setting(
            "echo", True, "Enables Twitch chat acknowledge message.", "toggle"
        )
        await self.define_setting(
            "sub-only", False, "Subscribers only request mode.", "toggle"
        )
        await self.define_setting(
            "points-only", False, "Channel Points only request mode.", "toggle"
        )
        await self.define_setting(
            "test", False, "Enables test mode. (Removes all restrictions.)", "toggle"
        )
        await self.define_setting("cooldown", 30, "Cooldown for requests.", "value")
        await self.define_setting(
            "sr", [0, -1], "Star rating limit for requests.", "range"
        )

        logger.info(f"Successfully initialized {self.__class__.__name__}")

    async def get_multiple_users_by_username(
        self, twitch_names: List[str]
    ) -> Iterable[DBUser]:
        """
        Gets multiple users from database
        :param twitch_names: List of twitch names
        :return: List of users
        """
        users = await self.users_col.find(
            {"twitchUsername": {"$in": twitch_names}}
        ).to_list(length=len(twitch_names))
        return [DBUser(**user) for user in users]

    async def get_user_from_osu_username(self, osu_username: str) -> sqlite3.Row:
        """
        Gets the user details from database using osu username
        :param osu_username: osu username
        :return: User details of the user associated with osu username
        """
        osu_username = osu_username.lower().replace(" ", "_")

        cursor = await self.conn.execute(
            f"SELECT * from users WHERE osu_username=?", (osu_username,)
        )
        result = await cursor.fetchone()
        return result

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
        update_key = UpdateOne(
            {"name": name},
            {
                "$set": {
                    "name": name,
                    "value": default_value,
                    "description": description,
                    "type": _type,
                }
            },
            upsert=True,
        )
        await self.bulk_write_operations(operations=[update_key], col=self.settings_col)
        return

    async def bulk_write_operations(
        self,
        operations: Sequence[_WriteOp[_DocumentType]],
        col: Optional[Collection] = None,
    ):
        """Bulk write multiple operations to the given collection. \
        Defaults writing to "Metrics" collection."""
        if col is None:
            col = self.col
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

    async def toggle_setting(self, setting_key: str, twitch_username: str):
        """
        Toggles setting of given user
        :param setting_key: Key of the setting
        :param twitch_username: Twitch username
        :return: New value of the toggled setting.
        """
        twitch_username = twitch_username.lower()

        # Get current status of setting
        cur_value = await self.get_setting(setting_key, twitch_username)
        # Toggle it
        new_value = not cur_value
        # Set new value to setting
        await self.set_setting(setting_key, twitch_username, new_value)
        # Return new value
        return new_value

    async def get_enabled_status(self, twitch_username: str):
        """
        Returns if the channel has enabled requests or not
        :param twitch_username: Twitch username of the requested user
        :return: Channel enabled status
        """
        return await self.get_setting("enable", twitch_username)

    async def disable_channel(self, twitch_username: str):
        await self.set_setting(
            setting_key="enable", twitch_username=twitch_username, new_value=0
        )

    async def enable_channel(self, twitch_username: str):
        await self.set_setting(
            setting_key="enable", twitch_username=twitch_username, new_value=1
        )

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

    async def get_enabled_users(self) -> Iterable[DBUser]:
        """
        Gets all enabled users in db
        :return:
        """
        users = await self.users_col.find({"settings.enable": True}).to_list(length=None)
        return [DBUser(**user) for user in users]

    async def get_excluded_users(self, twitch_username: str) -> List[str]:
        """
        Gets excluded user settings of a user
        :param twitch_username: Twitch username
        :param return_mode: Can be 'str' (String of comma separated values) or 'list' (List of excluded users)
        :return: Comma separated values of excluded users
        """
        user = await self.get_user_from_twitch_username(twitch_username)
        return list(map(str.lower, user.excludedUsers))

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
        self.statistics_col.insert_one(
            {
                "requester_channel_name": requester_channel_name,
                "requested_beatmap_id": requested_beatmap_id,
                "requested_channel_name": requested_channel_name,
                "mods": mods,
                "timestamp": datetime.utcnow(),
            }
        )

    async def add_error(self, error_type: str, error_text: Optional[str] = None):
        """
        Adds an error entry to database
        This is used for statistics. It will keep information about osu! api issues and twitch api issues.
        For example, if we get rate-limited by osu, we will add:
        (timestamp.now(), 'echo', 'twitch', 'heyronii') to database
        """
        await self.conn.execute(
            "INSERT INTO errors (timestamp, type, error_text) VALUES (?,?,?)",
            (datetime.now(), error_type, error_text),
        )
        await self.conn.commit()

    async def update_user(self, user: DBUser):
        """
        Updates user in database
        :param user: User to update
        """
        await self.users_col.update_one(
            {"twitchId": user.twitchId}, {"$set": user.dict()}
        )
