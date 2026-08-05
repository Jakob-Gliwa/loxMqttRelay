import asyncio
import aiohttp
from collections.abc import Sequence
from typing import Any 
from loxmqttrelay.config import global_config
from loxmqttrelay.logging_config import get_lazy_logger
from loxwebsocket.lox_ws_api import loxwebsocket

logger = get_lazy_logger(__name__)

# Initialize global instances with default values


class HttpMiniserverHandler:

    ms_ip = global_config.miniserver.miniserver_ip
    ms_port = global_config.miniserver.miniserver_port
    ms_user = global_config.miniserver.miniserver_user
    ms_pass = global_config.miniserver.miniserver_pass
    # Read once at import; config is immutable between restarts (a config change
    # re-execs the process), so the per-message send path needs no live lookup.
    use_websocket = global_config.miniserver.use_websocket
    enable_mock_miniserver=global_config.debug.enable_mock
    mock_ms_ip=global_config.debug.mock_ip
    connection_semaphore = asyncio.Semaphore(global_config.miniserver.miniserver_max_parallel_connections)  # Default to 5 parallel connections
    target_ip = mock_ms_ip if (mock_ms_ip and enable_mock_miniserver) else ms_ip
    # Construct WebSocket URL with proper port handling
    protocol = "https" if ms_port == 443 else "http"
    if ms_port not in [80, 443]:
        ws_base_url = f"{protocol}://{target_ip}:{ms_port}"
        http_base_url = f"http://{target_ip}:{ms_port}"
    else:
        ws_base_url = f"{protocol}://{target_ip}"
        http_base_url = f"http://{target_ip}"
    auth = aiohttp.BasicAuth(ms_user, ms_pass) if ms_user and ms_pass else None
    # Increase the timeout to 10 seconds
    timeout = aiohttp.ClientTimeout(total=10)


    """Handler for processing and sending data to Miniserver via HTTP."""
    def __init__(self):
        logger.info("MQTT Miniserver Handler created")

    async def send_to_minisever_via_websocket(
        self,
        topic: str,
        normalized_topic: str,
        value: Any
    ) -> None:
        """
        Sends data to the Loxone Miniserver via a WebSocket connection.
        Returns a dictionary with results for each topic.
        """
        # Determine target IP
        logger.debug("Using miniserver address: %s %s", self.target_ip, '(mock)' if (self.mock_ms_ip and self.enable_mock_miniserver) else '(real)')

        ws_client = loxwebsocket
        if "CONNECTED" not in ws_client.state:
            await ws_client.connect(user=self.ms_user, password=self.ms_pass, loxone_url=self.ws_base_url, receive_updates=False)

        try:
            await ws_client.send_websocket_command(normalized_topic, str(value))
            logger.debug("Sent %s (as %s)=%s to Miniserver successfully via WebSocket.", topic, normalized_topic, value)
            return 
        except Exception as e:
            error_msg = f"Error sending {topic} (as {normalized_topic})={value} to Miniserver via WebSocket: {str(e)}"
            logger.error(error_msg)
            return 


    async def send_to_miniserver_via_http(
        self,
        topic: str,
        normalized_topic: str,
        value: Any
    ) -> None:
        """
        Send data to Miniserver with rate limiting.
        If mock_ms_ip is provided and enable_mock_miniserver is True, mock server will be used instead of ms_ip.
        Returns a dictionary with results for each topic.
        """
        # Use mock miniserver IP only if both provided and enabled
        logger.debug("Using miniserver address: %s %s", self.target_ip, '(mock)' if (self.mock_ms_ip and self.enable_mock_miniserver) else '(real)')

        async with aiohttp.ClientSession(auth=self.auth, timeout=self.timeout) as session:
            # Ensure value is converted to string
            safe_value = str(value)
            # Use pre-built HTTP base URL
            url = f"{self.http_base_url}/dev/sps/io/{normalized_topic}/{safe_value}"
            logger.debug("Sending to %s", url)
            
            try:
                # Use semaphore to limit concurrent connections
                async with self.connection_semaphore:
                    async with session.get(url) as resp:
                        if resp.status != 200:
                            logger.warning(f"Miniserver returned {resp.status} for topic {topic} (URL: {url})")
                        else:
                            logger.debug("Sent %s=%s to Miniserver successfully.", topic, value)
                        return { 'code': resp.status }
            except asyncio.TimeoutError:
                error_msg = f" Error 408: Timeout while sending {topic} (as {normalized_topic})={value} to Miniserver (URL: {url}): request timed out after 10 seconds"
                logger.error(error_msg)
                return 
            except asyncio.CancelledError:
                error_msg = f"Error 499: Request for {topic} (as {normalized_topic})={value} was cancelled (URL: {url})"
                logger.error(error_msg)
                return 
            except OSError as e:
                error_msg = f"Error 503: Connection error sending {topic} (as {normalized_topic})={value} to Miniserver (URL: {url}): {str(e)}"
                logger.error(error_msg)
                return 
            except aiohttp.ClientError as e:
                error_msg = f"Error 500: Client error sending {topic} (as {normalized_topic})={value} to Miniserver (URL: {url}): {str(e)}"
                logger.error(error_msg)
                return 
            except Exception as e:
                error_msg = f"Error 500: Unexpected error sending {topic} (as {normalized_topic})={value} to Miniserver (URL: {url}): {str(e)}"
                logger.error(error_msg)
                return 
    
    async def send_to_miniserver(
        self,
        topic: str,
        normalized_topic: str,
        value: Any,
    ) -> None:
        """
        Process data and send it to Miniserver.
        
        Args:
            data: The data to process and send
            mqtt_publish_callback: Callback for MQTT publishing (required for topic forwarding)
            
        Returns:
            None
        """
        logger.debug("Sending %s (as %s)=%s to Miniserver", topic, normalized_topic, value)
        # Send to Miniserver using WebSocket or HTTP based on config
        if self.use_websocket:
            await self.send_to_minisever_via_websocket(topic, normalized_topic, value)
        else:
            await self.send_to_miniserver_via_http(topic, normalized_topic, value)

        return 

    async def send_batch_to_miniserver(
        self,
        items: Sequence[tuple[str, str, Any]],
    ) -> None:
        """
        Send every value expanded out of a single MQTT message.

        The Rust side hands the whole message over in one call instead of one
        call per JSON leaf, which is where the crossing cost used to pile up.

        WebSocket mode walks the batch sequentially: all values share one
        connection, so sending them in order also removes the race where several
        concurrent values each found the socket disconnected and opened it.
        HTTP mode keeps the requests concurrent - they are independent round
        trips, still bounded by ``connection_semaphore``.
        """
        if not items:
            return

        if self.use_websocket:
            for topic, normalized_topic, value in items:
                await self.send_to_miniserver(topic, normalized_topic, value)
            return

        results = await asyncio.gather(
            *(
                self.send_to_miniserver(topic, normalized_topic, value)
                for topic, normalized_topic, value in items
            ),
            return_exceptions=True,
        )
        for (topic, normalized_topic, value), result in zip(items, results):
            if isinstance(result, BaseException):
                logger.error(
                    "Error sending %s (as %s)=%s to Miniserver: %s",
                    topic, normalized_topic, value, result,
                )

http_miniserver_handler = HttpMiniserverHandler()