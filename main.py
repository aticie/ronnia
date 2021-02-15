import json

from dotenv import load_dotenv
from twitch_bot import TwitchBot

load_dotenv()

if __name__ == "__main__":
    # channels is a dict of twitch_channel: osu_nickname
    with open("channels.json") as f:
        channel_mappings = json.load(f)

    bot = TwitchBot(channel_mappings)
    bot.run()
