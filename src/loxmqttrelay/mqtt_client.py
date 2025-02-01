import asyncio
import logging
import time
from typing import List, Callable, Awaitable
from gmqtt import Client
from gmqtt import constants as MQTTconstants
from gmqtt.mqtt.constants import PubAckReasonCode
from .config import global_config

logger = logging.getLogger(__name__)

class MQTTClient:
    """
    Encapsulates MQTT connection handling, subscription and publishing.
    Using aiomqtt, connection is handled via async context managers.
    Configuration is accessed through Config singleton.
    """
    def __init__(self):
        unique_id = f"loxberry_{int(time.time())}"
        self.client = Client(client_id=unique_id, logger=logger)
        self.base_topic = global_config.general.base_topic
        self._callback: Callable[[str, str], Awaitable[None]]
        self._max_reconnect_delay = 15 
        self._reconnect_attempt = 0
        self._conn = asyncio.Event()
        self.client.on_connect = self._on_connect
        self.client.on_disconnect = self._on_disconnect
        self.client.on_message = self._on_message
        self.client.set_config({'reconnect_retries': MQTTconstants.UNLIMITED_RECONNECTS, 'reconnect_delay': self._max_reconnect_delay})
        if global_config.broker.user is not None:
            self.client.set_auth_credentials(global_config.broker.user, global_config.broker.password)
                    
    async def connect(self, topics: List[str], callback: Callable[[str, str], Awaitable[None]]) -> None:
        """
        Connect to the MQTT broker and set up subscriptions.
        
        Args:
            topics: List of topics to subscribe to
            callback: Callback function for handling received messages
        """
        self._callback = callback
        self._topics = topics
        self._conn.clear()
        
        while True:
            try:
                logger.info(f"Attempting MQTT connection to {global_config.broker.host}:{global_config.broker.port}")
                await self.client.connect(
                    host=global_config.broker.host, 
                    port=global_config.broker.port, 
                    version=MQTTconstants.MQTTv311
                    )
                
                return
                    
            except Exception as e:
                logger.error(f"Failed to connect to MQTT broker: {e}")
                logger.warning(f"Retrying connection in {self._max_reconnect_delay} seconds...")
                await asyncio.sleep(self._max_reconnect_delay)

    async def disconnect(self) -> None:
        """Disconnect from the MQTT broker."""
        if self.client:
            try:
                if self.client.is_connected:
                    self.client.publish(f"{self.base_topic}status", "Disconnecting")
            except Exception:
                logger.warning("Failed to publish disconnect status", exc_info=True)
            finally:
                try:
                    await self.client.disconnect()
                except Exception:
                    logger.warning("Error during MQTT client cleanup", exc_info=True)

    async def _on_message(self, client, topic, payload: bytes, qos, properties):
        try:
            payload_str = payload.decode('utf-8', errors='ignore')
            await self._callback(topic, payload_str)
        except Exception as e:
            logger.error(f"Error processing message: {e}")
            return PubAckReasonCode.UNSPECIFIED_ERROR
        return PubAckReasonCode.SUCCESS

    async def publish(self, topic: str, message: str | bytes, retain: bool = False) -> None:
        try:
            if not self._conn.is_set():
                logger.warning("MQTT publish attempted without connection")
                return

            self.client.publish(topic, message, qos=0, retain=retain)
            logger.debug(f"Published: {topic} = {message!r} (retain={retain})")

        except Exception as e:
            logger.error(f"Fatal error during publish: {e}")
            raise
    
    def _on_connect(self, session_present, result, properties, userdata):
        # Publish connection status
        self.client.publish(f"{self.base_topic}status", "Connected")
        logger.info(f"Connected to MQTT Server {global_config.broker.host}:{global_config.broker.port}")
        logger.info("MQTT connected")
        # Wait for connection to be established
        # Connection successful, subscribe to topics
        logger.info(f"Subscribing {self._topics}")
        for topic in self._topics:
            self.client.subscribe(topic)
        self._conn.set()
    
    def _on_disconnect(self,client, packet, exc=None):
        logger.info("MQTT disconnected")
        if exc:
            logger.error(f"Disconnect error: {exc}")
        self._conn.clear()

mqtt_client = MQTTClient()