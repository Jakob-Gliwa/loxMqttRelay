import asyncio
import ipaddress
import socket
import struct
from typing import Any, List, Optional, Set, Tuple
from loxmqttrelay.config import global_config
from loxmqttrelay.logging_config import get_lazy_logger

logger = get_lazy_logger(__name__)

UserProperties = List[Tuple[str, str]]

# Upper bound for the "warn once per sender" bookkeeping, so a flood of spoofed
# source addresses cannot grow the set (or the log) without limit.
_MAX_TRACKED_REJECTED_SOURCES = 64


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


async def handle_udp_message(mqtt_client, udpmsg: str, addr) -> None:
    """Parse one UDP datagram and forward it to MQTT."""
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
    drop_reason = await mqtt_client.publish(topic, message, retain, user_properties)
    if drop_reason:
        # The datagram is gone: QoS 0, no local queue, nothing to retry. Say so
        # on the way in, where the sender and the payload are still known.
        logger.warning(
            "UDP message from %s was not forwarded to MQTT (%s): '%s'='%s'",
            addr, drop_reason, topic, message,
        )


def _as_bool(value: Any, field_name: str, default: bool) -> bool:
    """Read a config flag that a hand-edited TOML may hold as a string."""
    if isinstance(value, bool):
        return value
    if isinstance(value, str):
        lowered = value.strip().lower()
        if lowered in ("true", "yes", "on", "1"):
            return True
        if lowered in ("false", "no", "off", "0"):
            return False
    logger.warning(f"Invalid value for {field_name}: {value!r} - using {default}")
    return default


def _as_host_list(value: Any) -> List[str]:
    """Read udp_allowed_sources, tolerating a single string instead of a list."""
    if isinstance(value, str):
        return [value] if value.strip() else []
    if isinstance(value, (list, tuple, set, frozenset)):
        return [str(item) for item in value]
    logger.warning(f"Ignoring udp_allowed_sources - expected a list of addresses, got {value!r}")
    return []


def _host_part(address: Any) -> str:
    """Strip an optional port from a configured address."""
    address = str(address).strip()
    if address.startswith("["):  # [::1]:80
        closing_bracket = address.find("]")
        if closing_bracket != -1:
            return address[1:closing_bracket]
    try:
        ipaddress.ip_address(address)
        return address
    except ValueError:
        pass
    # A bare IPv6 address contains several colons and carries no port here
    if address.count(":") == 1:
        return address.split(":", 1)[0]
    return address


def _resolve(host: str) -> Set[str]:
    """Resolve a host (IP literal or DNS name) to its numeric addresses."""
    if not host:
        return set()
    try:
        return {info[4][0] for info in socket.getaddrinfo(host, None, proto=socket.IPPROTO_UDP)}
    except (OSError, ValueError) as e:
        # ValueError covers the UnicodeError getaddrinfo raises on malformed names
        logger.error(f"Cannot resolve configured UDP sender '{host}', ignoring it: {e}")
        return set()


def _allowed_source_addresses() -> Set[str]:
    """Numeric addresses that may send UDP datagrams to the relay."""
    hosts = [global_config.miniserver.miniserver_ip]
    if global_config.debug.enable_mock and global_config.debug.mock_ip:
        hosts.append(global_config.debug.mock_ip)
    hosts.extend(_as_host_list(global_config.udp.udp_allowed_sources))
    addresses: Set[str] = set()
    for host in hosts:
        addresses |= _resolve(_host_part(host))
    return addresses


def _warn_about_public_addresses(addresses: Set[str]) -> None:
    """
    Warn when an allowed sender is a public address.

    A DynDNS entry (Loxone Cloud DNS, No-IP, ...) resolves to the WAN address of
    the internet connection, while the Miniserver sends its datagrams from its
    local address - so such a configuration drops everything.
    """
    public = set()
    for address in addresses:
        try:
            if not ipaddress.ip_address(address).is_private:
                public.add(address)
        except ValueError:
            continue
    if public:
        logger.warning(
            f"Allowed UDP sender(s) {', '.join(sorted(public))} are public addresses. The "
            "Miniserver sends UDP from its local network address, so its datagrams would be "
            "dropped - configure its local address instead of a DynDNS entry."
        )


def _container_gateway() -> Optional[str]:
    """
    IPv4 default gateway from /proc/net/route, or None if it cannot be read
    (non-Linux hosts).

    Inside a Docker bridge network this is the address datagrams appear to come
    from whenever the userland proxy forwards them instead of iptables DNAT.
    """
    try:
        with open("/proc/net/route", "r") as route_table:
            for line in route_table.readlines()[1:]:
                fields = line.split()
                if len(fields) > 2 and fields[1] == "00000000":
                    gateway = int(fields[2], 16)
                    if gateway:
                        return socket.inet_ntoa(struct.pack("<L", gateway))
    except (OSError, ValueError):
        pass
    return None


class UDPProtocol(asyncio.DatagramProtocol):
    def __init__(self, mqtt_client):
        self._mqtt_client = mqtt_client
        self._gateway = _container_gateway()
        self._gateway_warning_logged = False
        self._rejected_sources: Set[str] = set()
        self._allowed_sources: Set[str] = set()
        try:
            self._allowed_sources = self._configure_source_filter()
        except Exception as e:
            # An unusable config value must never keep the relay from starting
            logger.error(f"Could not set up UDP source filtering ({e}) - accepting every sender")

    @staticmethod
    def _configure_source_filter() -> Set[str]:
        """Senders to accept; an empty set means every sender is accepted."""
        if not _as_bool(global_config.udp.udp_source_filter_enabled, "udp_source_filter_enabled", True):
            logger.warning(
                "UDP source filtering is switched off (udp_source_filter_enabled = false) - "
                "every host that can reach the UDP port can publish to MQTT"
            )
            return set()

        allowed = _allowed_source_addresses()
        if allowed:
            logger.info(f"UDP-IN accepts datagrams from {', '.join(sorted(allowed))}")
            _warn_about_public_addresses(allowed)
        else:
            logger.error(
                "No usable sender address configured "
                f"(miniserver_ip='{global_config.miniserver.miniserver_ip}') - UDP source "
                "filtering stays off and every host on the network can publish via UDP"
            )
        return allowed

    def _source_allowed(self, source: str) -> bool:
        if not self._allowed_sources:
            return True

        if source in self._allowed_sources:
            return True

        if source == self._gateway:
            if not self._gateway_warning_logged:
                self._gateway_warning_logged = True
                logger.warning(
                    f"UDP datagrams arrive from the container gateway {source} instead of the "
                    "Miniserver address, so Docker hides the real sender and source filtering "
                    "cannot take effect. Accepting them anyway - restrict the UDP port on the "
                    "host firewall or run the container with network_mode: host."
                )
            return True

        if source in self._rejected_sources:
            logger.debug(f"Dropped UDP datagram from {source}")
        elif len(self._rejected_sources) < _MAX_TRACKED_REJECTED_SOURCES:
            self._rejected_sources.add(source)
            logger.warning(
                f"Dropped UDP datagram from {source} - only "
                f"{', '.join(sorted(self._allowed_sources))} may publish via UDP"
            )
        else:
            logger.debug(f"Dropped UDP datagram from {source}")
        return False

    def datagram_received(self, data, addr):
        if not self._source_allowed(addr[0]):
            return
        msg = data.decode('utf-8', errors='ignore')
        asyncio.create_task(handle_udp_message(self._mqtt_client, msg, addr))


async def start_udp_server(mqtt_client):
    udpport = global_config.udp.udp_in_port
    loop = asyncio.get_running_loop()
    transport, protocol = await loop.create_datagram_endpoint(
        lambda: UDPProtocol(mqtt_client),
        local_addr=("0.0.0.0", udpport)
    )
    logger.info(f"UDP-IN listening on port {udpport}")
    return transport, protocol
