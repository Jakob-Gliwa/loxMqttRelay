import asyncio
import logging
import types
from typing import Dict, Any, Optional, Literal
import sys
import os
import orjson
import subprocess
import uvloop
import typing
import platform

from loxmqttrelay.config import ConfigError, ConfigSection, global_config
from loxmqttrelay.mqtt_client import mqtt_client
from loxmqttrelay.udp_handler import start_udp_server
from loxmqttrelay.miniserver_sync import sync_miniserver_whitelist
from loxmqttrelay.http_miniserver_handler import http_miniserver_handler
import loxmqttrelay.utils as utils

# The imports are now handled by __init__.py
from loxmqttrelay import MiniserverDataProcessor, init_rust_logger

TOPIC = types.SimpleNamespace(
    CONFIG_SET = f"{global_config.general.base_topic}config/set",
    CONFIG_ADD = f"{global_config.general.base_topic}config/add",
    CONFIG_REMOVE = f"{global_config.general.base_topic}config/remove",
    CONFIG_UPDATE = f"{global_config.general.base_topic}config/update",
    CONFIG_RESTART = f"{global_config.general.base_topic}config/restart",
    CONFIG_GET = f"{global_config.general.base_topic}config/get",
    CONFIG_RESPONSE = f"{global_config.general.base_topic}config/response",
    MINISERVER_STARTUP_EVENT = f"{global_config.general.base_topic}miniserverevent/startup",
    START_UI = f"{global_config.general.base_topic}startui",
    STOP_UI = f"{global_config.general.base_topic}stopui",
    UI_STATUS = f"{global_config.general.base_topic}ui/status"
)

logger = logging.getLogger(__name__)

# Initialize Rust logger
init_rust_logger()

class MQTTRelay:
    def __init__(self):
        self.ui_process: Optional[subprocess.Popen] = None
        self.miniserver_data_processor = MiniserverDataProcessor(TOPIC, global_config, self, mqtt_client, http_miniserver_handler, orjson)

    async def main(self):
        await self.connect_and_subscribe_mqtt()
        await self.handle_miniserver_sync()
        asyncio.create_task(start_udp_server())
        await self.start_ui()

        logger.info("MQTT Relay started")
        await asyncio.Future()

    async def handle_miniserver_sync(self):
        """Attempt to sync whitelist with miniserver if enabled"""        
        if not global_config.miniserver.sync_with_miniserver:
            return

        # Store initial whitelist from config
        initial_whitelist = global_config.topics.topic_whitelist.copy()

        try:
            inputs = sync_miniserver_whitelist()
            global_config.update_config(ConfigSection.TOPICS, {'topic_whitelist': inputs})
            self.miniserver_data_processor.update_topic_whitelist(list(inputs))
            logger.info("Whitelist updated from miniserver configuration")
        except Exception as e:
            logger.error(f"Failed to sync with miniserver: {str(e)}")
            logger.info("Keeping whitelist from config")
            global_config.update_config(ConfigSection.TOPICS, {'topic_whitelist': initial_whitelist})
            self.miniserver_data_processor.update_topic_whitelist(list(initial_whitelist))
    
    # UPDATED: Synchronous wrapper with added logging to help testing
    def schedule_miniserver_sync(self):
        """Schedule the asynchronous handle_miniserver_sync in the event loop."""
        logger.info("Miniserver startup detected, resyncing whitelist")
        asyncio.create_task(self.handle_miniserver_sync())

    async def connect_and_subscribe_mqtt(self):
        """Ensure MQTT client is connected with all required subscriptions."""
        # Subscribe to configuration topics and miniserver startup event
        all_topics = global_config.topics.subscriptions + [
            TOPIC.CONFIG_SET,
            TOPIC.CONFIG_ADD,
            TOPIC.CONFIG_REMOVE,
            TOPIC.CONFIG_UPDATE,
            TOPIC.CONFIG_RESTART,
            TOPIC.CONFIG_GET,
            TOPIC.MINISERVER_STARTUP_EVENT,
            TOPIC.START_UI,
            TOPIC.STOP_UI
        ]
        
        try:
            # Connect with all required subscriptions
            await mqtt_client.connect(all_topics, self.miniserver_data_processor.handle_mqtt_message)
        except Exception as e:
            logger.error(f"Failed to connect to MQTT broker: {e}")
            raise ConfigError(f"MQTT connection failed: {e}")

    async def start_ui(self):
        """Start the Streamlit UI if it's not already running."""
        if utils.get_args().headless:
            return
            
        if self.ui_process is None or self.ui_process.poll() is not None:
            try:
                # Start the UI using streamlit with absolute path
                ui_path = os.path.join(os.path.dirname(__file__), "ui.py")
                self.ui_process = subprocess.Popen(
                    ["streamlit", "run", ui_path],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE
                )
                logger.info("UI started successfully")
                await mqtt_client.publish(TOPIC.UI_STATUS, "UI started successfully")
            except Exception as e:
                error_msg = f"Failed to start UI: {e}"
                logger.error(error_msg)
                await mqtt_client.publish(TOPIC.UI_STATUS, error_msg)
        else:
            logger.info("UI is already running")
            await mqtt_client.publish(TOPIC.UI_STATUS, "UI is already running")

    async def stop_ui(self):
        """Stop the Streamlit UI if it's running."""
        if self.ui_process is not None:
            try:
                self.ui_process.terminate()
                self.ui_process.wait(timeout=5)  # Wait up to 5 seconds for process to terminate
                self.ui_process = None
                logger.info("UI stopped successfully")
                await mqtt_client.publish(TOPIC.UI_STATUS, "UI stopped successfully")
            except subprocess.TimeoutExpired:
                if self.ui_process is not None:
                    self.ui_process.kill()  # Force kill if termination takes too long
                self.ui_process = None
                logger.warning("UI process killed after timeout")
                await mqtt_client.publish(TOPIC.UI_STATUS, "UI process killed after timeout")
            except Exception as e:
                error_msg = f"Error stopping UI: {e}"
                logger.error(error_msg)
                await mqtt_client.publish(TOPIC.UI_STATUS, error_msg)
        else:
            logger.info("UI is not running")
            await mqtt_client.publish(TOPIC.UI_STATUS, "UI is not running")

    def restart_relay_incl_ui(self):
        if self.ui_process:
            self.ui_process.terminate()
        os.execv(sys.executable, [sys.executable] + sys.argv)

def main():
    asyncio.set_event_loop_policy(uvloop.EventLoopPolicy())
    relay = MQTTRelay()
    try:
        asyncio.run(relay.main())
    except KeyboardInterrupt:
        pass
    finally:
        logger.info("MQTT Relay exited")

if __name__ == "__main__":
    main()
