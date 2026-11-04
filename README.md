![codeql-analysis](https://github.com/aticie/ronnia/actions/workflows/codeql-analysis.yml/badge.svg)


<div align="center">

# Ronnia - A Beatmap Request Bot

</div>

Ronnia is a Twitch/osu! bot that sends beatmap requests from Twitch chat to the streamer's in-game messages.

# Usage✍️

### [Sign-up for the bot from here!](https://ronnia.me/)

# Ronnia Dashboard 📋

Ronnia Dashboard is available at https://ronnia.me/

Project page: https://github.com/aticie/ronnia-web

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

**A:** If you can see your settings page, then you are good to go! If something else happened and you see an error, dm me on discord: heyronii#9925

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

# Coexistence bridge (new dashboard is the source of truth)
RONNIA_API_URL=http://127.0.0.1:4343  # New dashboard internal API origin
RONNIA_INTERNAL_TOKEN=****  # Same value as the new service's INTERNAL_API_TOKEN
RONNIA_MIGRATION_URL=https://ronnia.example  # Shown in migration announcements
RONNIA_LEGACY_MODE=true  # Optional; true by default when URL+token are set
DRY_RUN=false  # Optional; suppresses migration announcements when true
```

### Legacy bridge mode

The new dashboard/SQLite service owns accounts and settings. When
`RONNIA_API_URL` and `RONNIA_INTERNAL_TOKEN` are both configured, the old bot:

- reads enabled users and settings from `GET /internal/legacy/channels*`,
- claims each request with `POST /internal/legacy/claim` before sending an osu! PM,
- reports the delivery outcome/statistics with `POST /internal/legacy/deliveries/{id}`,
- publishes the exact current live channel set (including an empty set) with
  `POST /internal/legacy/live` on every polling cycle, and uses the live Twitch
  login for migration announcements,
- never reads Mongo `Users`/`Settings`, never deletes accounts, and never writes
  old Mongo request statistics.

`remove_user` is a no-op in bridge mode, so failed Twitch channel joins cannot
delete an account from the new dashboard. If a claim is rejected (channel
migrated or duplicate delivery) the bot sends nothing. If the osu! PM fails,
the delivery is reported as failed and is not retried. Once per Twitch live
session, each eligible legacy user is sent one Twitch chat message and one osu!
PM containing `RONNIA_MIGRATION_URL`.

If the bridge is not configured, the bot keeps the original Mongo behaviour.
Setting `RONNIA_LEGACY_MODE=true` without both API settings is a startup error;
setting it to `false` forces the old behaviour even if the API settings exist.
Internal tokens are never logged.


### Tests

Run the unit tests from the repository root:

```
python -m unittest discover -s tests -p "test_*.py" -t .
```

The bridge tests use an in-process aiohttp fake server; they never contact the
real dashboard, Twitch, or osu!.
