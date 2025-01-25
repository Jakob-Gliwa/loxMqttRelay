import pytest
from unittest.mock import AsyncMock
from loxmqttrelay.udp_handler import parse_udp_message, handle_udp_message

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

@pytest.mark.asyncio
async def test_handle_udp_message_publish():
    # Test regular publish
    mock_publish = AsyncMock()
    mock_save = AsyncMock()
    
    await handle_udp_message(
        "publish test/topic test message",
        ("127.0.0.1", 1234),
        mock_publish
    )
    
    mock_publish.assert_called_once_with(
        "test/topic",
        "test message",
        False
    )
    mock_save.assert_not_called()

@pytest.mark.asyncio
async def test_handle_udp_message_retain():
    # Test retained message
    mock_publish = AsyncMock()
    mock_save = AsyncMock()
    
    await handle_udp_message(
        "retain test/topic test message",
        ("127.0.0.1", 1234),
        mock_publish
    )
    
    mock_publish.assert_called_once_with(
        "test/topic",
        "test message",
        True
    )
    mock_save.assert_not_called()

@pytest.mark.asyncio
async def test_handle_udp_message_default_publish():
    # Test default publish without command
    mock_publish = AsyncMock()
    mock_save = AsyncMock()
    
    await handle_udp_message(
        "test/topic test message",
        ("127.0.0.1", 1234),
        mock_publish
    )
    
    mock_publish.assert_called_once_with(
        "test/topic",
        "test message",
        False
    )
    mock_save.assert_not_called()

@pytest.mark.asyncio
async def test_handle_udp_message_invalid():
    # Test handling of invalid message
    mock_publish = AsyncMock()
    mock_save = AsyncMock()
    
    await handle_udp_message(
        "invalid",  # Single word message should be treated as invalid
        ("127.0.0.1", 1234),
        mock_publish
    )
    
    mock_publish.assert_not_called()
    mock_save.assert_not_called()

@pytest.mark.asyncio
async def test_handle_udp_message_empty():
    # Test handling of empty message
    mock_publish = AsyncMock()
    mock_save = AsyncMock()
    
    await handle_udp_message(
        "",
        ("127.0.0.1", 1234),
        mock_publish
    )
    
    mock_publish.assert_not_called()
    mock_save.assert_not_called()

@pytest.mark.asyncio
async def test_handle_udp_message_with_special_chars():
    # Test handling of messages with special characters
    mock_publish = AsyncMock()
    mock_save = AsyncMock()
    
    await handle_udp_message(
        "publish test/topic/path message/with/slashes",
        ("127.0.0.1", 1234),
        mock_publish
    )
    
    mock_publish.assert_called_once_with(
        "test/topic/path",
        "message/with/slashes",
        False
    )
    mock_save.assert_not_called()
