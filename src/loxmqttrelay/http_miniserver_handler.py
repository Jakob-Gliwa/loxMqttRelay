import asyncio
import time
from collections.abc import Sequence
from typing import Any 
from loxmqttrelay.config import global_config
from loxmqttrelay.logging_config import get_lazy_logger
from loxwebsocket.lox_ws_api import loxwebsocket

logger = get_lazy_logger(__name__)


class HttpMiniserverHandler:
    """Handler for processing and sending data to the Miniserver via websocket."""

    ms_ip = global_config.miniserver.miniserver_ip
    ms_port = global_config.miniserver.miniserver_port
    ms_user = global_config.miniserver.miniserver_user
    ms_pass = global_config.miniserver.miniserver_pass
    # Construct WebSocket URL with proper port handling
    protocol = "https" if ms_port == 443 else "http"
    if ms_port not in [80, 443]:
        ws_base_url = f"{protocol}://{ms_ip}:{ms_port}"
    else:
        ws_base_url = f"{protocol}://{ms_ip}"
    # A handshake is an HTTP round trip for the session key, the websocket
    # upgrade, the key exchange and a token. While the Miniserver is
    # unreachable, every message would otherwise pay that price again.
    connect_retry_delay = 15.0

    def __init__(self):
        # One connect at a time: a burst of messages arriving while the socket
        # is down must not turn into a burst of handshakes.
        self._connect_lock = asyncio.Lock()
        self._last_connect_failure = 0.0
        logger.info("MQTT Miniserver Handler created")

    async def _ensure_websocket(self) -> bool:
        """Report whether a websocket connection is available for sending.

        Connect failures are answered with False rather than raised: a batch is
        one MQTT message, and an exception escaping the first value would take
        the rest of that message with it.
        """
        if loxwebsocket.state == "CONNECTED":
            return True
        if loxwebsocket.state != "CLOSED":
            # RECONNECTING means the library is already retrying and a second
            # handshake would race it; STOPPING means the relay is going down.
            logger.debug("Websocket is %s, not sending", loxwebsocket.state)
            return False

        async with self._connect_lock:
            # Another message may have connected - or just failed - while this
            # one waited for the lock.
            if loxwebsocket.state == "CONNECTED":
                return True
            since_failure = time.monotonic() - self._last_connect_failure
            if since_failure < self.connect_retry_delay:
                logger.debug(
                    "Not connecting: the last attempt failed %.1fs ago", since_failure,
                )
                return False

            try:
                await loxwebsocket.connect(
                    user=self.ms_user,
                    password=self.ms_pass,
                    loxone_url=self.ws_base_url,
                    receive_updates=False,
                )
            except Exception as e:
                self._last_connect_failure = time.monotonic()
                logger.error(
                    f"Cannot reach the Miniserver websocket at {self.ws_base_url}: {e} - "
                    f"not trying again for {self.connect_retry_delay:.0f}s"
                )
                return False
            return loxwebsocket.state == "CONNECTED"

    async def _send(
        self,
        topic: str,
        normalized_topic: str,
        value: Any,
    ) -> None:
        """Put one value on the wire, with the connection already established.

        Split out so a batch pays the connection check once instead of once per
        value: all values of a message share the socket, so re-deciding per value
        cost a coroutine and a state read for an answer that cannot have changed.
        """
        logger.debug("Sending %s (as %s)=%s to Miniserver", topic, normalized_topic, value)
        try:
            await loxwebsocket.send_websocket_command(normalized_topic, str(value))
            logger.debug("Sent %s (as %s)=%s to Miniserver successfully.", topic, normalized_topic, value)
        except Exception as e:
            logger.error(
                f"Error sending {topic} (as {normalized_topic})={value} to Miniserver: {str(e)}"
            )

    async def send_to_miniserver(
        self,
        topic: str,
        normalized_topic: str,
        value: Any,
    ) -> None:
        """
        Send a single value to the Loxone Miniserver over the websocket.
        """
        if not await self._ensure_websocket():
            logger.warning(
                "Dropped %s (as %s)=%s: no websocket connection to the Miniserver",
                topic, normalized_topic, value,
            )
            return

        await self._send(topic, normalized_topic, value)

    async def send_batch_to_miniserver(
        self,
        items: Sequence[tuple[str, str, Any]],
    ) -> None:
        """
        Send every value expanded out of a single MQTT message.

        The Rust side hands the whole message over in one call instead of one
        call per JSON leaf, which is where the crossing cost used to pile up.

        The batch is walked sequentially: all values share one connection, so
        sending them in order also removes the race where several concurrent
        values each found the socket disconnected and opened it. That shared fate
        is also why the connection is settled once here and the loop below sends
        through `_send`, which does not check again.
        """
        if not items:
            return

        if not await self._ensure_websocket():
            # Nothing retries these, so name what was lost. One line for the
            # message instead of one per value - they share the connection,
            # so they share its fate.
            logger.warning(
                "Dropped %d value(s) from '%s': no websocket connection to the Miniserver",
                len(items), items[0][0],
            )
            return

        for topic, normalized_topic, value in items:
            # One bad value must not take the rest of the message with it. `_send`
            # already absorbs a failing send; this catches what it cannot, such as
            # a value whose own formatting raises.
            try:
                await self._send(topic, normalized_topic, value)
            except Exception as e:
                logger.error(
                    "Error sending %s (as %s)=%s to Miniserver: %s",
                    topic, normalized_topic, value, e,
                )

http_miniserver_handler = HttpMiniserverHandler()
