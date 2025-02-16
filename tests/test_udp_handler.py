import pytest
import pytest_asyncio
import asyncio
from unittest.mock import AsyncMock, MagicMock, patch
from loxmqttrelay.udp_handler import parse_udp_message, handle_udp_message, UDPProtocol, start_udp_server

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
    ("publish topic6 message with spaces", ("publish", "topic6", "message with spaces")),
    ("topic7 message with spaces", ("publish", "topic7", "message with spaces")),
    
    # Test invalid formats - should return None
    ("single", None),
    ("", None),
    
    # Test messages with special characters
    ("publish topic/with/slashes message/with/slashes", 
     ("publish", "topic/with/slashes", "message/with/slashes")),
    
    # Test messages with leading/trailing spaces
    ("  publish  topic8  message8  ", ("publish", "topic8", "message8")),
    ("  topic9  message9  ", ("publish", "topic9", "message9")),
])
def test_parse_udp_message(udp_message, expected):
    result = parse_udp_message(udp_message)
    assert result == expected

@pytest.fixture
def mock_mqtt_client(monkeypatch):
    mock_client = AsyncMock()
    mock_client.publish = AsyncMock()
    monkeypatch.setattr('loxmqttrelay.udp_handler.mqtt_client', mock_client)
    return mock_client

@pytest.mark.asyncio
async def test_handle_udp_message_publish(mock_mqtt_client):
    # Test regular publish
    await handle_udp_message(
        "publish test/topic test message",
        ("127.0.0.1", 1234)
    )
    
    mock_mqtt_client.publish.assert_called_once_with(
        "test/topic",
        "test message",
        False
    )

@pytest.mark.asyncio
async def test_handle_udp_message_retain(mock_mqtt_client):
    # Test retained message
    await handle_udp_message(
        "retain test/topic test message",
        ("127.0.0.1", 1234)
    )
    
    mock_mqtt_client.publish.assert_called_once_with(
        "test/topic",
        "test message",
        True
    )

@pytest.mark.asyncio
async def test_handle_udp_message_default_publish(mock_mqtt_client):
    # Test default publish without command
    await handle_udp_message(
        "test/topic test message",
        ("127.0.0.1", 1234)
    )
    
    mock_mqtt_client.publish.assert_called_once_with(
        "test/topic",
        "test message",
        False
    )

@pytest.mark.asyncio
async def test_handle_udp_message_invalid(mock_mqtt_client):
    # Test handling of invalid message
    await handle_udp_message(
        "invalid",  # Single word message should be treated as invalid
        ("127.0.0.1", 1234)
    )
    
    mock_mqtt_client.publish.assert_not_called()

@pytest.mark.asyncio
async def test_handle_udp_message_empty(mock_mqtt_client):
    # Test handling of empty message
    await handle_udp_message(
        "",
        ("127.0.0.1", 1234)
    )
    
    mock_mqtt_client.publish.assert_not_called()

@pytest.mark.asyncio
async def test_handle_udp_message_with_special_chars(mock_mqtt_client):
    # Test handling of messages with special characters
    await handle_udp_message(
        "publish test/topic/path message/with/slashes",
        ("127.0.0.1", 1234)
    )
    
    mock_mqtt_client.publish.assert_called_once_with(
        "test/topic/path",
        "message/with/slashes",
        False
    )

@pytest.mark.asyncio
async def test_udp_protocol(mock_mqtt_client):
    protocol = UDPProtocol()
    test_data = "publish test/topic test message".encode('utf-8')
    test_addr = ("127.0.0.1", 1234)
    
    # Call datagram_received and wait for the task to complete
    protocol.datagram_received(test_data, test_addr)
    await asyncio.sleep(0.1)  # Give time for the async task to complete
    
    mock_mqtt_client.publish.assert_called_once_with(
        "test/topic",
        "test message",
        False
    )

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
