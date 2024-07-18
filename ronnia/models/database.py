from typing import List

from pydantic import BaseModel, Field


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
