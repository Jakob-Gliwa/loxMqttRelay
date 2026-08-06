import asyncio
import logging
import signal
import types
import sys
import os
import orjson
import uvloop

from loxmqttrelay.config import ConfigError, ConfigSection, global_config
from loxmqttrelay.logging_config import get_lazy_logger
from loxmqttrelay.miniserver_sync import sync_miniserver_whitelist
from loxmqttrelay.http_miniserver_handler import http_miniserver_handler
from loxwebsocket.lox_ws_api import loxwebsocket
import loxmqttrelay.utils as utils

# The imports are now handled by __init__.py
from loxmqttrelay import MiniserverDataProcessor, MqttClient, UdpServer, init_rust_logger

TOPIC = types.SimpleNamespace(
    CONFIG_SET = f"{global_config.general.base_topic}config/set",
    CONFIG_ADD = f"{global_config.general.base_topic}config/add",
    CONFIG_REMOVE = f"{global_config.general.base_topic}config/remove",
    CONFIG_UPDATE = f"{global_config.general.base_topic}config/update",
    CONFIG_RESTART = f"{global_config.general.base_topic}config/restart",
    CONFIG_GET = f"{global_config.general.base_topic}config/get",
    CONFIG_RESPONSE = f"{global_config.general.base_topic}config/response",
    MINISERVER_STARTUP_EVENT = f"{global_config.general.base_topic}miniserverevent/startup"
)

logger = get_lazy_logger(__name__)

# Initialize Rust logger (native call — log a breadcrumb so a hard crash here
# is preceded by a traceable log line). The level is handed over rather than
# left to RUST_LOG: unset, that would silence everything below ERROR, and the
# UDP and MQTT paths report their dropped messages at WARNING.
logger.info("Initializing Rust logger ...")
if not init_rust_logger(logging.getLevelName(logging.getLogger().getEffectiveLevel())):
    # A logger was already installed, so LOG_LEVEL did not reach the Rust half
    # at all - which is exactly the situation where its warnings about dropped
    # messages go missing without anyone noticing.
    logger.warning("Rust logger was already installed; LOG_LEVEL does not apply to it")

class MQTTRelay:
    def __init__(self):
        # The client owns the connection state that the Rust processor shares,
        # so it has to exist before the processor is built.
        self.mqtt_client = MqttClient(global_config)
        self.miniserver_data_processor = MiniserverDataProcessor(TOPIC, global_config, self, self.mqtt_client, http_miniserver_handler, orjson)
        # Shares the client's connection state, so a datagram is parsed and
        # published entirely in Rust. Binds nothing until start().
        self.udp_server = UdpServer(global_config, self.mqtt_client)
        self._loop: asyncio.AbstractEventLoop | None = None
        self._stop: asyncio.Event | None = None
        self._udp_running = False
        self._restart_requested = False
        self._shutdown_done = False

    async def main(self) -> bool:
        """Run until a signal or a config change asks for shutdown.

        Returns True when the caller should re-exec the process.
        """
        # The Rust ingress worker calls back from a tokio thread, so anything it
        # triggers must be scheduled onto this loop explicitly.
        self._loop = asyncio.get_running_loop()
        self._stop = asyncio.Event()
        self._install_signal_handlers()

        await self.connect_and_subscribe_mqtt()
        await self.handle_miniserver_sync()
        # A websocket reconnect means the Miniserver went away and came back,
        # which usually follows a configuration upload - so resync the whitelist.
        loxwebsocket.add_event_callback(self.handle_miniserver_sync, [loxwebsocket.EventType.RECONNECTED])
        # Awaited rather than spawned: a failed bind has to abort startup, not
        # vanish into a discarded task and leave a relay with no inbound path.
        await self.udp_server.start()
        self._udp_running = True

        logger.info("MQTT Relay started")
        await self._stop.wait()
        await self.shutdown()
        return self._restart_requested

    def _install_signal_handlers(self) -> None:
        """Route SIGINT/SIGTERM into the shutdown path.

        Without this, SIGTERM - what `docker stop` sends - kills the process
        outright: no DISCONNECT, and the broker keeps the last status message
        until the keep-alive expires.
        """
        assert self._loop is not None
        for sig in (signal.SIGINT, signal.SIGTERM):
            try:
                self._loop.add_signal_handler(sig, self.request_stop, sig.name)
            except NotImplementedError:
                # Windows has no signal handlers on the loop; KeyboardInterrupt
                # in main() stays the fallback there.
                logger.debug(f"No loop handler for {sig.name} on this platform")

    def request_stop(self, reason: str, restart: bool = False) -> None:
        """Ask the running relay to shut down. Safe to call more than once."""
        if self._stop is None:
            return
        if self._stop.is_set():
            logger.info(f"Shutdown already under way, ignoring: {reason}")
            return
        self._restart_requested = restart
        logger.info(f"Shutting down: {reason}")
        self._stop.set()

    async def shutdown(self):
        """Close the inputs first, then the connections.

        UDP and MQTT are the two ways work enters the relay, so closing them
        first means the Miniserver requests still in flight are not joined by
        new ones. Those requests are cancelled when the loop ends and report
        that themselves - nothing here waits for them, because at QoS 0 the
        relay promises no delivery anyway.
        """
        if self._shutdown_done:
            return
        self._shutdown_done = True

        if self._udp_running:
            self._udp_running = False
            try:
                await self.udp_server.stop()
            except Exception:
                logger.warning("Error while closing the UDP socket", exc_info=True)

        try:
            await self.mqtt_client.disconnect()
        except Exception:
            logger.warning("Error during MQTT shutdown", exc_info=True)

        if "CONNECTED" in loxwebsocket.state:
            try:
                await loxwebsocket.stop()
            except Exception:
                logger.warning("Error while closing the Miniserver websocket", exc_info=True)

        logger.info("Shutdown complete")

    async def handle_miniserver_sync(self):
        """Attempt to sync whitelist with miniserver if enabled"""        
        if not global_config.miniserver.sync_with_miniserver:
            return

        # Store initial whitelist from config
        initial_whitelist = global_config.topics.topic_whitelist.copy()

        try:
            inputs = await sync_miniserver_whitelist()
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
        if self._loop is None:
            logger.error("Cannot resync whitelist: event loop not running")
            return
        # Reached from the Rust ingress worker, i.e. off the event loop thread.
        asyncio.run_coroutine_threadsafe(self.handle_miniserver_sync(), self._loop)

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
            TOPIC.MINISERVER_STARTUP_EVENT
        ]
        
        try:
            # Connect with all required subscriptions. The processor is handed
            # over whole so the Rust side can dispatch without a Python callback.
            await self.mqtt_client.connect(all_topics, self.miniserver_data_processor)
        except Exception as e:
            logger.error(f"Failed to connect to MQTT broker: {e}")
            raise ConfigError(f"MQTT connection failed: {e}")

    def restart_relay(self):
        """Restart the relay after a configuration change.

        Reached from the Rust ingress worker, i.e. off the event loop thread.
        The exec itself is left to main() once the loop is closed - doing it
        here would replace the process image from inside handle_mqtt_message,
        with the MQTT session and the UDP socket still open.
        """
        if self._loop is None:
            os.execv(sys.executable, [sys.executable] + sys.argv)
        try:
            self._loop.call_soon_threadsafe(self.request_stop, "configuration changed", True)
        except RuntimeError:
            # Loop already gone, so there is nothing left to close cleanly
            os.execv(sys.executable, [sys.executable] + sys.argv)

def main():
    # Initialize logging first
    utils.setup_logging()

    # Report the active build / parser / deps right away, so any later failure
    # can be traced back to the exact runtime configuration.
    utils.log_runtime_environment()

    asyncio.set_event_loop_policy(uvloop.EventLoopPolicy())
    relay = MQTTRelay()
    restart = False
    try:
        restart = asyncio.run(relay.main())
    except KeyboardInterrupt:
        # Ctrl-C before the signal handlers were in place
        logger.info("Interrupted during startup")
    finally:
        logger.info("MQTT Relay exited")

    if restart:
        os.execv(sys.executable, [sys.executable] + sys.argv)

if __name__ == "__main__":
    main()
