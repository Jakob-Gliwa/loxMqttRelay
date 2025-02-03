import pytest
import pytest_asyncio
import asyncio
import logging
from unittest.mock import AsyncMock, MagicMock, patch, call
from loxmqttrelay.mqtt_client import MQTTClient
from loxmqttrelay.config import (
    Config, BrokerConfig, AppConfig,
    GeneralConfig, global_config
)
from gmqtt import Client as GmqttClient
from gmqtt.mqtt.constants import PubAckReasonCode

logger = logging.getLogger(__name__)

@pytest.fixture(autouse=True)
def mock_args(monkeypatch):
    """Mock command line arguments for all tests"""
    mock_args = MagicMock()
    mock_args.log_level = "DEBUG"
    mock_args.headless = True
    monkeypatch.setattr('loxmqttrelay.utils._args', mock_args)
    return mock_args

@pytest.fixture(autouse=True)
async def cleanup_tasks():
    """Cleanup any pending tasks after each test"""
    yield
    # Get all tasks and cancel them
    tasks = [t for t in asyncio.all_tasks() if t is not asyncio.current_task()]
    for task in tasks:
        task.cancel()
    if tasks:
        await asyncio.gather(*tasks, return_exceptions=True)

@pytest.fixture
def mock_config(monkeypatch):
    config = AppConfig()
    config.broker = BrokerConfig(
        host="test.mosquitto.org",
        port=1883,
        user="testuser",
        password="testpass"
    )
    config.general = GeneralConfig(
        base_topic="test/topic/",
        log_level="DEBUG"
    )
    
    # Patch the global_config
    monkeypatch.setattr('loxmqttrelay.mqtt_client.global_config', config)
    return config

class MockClient:
    def __init__(self):
        self.mock = MagicMock()
        self.mock.is_connected = True
        self.mock.disconnect = AsyncMock()
        self.mock.publish = MagicMock()
        self.mock.subscribe = MagicMock()
        self.mock.set_auth_credentials = MagicMock()
        self.mock.set_config = MagicMock()
        self.mock.connect = AsyncMock()

@pytest_asyncio.fixture
async def mock_client(monkeypatch):
    """Creates a mock client object for gmqtt.Client."""
    mock = MockClient()
    
    def mock_client_factory(*args, **kwargs):
        return mock.mock
    
    monkeypatch.setattr('loxmqttrelay.mqtt_client.Client', mock_client_factory)
    return mock.mock

@pytest_asyncio.fixture
async def mqtt_client(mock_config, mock_client):
    """Creates an MQTTClient and automatically calls disconnect() after the test."""
    client = MQTTClient()
    client.base_topic = mock_config.general.base_topic
    
    # Set up the connect behavior to trigger on_connect
    async def mock_connect(*args, **kwargs):
        client._on_connect(None, None, None, None)
        return None
    mock_client.connect = AsyncMock(side_effect=mock_connect)
    
    try:
        yield client
    finally:
        await client.disconnect()

@pytest.mark.asyncio
async def test_connect(mock_client, mqtt_client):
    """Test successful connection and topic subscription"""
    test_topics = ["test/topic1"]
    callback = AsyncMock()

    await mqtt_client.connect(test_topics, callback)

    mock_client.connect.assert_called_once_with(
        host="test.mosquitto.org",
        port=1883,
        version=4
    )
    mock_client.subscribe.assert_called_with(test_topics[0])
    mock_client.publish.assert_called_with(
        "test/topic/status",
        "Connected"
    )

    await mqtt_client.disconnect()

@pytest.mark.asyncio
async def test_disconnect(mock_client, mqtt_client):
    """Test proper disconnection and cleanup"""
    test_topics = ["test/topic1"]
    callback = AsyncMock()

    await mqtt_client.connect(test_topics, callback)
    await mqtt_client.disconnect()

    mock_client.publish.assert_called_with(
        "test/topic/status",
        "Disconnecting"
    )
    mock_client.disconnect.assert_called_once()

@pytest.mark.asyncio
async def test_subscribe_topics(mock_client, mqtt_client):
    """Test subscription to multiple topics"""
    test_topics = ["test/topic1", "test/topic2"]
    callback = AsyncMock()

    await mqtt_client.connect(test_topics, callback)

    assert mock_client.subscribe.call_count == len(test_topics)
    for topic in test_topics:
        mock_client.subscribe.assert_any_call(topic)

    await mqtt_client.disconnect()

@pytest.mark.asyncio
async def test_publish(mock_client, mqtt_client):
    """Test successful message publication"""
    test_topics = ["test/topic1"]
    callback = AsyncMock()

    await mqtt_client.connect(test_topics, callback)
    mock_client.publish.reset_mock()

    test_topic = "test/topic"
    test_message = "test message"
    await mqtt_client.publish(test_topic, test_message)

    mock_client.publish.assert_called_with(
        test_topic,
        test_message,
        qos=0,
        retain=False
    )

    await mqtt_client.disconnect()

@pytest.mark.asyncio
async def test_publish_without_connection(mock_client, mqtt_client):
    """Test that publishing without connection only logs warning"""
    mock_client.is_connected = False
    await mqtt_client.publish("test/topic", "test message")
    mock_client.publish.assert_not_called()

@pytest.mark.asyncio
async def test_reconnection_delay(mock_client, mqtt_client):
    """Test reconnection delay behavior"""
    test_topics = ["test/topic1"]
    callback = AsyncMock()

    # Set up connect to fail
    mock_client.connect.side_effect = Exception("Connection failed")

    # Create an event to track when sleep is called
    sleep_called = asyncio.Event()
    original_sleep = asyncio.sleep
    
    async def mock_sleep(delay):
        assert delay == 15  # Verify correct delay
        sleep_called.set()
        return await original_sleep(0)  # Return immediately in tests

    with patch('asyncio.sleep', mock_sleep):
        # Start connect but don't wait for it to complete
        connect_task = asyncio.create_task(mqtt_client.connect(test_topics, callback))
        
        # Wait for the first connection attempt and sleep
        try:
            await asyncio.wait_for(sleep_called.wait(), timeout=1.0)
            assert mock_client.connect.call_count == 1
        finally:
            # Cancel the connect task since we don't want infinite retries in test
            connect_task.cancel()
            try:
                await connect_task
            except asyncio.CancelledError:
                pass

@pytest.mark.asyncio
async def test_message_callback(mock_client, mqtt_client):
    """Test message callback handling"""
    test_topics = ["test/topic1"]
    callback = AsyncMock()
    
    await mqtt_client.connect(test_topics, callback)
    
    # Simulate message received
    message = b"test message"
    await mqtt_client._on_message(mock_client, "test/topic1", message, 0, None)
    
    # Add logging to check if the callback is being called
    await asyncio.sleep(0.1)
    
    callback.assert_called_once_with("test/topic1", "test message")

@pytest.mark.asyncio
async def test_message_callback_error(mock_client, mqtt_client):
    """Test message callback error handling"""
    test_topics = ["test/topic1"]
    callback = MagicMock(side_effect=Exception("Callback error"))
    
    await mqtt_client.connect(test_topics, callback)
    
    # Simulate message received
    message = b"test message"
    result = await mqtt_client._on_message(mock_client, "test/topic1", message, 0, None)
    await asyncio.sleep(0.1)
    assert result == PubAckReasonCode.UNSPECIFIED_ERROR
