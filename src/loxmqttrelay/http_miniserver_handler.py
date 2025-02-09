import logging
import asyncio
import aiohttp
from typing import Any 
from loxmqttrelay.config import global_config
from loxwebsocket.lox_ws_api import loxwebsocket

logger = logging.getLogger(__name__)

# Initialize global instances with default values


class HttpMiniserverHandler:

    ms_ip = global_config.miniserver.miniserver_ip
    ms_port = global_config.miniserver.miniserver_port
    ms_user = global_config.miniserver.miniserver_user
    ms_pass = global_config.miniserver.miniserver_pass
    enable_mock_miniserver=global_config.debug.enable_mock
    mock_ms_ip=global_config.debug.mock_ip
    connection_semaphore = asyncio.Semaphore(global_config.miniserver.miniserver_max_parallel_connections)  # Default to 5 parallel connections
    target_ip = mock_ms_ip if (mock_ms_ip and enable_mock_miniserver) else ms_ip
    ws_base_url = f"{"https" if ms_port == 443 else "http"}://{target_ip}"
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
        logger.debug(f"Using miniserver address: {self.target_ip} {'(mock)' if (self.mock_ms_ip and self.enable_mock_miniserver) else '(real)'}")

        ws_client = loxwebsocket
        if "CONNECTED" not in ws_client.state:
            await ws_client.connect(user=self.ms_user, password=self.ms_pass, loxone_url=self.ws_base_url, receive_updates=False)

        try:
            await ws_client.send_websocket_command(normalized_topic, str(value))
            logger.debug(f"Sent {topic} (as {normalized_topic})={value} to Miniserver successfully via WebSocket.")
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
        logger.debug(f"Using miniserver address: {self.target_ip} {'(mock)' if (self.mock_ms_ip and self.enable_mock_miniserver) else '(real)'}")

        async with aiohttp.ClientSession(auth=self.auth, timeout=self.timeout) as session:
            # Ensure value is converted to string
            safe_value = str(value)
            url = f"http://{self.target_ip}/dev/sps/io/{normalized_topic}/{safe_value}"
            logger.debug(f"Sending to {url}")
            
            try:
                # Use semaphore to limit concurrent connections
                async with self.connection_semaphore:
                    async with session.get(url) as resp:
                        if resp.status != 200:
                            logger.warning(f"Miniserver returned {resp.status} for topic {topic} (URL: {url})")
                        else:
                            logger.debug(f"Sent {topic}={value} to Miniserver successfully.")
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
        logger.debug(f"Sending {topic} (as {normalized_topic})={value} to Miniserver")
        # Send to Miniserver using WebSocket or HTTP based on config
        if global_config.miniserver.use_websocket:
            await self.send_to_minisever_via_websocket(topic, normalized_topic, value)
        else:
            await self.send_to_miniserver_via_http(topic, normalized_topic, value)

        return 

http_miniserver_handler = HttpMiniserverHandler()