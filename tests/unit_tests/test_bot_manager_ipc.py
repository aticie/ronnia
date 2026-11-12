"""Regression tests for the internal BotManager -> TwitchBot socket.

That socket carried one update per polling interval but the receiver closed it
after a single quiet interval, so every cycle ended in a reset traceback on both
sides. The receiver must tolerate a quiet socket and the sender must reconnect
immediately instead of writing into a dead pipe.
"""

import asyncio
import socket
import unittest
from unittest.mock import patch

from ronnia.bots.bot_manager import BotManager


class FakeReceiver:
    """Stand-in for TwitchBot: only the attributes the handler touches."""

    MAX_IPC_IDLE_MISSES = 3

    def __init__(self, read_timeout: float = 0.05) -> None:
        self.ipc_read_timeout = read_timeout
        self.batches: list[list[str]] = []
        self.closed = asyncio.Event()

    async def join_streaming_channels(self, message: list[str]) -> None:
        self.batches.append(message)

    async def handle_bot_manager_message(self, reader, writer):
        await TwitchBotHandlerTest.call_handler(self, reader, writer)


class TwitchBotHandlerTest(unittest.IsolatedAsyncioTestCase):
    @staticmethod
    async def call_handler(receiver, reader, writer):
        from ronnia.bots.twitch_bot import TwitchBot

        try:
            await TwitchBot.handle_bot_manager_message(receiver, reader, writer)
        finally:
            receiver.closed.set()

    async def start(self, receiver):
        server = await asyncio.start_server(
            lambda r, w: receiver.handle_bot_manager_message(r, w),
            "127.0.0.1",
            0,
        )
        port = server.sockets[0].getsockname()[1]
        return server, port

    async def test_quiet_socket_survives_a_missed_interval(self) -> None:
        receiver = FakeReceiver()
        server, port = await self.start(receiver)
        try:
            reader, writer = await asyncio.open_connection("127.0.0.1", port)
            writer.write(b"alpha,beta\n")
            await writer.drain()

            # Longer than one read timeout, shorter than the close threshold.
            await asyncio.sleep(0.12)
            self.assertFalse(receiver.closed.is_set(), "one quiet interval must not close the socket")

            writer.write(b"alpha\n")
            await writer.drain()
            for _ in range(40):
                if len(receiver.batches) == 2:
                    break
                await asyncio.sleep(0.02)
            self.assertEqual(receiver.batches, [["alpha", "beta"], ["alpha"]])
            self.assertFalse(receiver.closed.is_set())

            writer.close()
            await writer.wait_closed()
        finally:
            server.close()
            await server.wait_closed()

    async def test_stale_socket_is_closed_after_repeated_misses(self) -> None:
        receiver = FakeReceiver()
        server, port = await self.start(receiver)
        try:
            _, writer = await asyncio.open_connection("127.0.0.1", port)
            writer.write(b"alpha\n")
            await writer.drain()

            for _ in range(60):
                if receiver.closed.is_set():
                    break
                await asyncio.sleep(0.02)
            self.assertTrue(receiver.closed.is_set(), "a silent manager must eventually be dropped")
            writer.close()
            await writer.wait_closed()
        finally:
            server.close()
            await server.wait_closed()


class ManagerReconnectTest(unittest.IsolatedAsyncioTestCase):
    async def test_connection_loss_reconnects_without_waiting_a_cycle(self) -> None:
        manager = object.__new__(BotManager)
        manager.twitch_bot = None
        manager.announcement_tracker = None

        async def no_users():
            return set()

        async def no_announcements():
            return None

        manager.get_streaming_users = no_users
        manager.announce_pending = no_announcements

        accepted = 0
        server = None

        async def handler(reader, writer):
            nonlocal accepted
            accepted += 1
            writer.close()  # drop the peer immediately: the reset path

        server = await asyncio.start_server(handler, "127.0.0.1", 0)
        port = server.sockets[0].getsockname()[1]
        fake_socket = type(
            "Sock", (), {"getsockname": lambda self: ("127.0.0.1", port)}
        )()
        try:
            with patch("ronnia.bots.bot_manager.STREAMING_USERS_UPDATE_SLEEP", 0.01):
                task = asyncio.create_task(manager.listener(fake_socket))
                for _ in range(80):
                    if accepted >= 3:
                        break
                    await asyncio.sleep(0.02)
                task.cancel()
                await asyncio.gather(task, return_exceptions=True)
        finally:
            server.close()
            await server.wait_closed()

        self.assertGreaterEqual(accepted, 3, "the manager must reconnect promptly after a loss")


if __name__ == "__main__":  # pragma: no cover
    unittest.main()
