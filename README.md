![codeql-analysis](https://github.com/aticie/ronnia/actions/workflows/codeql-analysis.yml/badge.svg)
![docker-build](https://img.shields.io/docker/cloud/build/eatici/ronnia)


<div align="center">

# Ronnia - A Beatmap Request Bot

</div>

Ronnia is a Twitch/osu! bot that sends beatmap requests from Twitch chat to the streamer's in-game messages.

# Ronnia Dashboard - https://github.com/aticie/ronnia-web

Ronnia Dashboard is available at https://ronnia.me/

Registered streamers can now change their settings from website instead of IRC commmands!

## Disclaimer 📝

Currently, the bot runs under my personal account `heyronii`. I'm planning to change its name to `Ronnia` when I can get
a bot account on osu!. The only criteria I haven't met is the 6 months criteria. (Running since 2021/02/19)

## Usage✍️

You can sign-up to Ronnia if you have more than 100 followers on Twitch. To enable it on your Twitch channel, fill out this form:

[Ronnia Sign-up Form](https://forms.gle/x7iGkiEf1xQGzK7D9)

and I will set it up for you!

The reason I'm limiting sign-ups to 100 followers is that the bot is still in the testing stage. For now, I'm examining the memory usage and its behaviour under heavy load by allowing channels gradually.

After signing up you can:

[Check out the commands that you can use!](https://github.com/aticie/ronnia/wiki/Commands)

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

- Indicate requests with channel points. (or an option to only allow channel point requests) ✅
- Indicate requests for people that are loyal to streamer. (subs, vips, mods) ✅
- Recommend popular beatmaps requested in other streams. (ex: top5 most requested beatmaps of this week)
- Dockerize the bot. ✅
- Star rating limit for requests. (Min:5 - Max:10 stars) ✅
- Accept beatmaps with only selected ranked status. (Graveyard, Loved, Approved ...)
- External program to post now playing beatmap with `!np`.
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
LOG_LEVEL=INFO (https://docs.python.org/3/howto/logging.html#logging-levels Check other logging options here)
DB_DIR=mount/
```

Add yourself to database by doing:

```python
from helpers.database_helper import UserDatabase

twitch_username = heyronii  # Change this line to your username
osu_username = heyronii  # Change this line as well
twitch_user_id = # You need to find your twitch user id from twitch api (somehow)
osu_user_id = # osu.ppy.sh/u/{this_id_goes_here}

users_db = UserDatabase()
users_db.initialize()
users_db.add_user(twitch_username, osu_username, twitch_user_id, osu_user_id)
```

And then, the bot will be listening to messages on your channel. You can add other users by:

`!adduser <twitch_username> <osu_username>`

### Docker 🐳
#### Build and Run
To use docker either build dockerfile and supply a .env file for running:

`docker build -t ronnia-bot .`and `docker run --name ronnia --env-file .env ronnia-bot`

#### Docker Hub releases

Releases from 1.1.0 and onwards are published to Docker hub automatically. 
[You can find the repository here.](https://hub.docker.com/r/eatici/ronnia)

Use the release tag you want to use in docker-compose with the given template. 

```yaml
...
services:
  ronnia-bot:
    build: .
    image: eatici/ronnia:release-v1.x.x <- change the tag here!
```

To run the bot:

`docker-compose up -d`
