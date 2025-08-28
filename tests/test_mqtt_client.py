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
    
    callback.assert_called_once_with("test/topic1", b"test message")

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

class TestDownstreamBinaryDataFlow:
    """Test cases for complete downstream data flow with binary data in MQTT client"""
    
    @pytest.mark.asyncio
    async def test_binary_data_flows_to_callback(self, mock_client, mqtt_client):
        """Test that binary data flows correctly to the callback function"""
        test_topics = ["test/binary_flow"]
        callback = AsyncMock()
        
        await mqtt_client.connect(test_topics, callback)
        
        # Test with zlib compressed data (like the problematic data from the error)
        binary_message = bytes([120, 156, 165, 125, 217, 142, 158, 201, 145, 221, 187, 212, 245, 47])
        
        await mqtt_client._on_message(mock_client, "test/binary_flow", binary_message, 0, None)
        await asyncio.sleep(0.1)
        
        # Callback should be called with the exact binary data
        callback.assert_called_once_with("test/binary_flow", binary_message)
        
        # Verify the binary data is preserved exactly
        call_args = callback.call_args
        assert call_args[0][1] == binary_message
        assert len(call_args[0][1]) == len(binary_message)
    
    @pytest.mark.asyncio
    async def test_binary_data_preserves_exact_bytes(self, mock_client, mqtt_client):
        """Test that binary data preserves exact byte values through the pipeline"""
        test_topics = ["test/byte_preservation"]
        callback = AsyncMock()
        
        await mqtt_client.connect(test_topics, callback)
        
        # Create binary data with specific byte patterns
        binary_data = bytes([0x00, 0xFF, 0x01, 0xFE, 0x02, 0xFD])
        
        await mqtt_client._on_message(mock_client, "test/byte_preservation", binary_data, 0, None)
        await asyncio.sleep(0.1)
        
        # Verify exact byte preservation
        call_args = callback.call_args
        received_data = call_args[0][1]
        
        assert received_data == binary_data
        assert received_data[0] == 0x00
        assert received_data[1] == 0xFF
        assert received_data[2] == 0x01
        assert received_data[3] == 0xFE
        assert received_data[4] == 0x02
        assert received_data[5] == 0xFD
    
    @pytest.mark.asyncio
    async def test_binary_data_with_null_bytes(self, mock_client, mqtt_client):
        """Test that binary data with null bytes is handled correctly"""
        test_topics = ["test/null_bytes"]
        callback = AsyncMock()
        
        await mqtt_client.connect(test_topics, callback)
        
        # Binary data with null bytes
        null_data = bytes([0x48, 0x65, 0x6C, 0x6C, 0x6F, 0x00, 0x57, 0x6F, 0x72, 0x6C, 0x64])
        
        await mqtt_client._on_message(mock_client, "test/null_bytes", null_data, 0, None)
        await asyncio.sleep(0.1)
        
        # Verify null bytes are preserved
        call_args = callback.call_args
        received_data = call_args[0][1]
        
        assert received_data == null_data
        assert received_data[5] == 0x00  # null byte
    
    @pytest.mark.asyncio
    async def test_binary_data_compression_signatures(self, mock_client, mqtt_client):
        """Test that compression signatures are preserved exactly"""
        test_topics = ["test/compression"]
        callback = AsyncMock()
        
        await mqtt_client.connect(test_topics, callback)
        
        # Test various compression signatures
        compression_signatures = {
            "zlib": bytes([120, 156, 165, 125, 217, 142]),
            "gzip": bytes([0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
            "png": bytes([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]),
            "jpeg": bytes([0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46])
        }
        
        for format_name, signature in compression_signatures.items():
            callback.reset_mock()
            
            await mqtt_client._on_message(mock_client, f"test/{format_name}", signature, 0, None)
            await asyncio.sleep(0.1)
            
            # Verify signature is preserved exactly
            call_args = callback.call_args
            received_data = call_args[0][1]
            
            assert received_data == signature, f"{format_name} signature not preserved"
            assert len(received_data) == len(signature), f"{format_name} length mismatch"
    
    @pytest.mark.asyncio
    async def test_binary_data_encryption_headers(self, mock_client, mqtt_client):
        """Test that encryption headers are preserved exactly"""
        test_topics = ["test/encryption"]
        callback = AsyncMock()
        
        await mqtt_client.connect(test_topics, callback)
        
        # Simulate encrypted data with headers
        encrypted_data = bytes([
            0x45, 0x4E, 0x43, 0x52,  # "ENCR" header
            0x01, 0x00,              # Version 1.0
            0x00, 0x00, 0x00, 0x20,  # 32 bytes of encrypted data
            0xDE, 0xAD, 0xBE, 0xEF,  # Sample encrypted bytes
            0xCA, 0xFE, 0xBA, 0xBE,
            0xDE, 0xAD, 0xBE, 0xEF,
            0xCA, 0xFE, 0xBA, 0xBE,
            0xDE, 0xAD, 0xBE, 0xEF,
            0xCA, 0xFE, 0xBA, 0xBE,
            0xDE, 0xAD, 0xBE, 0xEF,
            0xCA, 0xFE, 0xBA, 0xBE
        ])
        
        await mqtt_client._on_message(mock_client, "test/encryption", encrypted_data, 0, None)
        await asyncio.sleep(0.1)
        
        # Verify encryption headers are preserved
        call_args = callback.call_args
        received_data = call_args[0][1]
        
        assert received_data == encrypted_data
        assert received_data[:4] == b"ENCR"  # Header preserved
        assert received_data[4:6] == bytes([0x01, 0x00])  # Version preserved
        assert len(received_data) == 42  # Total length preserved
    
    @pytest.mark.asyncio
    async def test_binary_data_protocol_buffers(self, mock_client, mqtt_client):
        """Test that protocol buffer data is preserved exactly"""
        test_topics = ["test/protobuf"]
        callback = AsyncMock()
        
        await mqtt_client.connect(test_topics, callback)
        
        # Simulate protobuf data
        protobuf_data = bytes([
            0x08, 0x96, 0x01,        # Field 1, wire type 0, value 150
            0x12, 0x07, 0x74, 0x65, 0x73, 0x74, 0x69, 0x6E, 0x67,  # Field 2, wire type 2, string "testing"
            0x18, 0x01,               # Field 3, wire type 0, value 1
            0x20, 0x00                # Field 4, wire type 0, value 0
        ])
        
        await mqtt_client._on_message(mock_client, "test/protobuf", protobuf_data, 0, None)
        await asyncio.sleep(0.1)
        
        # Verify protobuf data is preserved
        call_args = callback.call_args
        received_data = call_args[0][1]
        
        assert received_data == protobuf_data
        assert received_data[0] == 0x08  # Field 1 tag
        assert received_data[1] == 0x96  # Field 1 value
        assert received_data[2] == 0x01  # Field 1 value (continued)
    
    @pytest.mark.asyncio
    async def test_binary_data_messagepack(self, mock_client, mqtt_client):
        """Test that MessagePack data is preserved exactly"""
        test_topics = ["test/msgpack"]
        callback = AsyncMock()
        
        await mqtt_client.connect(test_topics, callback)
        
        # Simulate MessagePack data
        msgpack_data = bytes([
            0x83,                     # Map with 3 key-value pairs
            0xA3, 0x6B, 0x65, 0x79,  # String "key" (3 bytes)
            0xA5, 0x76, 0x61, 0x6C, 0x75, 0x65,  # String "value" (5 bytes)
            0xA4, 0x6E, 0x75, 0x6D, 0x62,  # String "numb" (4 bytes)
            0x2A,                     # Integer 42
            0xA4, 0x62, 0x6F, 0x6F, 0x6C,  # String "bool" (4 bytes)
            0xC3                      # Boolean true
        ])
        
        await mqtt_client._on_message(mock_client, "test/msgpack", msgpack_data, 0, None)
        await asyncio.sleep(0.1)
        
        # Verify MessagePack data is preserved
        call_args = callback.call_args
        received_data = call_args[0][1]
        
        assert received_data == msgpack_data
        assert received_data[0] == 0x83  # Map header
        assert received_data[1] == 0xA3  # String "key" header
        assert received_data[2:5] == b"key"  # String "key"
    
    @pytest.mark.asyncio
    async def test_binary_data_flow_with_different_qos(self, mock_client, mqtt_client):
        """Test that binary data flows correctly with different QoS levels"""
        test_topics = ["test/qos_flow"]
        callback = AsyncMock()
        
        await mqtt_client.connect(test_topics, callback)
        
        binary_message = bytes([120, 156, 165, 125, 217, 142])
        
        # Test QoS 0
        await mqtt_client._on_message(mock_client, "test/qos_flow", binary_message, 0, None)
        await asyncio.sleep(0.1)
        
        # Test QoS 1
        await mqtt_client._on_message(mock_client, "test/qos_flow", binary_message, 1, None)
        await asyncio.sleep(0.1)
        
        # Test QoS 2
        await mqtt_client._on_message(mock_client, "test/qos_flow", binary_message, 2, None)
        await asyncio.sleep(0.1)
        
        # Verify all calls preserved the binary data
        assert callback.call_count == 3
        for call_args in callback.call_args_list:
            received_data = call_args[0][1]
            assert received_data == binary_message
            assert len(received_data) == len(binary_message)
