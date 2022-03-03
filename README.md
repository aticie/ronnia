![codeql-analysis](https://github.com/aticie/ronnia/actions/workflows/codeql-analysis.yml/badge.svg)
![docker-build](https://img.shields.io/docker/cloud/build/eatici/ronnia)


<div align="center">

# Ronnia - A Beatmap Request Bot

</div>

Ronnia is a Twitch/osu! bot that sends beatmap requests from Twitch chat to the streamer's in-game messages.

# Usage✍️

### [Sign-up for the testing version from here!](https://testing.ronnia.me/)

# Ronnia Dashboard 📋

Ronnia Dashboard is available at https://ronnia.me/

Project page: https://github.com/aticie/ronnia-web

Registered streamers can now change their settings from website instead of IRC commmands!

## Disclaimer 📝

I am using [Azure Service Bus](https://azure.microsoft.com/en-us/services/service-bus/) in this project. I found it 
very useful when scaling up the user limit of the bot. If you are not familiar with it and want to host the bot yourself,
you can use the self-host implementation from [this repository](https://github.com/aticie/ronnia-selfhost). It's much 
easier to use and has Windows binaries.

## FAQ 🙋❓

**Q:** My request hasn't been sent? Why?

**A:** There could be multiple reasons for this. In order to process your request:
- The stream must be on.
- Streamer must be playing osu!
- You should not request more than 1 map per 30 seconds.
- Make sure you are not the streamer. (Some streamers have self-np bots)

**These restrictions are not applied to the testing version.** 

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

Please use the self-host version of the bot. 

https://github.com/aticie/ronnia-selfhost

### Docker 🐳
#### Build and Run
To use docker either build dockerfile and supply a .env file for running:

`docker build -t ronnia-bot .`and `docker run --name ronnia --env-file .env ronnia-bot`

#### ~~Docker Hub Releases [Deprecated]~~

Releases from 1.1.0 and onwards are published to Docker hub automatically.

I changed the cloud build provider to [Google Cloud Build](https://cloud.google.com/docs/build/quickstart-build). 
Versions 2.0.0 and above will be published to Google Cloud Container Registry automatically.

Environment variables required to run both the bot and the website are:

```
OSU_CLIENT_ID=****  # Create a client from osu! settings 
OSU_CLIENT_SECRET=****
OSU_REDIRECT_URI=****
OSU_USERNAME=heyronii  # Change this to your osu! username
OSU_API_KEY=****  # Get yours from here: https://osu.ppy.sh/p/api
IRC_PASSWORD=****  # Get yours from here: https://osu.ppy.sh/p/irc

TWITCH_CLIENT_ID=****  # Get these from https://dev.twitch.tv/console
TWITCH_CLIENT_SECRET=****
TWITCH_REDIRECT_URI=****
TMI_TOKEN=****  # Get your credentials from here https://twitchapps.com/tmi/
BOT_NICK=ronnia_testing  # Change this to your Twitch username
BOT_PREFIX=!  # Twitch and osu! bot prefix for commands

JWT_SECRET_KEY=****  # Create a random key, doesn't matter what it is
JWT_ALGORITHM=HS256

SERVICE_BUS_CONNECTION_STR=Endpoint=sb://... # Get yours from azure portal: https://azure.microsoft.com/en-us/services/service-bus/

PUBLISH_PORT=9000 
DB_DIR=/mount
ENVIRONMENT=testing  # testing or production
PYTHONUNBUFFERED=1
LOG_LEVEL=DEBUG  # https://docs.python.org/3/howto/logging.html#logging-levels Check other logging options here
```

### Tests

To run the tests, you need to add the `src` folder to the PYTHONPATH variable. You can do this by adding:

`sys.path.insert(0, 'src')`

to `run.py`.
