import asyncio
from typing import Tuple, Optional
from loxmqttrelay.config import global_config
from loxmqttrelay.logging_config import get_lazy_logger
from loxmqttrelay.mqtt_client import mqtt_client

logger = get_lazy_logger(__name__)

def parse_udp_message(udpmsg: str) -> Optional[Tuple[str, str, str]]:
    """
    Parse an incoming UDP message into (command, topic, message) according to:
      - If the first word (case-insensitive) is "publish"/"retain", use it as command.
        Otherwise default to "publish".
      - Then parse the rest (topic + payload) as follows:
         * JSON Payload:If a '{' is found, everything from the first '{' to the end is payload,
           everything before is topic.
         * Otherwise split by whitespace:
             - If exactly 2 tokens, first = topic, second = payload.
             - If >2 tokens, apply a greedy topic-splitting rule:
                 - Start from left, keep tokens in topic as long as:
                     (token has Slash) OR
                     (token between 2 Tokens with Slash).
                   Stop when we find a token, that doesn't fit.
                   Everything after that (plus possibly the last token) is payload.
    Returns None, if parsing fails.
    """

    msg = udpmsg.strip()
    if not msg:
        logger.warning("Empty UDP message")
        return None

    # --- 1) Determine command
    parts = msg.split(None, 1)  # split once after the first whitespace
    if not parts:
        return None

    first_token = parts[0].lower()
    if first_token in ("publish", "retain"):
        command = first_token
        if len(parts) < 2:
            # Nothing left -> invalid
            logger.error(f"Missing topic/payload after command: {msg}")
            return None
        rest = parts[1].strip()
    else:
        command = "publish"
        # The whole msg is "rest"
        rest = msg

    if not rest:
        logger.error(f"No topic/message after command: {msg}")
        return None

    # --- 2) JSON-Special: If { in string, from first { -> payload, before -> topic
    brace_index = rest.find("{")
    if brace_index != -1:
        topic_part = rest[:brace_index].rstrip()
        payload_part = rest[brace_index:].strip()

        # minimal check
        if not topic_part or not payload_part:
            logger.error(f"Invalid format - topic or payload empty: {msg}")
            return None

        return (command, topic_part, payload_part)

    # --- 3) Otherwise split normally
    tokens = rest.split()
    if len(tokens) < 2:
        logger.error(f"Invalid format - need at least topic + payload: {msg}")
        return None

    if len(tokens) == 2:
        # Trivial: topic, message
        topic_part, payload_part = tokens
        return (command, topic_part, payload_part)

    # --- 4) More than 2 Tokens -> greedy topic-splitting rule
    def has_slash(s: str) -> bool:
        return "/" in s

    topic_list = []
    n = len(tokens)

    # Take first token in topic
    topic_list.append(tokens[0])
    i = 1
    # We run until the second-to-last token, because the last one must be a payload
    while i < (n - 1):
        t_current = tokens[i]
        # Condition: we keep t_current in topic, if
        #   (t_current has Slash) or
        #   (it is "sandwiched" between two Slash-containing tokens)
        # Otherwise we break and the rest -> payload
        left_has_slash = has_slash(tokens[i - 1])
        right_has_slash = has_slash(tokens[i + 1])
        curr_has_slash = has_slash(t_current)

        if curr_has_slash or (left_has_slash and right_has_slash):
            topic_list.append(t_current)
            i += 1
        else:
            # Break, rest -> payload
            break

    # i now points to the start of the payload (possibly n-1, if everything was "slash-framed")
    payload_tokens = tokens[i:]
    topic_str = " ".join(topic_list).strip()
    payload_str = " ".join(payload_tokens).strip()

    if not topic_str or not payload_str:
        logger.error(f"Invalid format - empty topic or payload: {msg}")
        return None

    return (command, topic_str, payload_str)


async def handle_udp_message(udpmsg: str, addr) -> None:
    """
    Handle an incoming UDP message:
      - parse
      - publish to MQTT with or without retain flag
    """
    logger.info(f"UDP IN: {addr}: {udpmsg}")
    result = parse_udp_message(udpmsg)
    if result is None:
        return

    command, topic, message = result
    if command == 'publish':
        logger.debug(f"Publishing: '{topic}'='{message}'")
        await mqtt_client.publish(topic, message, False)
    elif command == 'retain':
        logger.debug(f"Publishing (retain): '{topic}'='{message}'")
        await mqtt_client.publish(topic, message, True)
    else:
        logger.error(f"Unknown command in UDP handler: {command}")


class UDPProtocol(asyncio.DatagramProtocol):
    def datagram_received(self, data, addr):
        msg = data.decode('utf-8', errors='ignore')
        asyncio.create_task(handle_udp_message(msg, addr))


async def start_udp_server():
    udpport = global_config.udp.udp_in_port
    loop = asyncio.get_running_loop()
    transport, protocol = await loop.create_datagram_endpoint(
        lambda: UDPProtocol(),
        local_addr=("0.0.0.0", udpport)
    )
    logger.info(f"UDP-IN listening on port {udpport}")
    return transport, protocol