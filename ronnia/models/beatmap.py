import enum

from pydantic import BaseModel


class BeatmapType(enum.StrEnum):
    MAP = "Beatmap"
    MAPSET = "Beatmapset"


class Beatmap(BaseModel):
    id: int
    type: BeatmapType
    mods: str
