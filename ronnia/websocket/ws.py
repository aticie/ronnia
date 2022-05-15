import asyncio
import logging

from twitchio.websocket import WSConnection

log = logging.getLogger(__name__)


class RetryableWSConnection(WSConnection):

    async def _join_future_handle(self, fut: asyncio.Future, channel: str):
        try:
            await asyncio.wait_for(fut, timeout=11)
        except asyncio.TimeoutError:
            log.error(f'The channel "{channel}" was unable to be joined. Check the channel is valid.')
            self._join_pending.pop(channel)

            data = (
                f":{self.nick}.tmi.twitch.tv 353 {self.nick} = #TWITCHIOFAILURE :{channel}\r\n"
                f":{self.nick}.tmi.twitch.tv 366 {self.nick} #TWITCHIOFAILURE :End of /NAMES list"
            )

            self._client.channels_join_failed.append(channel)
            await self._process_data(data)
