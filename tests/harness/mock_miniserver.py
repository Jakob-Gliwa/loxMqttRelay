"""An in-process stand-in for the Loxone Miniserver.

The relay reaches the Miniserver over a websocket that negotiates an AES/RSA
session and a token, so a mock Miniserver cannot be an arbitrary server you
point an IP at - it has to take the place of the client library. This module
does that: it swaps out ``loxwebsocket`` and records what the relay would have
sent, while letting the caller decide how the connection behaves.

Meant for tests a developer runs deliberately. They carry the ``harness``
marker and a plain ``pytest`` run skips them - use ``pytest -m harness``.
"""

import asyncio
import enum
from contextlib import ExitStack, contextmanager
from typing import Any, Iterator
from unittest.mock import patch

# Where the relay holds its reference to the websocket singleton. Patching the
# name in each module is what makes the substitution work at all: both did
# ``from loxwebsocket.lox_ws_api import loxwebsocket`` at import time.
_PATCH_TARGETS = (
    "loxmqttrelay.http_miniserver_handler.loxwebsocket",
    "loxmqttrelay.main.loxwebsocket",
)


class MockMiniserver:
    """The subset of ``loxwebsocket`` the relay uses, with a memory.

    Every command the relay sends lands in :attr:`commands`. The connection
    itself is yours to steer: start it in any :attr:`state`, make the handshake
    fail or hang, drop it mid-test, bring it back.
    """

    class EventType(enum.IntEnum):
        ANY = 0
        INITIALIZED = 1
        CONNECTED = 2
        CONNECTION_CLOSED = 3
        RECONNECTED = 4

    def __init__(
        self,
        state: str = "CLOSED",
        fail_connect: bool = False,
        connect_delay: float = 0.0,
    ):
        self.state = state
        self.fail_connect = fail_connect
        self.connect_delay = connect_delay
        self.commands: list[tuple[str, str]] = []
        self.connects: list[dict[str, Any]] = []
        self._event_callbacks: dict[Any, list["MockMiniserver.EventType"]] = {}

    # -- the loxwebsocket surface the relay calls -------------------------

    async def connect(
        self,
        user: str,
        password: str,
        loxone_url: str,
        receive_updates: bool = True,
        **kwargs: Any,
    ) -> None:
        self.connects.append(
            {
                "user": user,
                "password": password,
                "loxone_url": loxone_url,
                "receive_updates": receive_updates,
            }
        )
        # Always yields, so concurrent senders can interleave the way they
        # would around a real handshake.
        await asyncio.sleep(self.connect_delay)
        if self.fail_connect:
            self.state = "CLOSED"
            raise ConnectionError(f"mock Miniserver at {loxone_url} is not answering")
        self.state = "CONNECTED"

    async def send_websocket_command(self, device_uuid: str, value: str) -> None:
        if self.state != "CONNECTED":
            raise ConnectionError(f"mock Miniserver is {self.state}, not connected")
        self.commands.append((device_uuid, value))

    async def stop(self) -> int:
        self.state = "CLOSED"
        return 0

    def add_event_callback(self, callback: Any, event_types: list[Any] | None = None) -> None:
        self._event_callbacks[callback] = event_types or [self.EventType.ANY]

    async def send_event(self, event_type: "MockMiniserver.EventType") -> None:
        """Dispatch a lifecycle event.

        Unlike the real library the callbacks are awaited rather than spawned,
        so a test can assert on their effect the moment this returns.
        """
        for callback, event_types in list(self._event_callbacks.items()):
            if self.EventType.ANY in event_types or event_type in event_types:
                await callback()

    # -- knobs for the test -----------------------------------------------

    def drop_connection(self) -> None:
        """Pull the socket out from under the relay."""
        self.state = "CLOSED"

    async def reconnect(self) -> None:
        """Come back the way the library does - by announcing it."""
        self.state = "CONNECTED"
        await self.send_event(self.EventType.RECONNECTED)

    def values_for(self, normalized_topic: str) -> list[str]:
        """Every value that arrived for one Loxone input, in order."""
        return [value for topic, value in self.commands if topic == normalized_topic]

    def clear(self) -> None:
        self.commands.clear()
        self.connects.clear()


@contextmanager
def mock_miniserver(**kwargs: Any) -> Iterator[MockMiniserver]:
    """Run the relay against a :class:`MockMiniserver` instead of a real one.

    Keyword arguments are handed to :class:`MockMiniserver`, e.g.
    ``mock_miniserver(state="CONNECTED")`` to skip the handshake.
    """
    miniserver = MockMiniserver(**kwargs)
    with ExitStack() as stack:
        for target in _PATCH_TARGETS:
            stack.enter_context(patch(target, miniserver))
        yield miniserver
