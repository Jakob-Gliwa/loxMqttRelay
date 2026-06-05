import pytest
import pytest_asyncio
import asyncio
from unittest.mock import AsyncMock, MagicMock, patch
from loxmqttrelay.udp_handler import (
    parse_udp_message_mqtt3,
    parse_udp_message_mqtt5,
    _handle_mqtt3,
    _handle_mqtt5,
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
def test_parse_udp_message_mqtt3(udp_message, expected):
    result = parse_udp_message_mqtt3(udp_message)
    assert result == expected


def test_mqtt3_parser_ignores_property_block():
    # In MQTT3 mode the '[...]' block is NOT special - it stays part of the topic
    result = parse_udp_message_mqtt3("publish [source=loxone] home/light on")
    assert result == ("publish", "[source=loxone] home/light", "on")


@pytest.mark.parametrize("udp_message,expected", [
    # Without a property block -> properties are None, behaves like the v3 parser
    ("publish topic1 message1", ("publish", "topic1", "message1", None)),
    ("topic3 message3", ("publish", "topic3", "message3", None)),
    ("retain topic2 message2", ("retain", "topic2", "message2", None)),

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
def mock_mqtt_client(monkeypatch):
    mock_client = AsyncMock()
    mock_client.publish = AsyncMock()
    monkeypatch.setattr('loxmqttrelay.udp_handler.mqtt_client', mock_client)
    return mock_client


@pytest.mark.asyncio
async def test_handle_mqtt3_publish(mock_mqtt_client):
    await _handle_mqtt3(
        "publish test/topic test message",
        ("127.0.0.1", 1234)
    )

    mock_mqtt_client.publish.assert_called_once_with(
        "test/topic",
        "test message",
        False
    )


@pytest.mark.asyncio
async def test_handle_mqtt3_retain(mock_mqtt_client):
    await _handle_mqtt3(
        "retain test/topic test message",
        ("127.0.0.1", 1234)
    )

    mock_mqtt_client.publish.assert_called_once_with(
        "test/topic",
        "test message",
        True
    )


@pytest.mark.asyncio
async def test_handle_mqtt3_default_publish(mock_mqtt_client):
    await _handle_mqtt3(
        "test/topic test message",
        ("127.0.0.1", 1234)
    )

    mock_mqtt_client.publish.assert_called_once_with(
        "test/topic",
        "test message",
        False
    )


@pytest.mark.asyncio
async def test_handle_mqtt3_invalid(mock_mqtt_client):
    await _handle_mqtt3(
        "invalid",  # Single word message should be treated as invalid
        ("127.0.0.1", 1234)
    )

    mock_mqtt_client.publish.assert_not_called()


@pytest.mark.asyncio
async def test_handle_mqtt3_empty(mock_mqtt_client):
    await _handle_mqtt3(
        "",
        ("127.0.0.1", 1234)
    )

    mock_mqtt_client.publish.assert_not_called()


@pytest.mark.asyncio
async def test_handle_mqtt3_with_special_chars(mock_mqtt_client):
    await _handle_mqtt3(
        "publish test/topic/path message/with/slashes",
        ("127.0.0.1", 1234)
    )

    mock_mqtt_client.publish.assert_called_once_with(
        "test/topic/path",
        "message/with/slashes",
        False
    )


@pytest.mark.asyncio
async def test_handle_mqtt5_without_properties(mock_mqtt_client):
    await _handle_mqtt5(
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
async def test_handle_mqtt5_with_properties(mock_mqtt_client):
    await _handle_mqtt5(
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
async def test_handle_mqtt5_retain_with_properties(mock_mqtt_client):
    await _handle_mqtt5(
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
async def test_udp_protocol_uses_injected_handler(mock_mqtt_client):
    handler = AsyncMock()
    protocol = UDPProtocol(handler)
    test_data = "publish test/topic test message".encode('utf-8')
    test_addr = ("127.0.0.1", 1234)

    protocol.datagram_received(test_data, test_addr)
    await asyncio.sleep(0.1)  # Give time for the async task to complete

    handler.assert_called_once_with("publish test/topic test message", test_addr)


@pytest.mark.asyncio
async def test_start_udp_server(mock_mqtt_client):
    mock_transport = MagicMock()
    mock_protocol = MagicMock()

    with patch('asyncio.get_running_loop') as mock_loop:
        mock_loop.return_value = AsyncMock()
        mock_loop.return_value.create_datagram_endpoint = AsyncMock(
            return_value=(mock_transport, mock_protocol)
        )

        transport, protocol = await start_udp_server()

        assert transport == mock_transport
        assert protocol == mock_protocol
        mock_loop.return_value.create_datagram_endpoint.assert_called_once()
