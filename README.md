<div align="center">

# ronnia - A Beatmap Request Bot

</div>

ronnia is a Twitch/osu! bot that sends beatmap requests from Twitch chat to the streamer's in-game messages.

## Disclaimer 📝

Currently, the bot runs under my personal account `heyronii`. I'm planning to change its name to `ronnia` when I can get
a bot account on osu!. The only criteria I haven't met is the 6 months criteria. (Running since 2021/02/20)

## Usage✍️

[Check out the commands that you can use!](https://github.com/aticie/ronnia/wiki/Commands)

Currently, I'm manually adding channels that want to use the bot. To enable it on your Twitch channel, send me a pm on:

- Discord: `heyronii#9925`
- osu!: https://osu.ppy.sh/users/5642779
- Twitch: https://www.twitch.tv/heyronii

and I will set it up for you!

The reason I'm not letting everyone set-up the bot by themselves is that I don't know if it might get too many requests
and my osu! account might get silenced. For now, I'm examining the memory usage and its behaviour under heavy load by allowing channels gradually.

## FAQ 🙋❓

**Q:** My request hasn't been sent? Why?

**A:** There could be multiple reasons for this. In order to process your request:
- The stream must be on.
- Streamer must be playing osu!
- You should not request more than 1 map per 30 seconds.
- Make sure you are not the streamer. (Some streamers have self-np bots)

**Q:** How do I get this bot on my channel?

**A:** Just dm me on discord. heyronii#9925

**Q:** OMG why is this command not working????

**A:** Please open up an issue or inform me on discord about it. I'll try to fix it as soon as I can.

## Cool Gifs 😎
![Just send a beatmap link](cool_gifs/usage.gif)

## Features on the Roadmap 🏗️

- Priority requests with channel points. (or an option to only allow channel point requests)
- Priority requests for people that are loyal to streamer. (subs, vips, mods)
- Recommend popular beatmaps requested in other streams. (ex: top5 most requested beatmaps of this week)
- Dockerize the bot. ✅
- Star rating limit for requests. (Min:5 - Max:10 stars)
- Accept beatmaps with only selected ranked status. (Graveyard, Loved, Approved ...)
- Other game modes than standard.

## Setup 📦

### Requirements

- Python 3.6+

Library requirements can be installed with:

`pip install -r requirements.txt`

### Hosting the bot

To host the bot, clone this repository. Create a .env file at the root folder with the following environment variables:

```
TMI_TOKEN=**** (Get your credentials from here https://twitchapps.com/tmi/)
CLIENT_ID=**** (Get these from https://dev.twitch.tv/console)
CLIENT_SECRET=****
BOT_NICK=heyronii (Change this to your Twitch username)
BOT_PREFIX=! (Currently unused, might change in future)
OSU_USERNAME=heyronii (Change this to your osu! username)
IRC_PASSWORD=**** (Get yours from here: https://osu.ppy.sh/p/irc)
OSU_API_KEY=**** (Get yours from here: https://osu.ppy.sh/p/api)
```

Add yourself to database by doing:

```python
from utils.database_helper import UserDatabase

twitch_username = heyronii  # Change this line to your username
osu_username = heyronii  # Change this line as well

users_db = UserDatabase()
users_db.initialize()
users_db.add_user(twitch_username, osu_username)
```

And then, the bot will be listening to messages on your channel. You can add other users by:

`!adduser <twitch_username> <osu_username>`
