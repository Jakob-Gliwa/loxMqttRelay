import asyncio
import logging
from typing import Tuple, Callable, Coroutine, Any, Optional
from loxmqttrelay.config import global_config
logger = logging.getLogger(__name__)

def parse_udp_message(udpmsg: str) -> Optional[Tuple[str, str, str]]:
    """
    Parse an incoming UDP message into command, topic, message.
    Supports formats:
    1. "publish topic message" - Publish a message
    2. "retain topic message" - Publish a retained message
    3. "topic message" - Defaults to publish

    :param udpmsg: Raw UDP message as a string.
    :return: (command, topic, message) or None if parsing fails
    """
    parts = udpmsg.strip().split(maxsplit=2)
    
    if len(parts) >= 3:
        first, second, rest = parts
        # Check if first part is a command
        if first.lower().startswith('retain') or first.lower() == 'publish':
            command = 'retain' if first.lower().startswith('retain') else 'publish'
            return (command, second, rest)
        # If first part isn't a command, treat as topic
        return ('publish', first, second + " " + rest)
    elif len(parts) == 2:
        # No command provided, default to publish
        return ('publish', parts[0], parts[1])
    else:
        # Invalid format
        logger.error(f"Invalid UDP message format: {udpmsg}")
        return None

async def handle_udp_message(udpmsg: str, addr,
                           mqtt_publish: Callable[[str,str,bool], Coroutine[Any, Any, None]]) -> None:
    """
    Handle an incoming UDP message:
      - parse
      - publish to MQTT with or without retain flag

    :param udpmsg: Raw UDP message
    :param addr: Sender address
    :param mqtt_publish: Function to publish to MQTT
    """
    logger.info(f"UDP IN: {addr}: {udpmsg}")
    result = parse_udp_message(udpmsg)
    if result is None:
        return

    command, topic, message = result
    if command == 'publish':
        logger.debug(f"Publishing: '{topic}'='{message}'")
        await mqtt_publish(topic, message, False)
    elif command == 'retain':
        logger.debug(f"Publish (retain): '{topic}'='{message}'")
        await mqtt_publish(topic, message, True)
    else:
        logger.error("Unknown command in UDP handler")


class UDPProtocol(asyncio.DatagramProtocol):
    def __init__(self, relay):
        self.relay = relay

    def datagram_received(self, data, addr):
        msg = data.decode('utf-8', errors='ignore')
        asyncio.create_task(
            handle_udp_message(msg, addr, self.relay.mqtt_publish)
        )
        self.relay.udp_data_received = 1

async def start_udp_server(self):
    udpport = global_config.udp.udp_in_port
    loop = asyncio.get_running_loop()
    transport, protocol = await loop.create_datagram_endpoint(
        lambda: UDPProtocol(self),
        local_addr=("0.0.0.0", udpport)
    )
    logger.info(f"UDP-IN listening on port {udpport}")
    return transport, protocol