import pytest
import pytest_asyncio
import asyncio
from unittest.mock import AsyncMock, MagicMock, patch
from loxmqttrelay.config import global_config
from loxmqttrelay.udp_handler import (
    parse_udp_message_mqtt5,
    handle_udp_message,
    UDPProtocol,
    start_udp_server,
)


@pytest.mark.parametrize("udp_message,expected", [
    # Test explicit publish command
    ("publish topic1 message1", ("publish", "topic1", "message1")),

    # Test retain command
    ("retain topic2 message2", ("retain", "topic2", "message2")),

    # Test default publish (no command)
    ("topic3 message3", ("publish", "topic3", "message3")),

    # Test case insensitive retain
    ("RETAIN topic4 message4", ("retain", "topic4", "message4")),
    ("Retain topic5 message5", ("retain", "topic5", "message5")),

    # Test messages with multiple spaces
    ("publish a/b c/d message with spaces", ("publish", "a/b c/d", "message with spaces")),
    ("a/b c/d message with spaces", ("publish", "a/b c/d", "message with spaces")),
    ("publish topic6 message with spaces", ("publish", "topic6", "message with spaces")),
    ("topic7 message with spaces", ("publish", "topic7", "message with spaces")),

    # Test invalid formats - should return None
    ("single", None),
    ("", None),

    # Test messages with special characters
    ("publish topic/with/slashes message/with/slashes",
     ("publish", "topic/with/slashes", "message/with/slashes")),
    ("publish test/topic/path message/with/slashes",
     ("publish", "test/topic/path", "message/with/slashes")),
    ("test/topic/path message/with/slashes",
     ("publish", "test/topic/path", "message/with/slashes")),

    # Test messages with leading/trailing spaces
    ("  publish  a/path with/spaces  message8  ", ("publish", "a/path with/spaces", "message8")),
    ("  a/path with/spaces  message9  ", ("publish", "a/path with/spaces", "message9")),
    ("  publish  topic8  message8  ", ("publish", "topic8", "message8")),
    ("  topic9  message9  ", ("publish", "topic9", "message9")),

    # Test case: Topic with spaces in the topic (bug fix case)
    ("zigbee2mqtt/Rollo Gallerie links/set 100", ("publish", "zigbee2mqtt/Rollo Gallerie links/set", "100")),

    # Test case: publish command with JSON payload without inner spaces
    ("publish test/complex topic {\"key\":\"value\"}", ("publish", "test/complex topic", "{\"key\":\"value\"}")),
    ("publish test/topic {\"key\":\"value\"}", ("publish", "test/topic", "{\"key\":\"value\"}")),

    # Test case: publish command with JSON payload that contains spaces (note: will split at the last space)
    ("publish test/complex/topic {\"key\": \"value\"}", ("publish", "test/complex/topic", "{\"key\": \"value\"}")),
    ("publish test/topic {\"key\": \"value with spaces\"}", ("publish", "test/topic", "{\"key\": \"value with spaces\"}")),
    ("publish test/complex topic {\"key\": \"value\"}", ("publish", "test/complex topic", "{\"key\": \"value\"}")),
    ("publish test/topic {\"key\": \"value\"}", ("publish", "test/topic", "{\"key\": \"value\"}")),

    # Test case: retain command with JSON payload without inner spaces
    ("retain test/complex/topic {\"number\":42}", ("retain", "test/complex/topic", "{\"number\":42}")),
    ("retain test/complex topic {\"number\":42}", ("retain", "test/complex topic", "{\"number\":42}")),
    ("retain test/topic {\"number\":42}", ("retain", "test/topic", "{\"number\":42}")),

    # Test case: Topic with spaces and JSON payload when no explicit command provided
    ("a/b/c/d/set {\"action\":\"toggle\"}", ("publish", "a/b/c/d/set", "{\"action\":\"toggle\"}")),
    ("a/b c d/set {\"action\":\"toggle\"}", ("publish", "a/b c d/set", "{\"action\":\"toggle\"}")),

    # Test case: publish command with topic containing spaces and JSON payload with spaces
    ("publish Home/Automation/Light Control {\"mode\": \"auto on\"}", ("publish", "Home/Automation/Light Control", "{\"mode\": \"auto on\"}")),

    # Test case: Simple topic with spaces followed by two-word message
    ("a/b c/d e f", ("publish", "a/b c/d", "e f")),
    ("publish Home/Automation/Light/Control {\"mode\": \"auto on\"}", ("publish", "Home/Automation/Light/Control", "{\"mode\": \"auto on\"}")),
])
def test_parse_udp_message_without_property_block(udp_message, expected):
    """Messages without a '[key=value]' block yield no user properties."""
    result = parse_udp_message_mqtt5(udp_message)
    assert result == (expected + (None,) if expected is not None else None)


@pytest.mark.parametrize("udp_message,expected", [
    # Single valid property
    ("publish [source=loxone] home/light on",
     ("publish", "home/light", "on", [("source", "loxone")])),

    # Multiple properties
    ("publish [source=loxone;room=kitchen] home/light on",
     ("publish", "home/light", "on", [("source", "loxone"), ("room", "kitchen")])),

    # Default publish (no command) with property block
    ("[unit=celsius] home/temp 22.5",
     ("publish", "home/temp", "22.5", [("unit", "celsius")])),

    # Retain with property block
    ("retain [origin=ms1] home/status online",
     ("retain", "home/status", "online", [("origin", "ms1")])),

    # Empty value is allowed
    ("publish [flag=] home/light on",
     ("publish", "home/light", "on", [("flag", "")])),

    # Value containing additional '=' (split only on first '=')
    ("publish [token=a=b=c] home/light on",
     ("publish", "home/light", "on", [("token", "a=b=c")])),

    # Value containing spaces (block is delimited by ']')
    ("publish [note=hello world] home/light on",
     ("publish", "home/light", "on", [("note", "hello world")])),

    # Duplicate keys allowed
    ("publish [tag=a;tag=b] home/light on",
     ("publish", "home/light", "on", [("tag", "a"), ("tag", "b")])),

    # Property block combined with JSON payload
    ("publish [source=loxone] home/thermostat {\"mode\": \"heat\"}",
     ("publish", "home/thermostat", "{\"mode\": \"heat\"}", [("source", "loxone")])),

    # Invalid blocks -> NOT treated as properties, '[...]' stays in topic
    ("publish [foo] home/light on", ("publish", "[foo] home/light", "on", None)),
    ("publish [] home/light on", ("publish", "[] home/light", "on", None)),
    ("publish [=x] home/light on", ("publish", "[=x] home/light", "on", None)),
    # Whitespace-only block: not properties; '[' and ']' become separate tokens
    ("publish [   ] home/light on", ("publish", "[", "] home/light on", None)),

    # Mixed valid/invalid segments -> only valid pairs are kept
    ("publish [foo;room=kitchen] home/light on",
     ("publish", "home/light", "on", [("room", "kitchen")])),
])
def test_parse_udp_message_mqtt5(udp_message, expected):
    result = parse_udp_message_mqtt5(udp_message)
    assert result == expected


@pytest.fixture
def mock_mqtt_client():
    mock_client = AsyncMock()
    # publish returns the drop reason, so None means "handed to the broker"
    mock_client.publish = AsyncMock(return_value=None)
    return mock_client


@pytest.mark.asyncio
async def test_handle_udp_message_publish(mock_mqtt_client):
    await handle_udp_message(
        mock_mqtt_client,
        "publish test/topic test message",
        ("127.0.0.1", 1234)
    )

    mock_mqtt_client.publish.assert_called_once_with(
        "test/topic",
        "test message",
        False,
        None
    )


@pytest.mark.asyncio
async def test_handle_udp_message_retain(mock_mqtt_client):
    await handle_udp_message(
        mock_mqtt_client,
        "retain test/topic test message",
        ("127.0.0.1", 1234)
    )

    mock_mqtt_client.publish.assert_called_once_with(
        "test/topic",
        "test message",
        True,
        None
    )


@pytest.mark.asyncio
async def test_handle_udp_message_default_publish(mock_mqtt_client):
    await handle_udp_message(
        mock_mqtt_client,
        "test/topic test message",
        ("127.0.0.1", 1234)
    )

    mock_mqtt_client.publish.assert_called_once_with(
        "test/topic",
        "test message",
        False,
        None
    )


@pytest.mark.asyncio
async def test_handle_udp_message_invalid(mock_mqtt_client):
    await handle_udp_message(
        mock_mqtt_client,
        "invalid",  # Single word message should be treated as invalid
        ("127.0.0.1", 1234)
    )

    mock_mqtt_client.publish.assert_not_called()


@pytest.mark.asyncio
async def test_handle_udp_message_empty(mock_mqtt_client):
    await handle_udp_message(
        mock_mqtt_client,
        "",
        ("127.0.0.1", 1234)
    )

    mock_mqtt_client.publish.assert_not_called()


@pytest.mark.asyncio
async def test_handle_udp_message_with_special_chars(mock_mqtt_client):
    await handle_udp_message(
        mock_mqtt_client,
        "publish test/topic/path message/with/slashes",
        ("127.0.0.1", 1234)
    )

    mock_mqtt_client.publish.assert_called_once_with(
        "test/topic/path",
        "message/with/slashes",
        False,
        None
    )


@pytest.mark.asyncio
async def test_handle_udp_message_with_properties(mock_mqtt_client):
    await handle_udp_message(
        mock_mqtt_client,
        "publish [source=loxone;room=kitchen] test/topic test message",
        ("127.0.0.1", 1234)
    )

    mock_mqtt_client.publish.assert_called_once_with(
        "test/topic",
        "test message",
        False,
        [("source", "loxone"), ("room", "kitchen")]
    )


@pytest.mark.asyncio
async def test_handle_udp_message_retain_with_properties(mock_mqtt_client):
    await handle_udp_message(
        mock_mqtt_client,
        "retain [origin=ms1] test/topic online",
        ("127.0.0.1", 1234)
    )

    mock_mqtt_client.publish.assert_called_once_with(
        "test/topic",
        "online",
        True,
        [("origin", "ms1")]
    )


@pytest.mark.asyncio
async def test_handle_udp_message_logs_dropped_publish(mock_mqtt_client):
    """A datagram that never reaches the broker must not pass unnoticed.

    Nothing retries it, so the log line is the only trace left of the command
    the Miniserver sent.
    """
    mock_mqtt_client.publish = AsyncMock(return_value="broker not connected")

    with patch('loxmqttrelay.udp_handler.logger') as mock_logger:
        await handle_udp_message(
            mock_mqtt_client,
            "publish test/topic test message",
            ("127.0.0.1", 1234)
        )

    mock_logger.warning.assert_called_once()
    logged = " ".join(str(arg) for arg in mock_logger.warning.call_args[0])
    assert "broker not connected" in logged
    assert "test/topic" in logged


@pytest.mark.asyncio
async def test_handle_udp_message_stays_quiet_when_published(mock_mqtt_client):
    with patch('loxmqttrelay.udp_handler.logger') as mock_logger:
        await handle_udp_message(
            mock_mqtt_client,
            "publish test/topic test message",
            ("127.0.0.1", 1234)
        )

    mock_logger.warning.assert_not_called()


@pytest.mark.asyncio
async def test_udp_protocol_publishes_via_injected_client(mock_mqtt_client):
    protocol = UDPProtocol(mock_mqtt_client)
    test_data = "publish test/topic test message".encode('utf-8')
    test_addr = ("127.0.0.1", 1234)

    protocol.datagram_received(test_data, test_addr)
    await asyncio.sleep(0.1)  # Give time for the async task to complete

    mock_mqtt_client.publish.assert_called_once_with(
        "test/topic",
        "test message",
        False,
        None
    )


@pytest.mark.asyncio
async def test_udp_protocol_drops_foreign_source(mock_mqtt_client):
    global_config._config.miniserver.miniserver_ip = "192.168.1.10"

    with patch('loxmqttrelay.udp_handler._container_gateway', return_value=None):
        protocol = UDPProtocol(mock_mqtt_client)
    protocol.datagram_received(b"publish test/topic test message", ("192.168.1.99", 1234))
    await asyncio.sleep(0.1)

    mock_mqtt_client.publish.assert_not_called()


@pytest.mark.asyncio
async def test_udp_protocol_accepts_configured_miniserver(mock_mqtt_client):
    global_config._config.miniserver.miniserver_ip = "192.168.1.10"

    with patch('loxmqttrelay.udp_handler._container_gateway', return_value=None):
        protocol = UDPProtocol(mock_mqtt_client)
    protocol.datagram_received(b"publish test/topic test message", ("192.168.1.10", 1234))
    await asyncio.sleep(0.1)

    mock_mqtt_client.publish.assert_called_once_with(
        "test/topic",
        "test message",
        False,
        None
    )


@pytest.mark.asyncio
async def test_udp_protocol_accepts_container_gateway_with_warning(mock_mqtt_client):
    global_config._config.miniserver.miniserver_ip = "192.168.1.10"

    with patch('loxmqttrelay.udp_handler._container_gateway', return_value="172.17.0.1"):
        protocol = UDPProtocol(mock_mqtt_client)

    with patch('loxmqttrelay.udp_handler.logger') as mock_logger:
        protocol.datagram_received(b"publish test/topic test message", ("172.17.0.1", 1234))
        protocol.datagram_received(b"publish test/topic test message", ("172.17.0.1", 1234))
        await asyncio.sleep(0.1)

        assert mock_logger.warning.call_count == 1

    assert mock_mqtt_client.publish.call_count == 2


@pytest.mark.asyncio
async def test_udp_protocol_resolves_miniserver_hostname(mock_mqtt_client):
    global_config._config.miniserver.miniserver_ip = "miniserver.local"

    with patch('loxmqttrelay.udp_handler.socket.getaddrinfo',
               return_value=[(None, None, None, '', ("192.168.1.10", 0))]), \
         patch('loxmqttrelay.udp_handler._container_gateway', return_value=None):
        protocol = UDPProtocol(mock_mqtt_client)

    protocol.datagram_received(b"publish test/topic test message", ("192.168.1.10", 1234))
    await asyncio.sleep(0.1)

    mock_mqtt_client.publish.assert_called_once()


@pytest.mark.asyncio
async def test_udp_protocol_accepts_additional_allowed_source(mock_mqtt_client):
    global_config._config.miniserver.miniserver_ip = "192.168.1.10"
    global_config._config.udp.udp_allowed_sources = ["192.168.1.50"]

    with patch('loxmqttrelay.udp_handler._container_gateway', return_value=None):
        protocol = UDPProtocol(mock_mqtt_client)
    protocol.datagram_received(b"publish test/topic test message", ("192.168.1.50", 1234))
    await asyncio.sleep(0.1)

    mock_mqtt_client.publish.assert_called_once()


@pytest.mark.asyncio
async def test_udp_protocol_accepts_any_source_when_filter_disabled(mock_mqtt_client):
    global_config._config.miniserver.miniserver_ip = "192.168.1.10"
    global_config._config.udp.udp_source_filter_enabled = False

    with patch('loxmqttrelay.udp_handler._container_gateway', return_value=None):
        protocol = UDPProtocol(mock_mqtt_client)
    protocol.datagram_received(b"publish test/topic test message", ("192.168.1.99", 1234))
    await asyncio.sleep(0.1)

    mock_mqtt_client.publish.assert_called_once()


@pytest.mark.asyncio
async def test_udp_protocol_starts_when_filter_setup_fails(mock_mqtt_client):
    with patch('loxmqttrelay.udp_handler._container_gateway', return_value=None), \
         patch('loxmqttrelay.udp_handler._allowed_source_addresses',
               side_effect=RuntimeError("broken config")):
        protocol = UDPProtocol(mock_mqtt_client)

    protocol.datagram_received(b"publish test/topic test message", ("192.168.1.99", 1234))
    await asyncio.sleep(0.1)

    mock_mqtt_client.publish.assert_called_once()


@pytest.mark.asyncio
async def test_udp_protocol_warns_about_public_miniserver_address(mock_mqtt_client):
    global_config._config.miniserver.miniserver_ip = "myhome.loxonecloud.com"

    with patch('loxmqttrelay.udp_handler.socket.getaddrinfo',
               return_value=[(None, None, None, '', ("84.1.2.3", 0))]), \
         patch('loxmqttrelay.udp_handler._container_gateway', return_value=None), \
         patch('loxmqttrelay.udp_handler.logger') as mock_logger:
        UDPProtocol(mock_mqtt_client)

    assert any("public" in str(call) for call in mock_logger.warning.call_args_list)


@pytest.mark.asyncio
async def test_udp_protocol_allows_everything_when_address_unresolvable(mock_mqtt_client):
    global_config._config.miniserver.miniserver_ip = "does-not-resolve"

    with patch('loxmqttrelay.udp_handler.socket.getaddrinfo', side_effect=OSError("no dns")), \
         patch('loxmqttrelay.udp_handler._container_gateway', return_value=None):
        protocol = UDPProtocol(mock_mqtt_client)

    protocol.datagram_received(b"publish test/topic test message", ("192.168.1.99", 1234))
    await asyncio.sleep(0.1)

    mock_mqtt_client.publish.assert_called_once()


@pytest.mark.asyncio
async def test_start_udp_server(mock_mqtt_client):
    mock_transport = MagicMock()
    mock_protocol = MagicMock()

    with patch('asyncio.get_running_loop') as mock_loop:
        mock_loop.return_value = AsyncMock()
        mock_loop.return_value.create_datagram_endpoint = AsyncMock(
            return_value=(mock_transport, mock_protocol)
        )

        transport, protocol = await start_udp_server(mock_mqtt_client)

        assert transport == mock_transport
        assert protocol == mock_protocol
        mock_loop.return_value.create_datagram_endpoint.assert_called_once()
