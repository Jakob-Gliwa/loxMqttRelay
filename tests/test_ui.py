import pytest
from unittest.mock import Mock, patch, MagicMock, AsyncMock
import os
from pathlib import Path
import asyncio
import tomlkit
from gmqtt.mqtt.constants import MQTTv311
from typing import AsyncGenerator, Generator, Any
import types

from loxmqttrelay.main import MQTTRelay
from loxmqttrelay.config import Config, global_config, AppConfig
from loxmqttrelay.compatible._loxmqttrelay import MiniserverDataProcessor

@pytest.fixture(autouse=True)
def reset_singletons() -> Generator[None, None, None]:
    """Reset all singletons before and after each test"""
    Config._instance = None
    MiniserverDataProcessor._instance = None  # type: ignore
    yield
    Config._instance = None
    MiniserverDataProcessor._instance = None  # type: ignore

@pytest.fixture
def mock_args(monkeypatch: pytest.MonkeyPatch) -> MagicMock:
    """Mock command line arguments"""
    mock_args = MagicMock()
    mock_args.headless = False
    mock_args.log_level = "INFO"
    monkeypatch.setattr('loxmqttrelay.utils._args', mock_args)
    return mock_args

@pytest.fixture
def mock_config(monkeypatch: pytest.MonkeyPatch) -> AppConfig:
    config = AppConfig()
    config.general.base_topic = "test/topic/"
    
    # Patch the global_config
    monkeypatch.setattr('loxmqttrelay.main.global_config', config)
    return config

@pytest.fixture
def mock_subprocess() -> Generator[MagicMock, None, None]:
    with patch('subprocess.Popen') as mock_popen:
        # Configure mock process
        mock_process = Mock()
        mock_process.poll.return_value = None  # Process is running
        mock_process.terminate.return_value = None
        mock_process.wait.return_value = 0
        mock_popen.return_value = mock_process
        yield mock_popen

@pytest.fixture
def mock_mqtt_client(monkeypatch: pytest.MonkeyPatch) -> MagicMock:
    mock_instance = Mock()
    mock_instance.publish = AsyncMock()
    monkeypatch.setattr('loxmqttrelay.main.mqtt_client', mock_instance)
    return mock_instance

@pytest.fixture
def mock_data_processor(monkeypatch: pytest.MonkeyPatch) -> MagicMock:
    """Mock MiniserverDataProcessor"""
    processor = MagicMock()
    processor.return_value = processor  # Make the mock work as both class and instance
    monkeypatch.setattr('loxmqttrelay.main.MiniserverDataProcessor', processor)
    return processor

@pytest.fixture
def sample_toml_config() -> str:
    """Create a sample TOML config for testing"""
    config = {
        'broker': {
            'host': 'test.mosquitto.org',
            'port': 1883,
            'user': 'test_user',
            'password': 'test_pass',
            'client_id': 'test_client'
        },
        'general': {
            'base_topic': 'test/',
            'log_level': 'INFO',
            'cache_size': 100000
        },
        'udp': {
            'udp_in_port': 11884
        },
        'processing': {
            'expand_json': False,
            'convert_booleans': False
        },
        'miniserver': {
            'miniserver_ip': '127.0.0.1',
            'miniserver_port': 80,
            'miniserver_user': '',
            'miniserver_pass': '',
            'miniserver_max_parallel_connections': 5,
            'use_websocket': True,
            'sync_with_miniserver': False
        },
        'debug': {
            'mock_ip': '',
            'enable_mock': False
        },
        'topics': {
            'subscriptions': ['topic1', 'topic2'],
            'subscription_filters': ['^ignore/before/.*'],
            'do_not_forward': [],
            'topic_whitelist': ['whitelist_topic']
        }
    }
    return tomlkit.dumps(config)

@pytest.fixture
def mock_topic(monkeypatch: pytest.MonkeyPatch) -> types.SimpleNamespace:
    """Mock TOPIC constant"""
    topic = types.SimpleNamespace(
        START_UI="test/startui",
        STOP_UI="test/stopui",
        MINISERVER_STARTUP_EVENT="test/miniserver/startup",
        CONFIG_GET="test/config/get",
        CONFIG_RESPONSE="test/config/response",
        CONFIG_SET="test/config/set",
        CONFIG_ADD="test/config/add",
        CONFIG_REMOVE="test/config/remove",
        CONFIG_UPDATE="test/config/update",
        CONFIG_RESTART="test/config/restart",
        UI_STATUS="test/ui/status"
    )
    monkeypatch.setattr('loxmqttrelay.main.TOPIC', topic)
    return topic

@pytest.mark.asyncio
async def test_ui_starts_properly(
    mock_subprocess: MagicMock,
    mock_mqtt_client: MagicMock,
    mock_config: AppConfig,
    mock_data_processor: MagicMock,
    mock_args: MagicMock,
    mock_topic: types.SimpleNamespace
) -> None:
    # Create relay instance
    relay = MQTTRelay()
    
    # Start UI
    await relay.start_ui()
    
    # Verify Streamlit was called correctly
    mock_subprocess.assert_called_once()
    call_args = mock_subprocess.call_args[0][0]  # type: ignore
    assert call_args[0] == "streamlit"
    assert call_args[1] == "run"
    assert os.path.basename(call_args[2]) == "ui.py"
    
    # Verify UI path is absolute and points to correct file
    ui_path = call_args[2]
    assert os.path.isabs(ui_path)
    assert Path(ui_path).exists()
    
    # Verify MQTT status message was published
    mock_mqtt_client.publish.assert_called_once()
    topic = mock_mqtt_client.publish.call_args[0][0]  # type: ignore
    message = mock_mqtt_client.publish.call_args[0][1]  # type: ignore
    assert "ui/status" in topic
    assert "UI started successfully" in message

@pytest.mark.asyncio
async def test_ui_stops_properly(
    mock_subprocess: MagicMock,
    mock_mqtt_client: MagicMock,
    mock_config: AppConfig,
    mock_data_processor: MagicMock,
    mock_args: MagicMock,
    mock_topic: types.SimpleNamespace
) -> None:
    # Create relay instance and start UI
    relay = MQTTRelay()
    await relay.start_ui()
    
    # Stop UI
    await relay.stop_ui()
    
    # Verify process was terminated
    mock_process = mock_subprocess.return_value
    mock_process.terminate.assert_called_once()
    mock_process.wait.assert_called_once()
    
    # Verify MQTT status message was published
    assert mock_mqtt_client.publish.call_count == 2  # Start and stop messages
    stop_message = mock_mqtt_client.publish.call_args[0][1]  # type: ignore
    assert "UI stopped successfully" in stop_message

@pytest.mark.asyncio
async def test_ui_already_running(
    mock_subprocess: MagicMock,
    mock_mqtt_client: MagicMock,
    mock_config: AppConfig,
    mock_data_processor: MagicMock,
    mock_args: MagicMock,
    mock_topic: types.SimpleNamespace
) -> None:
    # Create relay instance
    relay = MQTTRelay()
    
    # Start UI twice
    await relay.start_ui()
    await relay.start_ui()
    
    # Verify Streamlit was only called once
    mock_subprocess.assert_called_once()
    
    # Verify appropriate MQTT messages were published
    assert mock_mqtt_client.publish.call_count == 2
    second_message = mock_mqtt_client.publish.call_args[0][1]  # type: ignore
    assert "UI is already running" in second_message

@pytest.mark.asyncio
async def test_ui_not_running_when_stopping(
    mock_subprocess: MagicMock,
    mock_mqtt_client: MagicMock,
    mock_config: AppConfig,
    mock_data_processor: MagicMock,
    mock_args: MagicMock,
    mock_topic: types.SimpleNamespace
) -> None:
    # Create relay instance without starting UI
    relay = MQTTRelay()
    
    # Try to stop UI
    await relay.stop_ui()
    
    # Verify process was not terminated (since it wasn't running)
    mock_process = mock_subprocess.return_value
    mock_process.terminate.assert_not_called()
    
    # Verify appropriate MQTT message was published
    mock_mqtt_client.publish.assert_called_once()
    message = mock_mqtt_client.publish.call_args[0][1]  # type: ignore
    assert "UI is not running" in message

@pytest.mark.asyncio
async def test_ui_does_not_start_in_headless_mode(
    mock_subprocess: MagicMock,
    mock_mqtt_client: MagicMock,
    mock_config: AppConfig,
    mock_data_processor: MagicMock,
    mock_args: MagicMock,
    mock_topic: types.SimpleNamespace
) -> None:
    # Set headless mode
    mock_args.headless = True
    
    # Create relay instance
    relay = MQTTRelay()
    
    # Try to start UI
    await relay.start_ui()
    
    # Verify Streamlit was not called
    mock_subprocess.assert_not_called()
    
    # Verify no MQTT messages were published
    mock_mqtt_client.publish.assert_not_called()

@pytest.mark.asyncio
async def test_restart_relay_command(
    mock_mqtt_client: MagicMock,
    mock_config: AppConfig,
    sample_toml_config: str
) -> None:
    """Test the restart relay command with gmqtt"""
    from loxmqttrelay.ui import restart_relay
    
    # Parse the sample config
    config = tomlkit.loads(sample_toml_config)
    
    # Mock gmqtt client
    with patch('loxmqttrelay.ui.MQTTClient') as mock_gmqtt:
        mock_client = AsyncMock()
        mock_gmqtt.return_value = mock_client
        
        # Call restart_relay
        await restart_relay(config)
        
        # Verify client was created with correct ID pattern
        client_id = mock_gmqtt.call_args[0][0]
        assert client_id.startswith('loxberry_ui_')
        
        # Verify auth credentials were set
        mock_client.set_auth_credentials.assert_called_once_with('test_user', 'test_pass')
        
        # Verify connect was called with correct parameters
        mock_client.connect.assert_called_once_with(
            'test.mosquitto.org',
            port=1883,
            version=MQTTv311
        )
        
        # Verify publish was called with correct parameters
        mock_client.publish.assert_called_once_with(
            'test/config/restart',
            b'',
            qos=1,
            retain=False
        )
        
        # Verify disconnect was called
        mock_client.disconnect.assert_called_once()

@pytest.mark.asyncio
async def test_restart_relay_no_auth(
    mock_mqtt_client: MagicMock,
    mock_config: AppConfig,
    sample_toml_config: str
) -> None:
    """Test restart relay without auth credentials"""
    from loxmqttrelay.ui import restart_relay
    
    # Parse the sample config
    config = tomlkit.loads(sample_toml_config)
    config['broker']['user'] = ''  # type: ignore
    config['broker']['password'] = ''  # type: ignore
    
    # Mock gmqtt client
    with patch('loxmqttrelay.ui.MQTTClient') as mock_gmqtt:
        mock_client = AsyncMock()
        mock_gmqtt.return_value = mock_client
        
        # Call restart_relay
        await restart_relay(config)
        
        # Verify auth credentials were not set
        mock_client.set_auth_credentials.assert_not_called()
        
        # Verify other calls were made
        mock_client.connect.assert_called_once()
        mock_client.publish.assert_called_once()
        mock_client.disconnect.assert_called_once()

@pytest.mark.asyncio
async def test_restart_relay_connection_retry(
    mock_mqtt_client: MagicMock,
    mock_config: AppConfig,
    sample_toml_config: str
) -> None:
    """Test restart relay connection retry logic"""
    from loxmqttrelay.ui import restart_relay
    
    config = tomlkit.loads(sample_toml_config)
    
    # Mock gmqtt client with connection failure
    with patch('loxmqttrelay.ui.MQTTClient') as mock_gmqtt:
        mock_client = AsyncMock()
        mock_client.connect.side_effect = [
            Exception("Connection failed"),  # First attempt fails
            None  # Second attempt succeeds
        ]
        mock_gmqtt.return_value = mock_client
        
        # Call restart_relay
        await restart_relay(config)
        
        # Verify connect was called twice
        assert mock_client.connect.call_count == 2
        
        # Verify publish and disconnect were called after successful connection
        mock_client.publish.assert_called_once()
        mock_client.disconnect.assert_called_once()
