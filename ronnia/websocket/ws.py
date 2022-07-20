import asyncio
import logging

from twitchio.websocket import WSConnection

log = logging.getLogger(__name__)


class PatchedWSConnection(WSConnection):
    async def join_channels(self, *channels: str):
        """|coro|
        Attempt to join the provided channels.
        Parameters
        ------------
        *channels : str
            An argument list of channels to attempt joining.
        """
        async with self._join_lock:  # acquire a lock, allowing only one join_channels at once...
            if len(channels) > 20:
                chunks = [channels[i:i + 20] for i in range(0, len(channels), 20)]
                for chunk in chunks:
                    for channel in chunk:
                        asyncio.create_task(self._join_channel(channel))
                    await asyncio.sleep(11)
            else:
                for channel in channels:
                    asyncio.create_task(self._join_channel(channel))

    async def _join_future_handle(self, fut: asyncio.Future, channel: str):
        try:
            await asyncio.wait_for(fut, timeout=60)
        except asyncio.TimeoutError:
            log.error(f'The channel "{channel}" was unable to be joined. Check the channel is valid.')
            self._join_pending.pop(channel)

            data = (
                f":{self.nick}.tmi.twitch.tv 353 {self.nick} = #TWITCHIOFAILURE :{channel}\r\n"
                f":{self.nick}.tmi.twitch.tv 366 {self.nick} #TWITCHIOFAILURE :End of /NAMES list"
            )

            await self._process_data(data)
