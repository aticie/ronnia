<div align="center">

# ronnia - A Beatmap Request Bot

</div>

ronnia is a Twitch/osu! bot that sends beatmap requests from Twitch chat to the streamer's in-game messages.

## Disclaimer 🤓

Currently, the bot runs under my personal account `heyronii`. I'm planning to change its name to `ronnia` when I can get
a bot account on osu!. The only criteria I haven't met is the 6 months criteria. (Running since 2021/02/20)

## Usage✍️

The bot right now is still in very early stages. You may encounter many bugs as I add new features. (Use at your own
risk!)

Currently I'm manually adding channels that want to use the bot. To enable it on your Twitch channel, send me a pm on:

- Discord: `heyronii#9925`
- osu!: https://osu.ppy.sh/users/5642779
- Twitch: https://www.twitch.tv/heyronii

and I will set it up for you!

The reason I'm not letting everyone set-up the bot by themselves is that I don't know if it might get too many requests
and my osu! account might get silenced. For now, I'm testing the stress by allowing channels in a slow manner.

## Cool Gifs 😎
![Just send a beatmap link](cool_gifs/usage.gif)

## Setup 📦

### Requirements

- Python 3.6+

Library requirements can be installed with:

`pip install -r requirements.txt`

### Hosting the bot

To host the bot, clone this repository. Create a .env file at the root folder with the following environment variables:

```
TMI_TOKEN=**** (Get your credentials from here https://twitchapps.com/tmi/)
CLIENT_ID=**** 
BOT_NICK=heyronii (Change this to your Twitch username)
BOT_PREFIX=! (Currently unused, might change in future)
IRC_PASSWORD=**** (Get yours from here: https://osu.ppy.sh/p/irc)
OSU_API_KEY=**** (Get yours from here: https://osu.ppy.sh/p/api)
```

Finally, edit the channels.json file for the people you are serving. It should be json parseable.

```
{ 
  "twitch_username": "osu_username",
  "second_twitch_username": "second_osu_username",
  ...
}
```