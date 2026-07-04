import asyncio
from typing import Callable, List, Optional, Tuple
from gmqtt import constants as MQTTconstants
from loxmqttrelay.config import global_config
from loxmqttrelay.logging_config import get_lazy_logger
from loxmqttrelay.mqtt_client import mqtt_client, resolve_mqtt_version

logger = get_lazy_logger(__name__)

UserProperties = List[Tuple[str, str]]


def _parse_command(udpmsg: str) -> Optional[Tuple[str, str]]:
    """
    Determine the command (publish/retain) and return (command, rest).

    If the first word (case-insensitive) is "publish"/"retain", it is used as
    the command and stripped off. Otherwise the command defaults to "publish"
    and the whole (stripped) message is treated as the rest.
    Returns None if there is nothing usable left.
    """
    msg = udpmsg.strip()
    if not msg:
        logger.warning("Empty UDP message")
        return None

    parts = msg.split(None, 1)  # split once after the first whitespace
    if not parts:
        return None

    first_token = parts[0].lower()
    if first_token in ("publish", "retain"):
        command = first_token
        if len(parts) < 2:
            logger.error(f"Missing topic/payload after command: {msg}")
            return None
        rest = parts[1].strip()
    else:
        command = "publish"
        rest = msg

    if not rest:
        logger.error(f"No topic/message after command: {msg}")
        return None

    return command, rest


def _parse_topic_payload(rest: str) -> Optional[Tuple[str, str]]:
    """
    Split the remaining message into (topic, payload).

      * JSON Payload: If a '{' is found, everything from the first '{' to the end
        is payload, everything before is topic.
      * Otherwise split by whitespace:
          - If exactly 2 tokens, first = topic, second = payload.
          - If >2 tokens, apply a greedy topic-splitting rule:
              - Start from left, keep tokens in topic as long as:
                  (token has Slash) OR (token between 2 Tokens with Slash).
                Stop when we find a token that doesn't fit.
                Everything after that (plus possibly the last token) is payload.
    Returns None if parsing fails.
    """
    # --- 1) JSON-Special: If { in string, from first { -> payload, before -> topic
    brace_index = rest.find("{")
    if brace_index != -1:
        topic_part = rest[:brace_index].rstrip()
        payload_part = rest[brace_index:].strip()

        if not topic_part or not payload_part:
            logger.error(f"Invalid format - topic or payload empty: {rest}")
            return None

        return topic_part, payload_part

    # --- 2) Otherwise split normally
    tokens = rest.split()
    if len(tokens) < 2:
        logger.error(f"Invalid format - need at least topic + payload: {rest}")
        return None

    if len(tokens) == 2:
        return tokens[0], tokens[1]

    # --- 3) More than 2 Tokens -> greedy topic-splitting rule
    def has_slash(s: str) -> bool:
        return "/" in s

    topic_list = [tokens[0]]
    n = len(tokens)
    i = 1
    # Run until the second-to-last token, because the last one must be a payload
    while i < (n - 1):
        t_current = tokens[i]
        left_has_slash = has_slash(tokens[i - 1])
        right_has_slash = has_slash(tokens[i + 1])
        curr_has_slash = has_slash(t_current)

        if curr_has_slash or (left_has_slash and right_has_slash):
            topic_list.append(t_current)
            i += 1
        else:
            break

    payload_tokens = tokens[i:]
    topic_str = " ".join(topic_list).strip()
    payload_str = " ".join(payload_tokens).strip()

    if not topic_str or not payload_str:
        logger.error(f"Invalid format - empty topic or payload: {rest}")
        return None

    return topic_str, payload_str


def _parse_user_properties(block_content: str) -> Optional[UserProperties]:
    """
    Parse the content of a '[...]' block into a list of (key, value) tuples.

    Pairs are separated by ';', key/value by the first '='. A pair is only valid
    if it contains a '=' and a non-empty key (empty values are allowed). Invalid
    segments are skipped with a warning. Returns None if there is not a single
    valid key=value pair (so the block is NOT treated as user properties).
    """
    properties: UserProperties = []
    for segment in block_content.split(";"):
        if "=" not in segment:
            if segment.strip():
                logger.warning(f"Ignoring malformed user property segment (no '='): {segment!r}")
            continue
        key, _, value = segment.partition("=")
        key = key.strip()
        if not key:
            logger.warning(f"Ignoring user property with empty key: {segment!r}")
            continue
        properties.append((key, value))

    if not properties:
        return None
    return properties


def _extract_property_block(rest: str) -> Tuple[Optional[UserProperties], str]:
    """
    If 'rest' starts with a '[...]' block that contains at least one valid
    key=value pair, extract it as user properties and return (properties,
    remaining_rest). Otherwise return (None, rest) unchanged - the '[' is then
    treated as a normal part of the topic.
    """
    if not rest.startswith("["):
        return None, rest

    close_index = rest.find("]")
    if close_index == -1:
        return None, rest

    block_content = rest[1:close_index]
    properties = _parse_user_properties(block_content)
    if properties is None:
        # Not a real property block - leave 'rest' untouched
        return None, rest

    remaining = rest[close_index + 1:].strip()
    if not remaining:
        logger.error(f"Property block without topic/payload: {rest}")
        return None, rest

    return properties, remaining


def parse_udp_message_mqtt3(udpmsg: str) -> Optional[Tuple[str, str, str]]:
    """
    MQTT 3.1.x fast-path parser: (command, topic, message). No user properties.
    """
    parsed = _parse_command(udpmsg)
    if parsed is None:
        return None
    command, rest = parsed

    topic_payload = _parse_topic_payload(rest)
    if topic_payload is None:
        return None
    topic, message = topic_payload
    return command, topic, message


def parse_udp_message_mqtt5(
    udpmsg: str,
) -> Optional[Tuple[str, str, str, Optional[UserProperties]]]:
    """
    MQTT 5 parser: (command, topic, message, user_properties). Supports an
    optional, validated '[key=value;...]' block right after the command.
    """
    parsed = _parse_command(udpmsg)
    if parsed is None:
        return None
    command, rest = parsed

    user_properties, rest = _extract_property_block(rest)

    topic_payload = _parse_topic_payload(rest)
    if topic_payload is None:
        return None
    topic, message = topic_payload
    return command, topic, message, user_properties


async def _handle_mqtt3(udpmsg: str, addr) -> None:
    """Handle an incoming UDP message on the MQTT 3.1.x fast path."""
    logger.info(f"UDP IN: {addr}: {udpmsg}")
    result = parse_udp_message_mqtt3(udpmsg)
    if result is None:
        return

    command, topic, message = result
    retain = command == "retain"
    logger.debug("Publishing%s: '%s'='%s'", ' (retain)' if retain else '', topic, message)
    await mqtt_client.publish(topic, message, retain)


async def _handle_mqtt5(udpmsg: str, addr) -> None:
    """Handle an incoming UDP message including optional MQTT5 user properties."""
    logger.info(f"UDP IN: {addr}: {udpmsg}")
    result = parse_udp_message_mqtt5(udpmsg)
    if result is None:
        return

    command, topic, message, user_properties = result
    retain = command == "retain"
    logger.debug(
        "Publishing%s: '%s'='%s' properties=%s",
        ' (retain)' if retain else '', topic, message, user_properties,
    )
    await mqtt_client.publish(topic, message, retain, user_properties)


class UDPProtocol(asyncio.DatagramProtocol):
    def __init__(self, handler: Callable):
        self._handler = handler

    def datagram_received(self, data, addr):
        msg = data.decode('utf-8', errors='ignore')
        asyncio.create_task(self._handler(msg, addr))


def _select_handler() -> Callable:
    """Pick the UDP handler once based on the configured MQTT version."""
    if resolve_mqtt_version(global_config.broker.mqtt_version) == MQTTconstants.MQTTv50:
        logger.info("UDP handler running in MQTT5 mode (user properties enabled)")
        return _handle_mqtt5
    logger.info("UDP handler running in MQTT 3.1.x mode")
    return _handle_mqtt3


async def start_udp_server():
    udpport = global_config.udp.udp_in_port
    handler = _select_handler()
    loop = asyncio.get_running_loop()
    transport, protocol = await loop.create_datagram_endpoint(
        lambda: UDPProtocol(handler),
        local_addr=("0.0.0.0", udpport)
    )
    logger.info(f"UDP-IN listening on port {udpport}")
    return transport, protocol
