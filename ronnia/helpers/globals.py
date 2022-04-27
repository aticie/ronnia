import os

from helpers.osu_api_helper import OsuApiV2, OsuChatApiV2

OSU_API_V2 = OsuApiV2(client_id=os.getenv('OSU_CLIENT_ID'), client_secret=os.getenv('OSU_CLIENT_SECRET'))
OSU_CHAT_API_V2 = OsuChatApiV2(client_id=os.getenv('OSU_CLIENT_ID'), client_secret=os.getenv('OSU_CLIENT_SECRET'))
