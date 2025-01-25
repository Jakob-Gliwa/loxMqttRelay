import logging
import asyncio
import aiohttp
from typing import Dict, Any, Callable, Awaitable, List, Optional, Union
from loxmqttrelay.miniserver_data_processor import miniserver_data_processor
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


    """Handler for processing and sending data to Miniserver via HTTP."""
    def __init__(self):
        logger.info("MQTT Miniserver Handler created")

    async def send_to_minisever_via_websocket(
        self,
        data: List[tuple[str, Any]]
    ) -> Dict[str, Dict[str, Union[int, str]]]:
        """
        Sends data to the Loxone Miniserver via a WebSocket connection.
        Returns a dictionary with results for each topic.
        """
        # Determine target IP
        logger.debug(f"Using miniserver address: {self.target_ip} {'(mock)' if (self.mock_ms_ip and self.enable_mock_miniserver) else '(real)'}")

        # Base URL based on the port
        base_url = f"{"https" if self.ms_port == 443 else "http"}://{self.target_ip}"

        ws_client = loxwebsocket
        if "CONNECTED" not in ws_client.state:
            await ws_client.connect(user=self.ms_user, password=self.ms_pass, loxone_url=base_url, receive_updates=False)

        results = {}
        # Send commands over the WebSocket connection
        for topic, value in data:
            safe_topic = miniserver_data_processor.normalize_topic(topic)
            try:
                await ws_client.send_websocket_command(safe_topic, str(value))
                results[topic] = {'code': 200}
                logger.debug(f"Sent {topic}={value} to Miniserver successfully via WebSocket.")
            except Exception as e:
                error_msg = f"Error sending {topic} to Miniserver via WebSocket: {str(e)}"
                logger.error(error_msg)
                results[topic] = {'code': 500, 'error': error_msg}

        # Do not close the connection; keep it open for future use
        return results


    async def send_to_miniserver_via_http(
        self,
        data: List[tuple[str, Any]]
    ) -> Dict[str, Dict[str, Union[int, str]]]:
        """
        Send data to Miniserver with rate limiting.
        If mock_ms_ip is provided and enable_mock_miniserver is True, mock server will be used instead of ms_ip.
        Returns a dictionary with results for each topic.
        """
        # Use mock miniserver IP only if both provided and enabled
        logger.debug(f"Using miniserver address: {self.target_ip} {'(mock)' if (self.mock_ms_ip and self.enable_mock_miniserver) else '(real)'}")

        results = {}
        auth = aiohttp.BasicAuth(self.ms_user, self.ms_pass) if self.ms_user and self.ms_pass else None

        # Increase the timeout to 10 seconds
        timeout = aiohttp.ClientTimeout(total=10)

        async with aiohttp.ClientSession(auth=auth, timeout=timeout) as session:
            async def send_single_request(topic: str, value: Any) -> None:
                safe_topic = miniserver_data_processor.normalize_topic(topic)
                # Ensure value is converted to string
                safe_value = str(value)
                url = f"http://{self.target_ip}/dev/sps/io/{safe_topic}/{safe_value}"
                logger.debug(f"Sending to {url}")
                
                try:
                    # Use semaphore to limit concurrent connections
                    async with self.connection_semaphore:
                        async with session.get(url) as resp:
                            results[topic] = { 'code': resp.status }
                            if resp.status != 200:
                                logger.warning(f"Miniserver returned {resp.status} for topic {topic} (URL: {url})")
                            else:
                                logger.debug(f"Sent {topic}={value} to Miniserver successfully.")
                except asyncio.TimeoutError:
                    error_msg = f"Timeout while sending {topic} to Miniserver (URL: {url}): request timed out after 10 seconds"
                    logger.error(error_msg)
                    results[topic] = { 'code': 408, 'error': error_msg }
                except asyncio.CancelledError:
                    error_msg = f"Request for {topic} was cancelled (URL: {url})"
                    logger.error(error_msg)
                    results[topic] = { 'code': 499, 'error': error_msg }
                except OSError as e:
                    error_msg = f"Connection error sending {topic} to Miniserver (URL: {url}): {str(e)}"
                    logger.error(error_msg)
                    results[topic] = { 'code': 503, 'error': error_msg }
                except aiohttp.ClientError as e:
                    error_msg = f"Client error sending {topic} to Miniserver (URL: {url}): {str(e)}"
                    logger.error(error_msg)
                    results[topic] = { 'code': 500, 'error': error_msg }
                except Exception as e:
                    error_msg = f"Unexpected error sending {topic} to Miniserver (URL: {url}): {str(e)}"
                    logger.error(error_msg)
                    results[topic] = { 'code': 500, 'error': error_msg }

            # Create tasks for all requests
            tasks = [send_single_request(topic, value) for topic, value in data]
            
            # Wait for all requests to complete
            await asyncio.gather(*tasks)

        return results


    async def send_to_miniserver(
        self,
        data: List[tuple[str, Any]],
        mqtt_publish_callback: Optional[Callable[[str, str, bool], Awaitable[None]]] = None
    ) -> Dict[str, Dict[str, Union[int, str]]]:
        """
        Process data and send it to Miniserver.
        
        Args:
            data: The data to process and send
            mqtt_publish_callback: Callback for MQTT publishing (required for topic forwarding)
            
        Returns:
            Dictionary mapping topics to their HTTP response codes
        """
        
        # Send to Miniserver using WebSocket or HTTP based on config
        if global_config.miniserver.use_websocket:
            response_codes = await self.send_to_minisever_via_websocket(data)
        else:
            response_codes = await self.send_to_miniserver_via_http(data)

        # Publish forwarded topics with response codes if enabled
        if global_config.debug.publish_forwarded_topics and mqtt_publish_callback:
            for topic, value in data:
                code = response_codes.get(topic, {}).get('code', 0)
                # Ensure code is int for publish_forwarded_topic
                http_code = code if isinstance(code, int) else 0
                await miniserver_data_processor.publish_forwarded_topic(
                    topic, value, http_code,
                    mqtt_publish_callback
                )

        return response_codes

http_miniserver_handler = HttpMiniserverHandler()